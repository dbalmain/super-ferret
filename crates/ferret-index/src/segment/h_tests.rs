//! H regression cases drive the production writer, term cursor and spilling
//! merge. The oracle retains strings that codec 2 deliberately discards.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::{Certainty, Cursor, DocSet, postings::Term};

fn build(first: u32, last: u32, terms: &BTreeMap<Vec<u8>, Vec<u32>>) -> Segment<Vec<u8>> {
    let mut writer = Writer::with_codec(first, last, FINGERPRINT_CODEC).unwrap();
    for (term, docs) in terms {
        writer.push(term, docs).unwrap();
    }
    let mut bytes = Vec::new();
    writer.finish(&mut bytes).unwrap();
    Segment::open(bytes).unwrap()
}

fn candidates(segments: &[Segment<Vec<u8>>], term: &[u8]) -> Vec<(u32, Certainty)> {
    let term = Term::read(segments, term).unwrap();
    let mut cursor = Cursor::Postings(Box::new(term.cursor()));
    let mut found = Vec::new();
    let mut target = 0;
    while let Some((doc, certainty)) = cursor.next_geq(target) {
        found.push((doc, certainty));
        target = doc + 1;
    }
    found
}

fn merged(inputs: &[Segment<Vec<u8>>], live: &DocSet) -> Segment<Vec<u8>> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ferret-h-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&dir).unwrap();
    let path = dir.join("segment");
    let scratch = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(dir.join("scratch"))
        .unwrap();
    let writer = Writer::spilling_with_codec(
        inputs[0].info().first,
        inputs.last().unwrap().info().last,
        FINGERPRINT_CODEC,
        File::create(&path).unwrap(),
        scratch,
    )
    .unwrap();
    crate::merge::stream(inputs, live, writer, &|_| Ok(()), &|| false).unwrap();
    let result = Segment::open(fs::read(path).unwrap()).unwrap();
    fs::remove_dir_all(dir).unwrap();
    result
}

struct Forced;
impl Forced {
    fn new(function: fn(&[u8]) -> u32) -> Self {
        fingerprint::FORCED.with(|forced| forced.set(Some(function)));
        Self
    }
}
impl Drop for Forced {
    fn drop(&mut self) {
        fingerprint::FORCED.with(|forced| forced.set(None));
    }
}
fn numbered(term: &[u8]) -> u32 {
    std::str::from_utf8(term).unwrap().parse().unwrap()
}

#[test]
fn h_run_starts_at_previous_blocks_last_record_and_spans_four_blocks() {
    let _forced = Forced::new(numbered);
    let mut terms = BTreeMap::new();
    for fp in 0..127 {
        terms.insert(format!("{fp:04}").into_bytes(), vec![fp]);
    }
    let mut writer = Writer::with_codec(0, 600, FINGERPRINT_CODEC).unwrap();
    for (term, docs) in &terms {
        writer.push(term, docs).unwrap();
    }
    // The run starts at record 127, the last record of the predecessor.
    for doc in 127..550 {
        writer.push_record(127, doc).unwrap();
    }
    writer.push_record(128, 550).unwrap();
    let mut bytes = Vec::new();
    writer.finish(&mut bytes).unwrap();
    let segment = Segment::open(bytes).unwrap();
    let found = candidates(&[segment], b"0127");
    assert_eq!(
        found,
        (127..550)
            .map(|doc| (doc, Certainty::Maybe))
            .collect::<Vec<_>>()
    );
}

#[test]
fn h_empty_section_and_lower_bound_at_end() {
    let _forced = Forced::new(numbered);
    let empty = build(0, 9, &BTreeMap::from([(b"0002".to_vec(), vec![0, 1])]));
    assert_eq!(empty.fingerprint_records(), 0);
    assert!(empty.fingerprint_docs(b"0000").unwrap().is_empty());
    assert!(empty.fingerprint_docs(b"9999").unwrap().is_empty());
    let singleton = build(10, 19, &BTreeMap::from([(b"0002".to_vec(), vec![10])]));
    assert_eq!(singleton.fingerprint_docs(b"0002").unwrap(), [10]);
    assert!(singleton.fingerprint_docs(b"0000").unwrap().is_empty());
    assert!(singleton.fingerprint_docs(b"9999").unwrap().is_empty());
}

#[test]
fn h_dictionary_front_coding_ignores_removed_singletons() {
    let terms = BTreeMap::from([
        (b"aaa".to_vec(), vec![0, 1]),
        (b"aab".to_vec(), vec![2]),
        (b"aac".to_vec(), vec![0, 2]),
    ]);
    let segment = build(0, 2, &terms);
    assert!(segment.entry(b"aab").unwrap().is_none());
    let mut stream = segment.terms();
    assert_eq!(stream.next_term().unwrap().unwrap().0, b"aaa");
    assert_eq!(stream.next_term().unwrap().unwrap().0, b"aac");
    assert!(stream.next_term().unwrap().is_none());
}

#[test]
fn h_mixed_dictionary_and_fingerprint_occurrences_survive_merges_and_purge() {
    let first = build(0, 0, &BTreeMap::from([(b"shared".to_vec(), vec![0])]));
    let second = build(1, 3, &BTreeMap::from([(b"shared".to_vec(), vec![1, 2, 3])]));
    let inputs = [first, second];
    // A dictionary hit must never suppress the other segment's singleton.
    assert_eq!(
        candidates(&inputs, b"shared"),
        [
            (0, Certainty::Maybe),
            (1, Certainty::Yes),
            (2, Certainty::Yes),
            (3, Certainty::Yes)
        ]
    );
    let full = merged(&inputs, &DocSet::new(4, 0..4));
    assert_eq!(full.fingerprint_records(), 1);
    let full = [full];
    assert_eq!(candidates(&full, b"shared"), candidates(&inputs, b"shared"));
    let purged = merged(&full, &DocSet::new(4, [0, 3]));
    assert!(purged.entry(b"shared").unwrap().is_none());
    assert_eq!(
        candidates(&[purged], b"shared"),
        [(0, Certainty::Maybe), (3, Certainty::Maybe)]
    );
}

#[test]
fn h_forced_collisions_use_real_lookup_merge_and_verification_oracle() {
    let _forced = Forced::new(|_| 7);
    let first = build(
        0,
        1,
        &BTreeMap::from([
            (b"alpha".to_vec(), vec![0]),
            (b"beta".to_vec(), vec![0]),
            (b"gamma".to_vec(), vec![1]),
        ]),
    );
    let second = build(2, 3, &BTreeMap::from([(b"alpha".to_vec(), vec![2, 3])]));
    let inputs = [first, second];
    assert_eq!(
        candidates(&inputs, b"alpha"),
        [
            (0, Certainty::Maybe),
            (1, Certainty::Maybe),
            (2, Certainty::Yes),
            (3, Certainty::Yes)
        ]
    );
    let full = merged(&inputs, &DocSet::new(4, [0, 2]));
    assert_eq!(full.fingerprint_records(), 2);
    assert_eq!(
        candidates(&[full], b"absent"),
        [(0, Certainty::Maybe), (2, Certainty::Maybe)]
    );
}

#[test]
fn h_random_corpora_and_repeated_merges_match_brute_force() {
    for seed in 1..=32u64 {
        let _forced =
            (seed % 2 == 0).then(|| Forced::new(|term| u32::from(term[term.len() - 1] % 7)));
        let mut rng = seed;
        let mut corpus: Vec<BTreeSet<Vec<u8>>> = vec![BTreeSet::new(); 96];
        for terms in &mut corpus {
            for term in 0..40 {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                if rng % 11 == 0 {
                    terms.insert(format!("term{term}").into_bytes());
                }
            }
        }
        let mut segments = Vec::new();
        for first in (0..96).step_by(8) {
            let mut terms: BTreeMap<Vec<u8>, Vec<u32>> = BTreeMap::new();
            for (doc, content) in corpus.iter().enumerate().skip(first).take(8) {
                for term in content {
                    terms.entry(term.clone()).or_default().push(doc as u32);
                }
            }
            segments.push(build(first as u32, first as u32 + 7, &terms));
        }
        let mut alive: Vec<u32> = (0..96).collect();
        for round in 0..4 {
            alive.retain(|&doc| round == 0 || doc % (round + 2) != 0);
            let live = DocSet::new(96, alive.iter().copied());
            for term in 0..=40 {
                let term = format!("term{term}");
                let expected: Vec<_> = corpus
                    .iter()
                    .enumerate()
                    .filter(|(doc, content)| {
                        live.contains(*doc as u32) && content.contains(term.as_bytes())
                    })
                    .map(|(doc, _)| doc as u32)
                    .collect();
                let found = candidates(&segments, term.as_bytes());
                assert!(
                    expected
                        .iter()
                        .all(|doc| found.iter().any(|(candidate, _)| candidate == doc))
                );
                // Real verification rejects hash collisions and dead records.
                let verified: Vec<_> = found
                    .iter()
                    .filter(|(doc, _)| {
                        live.contains(*doc) && corpus[*doc as usize].contains(term.as_bytes())
                    })
                    .map(|(doc, _)| *doc)
                    .collect();
                assert_eq!(verified, expected);
                for (doc, certainty) in found {
                    if certainty == Certainty::Yes {
                        assert!(corpus[doc as usize].contains(term.as_bytes()));
                    }
                }
            }
            segments = vec![merged(&segments, &live)];
        }
    }
}

#[test]
fn h_purge_regroups_a_run_across_restart_boundaries() {
    let _forced = Forced::new(numbered);
    let mut writer = Writer::with_codec(0, 699, FINGERPRINT_CODEC).unwrap();
    for fp in 0..127 {
        writer.push(format!("{fp:04}").as_bytes(), &[fp]).unwrap();
    }
    for doc in 127..699 {
        writer.push_record(127, doc).unwrap();
    }
    let mut bytes = Vec::new();
    writer.finish(&mut bytes).unwrap();
    let inputs = [Segment::open(bytes).unwrap()];
    let live = DocSet::new(700, 20..700);
    let purged = merged(&inputs, &live);
    assert_eq!(
        candidates(&[purged], b"0127"),
        (127..699)
            .map(|doc| (doc, Certainty::Maybe))
            .collect::<Vec<_>>()
    );
}

#[test]
fn h_spilled_records_match_in_memory_bytes_and_detect_damage() {
    let dir = std::env::temp_dir().join(format!("ferret-h-spill-{}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    let path = dir.join("segment");
    let scratch = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(dir.join("scratch"))
        .unwrap();
    let mut spilled = Writer::spilling_with_codec(
        0,
        300_000,
        FINGERPRINT_CODEC,
        File::create(&path).unwrap(),
        scratch,
    )
    .unwrap();
    let mut memory = Writer::with_codec(0, 300_000, FINGERPRINT_CODEC).unwrap();
    for writer in [&mut spilled, &mut memory] {
        writer.push(b"aaa", &[0, 1]).unwrap();
        writer.push(b"aab", &[2]).unwrap();
        writer.push(b"aac", &[0, 2]).unwrap();
        for doc in 0..300_000 {
            writer.push_record(doc * 13, doc).unwrap();
            if doc % 1024 == 0 {
                writer.spill(&|_| Ok(())).unwrap();
            }
        }
    }
    let sizes = spilled.finish_spilled(&|_| Ok(())).unwrap();
    let mut expected = Vec::new();
    assert_eq!(memory.finish(&mut expected).unwrap(), sizes);
    assert_eq!(fs::read(&path).unwrap(), expected);
    let segment = Segment::open(expected.clone()).unwrap();
    assert!(segment.section_bytes(Section::Fingerprints) > super::write::SPILL as u64);
    let at = u64_at(&expected, FIELDS + 2 * TABLE_ENTRY) as usize;
    expected[at] ^= 1;
    let _forced = Forced::new(|_| 0);
    let damaged = Segment::open(expected).unwrap();
    assert!(matches!(
        damaged.fingerprint_docs(b"anything"),
        Err(ReadError::Corrupt(Section::Fingerprints))
    ));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn h_real_follow_purge_and_codec_change_rebuild_coverage() {
    use crate::{Budget, CatalogView, IndexWriter, Stopped};
    let _forced = Forced::new(|_| 7);
    let dir = std::env::temp_dir().join(format!("ferret-h-follow-{}", std::process::id()));
    let live = DocSet::new(4, 0..4);
    let catalog = CatalogView {
        incarnation: [9; 16],
        live: &live,
    };
    let mut writer = IndexWriter::open_with_codec(&dir, &catalog, FINGERPRINT_CODEC).unwrap();
    let text: [&[u8]; 4] = [b"alpha beta", b"beta", b"alpha gamma", b"gamma"];
    let budget = Budget {
        buffer: 1,
        ..Budget::unbounded()
    };
    loop {
        let followed = writer
            .follow(&catalog, &budget, &mut |doc, bytes| {
                bytes.extend_from_slice(text[doc as usize]);
                Ok(())
            })
            .unwrap();
        if followed.stopped == Stopped::Covered {
            break;
        }
    }
    assert_eq!(writer.view().lookup(b"absent").unwrap(), [0, 1, 2, 3]);
    let live = DocSet::new(4, [0, 3]);
    let catalog = CatalogView {
        incarnation: [9; 16],
        live: &live,
    };
    let merged = writer
        .merge_all(&catalog, &Budget::unbounded())
        .unwrap()
        .unwrap();
    assert_eq!(merged.records_removed, 2);
    assert_eq!(merged.records_added, 0);
    assert_eq!(writer.view().lookup(b"alpha").unwrap(), [0, 3]);
    drop(writer);
    // Codec 1 is still the default and cannot inherit H's stringless data.
    let writer = IndexWriter::open(&dir, &catalog).unwrap();
    assert!(writer.view().segments().is_empty());
    assert_eq!(writer.view().uncovered(&live), [0, 3]);
    fs::remove_dir_all(dir).unwrap();
}
