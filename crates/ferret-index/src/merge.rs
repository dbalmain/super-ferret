//! Merging: which adjacent segments to merge ([`choose`]), and the streaming
//! merge itself ([`stream`]) (docs/S2.md § Segments are disjoint DocId
//! ranges).
//!
//! Ranges are disjoint and ascending, so a merge is concatenation plus purge:
//! each term's lists are decoded input by input, in range order, filtered by
//! the pinned view's liveness and appended. There is no remapping and no
//! sort. Terms come out of a k-way merge of the inputs' dictionaries, one
//! list in memory at a time, and the output spills to disk as it grows.
//!
//! **Policy.** Adjacent levels, the shape of Lucene's `LogMergePolicy`,
//! not its `TieredMergePolicy`, whose non-adjacent merges would break range
//! order. A segment's level is `log10` of its file bytes over
//! [`TIER_FLOOR`] (0 at or below the floor). Walking from the left, each
//! group is every segment up to the last one within [`LEVEL_SPAN`] of the
//! largest level remaining; a group of [`MERGE_FACTOR`] or more merges its
//! first ten, the group with the lowest top level first. Failing that, the
//! segment with the largest dead fraction above [`DEAD_FRACTION`] is
//! rewritten alone. Levels go by whole-file bytes, not postings bytes as
//! S2.md first said: the dictionary is 87% of a segment (M2), so postings
//! bytes would misjudge both what a merge costs and what it saves.
//!
//! A first draft used whole decades (`floor(level)`) and required ten
//! adjacent segments of one decade. M3's measurement showed why Lucene uses
//! a span instead: a first build's 49 segments of about 10 MB straddled the
//! 10 MiB boundary, so no run of ten shared a decade and steady state kept
//! 31 segments.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ops::Range;

use crate::live::DocSet;
use crate::manifest::SegmentEntry;
use crate::segment::{Hit, ReadAt, ReadError, Segment, Sizes, TermEntry, Terms, Writer};
use crate::store::{Error, Point, hit};

/// Segments merged at once, and per tier before the next tier up.
pub const MERGE_FACTOR: usize = 10;

/// Bytes at or below which every segment is tier 0.
pub const TIER_FLOOR: u64 = 1 << 20;

/// A segment whose dead documents exceed this fraction of the documents it
/// was written with is rewritten alone: 1/4.
pub const DEAD_FRACTION: (u32, u32) = (1, 4);

/// Levels within this much of a group's top level merge together:
/// Lucene's `LEVEL_LOG_SPAN`.
pub const LEVEL_SPAN: f64 = 0.75;

/// `log10(bytes / TIER_FLOOR)`, and 0 at or below the floor.
pub fn level(bytes: u64) -> f64 {
    (bytes.max(TIER_FLOOR) as f64 / TIER_FLOOR as f64).log10()
}

/// The segments to merge next, by index into `segments`, or `None`.
/// `alive` gives a segment's indexed documents that are still live.
pub fn choose(
    segments: &[SegmentEntry],
    alive: &dyn Fn(&SegmentEntry) -> u32,
) -> Option<Range<usize>> {
    let levels: Vec<f64> = segments.iter().map(|s| level(s.total_bytes())).collect();
    let mut best: Option<(f64, usize)> = None;
    let mut start = 0;
    while start < levels.len() {
        let top = levels[start..].iter().copied().fold(0.0, f64::max);
        let lower = top - LEVEL_SPAN;
        let end = 1
            + (start..levels.len())
                .rev()
                .find(|&i| levels[i] >= lower)
                .unwrap_or(start);
        if end - start >= MERGE_FACTOR && best.is_none_or(|(best_top, _)| top < best_top) {
            best = Some((top, start));
        }
        start = end;
    }
    if let Some((_, start)) = best {
        return Some(start..start + MERGE_FACTOR);
    }
    let (num, den) = DEAD_FRACTION;
    segments
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            let dead = s.docs.saturating_sub(alive(s));
            u64::from(dead) * u64::from(den) > u64::from(s.docs) * u64::from(num)
        })
        .max_by(|(_, a), (_, b)| {
            let fraction = |s: &SegmentEntry| {
                f64::from(s.docs.saturating_sub(alive(s))) / f64::from(s.docs.max(1))
            };
            fraction(a).total_cmp(&fraction(b))
        })
        .map(|(i, _)| i..i + 1)
}

/// Merges `inputs`, adjacent and in range order, into `writer`, dropping
/// documents not in `live`, and completes the writer's file.
pub fn stream<R: ReadAt>(
    inputs: &[Segment<R>],
    live: &DocSet,
    mut writer: Writer,
    pace: &dyn Fn(usize) -> std::io::Result<()>,
    cancelled: &dyn Fn() -> bool,
) -> Result<Sizes, Error> {
    let mut cursors: Vec<Terms<'_, R>> = inputs.iter().map(Segment::terms).collect();
    // Each input's current term and entry, `None` once it is exhausted. The
    // buffers are reused, so stepping a cursor allocates nothing.
    let mut terms: Vec<Vec<u8>> = vec![Vec::new(); inputs.len()];
    let mut entries: Vec<Option<TermEntry>> = vec![None; inputs.len()];
    for (i, cursor) in cursors.iter_mut().enumerate() {
        entries[i] = next(cursor, &mut terms[i])?;
    }
    // Orders inputs by (term, input), exhausted last, so equal terms win in
    // input order, which is range order. An exhausted input's term is stale.
    let before = |entries: &[Option<TermEntry>], terms: &[Vec<u8>], a: usize, b: usize| {
        (entries[a].is_none(), &terms[a], a) < (entries[b].is_none(), &terms[b], b)
    };
    let mut tournament = Tournament::new(inputs.len(), |a, b| before(&entries, &terms, a, b));
    let (mut term, mut docs, mut list) = (Vec::new(), Vec::new(), Vec::new());
    let mut midway = false;
    while let Some(Some(_)) = entries.get(tournament.winner) {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        term.clear();
        term.extend_from_slice(&terms[tournament.winner]);
        docs.clear();
        loop {
            let j = tournament.winner;
            let Some(entry) = entries[j].filter(|_| terms[j] == term) else {
                break;
            };
            match cursors[j].read(&entry)? {
                Hit::Single(doc) => list.push(doc),
                Hit::Postings(postings) => postings.decode(&mut list),
            }
            docs.extend(list.drain(..).filter(|&doc| live.contains(doc)));
            entries[j] = next(&mut cursors[j], &mut terms[j])?;
            tournament.replay(j, |a, b| before(&entries, &terms, a, b));
        }
        if !docs.is_empty() {
            writer.push(&term, &docs)?;
            writer.spill(pace)?;
            if !midway {
                midway = true;
                hit(Point::MergeMidway)?;
            }
        }
    }
    let mut records: Vec<_> = inputs.iter().map(Segment::records).collect();
    let mut heap = BinaryHeap::new();
    for (i, cursor) in records.iter_mut().enumerate() {
        if let Some(record) = cursor.next_record()? {
            heap.push(Reverse((record, i)));
        }
    }
    let mut previous = None;
    let (mut carried, mut removed) = (0, 0);
    while let Some(Reverse((record, i))) = heap.pop() {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        if live.contains(record.1) && previous != Some(record) {
            carried += 1;
            writer.push_record(record.0, record.1)?;
            writer.spill(pace)?;
            previous = Some(record);
        }
        if !live.contains(record.1) {
            removed += 1;
        }
        if let Some(record) = records[i].next_record()? {
            heap.push(Reverse((record, i)));
        }
    }
    let mut sizes = writer.finish_spilled(pace)?;
    sizes.records_added = sizes.records.saturating_sub(carried);
    sizes.records_removed = removed;
    Ok(sizes)
}

/// Steps `cursor`, copying its term into `term`; `None` once it is done.
fn next<R: ReadAt>(
    cursor: &mut Terms<'_, R>,
    term: &mut Vec<u8>,
) -> Result<Option<TermEntry>, ReadError> {
    Ok(cursor.next_term()?.map(|(bytes, entry)| {
        term.clear();
        term.extend_from_slice(bytes);
        entry
    }))
}

/// A loser tree over `k` inputs: `winner` is the input first in the order
/// `before` gives, and replaying after it changes costs one comparison a
/// level, where a heap pays a pop and a push.
///
/// Nodes `1..k` are internal, each holding the input that lost there;
/// inputs are the leaves `k..2k`, input `i` at `k + i`.
struct Tournament {
    losers: Vec<usize>,
    winner: usize,
}

impl Tournament {
    fn new(k: usize, before: impl Fn(usize, usize) -> bool) -> Self {
        fn play(
            node: usize,
            k: usize,
            losers: &mut [usize],
            before: &impl Fn(usize, usize) -> bool,
        ) -> usize {
            if node >= k {
                return node - k;
            }
            let (a, b) = (
                play(2 * node, k, losers, before),
                play(2 * node + 1, k, losers, before),
            );
            let (winner, loser) = if before(b, a) { (b, a) } else { (a, b) };
            losers[node] = loser;
            winner
        }
        let mut losers = vec![0; k];
        let winner = if k > 1 {
            play(1, k, &mut losers, &before)
        } else {
            0
        };
        Self { losers, winner }
    }

    /// Re-runs `input`'s matches up to the root after it changed.
    fn replay(&mut self, input: usize, before: impl Fn(usize, usize) -> bool) {
        let mut winner = input;
        let mut node = (self.losers.len() + input) / 2;
        while node > 0 {
            if before(self.losers[node], winner) {
                std::mem::swap(&mut self.losers[node], &mut winner);
            }
            node /= 2;
        }
        self.winner = winner;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tournament_drains_in_key_then_input_order() {
        // Every k up to 13, so trees that are not powers of two are
        // covered; few distinct keys, so ties between inputs are common.
        for k in 1..=13 {
            let mut keys: Vec<Vec<u32>> = (0..k)
                .map(|i| (0..5).map(|j| ((i * 7 + j * 3) % 4) as u32).collect())
                .collect();
            for list in &mut keys {
                list.sort_unstable();
            }
            let mut at = vec![0; k];
            let key = |at: &[usize], i: usize| keys[i].get(at[i]).copied();
            let before = |at: &[usize], a: usize, b: usize| {
                let (x, y) = (key(at, a), key(at, b));
                (x.is_none(), x, a) < (y.is_none(), y, b)
            };
            let mut tournament = Tournament::new(k, |a, b| before(&at, a, b));
            let mut drained = Vec::new();
            while let Some(value) = key(&at, tournament.winner) {
                drained.push((value, tournament.winner));
                at[tournament.winner] += 1;
                let winner = tournament.winner;
                tournament.replay(winner, |a, b| before(&at, a, b));
            }
            let mut expected: Vec<(u32, usize)> = keys
                .iter()
                .enumerate()
                .flat_map(|(i, list)| list.iter().map(move |&value| (value, i)))
                .collect();
            expected.sort_unstable();
            assert_eq!(drained, expected, "k = {k}");
        }
    }

    #[test]
    fn keyed_versions_survive_ordinary_and_full_spilling_merges() {
        use ferret_text::{TERM_VERSION, TRIGRAM_VERSION};
        use std::fs::{self, File};

        let dir = std::env::temp_dir().join(format!("ferret-keyed-merge-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        for version in [TERM_VERSION, TRIGRAM_VERSION] {
            let mut inputs = Vec::new();
            for doc in 0..11 {
                let path = dir.join(format!("{version}-{doc}"));
                if doc % 2 == 0 {
                    let mut writer = Writer::keyed(doc, doc, version).unwrap();
                    writer.push(b"aaa", &[doc]).unwrap();
                    writer.finish(&mut File::create(&path).unwrap()).unwrap();
                } else {
                    let scratch = File::options()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .open(dir.join("scratch"))
                        .unwrap();
                    let mut writer = Writer::keyed_spilling(
                        doc,
                        doc,
                        version,
                        File::create(&path).unwrap(),
                        scratch,
                    )
                    .unwrap();
                    writer.push(b"aaa", &[doc]).unwrap();
                    writer.spill(&|_| Ok(())).unwrap();
                    writer.finish_spilled(&|_| Ok(())).unwrap();
                }
                let segment = Segment::open(File::open(path).unwrap()).unwrap();
                assert_version(&segment, version);
                inputs.push(segment);
            }
            let live = DocSet::new(11, 0..11);
            let entries = segments(&[MB; 11]);
            assert_eq!(choose(&entries, &|s| s.docs), Some(0..10));
            {
                let path = dir.join("ordinary");
                let scratch = File::options()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(dir.join("scratch"))
                    .unwrap();
                let writer =
                    Writer::keyed_spilling(0, 9, version, File::create(&path).unwrap(), scratch)
                        .unwrap();
                stream(&inputs[..10], &live, writer, &|_| Ok(()), &|| false).unwrap();
                let ordinary = Segment::open(File::open(path).unwrap()).unwrap();
                assert_version(&ordinary, version);
                let tail = inputs.pop().unwrap();
                let selected = [ordinary, tail];
                let path = dir.join("full");
                let scratch = File::options()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(dir.join("scratch"))
                    .unwrap();
                let writer =
                    Writer::keyed_spilling(0, 10, version, File::create(&path).unwrap(), scratch)
                        .unwrap();
                stream(&selected, &live, writer, &|_| Ok(()), &|| false).unwrap();
                let full = Segment::open(File::open(path).unwrap()).unwrap();
                assert_version(&full, version);
                let Hit::Postings(postings) = full.lookup(b"aaa").unwrap().unwrap() else {
                    panic!("all eleven documents must survive both merges");
                };
                let mut docs = Vec::new();
                postings.decode(&mut docs);
                assert_eq!(docs, (0..11).collect::<Vec<_>>());
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }

    fn assert_version(segment: &Segment<std::fs::File>, version: u32) {
        let info = segment.info();
        let bytes = segment.source().metadata().unwrap().len();
        let entry = SegmentEntry {
            number: 0,
            first: info.first,
            last: info.last,
            docs: 1,
            terms: info.terms,
            pairs: info.pairs,
            bytes,
            digest: segment.digest(),
            trigram_bytes: 0,
            trigram_keys: 0,
            trigram_pairs: 0,
            trigram_digest: [0; 16],
        };
        assert!(entry.matches(&info, &segment.digest(), bytes, version));
        assert!(!entry.matches(&info, &segment.digest(), bytes, version ^ 1));
    }

    fn segments(bytes: &[u64]) -> Vec<SegmentEntry> {
        bytes
            .iter()
            .enumerate()
            .map(|(i, &bytes)| SegmentEntry {
                number: i as u64,
                first: i as u32 * 10,
                last: i as u32 * 10 + 9,
                docs: 10,
                terms: 1,
                pairs: 1,
                bytes,
                digest: [0; 16],
                trigram_bytes: 0,
                trigram_keys: 0,
                trigram_pairs: 0,
                trigram_digest: [0; 16],
            })
            .collect()
    }

    const MB: u64 = TIER_FLOOR;

    #[test]
    fn levels_are_log10_above_the_floor() {
        let cases = [(0, 0.0), (MB, 0.0), (10 * MB, 1.0), (100 * MB, 2.0)];
        for (bytes, want) in cases {
            assert!((level(bytes) - want).abs() < 1e-9, "{bytes}");
        }
    }

    #[test]
    fn groups_span_three_quarters_of_a_level_and_the_lowest_merges_first() {
        let all_alive = |s: &SegmentEntry| s.docs;
        // Nine small segments are not enough.
        assert_eq!(choose(&segments(&[MB; 9]), &all_alive), None);
        // Ten around a decade boundary are one group: the case whole
        // decades missed.
        let straddling: Vec<u64> = (0..10).map(|i| [9 * MB, 11 * MB][i % 2]).collect();
        assert_eq!(choose(&segments(&straddling), &all_alive), Some(0..10));
        // A large segment, then ten small: the small group merges.
        let mut bytes = vec![100 * MB];
        bytes.extend([MB; 10]);
        assert_eq!(choose(&segments(&bytes), &all_alive), Some(1..11));
        // Two full groups: the lower one first.
        let mut bytes = vec![100 * MB; 10];
        bytes.extend([MB; 12]);
        assert_eq!(choose(&segments(&bytes), &all_alive), Some(10..20));
        // A large segment then nine small: the small wait for a tenth.
        let mut bytes = vec![100 * MB];
        bytes.extend([MB; 9]);
        assert_eq!(choose(&segments(&bytes), &all_alive), None);
        // Small segments left of a larger one join its group, as in Lucene.
        let mut bytes = vec![MB; 9];
        bytes.push(5 * MB);
        assert_eq!(choose(&segments(&bytes), &all_alive), Some(0..10));
    }

    #[test]
    fn the_most_dead_segment_past_a_quarter_is_rewritten_alone() {
        let segs = segments(&[MB, MB, MB, MB]);
        // Dead: 2/10, 3/10, 10/10, 1/10.
        let alive = |s: &SegmentEntry| [8, 7, 0, 9][s.number as usize];
        assert_eq!(choose(&segs, &alive), Some(2..3));
        let alive = |s: &SegmentEntry| [8, 7, 10, 9][s.number as usize];
        assert_eq!(choose(&segs, &alive), Some(1..2));
        // Exactly a quarter is not past it.
        let mut segs = segments(&[MB]);
        segs[0].docs = 8;
        assert_eq!(choose(&segs, &|_| 6), None);
        assert_eq!(choose(&segs, &|_| 5), Some(0..1));
    }
}
