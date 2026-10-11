//! Substring-local gram extraction. Fold before weighting: weighting original
//! case would make a query's keys differ from a matching document's keys.

use ferret_text::trigram_key;

#[derive(Clone, Copy, Debug)]
pub(super) enum Variant {
    Trigram,
    Sparse { frequency: bool, cap: usize },
    Fixed(usize),
}

impl Variant {
    pub(super) const ALL: [Self; 7] = [
        Self::Trigram,
        Self::Sparse {
            frequency: false,
            cap: 8,
        },
        Self::Sparse {
            frequency: false,
            cap: 32,
        },
        Self::Sparse {
            frequency: true,
            cap: 8,
        },
        Self::Sparse {
            frequency: true,
            cap: 32,
        },
        Self::Fixed(4),
        Self::Fixed(8),
    ];

    pub(super) fn label(self) -> String {
        match self {
            Self::Trigram => "trigram".into(),
            Self::Sparse { frequency, cap } => {
                format!(
                    "sparse-{}-{cap}",
                    if frequency { "frequency" } else { "hash" }
                )
            }
            Self::Fixed(length) => format!("fixed-{length}"),
        }
    }
}

pub(super) fn fold(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .map(|&byte| trigram_key([byte; 3])[0])
        .collect()
}

pub(super) fn weights(counts: &[u64], frequency: bool) -> Vec<(u64, u16)> {
    counts
        .iter()
        .enumerate()
        .map(|(pair, &count)| {
            let mut hash = (pair as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
            hash = (hash ^ (hash >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            hash = (hash ^ (hash >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            hash ^= hash >> 31;
            (if frequency { u64::MAX - count } else { hash }, pair as u16)
        })
        .collect()
}

/// Emits all endpoint-dominant intervals, with at most two emissions per
/// bigram. Equal endpoint ranks are allowed; an equal interior rank blocks
/// a longer interval. The stack replaces equal ranks after visiting the pair.
/// Absolute offsets never break ties, so extracting a literal in isolation
/// produces precisely the grams wholly contained in its document occurrence.
pub(super) fn extract<'a>(
    bytes: &'a [u8],
    variant: Variant,
    weights: &[(u64, u16)],
    mut emit: impl FnMut(&'a [u8]),
) {
    let cap = match variant {
        Variant::Trigram => {
            bytes.windows(3).for_each(emit);
            return;
        }
        Variant::Fixed(length) => {
            bytes.windows(3).for_each(&mut emit);
            bytes.windows(length).for_each(emit);
            return;
        }
        Variant::Sparse { cap, .. } => cap,
    };
    let mut stack: Vec<(usize, (u64, u16))> = Vec::new();
    for (right, pair) in bytes.windows(2).enumerate() {
        let rank = weights[usize::from(u16::from_be_bytes([pair[0], pair[1]]))];
        while let Some(&(left, previous)) = stack.last() {
            if previous >= rank {
                break;
            }
            stack.pop();
            if right + 2 - left <= cap {
                emit(&bytes[left..right + 2]);
            }
        }
        if let Some(&(left, previous)) = stack.last() {
            if right + 2 - left <= cap {
                emit(&bytes[left..right + 2]);
            }
            if previous == rank {
                stack.pop();
            }
        }
        stack.push((right, rank));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn stack_matches_the_definition_including_repeated_pair_ties() {
        // Exhaustive small alphabets distinguish strict interior ties from
        // offset tie-breaking, and test both weight functions and both caps.
        let mut counts = vec![0; 1 << 16];
        counts[usize::from(u16::from_be_bytes(*b"ab"))] = 3;
        let tables = [weights(&counts, false), weights(&counts, true)];
        for number in 0..3usize.pow(8) {
            let mut n = number;
            let bytes: Vec<_> = (0..8)
                .map(|_| {
                    let byte = b'a' + (n % 3) as u8;
                    n /= 3;
                    byte
                })
                .collect();
            for variant in Variant::ALL {
                let Variant::Sparse { frequency, cap } = variant else {
                    continue;
                };
                let weights = &tables[usize::from(frequency)];
                let mut actual = BTreeSet::new();
                extract(&bytes, variant, weights, |gram| {
                    actual.insert(gram.to_vec());
                });
                let mut expected = BTreeSet::new();
                for start in 0..bytes.len() {
                    for end in start + 3..=bytes.len().min(start + cap) {
                        let ranks: Vec<_> = bytes[start..end]
                            .windows(2)
                            .map(|p| weights[usize::from(u16::from_be_bytes([p[0], p[1]]))])
                            .collect();
                        if ranks[1..ranks.len() - 1]
                            .iter()
                            .all(|&rank| rank < ranks[0] && rank < ranks[ranks.len() - 1])
                        {
                            expected.insert(bytes[start..end].to_vec());
                        }
                    }
                }
                assert_eq!(actual, expected);
                assert!(bytes.windows(3).all(|gram| actual.contains(gram)));
            }
        }
    }

    #[test]
    fn extracting_substrings_is_independent_of_outside_boundaries() {
        let bytes = fold(b"AaBaCaBAaDDa");
        for variant in Variant::ALL {
            let weights = weights(&vec![0; 1 << 16], false);
            let mut whole = BTreeSet::new();
            extract(&bytes, variant, &weights, |gram| {
                whole.insert(gram);
            });
            for start in 0..bytes.len() {
                for end in start..=bytes.len() {
                    extract(&bytes[start..end], variant, &weights, |gram| {
                        assert!(whole.contains(gram));
                    });
                }
            }
        }
    }
}
