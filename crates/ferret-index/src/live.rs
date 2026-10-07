//! [`DocSet`]: a set of DocIds below a bound, as a bitset. The host builds
//! the live set of one published catalog view with it (docs/S2.md §
//! Liveness), and the index reads liveness from it rather than keeping
//! tombstones of its own (DESIGN § The catalog).

/// DocIds in `[0, bound)`, one bit each: 1.0 MB at 8M documents.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocSet {
    words: Vec<u64>,
    bound: u32,
    len: u32,
}

impl DocSet {
    /// The set of `docs`, each below `bound`, in any order. An id at or
    /// above `bound` is ignored, so a caller cannot widen the set past the
    /// view it describes.
    pub fn new(bound: u32, docs: impl IntoIterator<Item = u32>) -> Self {
        let mut set = Self::empty(bound);
        for doc in docs {
            set.insert(doc);
        }
        set
    }

    /// Ids a cancellable build reads or copies between asks.
    pub const CHECK_EVERY: usize = 4096;

    /// No members, with room for ids below `bound`.
    pub fn empty(bound: u32) -> Self {
        Self {
            words: vec![0; (bound as usize).div_ceil(64)],
            bound,
            len: 0,
        }
    }

    /// Adds `doc`; an id at or above the bound is ignored, as in
    /// [`DocSet::new`].
    pub fn insert(&mut self, doc: u32) {
        if doc < self.bound {
            let (word, bit) = (doc as usize / 64, 1u64 << (doc % 64));
            if self.words[word] & bit == 0 {
                self.words[word] |= bit;
                self.len += 1;
            }
        }
    }

    /// This set's members at or above `from`, and those of `extra` that
    /// are members: a view's uncovered set, with the same bound. Asks
    /// `cancelled` before allocating, after every [`DocSet::CHECK_EVERY`]
    /// ids of bitmap copied, empty words included, and after every
    /// [`DocSet::CHECK_EVERY`] ids of `extra` read, members or not: `None`
    /// once it answers true.
    pub fn tail_until(
        &self,
        from: u32,
        extra: impl IntoIterator<Item = u32>,
        cancelled: impl Fn() -> bool,
    ) -> Option<Self> {
        const WORDS: usize = DocSet::CHECK_EVERY / 64;
        if cancelled() {
            return None;
        }
        let mut set = Self::empty(self.bound);
        let from = from.min(self.bound);
        let first = from as usize / 64;
        for (i, &word) in self.words.iter().enumerate().skip(first) {
            if (i - first + 1).is_multiple_of(WORDS) && cancelled() {
                return None;
            }
            let word = if i == first {
                word & (!0u64 << (from % 64))
            } else {
                word
            };
            set.words[i] = word;
            set.len += word.count_ones();
        }
        for (seen, doc) in extra.into_iter().enumerate() {
            if (seen + 1).is_multiple_of(Self::CHECK_EVERY) && cancelled() {
                return None;
            }
            if self.contains(doc) {
                set.insert(doc);
            }
        }
        Some(set)
    }

    /// One past the largest id the set can hold: the view's `next_doc`.
    pub fn bound(&self) -> u32 {
        self.bound
    }

    pub fn len(&self) -> u32 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn contains(&self, doc: u32) -> bool {
        doc < self.bound && self.words[doc as usize / 64] >> (doc % 64) & 1 == 1
    }

    /// The smallest member `>= from`.
    pub fn next_geq(&self, from: u32) -> Option<u32> {
        self.next_geq_with(from, &mut || false)
    }

    pub(crate) fn next_geq_with(
        &self,
        from: u32,
        checkpoint: &mut impl FnMut() -> bool,
    ) -> Option<u32> {
        if checkpoint() {
            return None;
        }
        if from >= self.bound {
            return None;
        }
        let mut word = from as usize / 64;
        // Bits at or past `bound` are never set, so no end mask is needed.
        let mut bits = self.words[word] & (!0u64 << (from % 64));
        while bits == 0 {
            if checkpoint() {
                return None;
            }
            word += 1;
            bits = *self.words.get(word)?;
        }
        Some(word as u32 * 64 + bits.trailing_zeros())
    }

    /// Members in `[from, to)`, ascending.
    pub fn range(&self, from: u32, to: u32) -> impl Iterator<Item = u32> + '_ {
        let to = to.min(self.bound);
        let from = from.min(to);
        let words = (from as usize / 64)..(to as usize).div_ceil(64);
        words.flat_map(move |w| {
            let mut bits = self.words[w];
            std::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let doc = w as u32 * 64 + bits.trailing_zeros();
                bits &= bits - 1;
                Some(doc)
            })
            .filter(move |&doc| doc >= from && doc < to)
        })
    }

    /// Members in `[from, to)`, counted.
    pub fn count(&self, from: u32, to: u32) -> u32 {
        self.range(from, to).count() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_and_counts_agree_with_a_sorted_list() {
        let docs = [0, 1, 63, 64, 65, 127, 128, 200, 255];
        let set = DocSet::new(256, docs.iter().copied().chain([300, 1]));
        assert_eq!(
            set.len(),
            docs.len() as u32,
            "duplicates and ids past the bound are dropped"
        );
        for from in 0..=257 {
            for to in [from, from + 1, from + 63, from + 64, from + 65, 400] {
                let expected: Vec<u32> = docs
                    .iter()
                    .copied()
                    .filter(|&d| d >= from && d < to)
                    .collect();
                assert_eq!(
                    set.range(from, to).collect::<Vec<_>>(),
                    expected,
                    "[{from}, {to})"
                );
                assert_eq!(set.count(from, to), expected.len() as u32);
                assert_eq!(
                    set.next_geq(from),
                    docs.iter().copied().find(|&d| d >= from),
                    "next_geq({from})"
                );
            }
        }
        assert!(set.contains(64) && !set.contains(66) && !set.contains(300));
    }

    /// `tail_until` agrees with its definition: members at or above `from`,
    /// plus `extra`'s members, at word edges and past the bound.
    #[test]
    fn tail_is_the_members_from_a_point_plus_the_extra_members() {
        let docs = [0, 1, 63, 64, 65, 127, 128, 200, 255, 4200, 9000];
        let set = DocSet::new(9001, docs);
        let extra = [0, 2, 64, 300, 9000, 9001, 20000];
        for from in [
            0, 1, 63, 64, 65, 128, 129, 4096, 4200, 8999, 9000, 9001, 10000,
        ] {
            let expected = DocSet::new(
                set.bound(),
                docs.iter()
                    .copied()
                    .filter(|&d| d >= from)
                    .chain(extra.iter().copied().filter(|&d| set.contains(d))),
            );
            assert_eq!(
                set.tail_until(from, extra, || false),
                Some(expected),
                "{from}"
            );
        }
        assert_eq!(set.tail_until(0, extra, || true), None);
    }
}
