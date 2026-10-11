//! H regression cases drive the production writer, term cursor and spilling
//! merge. The oracle retains strings that codec 2 deliberately discards.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::{Certainty, Cursor, DocSet, postings::Term};

thread_local! { static ARM: std::cell::Cell<u32> = const { std::cell::Cell::new(FINGERPRINT_CODEC) }; }
fn arm() -> u32 {
    ARM.with(std::cell::Cell::get)
}
struct Arm;
impl Arm {
    fn new(codec: u32) -> Self {
        ARM.with(|c| c.set(codec));
        Self
    }
}
impl Drop for Arm {
    fn drop(&mut self) {
        ARM.with(|c| c.set(FINGERPRINT_CODEC));
    }
}

fn build(first: u32, last: u32, terms: &BTreeMap<Vec<u8>, Vec<u32>>) -> Segment<Vec<u8>> {
    let mut writer = Writer::with_codec(first, last, arm()).unwrap();
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
    merged_verified(inputs, live, &mut |_, _, _| None).0
}

fn merged_verified(
    inputs: &[Segment<Vec<u8>>],
    live: &DocSet,
    reader: &mut DocumentReader<'_>,
) -> (Segment<Vec<u8>>, Sizes) {
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
        arm(),
        File::create(&path).unwrap(),
        scratch,
    )
    .unwrap();
    let sizes = crate::merge::stream_verified(inputs, live, writer, &|_| Ok(()), &|| false, reader)
        .unwrap();
    let result = Segment::open(fs::read(path).unwrap()).unwrap();
    fs::remove_dir_all(dir).unwrap();
    (result, sizes)
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
    for codec in [FINGERPRINT_CODEC, RUN_CODEC, GRADUATION_CODEC, OFFSET_CODEC] {
        let _arm = Arm::new(codec);

        let _forced = Forced::new(numbered);
        let mut terms = BTreeMap::new();
        for fp in 0..127 {
            terms.insert(format!("{fp:04}").into_bytes(), vec![fp]);
        }
        let mut writer = Writer::with_codec(0, 600, arm()).unwrap();
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
        // The count includes both partial boundary blocks without reading them.
        fingerprint::BLOCK_READS.with(|reads| reads.set(0));
        assert_eq!(segment.fingerprint_bound(b"0127"), 551);
        fingerprint::BLOCK_READS.with(|reads| assert_eq!(reads.get(), 0));
        let found = candidates(&[segment], b"0127");
        fingerprint::BLOCK_READS.with(|reads| assert_eq!(reads.get(), 5));
        assert_eq!(
            found,
            (127..550)
                .map(|doc| (doc, Certainty::Maybe))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn h_empty_section_and_lower_bound_at_end() {
    for codec in [FINGERPRINT_CODEC, RUN_CODEC, GRADUATION_CODEC, OFFSET_CODEC] {
        let _arm = Arm::new(codec);

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
}

#[test]
fn h_dictionary_front_coding_ignores_removed_singletons() {
    for codec in [FINGERPRINT_CODEC, RUN_CODEC, GRADUATION_CODEC, OFFSET_CODEC] {
        let _arm = Arm::new(codec);

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
}

#[test]
fn h_mixed_dictionary_and_fingerprint_occurrences_survive_merges_and_purge() {
    for codec in [FINGERPRINT_CODEC, RUN_CODEC, GRADUATION_CODEC, OFFSET_CODEC] {
        let _arm = Arm::new(codec);

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
}

#[test]
fn h_forced_collisions_use_real_lookup_merge_and_verification_oracle() {
    for codec in [FINGERPRINT_CODEC, RUN_CODEC, GRADUATION_CODEC, OFFSET_CODEC] {
        let _arm = Arm::new(codec);

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
}

#[test]
fn h_random_corpora_and_merge_sequences_match_brute_force() {
    for codec in [FINGERPRINT_CODEC, RUN_CODEC, GRADUATION_CODEC, OFFSET_CODEC] {
        let _arm = Arm::new(codec);

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
            for round in 0..12 {
                alive.retain(|&doc| round == 0 || doc % (round + 2) != 0);
                let live = DocSet::new(96, alive.iter().copied());
                let check = |segments: &[Segment<Vec<u8>>]| {
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
                        let found = candidates(segments, term.as_bytes());
                        assert!(
                            expected
                                .iter()
                                .all(|doc| found.iter().any(|(candidate, _)| candidate == doc))
                        );
                        // Real verification rejects hash collisions and dead
                        // records.
                        let verified: Vec<_> = found
                            .iter()
                            .filter(|(doc, _)| {
                                live.contains(*doc)
                                    && corpus[*doc as usize].contains(term.as_bytes())
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
                };
                check(&segments);
                // Partial adjacent merges leave mixed representations in other
                // segments; full-only merges cannot exercise that sequence.
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let start = rng as usize % segments.len();
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let end = start + 1 + rng as usize % (segments.len() - start);
                let output = merged(&segments[start..end], &live);
                segments.splice(start..end, [output]);
                check(&segments);
            }
        }
    }
}

#[test]
fn h_purge_regroups_a_run_across_restart_boundaries() {
    for codec in [FINGERPRINT_CODEC, RUN_CODEC, GRADUATION_CODEC, OFFSET_CODEC] {
        let _arm = Arm::new(codec);

        let _forced = Forced::new(numbered);
        let mut writer = Writer::with_codec(0, 699, arm()).unwrap();
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
}

#[test]
fn h_spilled_records_match_in_memory_bytes_and_detect_damage() {
    for codec in [FINGERPRINT_CODEC, RUN_CODEC, GRADUATION_CODEC, OFFSET_CODEC] {
        let _arm = Arm::new(codec);

        let dir = std::env::temp_dir().join(format!("ferret-h-spill-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("segment");
        let scratch = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.join("scratch"))
            .unwrap();
        let mut spilled =
            Writer::spilling_with_codec(0, 300_000, arm(), File::create(&path).unwrap(), scratch)
                .unwrap();
        let mut memory = Writer::with_codec(0, 300_000, arm()).unwrap();
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
}

#[test]
fn h_real_follow_purge_and_codec_change_rebuild_coverage() {
    for codec in [FINGERPRINT_CODEC, RUN_CODEC, GRADUATION_CODEC, OFFSET_CODEC] {
        let _arm = Arm::new(codec);

        use crate::{Budget, CatalogView, IndexWriter, Stopped};
        let _forced = Forced::new(|_| 7);
        let dir = std::env::temp_dir().join(format!("ferret-h-follow-{}", std::process::id()));
        let live = DocSet::new(4, 0..4);
        let catalog = CatalogView {
            incarnation: [9; 16],
            live: &live,
        };
        let mut writer = IndexWriter::open_with_codec(&dir, &catalog, arm()).unwrap();
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
}

#[test]
fn h_long_candidate_run_cancels_before_reading_the_next_block() {
    for codec in [FINGERPRINT_CODEC, RUN_CODEC, GRADUATION_CODEC, OFFSET_CODEC] {
        let _arm = Arm::new(codec);

        let _forced = Forced::new(|_| 7);
        let mut writer = Writer::with_codec(0, 1023, arm()).unwrap();
        for doc in 0..1024 {
            writer.push_record(7, doc).unwrap();
        }
        let mut bytes = Vec::new();
        writer.finish(&mut bytes).unwrap();
        let segment = Segment::open(bytes).unwrap();
        fingerprint::BLOCK_READS.with(|reads| reads.set(0));
        let cancelled = || fingerprint::BLOCK_READS.with(|reads| reads.get() >= 2);
        assert!(matches!(
            Term::read_until([&segment], b"term", &cancelled),
            Err(ReadError::Cancelled)
        ));
        fingerprint::BLOCK_READS.with(|reads| assert_eq!(reads.get(), 2));
    }
}

#[test]
fn h_source_estimate_bounds_snapshot_maybe_without_fingerprint_reads() {
    for codec in [FINGERPRINT_CODEC, RUN_CODEC, GRADUATION_CODEC, OFFSET_CODEC] {
        let _arm = Arm::new(codec);

        use crate::{Atom, Budget, CatalogView, IndexWriter, Pinned, Source, Stopped};
        let _forced = Forced::new(|_| 7);
        let dir = std::env::temp_dir().join(format!("ferret-h-estimate-{}", std::process::id()));
        let live = DocSet::new(301, 0..301);
        let catalog = CatalogView {
            incarnation: [19; 16],
            live: &live,
        };
        let mut writer = IndexWriter::open_with_codec(&dir, &catalog, arm()).unwrap();
        let budget = Budget {
            buffer: 1,
            ..Budget::unbounded()
        };
        loop {
            let result = writer
                .follow(&catalog, &budget, &mut |_, bytes| {
                    bytes.extend_from_slice(b"term");
                    Ok(())
                })
                .unwrap();
            if result.stopped == Stopped::Covered {
                break;
            }
        }
        writer.merge_all(&catalog, &Budget::unbounded()).unwrap();
        let view = writer.view();
        let pinned = Pinned::new(Some(&view), &live);
        let source = Source::Postings(&view);
        fingerprint::BLOCK_READS.with(|reads| reads.set(0));
        let estimate = source.estimate(&Atom::Term(b"term")).unwrap().unwrap();
        fingerprint::BLOCK_READS.with(|reads| assert_eq!(reads.get(), 0));
        let answer = source.read(&Atom::Term(b"term")).unwrap().unwrap();
        fingerprint::BLOCK_READS.with(|reads| assert!(reads.get() > 0));
        let mut cursor = pinned.top(pinned.atom(answer.cursor()));
        let (mut target, mut maybe) = (0, 0);
        while let Some((doc, certainty)) = cursor.next_geq(target) {
            maybe += u64::from(certainty == Certainty::Maybe);
            target = doc + 1;
        }
        assert_eq!(maybe, 301);
        assert!(estimate.maybe_docs >= maybe);
        assert!(estimate.docs >= maybe);
        fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn graduation_rejects_dictionary_record_collisions_and_keeps_both_queries() {
    // A fingerprint match may never become an unverified certain hit, and
    // one shared record may represent two different tokens in one document.
    for codec in [GRADUATION_CODEC, OFFSET_CODEC] {
        let _arm = Arm::new(codec);
        let _forced = Forced::new(|_| 7);
        let inputs = [
            build(
                0,
                1,
                &BTreeMap::from([(b"alpha".to_vec(), vec![0]), (b"beta".to_vec(), vec![0, 1])]),
            ),
            build(2, 3, &BTreeMap::from([(b"alpha".to_vec(), vec![2, 3])])),
        ];
        let text: [&[u8]; 4] = [b"alpha beta", b"beta gamma", b"alpha", b"alpha"];
        let mut read = |doc: u32, _: Option<u32>, bytes: &mut Vec<u8>| {
            *bytes = text[doc as usize].to_vec();
            Some(Window {
                start: 0,
                complete: true,
            })
        };
        let (output, _) = merged_verified(&inputs, &DocSet::new(4, 0..4), &mut read);
        for (term, expected) in [(b"alpha".as_slice(), vec![0, 2, 3]), (b"beta", vec![0, 1])] {
            let found = candidates(std::slice::from_ref(&output), term);
            for &(doc, certainty) in &found {
                if certainty == Certainty::Yes {
                    assert!(ferret_text::has_token(text[doc as usize], term));
                }
            }
            let verified: Vec<_> = found
                .into_iter()
                .filter(|&(doc, certainty)| {
                    certainty == Certainty::Yes || ferret_text::has_token(text[doc as usize], term)
                })
                .map(|r| r.0)
                .collect();
            assert_eq!(verified, expected);
        }
        assert!(output.fingerprint_records() > 0);
    }
}

#[test]
fn changed_or_unreadable_disk_documents_cannot_graduate() {
    use std::os::unix::fs::MetadataExt;
    for codec in [GRADUATION_CODEC, OFFSET_CODEC] {
        let _arm = Arm::new(codec);
        let dir = std::env::temp_dir().join(format!(
            "ferret-graduation-version-{}-{codec}",
            std::process::id()
        ));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("doc");
        fs::write(&path, b"alpha").unwrap();
        let recorded = fs::metadata(&path).unwrap();
        let key = |m: &fs::Metadata| {
            (
                m.dev(),
                m.ino(),
                m.len(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
            )
        };
        let first = build(0, 0, &BTreeMap::from([(b"alpha".to_vec(), vec![0])]));
        let second = build(1, 2, &BTreeMap::from([(b"alpha".to_vec(), vec![1, 2])]));
        // Still contains alpha; a presence-only check would wrongly graduate.
        fs::write(&path, b"alpha alpha").unwrap();
        for missing in [false, true] {
            if missing {
                fs::remove_file(&path).unwrap();
            }
            let mut reader = |_: u32, _: Option<u32>, bytes: &mut Vec<u8>| {
                let before = fs::metadata(&path).ok()?;
                if key(&before) != key(&recorded) {
                    return None;
                }
                *bytes = fs::read(&path).ok()?;
                (key(&fs::metadata(&path).ok()?) == key(&recorded)).then_some(Window {
                    start: 0,
                    complete: true,
                })
            };
            let (output, sizes) = merged_verified(
                &[
                    Segment::open(first.source().clone()).unwrap(),
                    Segment::open(second.source().clone()).unwrap(),
                ],
                &DocSet::new(3, 0..3),
                &mut reader,
            );
            assert_eq!(sizes.records_graduated, 0);
            assert_eq!(
                candidates(&[output], b"alpha"),
                [
                    (0, Certainty::Maybe),
                    (1, Certainty::Yes),
                    (2, Certainty::Yes)
                ]
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn offset_mismatch_falls_back_and_unicode_tokens_graduate() {
    let _arm = Arm::new(OFFSET_CODEC);
    let mut writer = Writer::with_codec(0, 0, OFFSET_CODEC).unwrap();
    writer.push_at(b"kelvin", &[0], Some(0)).unwrap();
    let mut encoded = Vec::new();
    writer.finish(&mut encoded).unwrap();
    let first = Segment::open(encoded).unwrap();
    let second = build(1, 2, &BTreeMap::from([(b"kelvin".to_vec(), vec![1, 2])]));
    let mut requests = Vec::new();
    let mut reader = |doc: u32, offset: Option<u32>, bytes: &mut Vec<u8>| {
        requests.push((doc, offset));
        *bytes = if offset.is_some() {
            b"wrong".to_vec()
        } else {
            "wrong elsewhere Kelvin".as_bytes().to_vec()
        };
        Some(Window {
            start: 0,
            complete: offset.is_none(),
        })
    };
    let (output, sizes) = merged_verified(&[first, second], &DocSet::new(3, 0..3), &mut reader);
    assert_eq!(requests, [(0, Some(0)), (0, None)]);
    assert_eq!(sizes.records_graduated, 1);
    assert_eq!(
        candidates(&[output], b"kelvin"),
        [
            (0, Certainty::Yes),
            (1, Certainty::Yes),
            (2, Certainty::Yes)
        ]
    );
}

#[test]
fn offset_shared_by_whole_and_part_cannot_erase_a_collision() {
    let _arm = Arm::new(OFFSET_CODEC);
    let _forced = Forced::new(|_| 7);
    let mut writer = Writer::with_codec(0, 1, OFFSET_CODEC).unwrap();
    writer.push(b"alpha", &[0, 1]).unwrap();
    writer.push_at(b"alphabeta", &[0], Some(0)).unwrap();
    let mut encoded = Vec::new();
    writer.finish(&mut encoded).unwrap();
    let mut read = |doc: u32, _: Option<u32>, bytes: &mut Vec<u8>| {
        *bytes = if doc == 0 {
            b"alphaBeta".to_vec()
        } else {
            b"alpha".to_vec()
        };
        Some(Window {
            start: 0,
            complete: true,
        })
    };
    let (output, sizes) = merged_verified(
        &[Segment::open(encoded).unwrap()],
        &DocSet::new(2, 0..2),
        &mut read,
    );
    assert_eq!(sizes.records_graduated, 0);
    assert_eq!(output.fingerprint_docs(b"alphabeta").unwrap(), [0]);
}

#[test]
fn graduation_across_partial_merges_matches_a_token_oracle() {
    for codec in [RUN_CODEC, GRADUATION_CODEC, OFFSET_CODEC] {
        let _arm = Arm::new(codec);
        for collisions in [false, true] {
            let _forced = collisions.then(|| Forced::new(|t| u32::from(t[0] % 3)));
            let texts: Vec<_> = (0..64)
                .map(|d| {
                    let mut bytes = Vec::new();
                    for t in 0..12 {
                        if (d * 17 + t * 11) % 7 < 2 {
                            bytes.extend_from_slice(format!("term{t} ").as_bytes());
                        }
                    }
                    bytes
                })
                .collect();
            let mut segments = Vec::new();
            for first in (0..64).step_by(4) {
                let mut terms: BTreeMap<Vec<u8>, Vec<u32>> = BTreeMap::new();
                for (doc, text) in texts.iter().enumerate().skip(first).take(4) {
                    ferret_text::tokens(text, |t| {
                        let docs = terms.entry(t.to_vec()).or_default();
                        if docs.last() != Some(&(doc as u32)) {
                            docs.push(doc as u32);
                        }
                    });
                }
                segments.push(build(first as u32, first as u32 + 3, &terms));
            }
            let mut alive: Vec<u32> = (0..64).collect();
            for round in 0..8 {
                alive.retain(|d| round == 0 || d % (round + 3) != 0);
                let live = DocSet::new(64, alive.iter().copied());
                let count = segments.len().min(3);
                let mut read = |doc: u32, _: Option<u32>, bytes: &mut Vec<u8>| {
                    *bytes = texts[doc as usize].clone();
                    Some(Window {
                        start: 0,
                        complete: true,
                    })
                };
                let output = merged_verified(&segments[..count], &live, &mut read).0;
                segments.splice(..count, [output]);
                for term in (0..=12).map(|t| format!("term{t}")) {
                    let expected: Vec<_> = alive
                        .iter()
                        .copied()
                        .filter(|&d| ferret_text::has_token(&texts[d as usize], term.as_bytes()))
                        .collect();
                    let found = candidates(&segments, term.as_bytes());
                    for &(doc, certainty) in &found {
                        if certainty == Certainty::Yes {
                            assert!(ferret_text::has_token(
                                &texts[doc as usize],
                                term.as_bytes()
                            ));
                        }
                    }
                    let actual: Vec<_> = found
                        .into_iter()
                        .filter(|&(d, c)| {
                            live.contains(d)
                                && (c == Certainty::Yes
                                    || ferret_text::has_token(&texts[d as usize], term.as_bytes()))
                        })
                        .map(|r| r.0)
                        .collect();
                    assert_eq!(actual, expected);
                }
            }
        }
    }
}

#[test]
fn truncated_utf8_window_cannot_invent_a_certain_token() {
    let _arm = Arm::new(OFFSET_CODEC);
    let _forced = Forced::new(|_| 7);
    let mut writer = Writer::with_codec(0, 0, OFFSET_CODEC).unwrap();
    writer
        .push_at("alphabetå".as_bytes(), &[0], Some(0))
        .unwrap();
    let mut encoded = Vec::new();
    writer.finish(&mut encoded).unwrap();
    let second = build(1, 2, &BTreeMap::from([(b"alphabet".to_vec(), vec![1, 2])]));
    let mut full_reads = 0;
    let mut reader = |_: u32, offset: Option<u32>, bytes: &mut Vec<u8>| {
        *bytes = "alphabetå".as_bytes().to_vec();
        if offset.is_some() {
            bytes.pop();
        } else {
            full_reads += 1;
        }
        Some(Window {
            start: 0,
            complete: offset.is_none(),
        })
    };
    let (output, sizes) = merged_verified(
        &[Segment::open(encoded).unwrap(), second],
        &DocSet::new(3, 0..3),
        &mut reader,
    );
    assert_eq!(full_reads, 1);
    assert_eq!(sizes.records_graduated, 0);
    assert_eq!(
        candidates(&[output], b"alphabet"),
        [
            (0, Certainty::Maybe),
            (1, Certainty::Yes),
            (2, Certainty::Yes)
        ]
    );
    // A window starting in a multibyte alphanumeric has an uncertain left
    // edge too; a real byte separator is required before its enclosing run.
    assert!(!token_at(
        b"\xa5needle ",
        Window {
            start: 1,
            complete: false
        },
        2,
        b"needle"
    ));
}

#[test]
fn c2_batches_large_document_checks_through_the_real_merge() {
    let _arm = Arm::new(GRADUATION_CODEC);
    let terms = [b"alpha".as_slice(), b"beta", b"gamma"];
    let first = build(0, 0, &terms.iter().map(|t| (t.to_vec(), vec![0])).collect());
    let second = build(
        1,
        2,
        &terms.iter().map(|t| (t.to_vec(), vec![1, 2])).collect(),
    );
    let mut reads = 0;
    let mut reader = |doc: u32, offset: Option<u32>, bytes: &mut Vec<u8>| {
        assert_eq!((doc, offset), (0, None));
        reads += 1;
        *bytes = b"alpha beta gamma".to_vec();
        Some(Window {
            start: 0,
            complete: true,
        })
    };
    let (output, sizes) = merged_verified(&[first, second], &DocSet::new(3, 0..3), &mut reader);
    assert_eq!(reads, 1);
    assert_eq!(sizes.records_graduated, 3);
    for term in terms {
        assert_eq!(
            candidates(std::slice::from_ref(&output), term),
            [
                (0, Certainty::Yes),
                (1, Certainty::Yes),
                (2, Certainty::Yes)
            ]
        );
    }
}
