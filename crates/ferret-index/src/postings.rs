//! [`Term`]: one term's postings across a view's segments, read and owned,
//! and its [`Cursor`]. Segments cover disjoint, ascending DocId ranges
//! (docs/S2.md § Segments are disjoint DocId ranges), so a term's documents
//! across them are a concatenation in segment order, never a merge, and
//! `next_geq` skips a whole segment whose range ends below its target.
//!
//! Every segment's dictionary block and list are read when the term is
//! (D64 A: one positional read each), so the cursor does no I/O and cannot
//! fail. A [`crate::Cursor`] borrows the `Term` it walks, which is why a
//! query reads its atoms before it builds its cursor tree.

use crate::segment::{Hit, PostingCursor, ReadAt, ReadError, Segment};

/// One term's documents in every segment that holds it, in range order.
/// Dead documents are included; the query's live filter hides them.
#[derive(Debug)]
pub struct Term {
    parts: Vec<Part>,
    docs: u64,
    bytes: u64,
}

/// The term's documents in one segment.
#[derive(Debug)]
struct Part {
    /// The segment's last DocId, inclusive.
    last: u32,
    hit: Hit,
}

impl Term {
    /// Looks `term` up in each of `segments`, which must be in DocId order
    /// with disjoint ranges, as a view's are. Segments out of order are a
    /// bug in the caller, reported as [`ReadError::Layout`] rather than
    /// answered wrongly.
    pub fn read<'s, R: ReadAt + 's>(
        segments: impl IntoIterator<Item = &'s Segment<R>>,
        term: &[u8],
    ) -> Result<Self, ReadError> {
        Self::read_until(segments, term, &|| false)
    }

    /// Like `read`, checking cancellation before each dictionary and list.
    pub fn read_until<'s, R: ReadAt + 's>(
        segments: impl IntoIterator<Item = &'s Segment<R>>,
        term: &[u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self, ReadError> {
        let mut found = Self {
            parts: Vec::new(),
            docs: 0,
            bytes: 0,
        };
        let mut floor = 0u64;
        for segment in segments {
            if cancelled() {
                return Err(ReadError::Cancelled);
            }
            let info = segment.info();
            if u64::from(info.first) < floor {
                return Err(ReadError::Layout);
            }
            floor = u64::from(info.last) + 1;
            let Some(entry) = segment.entry(term)? else {
                continue;
            };
            if cancelled() {
                return Err(ReadError::Cancelled);
            }
            let hit = segment.read(&entry)?;
            match &hit {
                Hit::Single(_) => found.docs += 1,
                Hit::Postings(list) => {
                    found.docs += u64::from(list.len());
                    found.bytes += list.bytes();
                }
            }
            found.parts.push(Part {
                last: info.last,
                hit,
            });
        }
        Ok(found)
    }

    /// Documents in the term's postings: an exact count, dead ones included.
    pub fn docs(&self) -> u64 {
        self.docs
    }

    /// Bytes of postings lists read for it; inlined singletons cost none.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// A cursor before the first document.
    pub fn cursor(&self) -> Cursor<'_> {
        Cursor {
            parts: &self.parts,
            at: 0,
            inner: None,
            docs: self.docs,
        }
    }
}

/// Ascending DocIds of one [`Term`], with intpack's `next_geq` semantics:
/// the cursor stays on the returned document, and targets never decrease.
pub struct Cursor<'a> {
    parts: &'a [Part],
    /// The part the cursor is in.
    at: usize,
    /// The list cursor of `parts[at]`, once entered.
    inner: Option<PostingCursor<'a>>,
    docs: u64,
}

impl Cursor<'_> {
    /// The smallest document `>= target` at or after the current one.
    pub fn next_geq(&mut self, target: u32) -> Option<u32> {
        let parts = self.parts;
        while let Some(part) = parts.get(self.at) {
            if target <= part.last {
                let found = match &part.hit {
                    Hit::Single(doc) => (*doc >= target).then_some(*doc),
                    Hit::Postings(list) => self
                        .inner
                        .get_or_insert_with(|| list.cursor())
                        .next_geq(target),
                };
                if found.is_some() {
                    return found;
                }
            }
            self.at += 1;
            self.inner = None;
        }
        None
    }

    /// The term's document count, the cursor's upper bound.
    pub fn cost(&self) -> u64 {
        self.docs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::Writer;

    /// A segment over `[first, last]` holding each `(term, docs)`.
    fn segment(first: u32, last: u32, terms: &[(&str, &[u32])]) -> Segment<Vec<u8>> {
        let mut writer = Writer::new(first, last).unwrap();
        for (term, docs) in terms {
            writer.push(term.as_bytes(), docs).unwrap();
        }
        let mut bytes = Vec::new();
        writer.finish(&mut bytes).unwrap();
        Segment::open(bytes).unwrap()
    }

    /// Three segments with gaps between their ranges: `[10, 400]`, `[500,
    /// 509]`, `[600, 1000]`. `t` is absent from the middle one and is a
    /// singleton in the last; `u` is in all three.
    fn segments() -> Vec<Segment<Vec<u8>>> {
        let long: Vec<u32> = (10..=400).step_by(3).collect();
        vec![
            segment(10, 400, &[("t", &long), ("u", &[10, 400])]),
            segment(500, 509, &[("u", &[500, 509])]),
            segment(600, 1000, &[("t", &[1000]), ("u", &[600, 601])]),
        ]
    }

    fn all(term: &Term) -> Vec<u32> {
        let mut cursor = term.cursor();
        let mut docs = Vec::new();
        let mut target = 0;
        while let Some(doc) = cursor.next_geq(target) {
            docs.push(doc);
            target = doc + 1;
        }
        docs
    }

    #[test]
    fn a_term_concatenates_its_segments_and_skips_the_one_without_it() {
        let segments = segments();
        let t = Term::read(&segments, b"t").unwrap();
        let mut expected: Vec<u32> = (10..=400).step_by(3).collect();
        expected.push(1000);
        assert_eq!(all(&t), expected);
        assert_eq!(t.docs(), expected.len() as u64);
        assert_eq!(t.parts.len(), 2, "the middle segment holds no `t`");
        let u = Term::read(&segments, b"u").unwrap();
        assert_eq!(all(&u), [10, 400, 500, 509, 600, 601]);
        assert_eq!(Term::read(&segments, b"v").unwrap().docs(), 0);
    }

    #[test]
    fn next_geq_lands_in_gaps_and_on_range_edges() {
        let segments = segments();
        let u = Term::read(&segments, b"u").unwrap();
        // (target, expected): before everything, a segment's first and last
        // DocId, the gaps between ranges, and past the end.
        let cases = [
            (0, Some(10)),
            (10, Some(10)),
            (11, Some(400)),
            (400, Some(400)),
            (401, Some(500)),
            (450, Some(500)),
            (509, Some(509)),
            (510, Some(600)),
            (599, Some(600)),
            (601, Some(601)),
            (602, None),
            (1000, None),
        ];
        for (target, expected) in cases {
            assert_eq!(u.cursor().next_geq(target), expected, "fresh, {target}");
        }
        // The same targets in order on one cursor, which also stays on its
        // document when a target repeats or falls below it.
        let mut cursor = u.cursor();
        for (target, expected) in cases {
            assert_eq!(cursor.next_geq(target), expected, "walk, {target}");
            if let Some(doc) = expected {
                assert_eq!(cursor.next_geq(target), Some(doc), "repeat, {target}");
            }
        }

        let t = Term::read(&segments, b"t").unwrap();
        let mut cursor = t.cursor();
        assert_eq!(cursor.next_geq(399), Some(400));
        assert_eq!(cursor.next_geq(401), Some(1000), "skips the middle segment");
        assert_eq!(cursor.next_geq(1000), Some(1000));
        assert_eq!(cursor.next_geq(1001), None);
    }

    #[test]
    fn segments_out_of_order_are_an_error() {
        let mut segments = segments();
        segments.swap(0, 2);
        assert!(matches!(
            Term::read(&segments, b"u"),
            Err(ReadError::Layout)
        ));
    }
}
