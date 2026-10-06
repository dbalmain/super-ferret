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
        let mut set = Self {
            words: vec![0; (bound as usize).div_ceil(64)],
            bound,
            len: 0,
        };
        for doc in docs {
            if doc < bound {
                let (word, bit) = (doc as usize / 64, 1u64 << (doc % 64));
                if set.words[word] & bit == 0 {
                    set.words[word] |= bit;
                    set.len += 1;
                }
            }
        }
        set
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
        assert_eq!(set.len(), docs.len() as u32, "duplicates and ids past the bound are dropped");
        for from in 0..=257 {
            for to in [from, from + 1, from + 63, from + 64, from + 65, 400] {
                let expected: Vec<u32> =
                    docs.iter().copied().filter(|&d| d >= from && d < to).collect();
                assert_eq!(set.range(from, to).collect::<Vec<_>>(), expected, "[{from}, {to})");
                assert_eq!(set.count(from, to), expected.len() as u32);
            }
        }
        assert!(set.contains(64) && !set.contains(66) && !set.contains(300));
    }
}
