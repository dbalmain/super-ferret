//! The scanner's arms against a plain reference, and the regex matcher.
//!
//! [`reference`] is the one place a second implementation is right: it is
//! the oracle the fast arms are checked against, and it is written to be
//! obviously correct rather than fast.
#![allow(clippy::unwrap_used)]

use crate::{Arm, Finder, Matcher, Regex};

/// Every start at which `needle` occurs in `hay`, folding ASCII case when
/// `fold` is set.
fn reference(hay: &[u8], needle: &[u8], fold: bool) -> Vec<usize> {
    if needle.is_empty() {
        return (0..=hay.len()).collect();
    }
    hay.windows(needle.len())
        .enumerate()
        .filter(|(_, w)| {
            w.iter()
                .zip(needle)
                .all(|(&h, &n)| h == n || fold && h.eq_ignore_ascii_case(&n))
        })
        .map(|(i, _)| i)
        .collect()
}

/// Every start the finder reports, by resuming one past each match.
fn all(finder: &Finder, hay: &[u8]) -> Vec<usize> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = finder.find_from(hay, from) {
        found.push(at);
        from = at + 1;
    }
    found
}

/// SWAR always; AVX2 wherever the CPU has it, so the suite passes on any
/// host and checks the vector arm on every host that can run it.
fn arms() -> Vec<Arm> {
    let arms: Vec<Arm> = [Arm::Swar, Arm::Avx2]
        .into_iter()
        .filter(|a| a.is_available())
        .collect();
    assert_eq!(arms[0], Arm::Swar);
    arms
}

fn check(hay: &[u8], needle: &[u8], fold: bool) {
    let expect = reference(hay, needle, fold);
    for arm in arms() {
        let finder = Finder::with_arm(needle, fold, arm);
        assert_eq!(
            all(&finder, hay),
            expect,
            "{arm:?} fold={fold} needle={:?} hay={:?}",
            String::from_utf8_lossy(needle),
            String::from_utf8_lossy(hay)
        );
    }
}

/// xorshift64*: deterministic, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// A heap-like byte: mostly letters in both cases, some NULs (name
/// terminators), punctuation that sits 0x20 from a letter or from another
/// punctuation byte, and bytes above 0x7f whose low bits look like letters.
fn heap_byte(rng: &mut Rng) -> u8 {
    const ALPHABET: &[u8] = b"aAbBcCzZ.\0\0@`[{_-09\xc1\xe1\xc3\xa9";
    ALPHABET[rng.below(ALPHABET.len())]
}

#[test]
fn every_arm_agrees_with_the_reference_on_random_heaps() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..4000 {
        let len = rng.below(200);
        let hay: Vec<u8> = (0..len).map(|_| heap_byte(&mut rng)).collect();
        let needle: Vec<u8> = if len > 0 && rng.below(3) > 0 {
            // A slice of the heap, case-flipped at random, so matches exist.
            let start = rng.below(len);
            let n = 1 + rng.below((len - start).min(9));
            hay[start..start + n]
                .iter()
                .map(|&b| if rng.below(2) == 0 { b ^ 0x20 } else { b })
                .filter(|&b| b != 0)
                .collect()
        } else {
            (0..1 + rng.below(4)).map(|_| heap_byte(&mut rng)).collect()
        };
        check(&hay, &needle, false);
        check(&hay, &needle, true);
    }
}

#[test]
fn needles_at_the_heap_start_and_end_at_every_length() {
    // Lengths straddle the SWAR word (8) and the AVX2 block (32) so the
    // match falls in the vector loop, at its edge, and in the tail.
    for len in 1..100 {
        let mut hay = vec![b'x'; len];
        let needle = b"Qz";
        if len >= 2 {
            hay[..2].copy_from_slice(b"qZ");
            hay[len - 2..].copy_from_slice(b"QZ");
        }
        check(&hay, needle, true);
        check(&hay, needle, false);
        check(&hay, b"x", false);
    }
}

#[test]
fn a_match_never_spans_a_name_terminator() {
    let heap = b"alpha\0beta\0gamma\0";
    check(heap, b"ab", true);
    check(heap, b"aga", false);
    for arm in arms() {
        assert_eq!(Finder::with_arm(b"ab", true, arm).find_from(heap, 0), None);
        assert_eq!(
            Finder::with_arm(b"TAG", true, arm).find_from(heap, 0),
            None,
            "{arm:?}: bet[a\\0g]amma"
        );
        assert_eq!(
            Finder::with_arm(b"BETA", true, arm).find_from(heap, 0),
            Some(6)
        );
    }
}

#[test]
fn one_byte_needles() {
    let heap = b"a\0B\0c\0\xc1\0A";
    check(heap, b"a", true);
    check(heap, b"a", false);
    check(heap, b"b", true);
    check(heap, b"\xc1", true);
    check(heap, b"\0", false);
}

#[test]
fn only_ascii_letters_fold() {
    // 0xC1 and 0xE1 differ by 0x20 as `A` and `a` do; `@` and `` ` `` too;
    // none of them is a letter, so none folds.
    let heap = b"\xc1\0@\0[\0\xc3\x89";
    for arm in arms() {
        let finds = |needle: &[u8]| Finder::with_arm(needle, true, arm).find_from(heap, 0);
        assert_eq!(finds(b"\xe1"), None, "{arm:?}");
        assert_eq!(finds(b"`"), None, "{arm:?}");
        assert_eq!(finds(b"{"), None, "{arm:?}");
        assert_eq!(finds(b"\xc3\xa9"), None, "{arm:?}: e-acute is not E-acute");
        assert_eq!(finds(b"\xc1"), Some(0), "{arm:?}");
    }
    check(heap, b"\xe1", true);
    check(heap, b"`", true);
}

#[test]
fn folding_matches_either_case_and_exact_does_not() {
    let heap = b"README.md\0readme.MD\0ReadMe\0";
    for arm in arms() {
        let folded = Finder::with_arm(b"ReadMe", true, arm);
        assert_eq!(all(&folded, heap), [0, 10, 20]);
        let exact = Finder::with_arm(b"ReadMe", false, arm);
        assert_eq!(all(&exact, heap), [20]);
    }
}

#[test]
fn the_regex_matcher_matches_bytes_and_folds_on_request() {
    let re = Regex::new(r"^test_.*\.rs$", false).unwrap();
    assert!(re.is_match(b"test_scan.rs"));
    assert!(!re.is_match(b"TEST_scan.rs"));
    assert!(!re.is_match(b"a_test_scan.rs"));
    let folded = Regex::new(r"^test_.*\.rs$", true).unwrap();
    assert!(folded.is_match(b"TEST_scan.RS"));
    // A name that is not UTF-8 is matched as bytes, not refused.
    assert!(Regex::new("ab", false).unwrap().is_match(b"\xffab\xfe"));
    assert!(Regex::new("(", false).is_err());
}
