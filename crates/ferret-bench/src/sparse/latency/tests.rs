//! Disk execution must preserve the established volume selection and actual
//! oracle identities; attribution and byte accounting use the real reader.

use super::super::{add_document, encode};
use super::*;
use crate::support::Fixture;
use ferret_verify::GrepFlags;

fn disk_index(
    dir: &Path,
    documents: &[&[u8]],
    variant: Variant,
) -> (OwnedIndex, Lists, Segment<Meter>) {
    let mut lists = Lists::new();
    for (doc, bytes) in documents.iter().enumerate() {
        add_document(&mut lists, doc as u32, bytes, variant, &[], None);
    }
    let (owned, mut file) = OwnedIndex::create(dir, &variant.label()).unwrap();
    let mut writer = Writer::keyed(0, documents.len() as u32, 1).unwrap();
    let mut ordered: Vec<_> = lists.iter().collect();
    ordered.sort_unstable_by_key(|(key, _)| *key);
    for (key, docs) in ordered {
        writer.push(key, docs).unwrap();
    }
    let sizes = writer.finish(&mut file).unwrap();
    file.sync_all().unwrap();
    assert_eq!(
        sizes.total(),
        encode(&lists, documents.len() as u32 + 1).unwrap().total()
    );
    let segment = Meter::open(&owned.0).unwrap();
    (owned, lists, segment)
}

#[test]
fn disk_cursors_preserve_volume_selection_and_all_oracle_matches() {
    // A cursor implementation or selection mismatch can silently change
    // the workload. Include literal edges, case folding and disjoint OR.
    let fixture = Fixture::new("sparse-latency-cursors");
    let dir = fixture.out("files");
    fs::create_dir_all(&dir).unwrap();
    let documents: [&[u8]; 7] = [
        b"xabcdefy",
        b"zabcdefw",
        b"abc bcd cde def",
        b"UVWXYZ",
        b"abcdefgh",
        "Kilo".as_bytes(),
        b"none",
    ];
    let ids: Vec<_> = (0..8).collect();
    let live = DocSet::new(8, ids.iter().copied());
    for variant in [Variant::Word4, Variant::Word45] {
        let (owned, lists, segment) = disk_index(&dir, &documents, variant);
        for pattern in [
            "abcdef",
            "abcdef|uvwxyz",
            "abcdefgh|x",
            "kilo",
            "abc def",
            "a{5}",
        ] {
            let grep = GrepFlags::default().compile(pattern).unwrap();
            let facts = GrepFlags::default().required_literals(pattern).unwrap();
            let tree = Need::literals(&facts, variant, &[]);
            let plan = tree.selected(&|key| lists.get(key).map_or(0, Vec::len), ids.len());
            let mut expected = tree.candidates(&lists, &ids);
            expected.push(7);
            expected.sort_unstable();
            expected.dedup();
            let run = read_postings(&segment, &plan, &live, &[7]).unwrap();
            assert_eq!(run.docs, expected);
            let oracle: Vec<_> = documents
                .iter()
                .enumerate()
                .filter(|(_, bytes)| grep.regex().is_match(bytes))
                .map(|(doc, _)| doc as u32)
                .collect();
            assert!(oracle.iter().all(|doc| run.docs.binary_search(doc).is_ok()));
        }
        drop(segment);
        owned.remove().unwrap();
    }
}

#[test]
fn posting_byte_meter_counts_reloaded_chunks_and_inline_singletons() {
    // Each warm run must reread encoded lists; a retained decoded list would
    // report zero I/O. Inline singletons cost dictionary bytes, not postings.
    let fixture = Fixture::new("sparse-latency-meter");
    let dir = fixture.out("files");
    fs::create_dir_all(&dir).unwrap();
    let (owned, _, segment) = disk_index(&dir, &[b"abcdef", b"abcghi"], Variant::Word4);
    let live = DocSet::new(2, 0..2);
    let shared = Need::Key(b"abc".to_vec());
    let a = read_postings(&segment, &shared, &live, &[]).unwrap();
    segment.source().evict().unwrap();
    let b = read_postings(&segment, &shared, &live, &[]).unwrap();
    assert_eq!(a.docs, [0, 1]);
    assert_eq!(a.docs, b.docs);
    assert!(a.postings_bytes > 0);
    assert_eq!(a.postings_bytes, b.postings_bytes);
    assert!(a.index_bytes > a.postings_bytes);
    assert_eq!(a.index_bytes, b.index_bytes);
    let single = read_postings(&segment, &Need::Key(b"def".to_vec()), &live, &[]).unwrap();
    assert_eq!(single.docs, [0]);
    assert_eq!(single.postings_bytes, 0);
    assert!(single.index_bytes > 0);
    drop(segment);
    owned.remove().unwrap();
}

#[test]
fn verification_attributes_real_reads_and_matches_at_band_boundaries() {
    // Using candidate sizes as read bytes, or treating 1/16 MiB as the lower
    // band, misattributes the benefit and includes unreadable documents.
    let fixture = Fixture::new("sparse-latency-verify");
    let dir = fixture.out("files");
    fs::create_dir_all(&dir).unwrap();
    let sizes = [6, 1 << 20, 16 << 20, 19];
    for (doc, &size) in sizes[..3].iter().enumerate() {
        let mut bytes = vec![b'q'; size as usize];
        if doc < 2 {
            bytes[..6].copy_from_slice(b"ABCDEF");
        }
        fs::write(dir.join(doc.to_string()), bytes).unwrap();
    }
    let grep = GrepFlags::default().compile("abcdef").unwrap();
    let (timing, matches) = verify(
        &grep,
        &[0, 1, 2, 3],
        &sizes,
        &[true, true, true, false],
        &|doc| dir.join(doc.to_string()),
    )
    .unwrap();
    assert_eq!(matches, [0, 1]);
    assert_eq!(timing[0].verified_bytes, 6 + (1 << 20) + (16 << 20));
    for (b, &size) in sizes[..3].iter().enumerate() {
        assert_eq!(timing[b + 1].verified_bytes, size);
    }
    assert!(timing[0].verify >= timing[1..].iter().map(|t| t.verify).sum());
}

#[test]
fn intermediate_cleanup_cannot_remove_a_preexisting_file() {
    // The ownership guard must be created only after create_new succeeds.
    let fixture = Fixture::new("sparse-latency-ownership");
    let dir = fixture.out("files");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("word-fixed-4.{}.segment", std::process::id()));
    fs::write(&path, b"preexisting").unwrap();
    assert!(OwnedIndex::create(&dir, "word-fixed-4").is_err());
    assert_eq!(fs::read(&path).unwrap(), b"preexisting");
    let (owned, file) = OwnedIndex::create(&dir, "word-fixed-4+5").unwrap();
    let own_path = owned.0.clone();
    drop(file);
    drop(owned);
    assert!(!own_path.exists());
    assert!(path.exists());
}
