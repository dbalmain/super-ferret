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
//! **Policy.** Adjacent tiers, the shape of Lucene's `LogMergePolicy`, not
//! its `TieredMergePolicy`, whose non-adjacent merges would break range
//! order. A segment's tier is the decade of its file bytes above
//! [`TIER_FLOOR`]. The leftmost run of [`MERGE_FACTOR`] adjacent segments of
//! the lowest tier that has one merges first. Failing that, the segment
//! with the largest dead fraction above [`DEAD_FRACTION`] is rewritten
//! alone. Tiers go by whole-file bytes, not postings bytes as S2.md first
//! said: the dictionary is 87% of a segment (M2), so postings bytes would
//! misjudge both what a merge costs and what it saves.

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

/// The decade of `bytes` above [`TIER_FLOOR`]: 0 up to the floor, 1 up to
/// ten times it, and so on.
pub fn tier(bytes: u64) -> u32 {
    let (mut tier, mut ceiling) = (0, TIER_FLOOR);
    while bytes > ceiling {
        tier += 1;
        ceiling = ceiling.saturating_mul(10);
    }
    tier
}

/// The segments to merge next, by index into `segments`, or `None`.
/// `alive` gives a segment's indexed documents that are still live.
pub fn choose(
    segments: &[SegmentEntry],
    alive: &dyn Fn(&SegmentEntry) -> u32,
) -> Option<Range<usize>> {
    let tiers: Vec<u32> = segments.iter().map(|s| tier(s.bytes)).collect();
    let mut best: Option<(u32, usize)> = None;
    let mut start = 0;
    while start < tiers.len() {
        let end = start + tiers[start..].iter().take_while(|&&t| t == tiers[start]).count();
        if end - start >= MERGE_FACTOR && best.is_none_or(|(tier, _)| tiers[start] < tier) {
            best = Some((tiers[start], start));
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
        while heap.peek().is_some_and(|Reverse((other, _))| *other == term) {
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
