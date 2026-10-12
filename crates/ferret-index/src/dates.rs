//! The token-policy experiment's date index (docs/TOKENS.md, T2): which
//! documents mention an instant, at a fixed key granularity.
//!
//! Instants are whole seconds since the epoch, already normalised to UTC by
//! `ferret_text::timestamps`, which also drops the fraction. A key is
//! `seconds / granularity`. The index stores one postings list per key, keys
//! ascending: `vbyte key delta, vbyte count, vbyte first doc, vbyte doc
//! deltas`, with a skip entry every [`SKIP`] keys. A hit is only a candidate
//! whenever the query does not fall on key boundaries, or carries a
//! fraction: the host verifies it against the document.
//!
//! In memory only: the experiment measures its shape, not a file format.

use crate::segment::{put, take};

/// Keys between skip entries.
pub const SKIP: usize = 64;

#[derive(Debug)]
pub struct DateIndex {
    granularity: i64,
    bytes: Vec<u8>,
    /// Every [`SKIP`]th key, with the byte offset of its entry.
    skips: Vec<(i64, usize)>,
    keys: usize,
    pairs: usize,
}

impl DateIndex {
    /// Builds the index from `(seconds, doc)` pairs in any order, keyed by
    /// `granularity` seconds. Instants before the epoch are dropped. Sorts
    /// `pairs` in place.
    pub fn build(granularity: i64, pairs: &mut Vec<(i64, u32)>) -> Self {
        assert!(granularity > 0, "a key must span at least one second");
        pairs.retain(|&(seconds, _)| seconds >= 0);
        for pair in pairs.iter_mut() {
            pair.0 /= granularity;
        }
        pairs.sort_unstable();
        pairs.dedup();
        let mut index = Self {
            granularity,
            bytes: Vec::new(),
            skips: Vec::new(),
            keys: 0,
            pairs: pairs.len(),
        };
        let mut previous = 0;
        for group in pairs.chunk_by(|a, b| a.0 == b.0) {
            let key = group[0].0;
            if index.keys.is_multiple_of(SKIP) {
                index.skips.push((key, index.bytes.len()));
                previous = 0;
            }
            put((key - previous) as u64, &mut index.bytes);
            put(group.len() as u64, &mut index.bytes);
            let mut last = 0;
            for (i, &(_, doc)) in group.iter().enumerate() {
                put(
                    u64::from(if i == 0 { doc } else { doc - last }),
                    &mut index.bytes,
                );
                last = doc;
            }
            previous = key;
            index.keys += 1;
        }
        index
    }

    pub fn granularity(&self) -> i64 {
        self.granularity
    }

    /// Encoded bytes, skip table included (16 bytes an entry).
    pub fn bytes(&self) -> usize {
        self.bytes.len() + self.skips.len() * 16
    }

    pub fn keys(&self) -> usize {
        self.keys
    }

    /// Distinct `(key, doc)` pairs.
    pub fn pairs(&self) -> usize {
        self.pairs
    }

    /// Each key's document count, in key order.
    pub fn docs_per_key(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.keys);
        self.scan(0, i64::MIN, i64::MAX, |_, docs| out.push(docs.len() as u32));
        out
    }

    /// Documents with a key overlapping the seconds `from..to`, sorted and
    /// deduplicated, replacing `out`'s contents.
    pub fn range(&self, from: i64, to: i64, out: &mut Vec<u32>) {
        out.clear();
        if to <= from {
            return;
        }
        let (first, last) = (
            from.div_euclid(self.granularity),
            (to - 1).div_euclid(self.granularity),
        );
        let block = self.skips.partition_point(|&(key, _)| key <= first);
        let start = self
            .skips
            .get(block.saturating_sub(1))
            .map_or(0, |&(_, at)| at);
        self.scan(start, first, last, |_, docs| out.extend_from_slice(docs));
        out.sort_unstable();
        out.dedup();
    }

    /// Whether every document [`range`](Self::range) returns for `from..to`
    /// holds an instant inside it: both ends fall on key boundaries. A query
    /// with a fraction is never exact, since the index dropped fractions.
    pub fn exact(&self, from: i64, to: i64) -> bool {
        from % self.granularity == 0 && to % self.granularity == 0
    }

    /// Visits each key in `first..=last` from byte `start` (a skip entry's
    /// offset, or 0), with its documents.
    fn scan(&self, start: usize, first: i64, last: i64, mut visit: impl FnMut(i64, &[u32])) {
        let mut at = start;
        let mut key = 0;
        let mut docs = Vec::new();
        let mut ordinal = self
            .skips
            .iter()
            .position(|&(_, offset)| offset == start)
            .map_or(0, |i| i * SKIP);
        while at < self.bytes.len() {
            if ordinal.is_multiple_of(SKIP) {
                key = 0;
            }
            let (Some(delta), Some(count)) =
                (take(&self.bytes, &mut at), take(&self.bytes, &mut at))
            else {
                return;
            };
            key += delta as i64;
            docs.clear();
            let mut doc = 0u32;
            for i in 0..count {
                let value = take(&self.bytes, &mut at).unwrap_or(0) as u32;
                doc = if i == 0 { value } else { doc + value };
                docs.push(doc);
            }
            ordinal += 1;
            if key > last {
                return;
            }
            if key >= first {
                visit(key, &docs);
            }
        }
    }
}

/// Bytes of the alternative layout, one sorted `(key, doc)` pair at a time:
/// `vbyte key delta, vbyte doc` (the doc delta-coded within a key). Takes the
/// same pairs as [`DateIndex::build`] after it sorted them, as keys.
pub fn pair_layout_bytes(keyed: &[(i64, u32)]) -> usize {
    let mut out = Vec::new();
    let mut previous = (0i64, 0u32);
    let mut total = 0;
    for &(key, doc) in keyed {
        out.clear();
        put((key - previous.0) as u64, &mut out);
        put(
            u64::from(if key == previous.0 {
                doc - previous.1
            } else {
                doc
            }),
            &mut out,
        );
        total += out.len();
        previous = (key, doc);
    }
    total
}

#[cfg(test)]
mod tests {
    use ferret_text::{Timestamp, has_timestamp_in, timestamps};

    use super::*;

    const DOCS: [&str; 4] = [
        "log 2026-10-12T03:04:05.123Z start",
        "log 2026-10-12T03:04:05.456Z start",
        "\"ts\":1791774245123,\"id\":1791774245",
        "2026-10-13 nothing at that second",
    ];

    fn stamps(text: &str) -> Vec<Timestamp> {
        let mut out = Vec::new();
        timestamps(text.as_bytes(), |stamp| out.push(stamp));
        out
    }

    fn index(granularity: i64) -> DateIndex {
        let mut pairs = Vec::new();
        for (doc, text) in DOCS.iter().enumerate() {
            pairs.extend(stamps(text).iter().map(|s| (s.seconds, doc as u32)));
        }
        DateIndex::build(granularity, &mut pairs)
    }

    fn verified(doc: u32, from: (i64, u32), to: (i64, u32)) -> bool {
        has_timestamp_in(DOCS[doc as usize].as_bytes(), from, to)
    }

    #[test]
    fn a_fraction_survives_only_through_verification() {
        let at = 1_791_774_245;
        let index = index(1);
        let mut docs = Vec::new();
        index.range(at, at + 1, &mut docs);
        // The id beside the millisecond epoch is not a timestamp, and the
        // second ISO stamp shares the key.
        assert_eq!(docs, [0, 1, 2]);
        let (from, to) = ((at, 123_000_000), (at, 124_000_000));
        let confirmed: Vec<_> = docs.iter().filter(|&&d| verified(d, from, to)).collect();
        assert_eq!(confirmed, [&0, &2]);
    }

    #[test]
    fn coarser_keys_widen_candidates_and_stay_sorted() {
        let at = 1_791_774_245;
        for (granularity, expected) in [(60, vec![0, 1, 2]), (86_400, vec![0, 1, 2])] {
            let mut docs = Vec::new();
            index(granularity).range(at, at + 1, &mut docs);
            assert_eq!(docs, expected, "{granularity}");
        }
        let day = index(86_400);
        let mut docs = Vec::new();
        let midnight = at - at % 86_400;
        day.range(midnight, midnight + 2 * 86_400, &mut docs);
        assert_eq!(docs, [0, 1, 2, 3]);
        assert!(day.exact(midnight, midnight + 86_400));
        assert!(!day.exact(at, at + 1));
        assert_eq!(day.keys(), 2);
        assert_eq!(day.docs_per_key(), [3, 1]);
    }

    #[test]
    fn skips_find_keys_across_blocks() {
        let mut pairs: Vec<(i64, u32)> = (0..1000).map(|i| (i * 7, (i % 13) as u32)).collect();
        let index = DateIndex::build(1, &mut pairs);
        assert_eq!(index.keys(), 1000);
        for probe in [0, 6, 7, 450, 3_500, 6_993, 7_000] {
            let mut docs = Vec::new();
            index.range(probe, probe + 15, &mut docs);
            let mut expected: Vec<u32> = (0..1000i64)
                .filter(|i| (probe..probe + 15).contains(&(i * 7)))
                .map(|i| (i % 13) as u32)
                .collect();
            expected.sort_unstable();
            expected.dedup();
            assert_eq!(docs, expected, "{probe}");
        }
    }
}
