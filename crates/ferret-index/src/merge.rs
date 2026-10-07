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
    let levels: Vec<f64> = segments.iter().map(|s| level(s.bytes)).collect();
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
    pace: &dyn Fn(usize),
) -> Result<Sizes, Error> {
    let mut cursors: Vec<Terms<'_, R>> = inputs.iter().map(Segment::terms).collect();
    // Each input's current entry; the heap orders (term, input), so equal
    // terms pop in input order, which is range order.
    let mut entries: Vec<Option<TermEntry>> = vec![None; inputs.len()];
    let mut heap = BinaryHeap::new();
    for (i, cursor) in cursors.iter_mut().enumerate() {
        if let Some((term, entry)) = next(cursor)? {
            entries[i] = Some(entry);
            heap.push(Reverse((term, i)));
        }
    }
    let (mut docs, mut list) = (Vec::new(), Vec::new());
    let mut midway = false;
    while let Some(Reverse((term, i))) = heap.pop() {
        docs.clear();
        let mut taken = vec![i];
        while heap
            .peek()
            .is_some_and(|Reverse((other, _))| *other == term)
        {
            if let Some(Reverse((_, j))) = heap.pop() {
                taken.push(j);
            }
        }
        for &j in &taken {
            let Some(entry) = entries[j].take() else {
                continue;
            };
            match inputs[j].read(&entry)? {
                Hit::Single(doc) => list.push(doc),
                Hit::Postings(postings) => postings.decode(&mut list),
            }
            docs.extend(list.drain(..).filter(|&doc| live.contains(doc)));
            if let Some((term, entry)) = next(&mut cursors[j])? {
                entries[j] = Some(entry);
                heap.push(Reverse((term, j)));
            }
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
    Ok(writer.finish_spilled(pace)?)
}

fn next<R: ReadAt>(cursor: &mut Terms<'_, R>) -> Result<Option<(Vec<u8>, TermEntry)>, ReadError> {
    Ok(cursor
        .next_term()?
        .map(|(term, entry)| (term.to_vec(), entry)))
}

#[cfg(test)]
mod tests {
    use super::*;

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
