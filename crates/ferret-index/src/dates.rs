//! The token-policy experiment's date indexes (docs/TOKENS.md, T2): which
//! documents mention an instant. Dates are numbers, not terms: each layout
//! stores `(time, doc)` entries as integers, so a unique second costs only its
//! encoded bits.
//!
//! Instants are whole seconds since the epoch, already normalised to UTC by
//! `ferret_text::timestamps`, which also drops the fraction. Each layout keeps
//! time to a *resolution* (one second, a minute, ...): an entry stands for the
//! cell `[t, t + resolution)`. A query range covering a whole cell answers it
//! exactly; a cell the range only clips is a candidate the host verifies
//! against the document, as is every candidate of a query with a fraction.
//! Repeated `(cell, doc)` entries are stored once.
//!
//! Two layouts, both in memory only (the experiment measures their shape, not
//! a file format):
//!
//! - [`TimeMajor`]: entries sorted by `(cell, doc)` in blocks of [`BLOCK`].
//!   Each block header holds its first and last cell; cells are delta-coded and
//!   bit-packed at the block's widest delta, doc ids bit-packed at the width of
//!   the largest doc id. Doc ids in time order do not compress.
//! - [`Bucketed`]: one entry per bucket (an hour, a day) in a directory; per
//!   bucket a delta-coded doc posting list, and per doc its in-bucket offsets
//!   bit-packed at `log2(bucket / resolution)` bits. With the resolution equal
//!   to the bucket it stores no offsets at all and verifies instead.

use crate::segment::{put, take};

/// Entries per [`TimeMajor`] block.
pub const BLOCK: usize = 128;

/// Bits to hold `max`.
fn width(max: u64) -> u32 {
    64 - max.leading_zeros()
}

/// A bit-packed array, least significant bit first.
#[derive(Debug, Default)]
struct Bits {
    words: Vec<u64>,
    len: usize,
}

impl Bits {
    fn push(&mut self, value: u64, width: u32) {
        debug_assert!(width == 64 || value < 1 << width);
        if width == 0 {
            return;
        }
        let (word, bit) = (self.len / 64, (self.len % 64) as u32);
        if word == self.words.len() {
            self.words.push(0);
        }
        self.words[word] |= value << bit;
        if bit + width > 64 {
            self.words.push(value >> (64 - bit));
        }
        self.len += width as usize;
    }

    fn get(&self, at: usize, width: u32) -> u64 {
        if width == 0 {
            return 0;
        }
        let (word, bit) = (at / 64, (at % 64) as u32);
        let mut value = self.words[word] >> bit;
        if bit + width > 64 {
            value |= self.words[word + 1] << (64 - bit);
        }
        if width == 64 {
            value
        } else {
            value & ((1 << width) - 1)
        }
    }
}

/// Encoded size, split by what the bytes hold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Size {
    /// Stored `(cell, doc)` entries after deduplication.
    pub entries: usize,
    /// Time: cell deltas, or in-bucket offsets.
    pub time_bits: usize,
    /// Doc ids, and per-doc offset counts in [`Bucketed`].
    pub doc_bits: usize,
    /// Block headers or the bucket directory.
    pub header_bytes: usize,
}

impl Size {
    pub fn bytes(&self) -> usize {
        self.time_bits.div_ceil(8) + self.doc_bits.div_ceil(8) + self.header_bytes
    }
}

/// A query's documents: those an overlapping cell proves, and the rest, which
/// only clipped cells name and the host must verify. Both sorted and
/// disjoint.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Hits {
    pub exact: Vec<u32>,
    pub maybe: Vec<u32>,
}

impl Hits {
    fn add(&mut self, cell: i64, resolution: i64, from: i64, to: i64, doc: u32) {
        if cell >= from && cell + resolution <= to {
            self.exact.push(doc);
        } else if cell < to && cell + resolution > from {
            self.maybe.push(doc);
        }
    }

    fn finish(&mut self) {
        self.exact.sort_unstable();
        self.exact.dedup();
        self.maybe.sort_unstable();
        self.maybe.dedup();
        let exact = &self.exact;
        self.maybe.retain(|doc| exact.binary_search(doc).is_err());
    }

    pub fn len(&self) -> usize {
        self.exact.len() + self.maybe.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Cells of `resolution` seconds from `(seconds, doc)` pairs, sorted and
/// deduplicated. Instants outside `0..2^32` are dropped: a u32 holds UTC
/// seconds until 2106.
fn cells(resolution: i64, pairs: &[(i64, u32)]) -> Vec<(u32, u32)> {
    assert!(resolution > 0, "a cell must span at least one second");
    let mut out: Vec<(u32, u32)> = pairs
        .iter()
        .filter(|&&(seconds, _)| (0..1 << 32).contains(&seconds))
        .map(|&(seconds, doc)| ((seconds / resolution) as u32, doc))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

#[derive(Debug)]
struct Block {
    first: u32,
    last: u32,
    /// Bit offset of the block's cell deltas; its docs follow.
    at: usize,
    width: u32,
    len: usize,
}

/// Bytes a [`TimeMajor`] block header would take on disk: first and last
/// cell, bit offset, delta width.
const BLOCK_HEADER: usize = 4 + 4 + 4 + 1;

/// Time-major layout: sorted `(cell, doc)` entries in blocks.
#[derive(Debug)]
pub struct TimeMajor {
    resolution: i64,
    doc_width: u32,
    blocks: Vec<Block>,
    bits: Bits,
    size: Size,
}

impl TimeMajor {
    /// Builds from `(seconds, doc)` pairs in any order.
    pub fn build(resolution: i64, pairs: &[(i64, u32)]) -> Self {
        let entries = cells(resolution, pairs);
        let doc_width = width(entries.iter().map(|e| u64::from(e.1)).max().unwrap_or(0));
        let mut index = Self {
            resolution,
            doc_width,
            blocks: Vec::new(),
            bits: Bits::default(),
            size: Size {
                entries: entries.len(),
                ..Size::default()
            },
        };
        for block in entries.chunks(BLOCK) {
            let first = block[0].0;
            let width = width(
                block
                    .windows(2)
                    .map(|w| u64::from(w[1].0 - w[0].0))
                    .max()
                    .unwrap_or(0),
            );
            index.blocks.push(Block {
                first,
                last: block[block.len() - 1].0,
                at: index.bits.len,
                width,
                len: block.len(),
            });
            let mut previous = first;
            for &(cell, _) in block {
                index.bits.push(u64::from(cell - previous), width);
                previous = cell;
            }
            for &(_, doc) in block {
                index.bits.push(u64::from(doc), doc_width);
            }
            index.size.time_bits += width as usize * block.len();
            index.size.doc_bits += doc_width as usize * block.len();
        }
        index.size.header_bytes = index.blocks.len() * BLOCK_HEADER + 1;
        index
    }

    pub fn size(&self) -> Size {
        self.size
    }

    pub fn resolution(&self) -> i64 {
        self.resolution
    }

    /// Documents with an entry overlapping the seconds `from..to`.
    pub fn range(&self, from: i64, to: i64) -> Hits {
        let mut hits = Hits::default();
        if to <= from {
            return hits;
        }
        let r = self.resolution;
        let (first, last) = (from.div_euclid(r), (to - 1).div_euclid(r));
        let start = self.blocks.partition_point(|b| i64::from(b.last) < first);
        for block in &self.blocks[start..] {
            if i64::from(block.first) > last {
                break;
            }
            let docs_at = block.at + block.width as usize * block.len;
            let mut cell = i64::from(block.first);
            for i in 0..block.len {
                cell += self
                    .bits
                    .get(block.at + i * block.width as usize, block.width)
                    as i64;
                if cell < first {
                    continue;
                }
                if cell > last {
                    break;
                }
                let doc = self
                    .bits
                    .get(docs_at + i * self.doc_width as usize, self.doc_width);
                hits.add(cell * r, r, from, to, doc as u32);
            }
        }
        hits.finish();
        hits
    }
}

/// Bytes a [`Bucketed`] directory entry would take on disk: bucket key, byte
/// offset, bit offset.
const DIRECTORY_ENTRY: usize = 4 + 4 + 4;

/// Bucketed layout: per bucket a doc posting list with in-bucket offsets.
#[derive(Debug)]
pub struct Bucketed {
    bucket: i64,
    resolution: i64,
    offset_width: u32,
    /// Bucket key, then where its docs and its offsets start.
    directory: Vec<(u32, usize, usize)>,
    docs: Vec<u8>,
    offsets: Bits,
    size: Size,
}

impl Bucketed {
    /// Builds from `(seconds, doc)` pairs in any order. `bucket` must be a
    /// multiple of `resolution`; equal, it stores no offsets.
    pub fn build(bucket: i64, resolution: i64, pairs: &[(i64, u32)]) -> Self {
        assert!(
            resolution > 0 && bucket % resolution == 0,
            "a bucket holds whole cells"
        );
        let per = bucket / resolution;
        let mut entries: Vec<(u32, u32, u32)> = cells(resolution, pairs)
            .into_iter()
            .map(|(cell, doc)| {
                let cell = i64::from(cell);
                ((cell / per) as u32, doc, (cell % per) as u32)
            })
            .collect();
        entries.sort_unstable();
        let offset_width = width((per - 1) as u64);
        let mut index = Self {
            bucket,
            resolution,
            offset_width,
            directory: Vec::new(),
            docs: Vec::new(),
            offsets: Bits::default(),
            size: Size {
                entries: entries.len(),
                ..Size::default()
            },
        };
        for group in entries.chunk_by(|a, b| a.0 == b.0) {
            index
                .directory
                .push((group[0].0, index.docs.len(), index.offsets.len));
            let by_doc: Vec<&[(u32, u32, u32)]> = group.chunk_by(|a, b| a.1 == b.1).collect();
            put(by_doc.len() as u64, &mut index.docs);
            let mut last = 0;
            for (i, doc) in by_doc.iter().enumerate() {
                let id = doc[0].1;
                put(
                    u64::from(if i == 0 { id } else { id - last }),
                    &mut index.docs,
                );
                last = id;
                if offset_width > 0 {
                    put(doc.len() as u64 - 1, &mut index.docs);
                    for &(_, _, offset) in *doc {
                        index.offsets.push(u64::from(offset), offset_width);
                    }
                }
            }
        }
        index.size.time_bits = index.offsets.len;
        index.size.doc_bits = index.docs.len() * 8;
        index.size.header_bytes = index.directory.len() * DIRECTORY_ENTRY;
        index
    }

    pub fn size(&self) -> Size {
        self.size
    }

    pub fn bucket(&self) -> i64 {
        self.bucket
    }

    pub fn resolution(&self) -> i64 {
        self.resolution
    }

    /// Documents with an entry overlapping the seconds `from..to`.
    pub fn range(&self, from: i64, to: i64) -> Hits {
        let mut hits = Hits::default();
        if to <= from {
            return hits;
        }
        let (first, last) = (
            from.div_euclid(self.bucket),
            (to - 1).div_euclid(self.bucket),
        );
        let start = self
            .directory
            .partition_point(|&(key, _, _)| i64::from(key) < first);
        for &(key, mut at, mut bit) in &self.directory[start..] {
            let key = i64::from(key);
            if key > last {
                break;
            }
            let base = key * self.bucket;
            let count = take(&self.docs, &mut at).unwrap_or(0);
            let mut doc = 0u32;
            for i in 0..count {
                let value = take(&self.docs, &mut at).unwrap_or(0) as u32;
                doc = if i == 0 { value } else { doc + value };
                if self.offset_width == 0 {
                    hits.add(base, self.bucket, from, to, doc);
                    continue;
                }
                let n = take(&self.docs, &mut at).unwrap_or(0) + 1;
                for _ in 0..n {
                    let offset = self.offsets.get(bit, self.offset_width) as i64;
                    bit += self.offset_width as usize;
                    hits.add(
                        base + offset * self.resolution,
                        self.resolution,
                        from,
                        to,
                        doc,
                    );
                }
            }
        }
        hits.finish();
        hits
    }
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
    const AT: i64 = 1_791_774_245;

    fn stamps(text: &str) -> Vec<Timestamp> {
        let mut out = Vec::new();
        timestamps(text.as_bytes(), |stamp| out.push(stamp));
        out
    }

    fn pairs() -> Vec<(i64, u32)> {
        let mut pairs = Vec::new();
        for (doc, text) in DOCS.iter().enumerate() {
            pairs.extend(stamps(text).iter().map(|s| (s.seconds, doc as u32)));
        }
        pairs
    }

    type Range = Box<dyn Fn(i64, i64) -> Hits>;

    /// Every layout under test, labelled `kind/.../resolution`.
    fn layouts(pairs: &[(i64, u32)]) -> Vec<(String, Range)> {
        let mut out: Vec<(String, Range)> = Vec::new();
        for r in [1, 60] {
            let index = TimeMajor::build(r, pairs);
            out.push((
                format!("time-major/{r}"),
                Box::new(move |a, b| index.range(a, b)),
            ));
        }
        for (bucket, r) in [(86_400, 1), (86_400, 60), (3_600, 1), (86_400, 86_400)] {
            let index = Bucketed::build(bucket, r, pairs);
            out.push((
                format!("bucketed/{bucket}/{r}"),
                Box::new(move |a, b| index.range(a, b)),
            ));
        }
        out
    }

    fn all(hits: &Hits) -> Vec<u32> {
        let mut out: Vec<u32> = hits.exact.iter().chain(&hits.maybe).copied().collect();
        out.sort_unstable();
        out
    }

    #[test]
    fn a_fraction_survives_only_through_verification() {
        for (label, range) in layouts(&pairs()) {
            // The id beside the millisecond epoch is not a timestamp, and the
            // second ISO stamp shares the second.
            let found = all(&range(AT, AT + 1));
            assert_eq!(found, [0, 1, 2], "{label}");
            let (from, to) = ((AT, 123_000_000), (AT, 124_000_000));
            let confirmed: Vec<_> = found
                .iter()
                .filter(|&&d| has_timestamp_in(DOCS[d as usize].as_bytes(), from, to))
                .collect();
            assert_eq!(confirmed, [&0, &2], "{label}");
        }
    }

    #[test]
    fn whole_cells_are_exact_and_clipped_cells_are_candidates() {
        let midnight = AT - AT % 86_400;
        for (label, range) in layouts(&pairs()) {
            // A whole UTC day: exact at every resolution up to a day.
            let day = range(midnight, midnight + 86_400);
            assert_eq!(day.exact, [0, 1, 2], "{label}");
            assert!(day.maybe.is_empty(), "{label}");
            // The next day holds doc 3's bare date, at midnight.
            let next = range(midnight + 86_400, midnight + 2 * 86_400);
            assert_eq!(next.exact, [3], "{label}");
            // One second: exact only where the resolution is a second.
            let fine = label.ends_with("/1");
            assert_eq!(range(AT, AT + 1).maybe.is_empty(), fine, "{label}");
            // A range just missing the instant, inside its minute.
            let before = range(AT - 4, AT);
            assert!(before.exact.is_empty(), "{label}");
            assert_eq!(before.maybe.is_empty(), fine, "{label}");
        }
    }

    #[test]
    fn layouts_agree_with_a_scan_across_blocks() {
        // Many docs per second, many seconds per doc, spanning blocks and
        // buckets; a cell boundary falls inside most probes.
        let pairs: Vec<(i64, u32)> = (0..5_000i64)
            .map(|i| (1_700_000_000 + i * 37 % 200_000, (i * 7_919 % 1_000) as u32))
            .collect();
        for (label, range) in layouts(&pairs) {
            let r: i64 = label
                .rsplit('/')
                .next()
                .and_then(|r| r.parse().ok())
                .unwrap_or(1);
            for probe in [0i64, 1, 59, 3_599, 86_399, 100_000, 199_990] {
                let (from, to) = (1_700_000_000 + probe, 1_700_000_000 + probe + 61);
                let hits = range(from, to);
                let overlaps = |s: i64| {
                    let cell = s - s.rem_euclid(r);
                    cell < to && cell + r > from
                };
                let mut expected: Vec<u32> = pairs
                    .iter()
                    .filter(|&&(s, _)| overlaps(s))
                    .map(|&(_, d)| d)
                    .collect();
                expected.sort_unstable();
                expected.dedup();
                assert_eq!(all(&hits), expected, "{label} {probe}");
                // An exact doc truly holds an instant in range.
                for doc in &hits.exact {
                    assert!(
                        pairs
                            .iter()
                            .any(|&(s, d)| d == *doc && (from..to).contains(&s)),
                        "{label} {probe} {doc}"
                    );
                }
            }
        }
    }

    #[test]
    fn repeated_entries_are_stored_once_and_offsets_cost_their_bits() {
        let pairs = [(AT, 5), (AT, 5), (AT + 1, 5), (AT, 6)];
        assert_eq!(TimeMajor::build(1, &pairs).size().entries, 3);
        assert_eq!(TimeMajor::build(60, &pairs).size().entries, 2);
        let day = Bucketed::build(86_400, 1, &pairs);
        assert_eq!((day.size().entries, day.size().time_bits), (3, 3 * 17));
        let hour = Bucketed::build(3_600, 60, &pairs);
        assert_eq!((hour.size().entries, hour.size().time_bits), (2, 2 * 6));
        assert_eq!(Bucketed::build(86_400, 86_400, &pairs).size().time_bits, 0);
    }
}
