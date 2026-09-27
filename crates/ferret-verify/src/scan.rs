//! [`Finder`]: substring search with optional ASCII case folding, at SIMD
//! speed, without a folded copy of the haystack (D41 B).
//!
//! A two-byte candidate filter: two positions of the needle, chosen for
//! rarity, are tested at every start in the haystack, each in both ASCII
//! cases when folding; a start where both agree is verified against the
//! whole needle. The filter is exact on its two bytes, because a letter
//! matches either case exactly when `byte | 0x20` equals its lower case,
//! and any other byte is compared as is.
//!
//! Two arms run the filter:
//!
//! - [`Arm::Avx2`], 64 starts per step through `core::arch`, chosen at runtime
//!   when the CPU has AVX2. It is the crate's one `unsafe` item and its
//!   toolchain-ledger row ([`crate::toolchain`]).
//! - [`Arm::Swar`], 8 starts per step in a `u64`, safe and portable: the
//!   fallback on any other CPU, and the tail of every AVX2 scan.
//!
//! A needle never matches across a NUL unless it contains one, and folding
//! touches ASCII letters only: a byte of 0x80 or above matches itself.

/// Which implementation of the candidate filter runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arm {
    /// 8 starts per step in a `u64`; runs everywhere.
    Swar,
    /// 64 starts per step with AVX2; x86_64 with AVX2 only.
    Avx2,
}

impl Arm {
    /// The fastest arm this CPU runs.
    pub fn best() -> Arm {
        if Arm::Avx2.is_available() {
            Arm::Avx2
        } else {
            Arm::Swar
        }
    }

    /// Whether this CPU can run the arm.
    pub fn is_available(self) -> bool {
        match self {
            Arm::Swar => true,
            Arm::Avx2 => avx2::available(),
        }
    }
}

/// A needle, compiled for search. Cheap to build: pick two bytes, fold the
/// needle.
#[derive(Clone, Debug)]
pub struct Finder {
    /// The needle, lower-cased when folding.
    needle: Box<[u8]>,
    fold: bool,
    pair: Pair,
    arm: Arm,
}

/// The two candidate bytes: a start `s` is a candidate when
/// `hay[s + at] | mask == byte` for both.
#[derive(Clone, Copy, Debug)]
struct Pair {
    at: [usize; 2],
    byte: [u8; 2],
    mask: [u8; 2],
}

impl Finder {
    /// A finder for `needle`, folding ASCII case when `fold` is true, on the
    /// fastest arm this CPU runs. An empty needle matches at every offset.
    pub fn new(needle: &[u8], fold: bool) -> Finder {
        Self::with_arm(needle, fold, Arm::best())
    }

    /// A finder on a chosen arm, for tests and benchmarks.
    ///
    /// # Panics
    ///
    /// If the CPU cannot run `arm`.
    pub fn with_arm(needle: &[u8], fold: bool, arm: Arm) -> Finder {
        assert!(arm.is_available(), "{arm:?} is not available on this CPU");
        let needle: Box<[u8]> = if fold {
            needle.to_ascii_lowercase().into()
        } else {
            needle.into()
        };
        let pair = Pair::choose(&needle, fold);
        Finder {
            needle,
            fold,
            pair,
            arm,
        }
    }

    /// The needle, lower-cased if folding.
    pub fn needle(&self) -> &[u8] {
        &self.needle
    }

    /// Whether this finder folds ASCII case.
    pub fn folds(&self) -> bool {
        self.fold
    }

    /// The first match starting at or after `from`, as its start offset.
    pub fn find_from(&self, haystack: &[u8], from: usize) -> Option<usize> {
        if self.needle.is_empty() {
            return (from <= haystack.len()).then_some(from);
        }
        let last = haystack.len().checked_sub(self.needle.len())?;
        if from > last {
            return None;
        }
        match self.arm {
            Arm::Avx2 => avx2::find(self, haystack, from),
            Arm::Swar => swar(self, haystack, from),
        }
    }

    /// Whether `haystack` contains the needle anywhere.
    pub fn is_match(&self, haystack: &[u8]) -> bool {
        self.find_from(haystack, 0).is_some()
    }

    /// Whether the needle occurs at `start`. The caller has checked that it
    /// fits.
    fn matches_at(&self, haystack: &[u8], start: usize) -> bool {
        let window = &haystack[start..start + self.needle.len()];
        if self.fold {
            window.eq_ignore_ascii_case(&self.needle)
        } else {
            *window == *self.needle
        }
    }
}

impl Pair {
    /// Picks the two rarest positions with distinct bytes (a one-byte needle,
    /// or one of a single repeated byte, tests one position twice).
    fn choose(needle: &[u8], fold: bool) -> Pair {
        let rank = |i: usize| rank(needle[i], fold);
        let first = (0..needle.len()).min_by_key(|&i| rank(i)).unwrap_or(0);
        let second = (0..needle.len())
            .filter(|&i| needle[i] != needle[first])
            .min_by_key(|&i| rank(i))
            .unwrap_or(first);
        let probe = |i: usize| {
            let byte = needle.get(i).copied().unwrap_or(0);
            let mask = if fold && byte.is_ascii_alphabetic() {
                0x20
            } else {
                0
            };
            (byte | mask, mask)
        };
        let (b0, m0) = probe(first);
        let (b1, m1) = probe(second);
        Pair {
            at: [first, second],
            byte: [b0, b1],
            mask: [m0, m1],
        }
    }
}

/// How common a byte is in file names: lower is rarer. Letters follow
/// English frequency, with capitals rarer unless folded; `.`, digits, `_`
/// and `-` are common in names; anything else is rare.
fn rank(byte: u8, fold: bool) -> u8 {
    const LETTERS: &[u8; 26] = b"etaoinsrhldcumfpgwybvkxjqz";
    let lower = byte.to_ascii_lowercase();
    match LETTERS.iter().position(|&l| l == lower) {
        Some(i) if byte.is_ascii_lowercase() || fold => 200 - i as u8,
        Some(i) => 100 - i as u8,
        None => match byte {
            b'.' => 220,
            b'0'..=b'9' => 150,
            b'_' | b'-' => 140,
            _ => 10,
        },
    }
}

/// The candidate filter over one `u64` per probe: 8 starts per step, then a
/// scalar tail for the last starts that cannot load a whole word.
fn swar(finder: &Finder, hay: &[u8], from: usize) -> Option<usize> {
    const LOW7: u64 = 0x7f7f_7f7f_7f7f_7f7f;
    let pair = &finder.pair;
    let last = hay.len() - finder.needle.len();
    let splat = |b: u8| u64::from_ne_bytes([b; 8]);
    let (b0, b1) = (splat(pair.byte[0]), splat(pair.byte[1]));
    let (m0, m1) = (splat(pair.mask[0]), splat(pair.mask[1]));
    let word = |at: usize| u64::from_le_bytes(hay[at..at + 8].try_into().unwrap_or_default());
    let mut start = from;
    // Starts `start..start + 8` are all at most `last`, so each probe word
    // ends by `start + far + 8 <= hay.len()`.
    while start + 7 <= last {
        // A byte of `x` is zero exactly where both probes agree.
        let x = ((word(start + pair.at[0]) | m0) ^ b0) | ((word(start + pair.at[1]) | m1) ^ b1);
        // Exact per-byte zero test: no borrow crosses bytes.
        let mut zeros = !(((x & LOW7) + LOW7) | x | LOW7);
        while zeros != 0 {
            let candidate = start + zeros.trailing_zeros() as usize / 8;
            if finder.matches_at(hay, candidate) {
                return Some(candidate);
            }
            zeros &= zeros - 1;
        }
        start += 8;
    }
    (start..=last).find(|&s| {
        (0..2).all(|k| hay[s + pair.at[k]] | pair.mask[k] == pair.byte[k])
            && finder.matches_at(hay, s)
    })
}

#[cfg(target_arch = "x86_64")]
// D11: AVX2 intrinsics need `unsafe` for their unaligned loads and for
// calling a `target_feature` function. Every load is inside the haystack by
// the loop bound in `scan`, and `scan` runs only after runtime detection.
// Justified by the bench row in the toolchain ledger.
#[allow(unsafe_code)]
mod avx2 {
    use core::arch::x86_64::{
        __m256i, _mm256_and_si256, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8,
        _mm256_or_si256, _mm256_set1_epi8, _mm256_testz_si256,
    };

    use super::{Finder, swar};

    pub(super) fn available() -> bool {
        std::is_x86_feature_detected!("avx2")
    }

    pub(super) fn find(finder: &Finder, hay: &[u8], from: usize) -> Option<usize> {
        // SAFETY: `Finder::with_arm` refuses Avx2 unless the CPU has it.
        let (found, next) = unsafe {
            if finder.pair.mask == [0, 0] {
                scan::<false>(finder, hay, from)
            } else {
                scan::<true>(finder, hay, from)
            }
        };
        found.or_else(|| swar(finder, hay, next))
    }

    /// Runs the filter 64 starts at a time, then 32. Returns the first
    /// match, or where the SWAR arm must take over. `FOLD` is false when
    /// neither probe folds, which drops two ORs from every block.
    #[target_feature(enable = "avx2")]
    fn scan<const FOLD: bool>(finder: &Finder, hay: &[u8], from: usize) -> (Option<usize>, usize) {
        let pair = &finder.pair;
        let last = hay.len() - finder.needle.len();
        let splat = |b: u8| _mm256_set1_epi8(b as i8);
        let (b0, b1) = (splat(pair.byte[0]), splat(pair.byte[1]));
        let (m0, m1) = (splat(pair.mask[0]), splat(pair.mask[1]));
        let load = |at: usize| -> __m256i {
            // SAFETY: the loop bound keeps `at + 32 <= hay.len()`.
            unsafe { _mm256_loadu_si256(hay.as_ptr().add(at).cast()) }
        };
        // The candidates among the 32 starts from `at`, as a vector whose
        // bytes are all ones where both probes agree.
        let block = |at: usize| {
            let probe = |k: usize, m, b| {
                let bytes = load(at + pair.at[k]);
                let bytes = if FOLD {
                    _mm256_or_si256(bytes, m)
                } else {
                    bytes
                };
                _mm256_cmpeq_epi8(bytes, b)
            };
            _mm256_and_si256(probe(0, m0, b0), probe(1, m1, b1))
        };
        let verify = |at: usize, v: __m256i| {
            let mut hits = _mm256_movemask_epi8(v) as u32;
            while hits != 0 {
                let candidate = at + hits.trailing_zeros() as usize;
                if finder.matches_at(hay, candidate) {
                    return Some(candidate);
                }
                hits &= hits - 1;
            }
            None
        };
        let mut start = from;
        // Starts `start..start + 64` are all at most `last`, so every probe
        // byte lies before `start + far + 64 <= hay.len()`. Two blocks per
        // step, tested together, halve the branches on a candidate-free run.
        while start + 63 <= last {
            let (lo, hi) = (block(start), block(start + 32));
            let any = _mm256_or_si256(lo, hi);
            if _mm256_testz_si256(any, any) == 0
                && let Some(found) = verify(start, lo).or_else(|| verify(start + 32, hi))
            {
                return (Some(found), start);
            }
            start += 64;
        }
        // The same bound for one block.
        while start + 31 <= last {
            if let Some(found) = verify(start, block(start)) {
                return (Some(found), start);
            }
            start += 32;
        }
        (None, start)
    }
}

#[cfg(not(target_arch = "x86_64"))]
mod avx2 {
    use super::{Finder, swar};

    pub(super) fn available() -> bool {
        false
    }

    pub(super) fn find(finder: &Finder, hay: &[u8], from: usize) -> Option<usize> {
        swar(finder, hay, from)
    }
}
