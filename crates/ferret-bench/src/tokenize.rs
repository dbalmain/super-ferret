//! `ferret-bench tokenize <corpus-dir>`: the tokenizer's throughput and the
//! shape of its output over every file under `<corpus-dir>/docs`, as
//! `corpus-sample` writes them.
//!
//! Throughput is the best of five passes over the corpus held in memory, the
//! files loaded once beforehand: for all files, for files with no byte ≥ 0x80
//! (the byte-table path throughout), and for the rest. Old-versus-new is not
//! reported: the tokenizer this replaced lives only in `ferret-text`'s test
//! module as the oracle, and is not made public just to be timed.
//!
//! The counts feed S2's size estimate (§ Bytes per content byte): bytes per
//! whole-run occurrence (input a), distinct (term, document) pairs per
//! whole-run occurrence before splitting (b), the postings multiplier from
//! splitting (c), and distinct terms per content byte (f). Terms are counted
//! after [`cap`], as the index stores them.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

use ferret_text::{Kind, MAX_TOKEN_BYTES, Scratch, cap, tokenize};

/// The passes each throughput figure is the best of.
const PASSES: usize = 5;

/// Every file under `<dir>/docs`, in name order.
fn load(dir: &Path) -> crate::Result<Vec<Vec<u8>>> {
    let docs = dir.join("docs");
    let mut entries = fs::read_dir(&docs)
        .map_err(|error| format!("reading {}: {error}", docs.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut files = Vec::new();
    for entry in entries {
        if entry.file_type()?.is_file() {
            files.push(fs::read(entry.path())?);
        }
    }
    Ok(files)
}

/// One distinct term.
#[derive(Default)]
struct Term {
    occurrences: u64,
    /// An occurrence was longer than [`MAX_TOKEN_BYTES`] before capping.
    over_cap: bool,
    /// One past the index of the last file it occurred in, and of the last
    /// file it occurred in as a whole run; 0 for none. Counts each (term,
    /// document) pair once without a set per file.
    last_file: usize,
    last_whole_file: usize,
}

/// The corpus's token and term counts.
#[derive(Debug, Default, PartialEq)]
struct Counts {
    files: u64,
    bytes: u64,
    /// Whole-run occurrences.
    whole: u64,
    /// Identifier-part occurrences.
    part: u64,
    /// Occurrences longer than the cap before capping.
    over_cap: u64,
    /// Distinct (term, file) pairs: whole runs only, and with parts.
    whole_pairs: u64,
    pairs: u64,
    /// Distinct terms after [`cap`].
    distinct: u64,
    /// Distinct terms that occur once.
    hapax: u64,
    /// Distinct terms with an occurrence over the cap.
    distinct_over_cap: u64,
    /// Distinct hash-like terms, and their occurrences.
    hash_like: u64,
    hash_like_occurrences: u64,
}

fn counts(files: &[Vec<u8>]) -> Counts {
    let mut terms: HashMap<Vec<u8>, Term> = HashMap::new();
    let mut counts = Counts {
        files: files.len() as u64,
        bytes: files.iter().map(|file| file.len() as u64).sum(),
        ..Counts::default()
    };
    let mut scratch = Scratch::default();
    for (file, bytes) in (1..).zip(files) {
        tokenize(bytes, &mut scratch, |token| {
            let over = token.bytes.len() > MAX_TOKEN_BYTES;
            counts.over_cap += u64::from(over);
            // Copies every token: this pass is not the timed one.
            let term = terms.entry(cap(token.bytes).to_vec()).or_default();
            term.occurrences += 1;
            term.over_cap |= over;
            counts.pairs += u64::from(term.last_file != file);
            term.last_file = file;
            match token.kind {
                Kind::Whole => {
                    counts.whole += 1;
                    counts.whole_pairs += u64::from(term.last_whole_file != file);
                    term.last_whole_file = file;
                }
                Kind::Part => counts.part += 1,
            }
        });
    }
    for (bytes, term) in &terms {
        counts.distinct += 1;
        counts.hapax += u64::from(term.occurrences == 1);
        counts.distinct_over_cap += u64::from(term.over_cap);
        if is_hash_like(bytes) {
            counts.hash_like += 1;
            counts.hash_like_occurrences += term.occurrences;
        }
    }
    counts
}

/// Best-of-[`PASSES`] throughput of `tokenize` over `files`, in MB/s; `None`
/// for no bytes. Each pass counts its tokens into [`black_box`], so it cannot
/// be optimised away.
fn throughput<'a>(files: impl Iterator<Item = &'a Vec<u8>> + Clone) -> Option<f64> {
    let bytes: usize = files.clone().map(Vec::len).sum();
    let mut scratch = Scratch::default();
    let mut best = Duration::MAX;
    for _ in 0..PASSES {
        let start = Instant::now();
        let mut tokens = 0u64;
        for file in files.clone() {
            tokenize(file, &mut scratch, |_| tokens += 1);
        }
        black_box(tokens);
        best = best.min(start.elapsed());
    }
    (bytes > 0).then(|| bytes as f64 / best.as_secs_f64() / 1e6)
}

/// The report: throughput, then the counts and their ratios. A ratio with
/// nothing to divide by prints as `-`.
fn report(files: &[Vec<u8>], counts: &Counts) -> String {
    let ascii = |file: &&Vec<u8>| file.is_ascii();
    let mut out = String::new();
    let mut line = |key: &str, value: String| {
        let _ = writeln!(out, "{key}: {value}");
    };
    // Rates and shares can be tiny, so they print in scientific notation;
    // inputs a, b and c are near 1–10 and print plain.
    let ratio = |numerator: u64, denominator: u64| match denominator {
        0 => "-".to_string(),
        _ => format!("{:.4e}", numerator as f64 / denominator as f64),
    };
    let plain = |numerator: u64, denominator: u64| match denominator {
        0 => "-".to_string(),
        _ => format!("{:.4}", numerator as f64 / denominator as f64),
    };
    let speed = |mb_s: Option<f64>| mb_s.map_or_else(|| "-".to_string(), |v| format!("{v:.1}"));
    let ascii_files = files.iter().filter(ascii).count();

    line("files", counts.files.to_string());
    line("bytes", counts.bytes.to_string());
    line("ascii_files", ascii_files.to_string());
    line("non_ascii_files", (files.len() - ascii_files).to_string());
    line("mb_s_all", speed(throughput(files.iter())));
    line("mb_s_ascii", speed(throughput(files.iter().filter(ascii))));
    line(
        "mb_s_non_ascii",
        speed(throughput(files.iter().filter(|file| !file.is_ascii()))),
    );
    line("whole_occurrences", counts.whole.to_string());
    line("part_occurrences", counts.part.to_string());
    line("whole_per_byte", ratio(counts.whole, counts.bytes));
    line("part_per_byte", ratio(counts.part, counts.bytes));
    line("bytes_per_whole (a)", plain(counts.bytes, counts.whole));
    line(
        "whole_pairs_per_whole (b)",
        plain(counts.whole_pairs, counts.whole),
    );
    line(
        "postings_multiplier (c)",
        plain(counts.pairs, counts.whole_pairs),
    );
    line("distinct_terms", counts.distinct.to_string());
    line(
        "distinct_per_byte (f)",
        ratio(counts.distinct, counts.bytes),
    );
    line("hapax_share", ratio(counts.hapax, counts.distinct));
    line(
        "over_64_share_of_distinct",
        ratio(counts.distinct_over_cap, counts.distinct),
    );
    line(
        "over_64_share_of_occurrences",
        ratio(counts.over_cap, counts.whole + counts.part),
    );
    line(
        "hash_like_share_of_distinct",
        ratio(counts.hash_like, counts.distinct),
    );
    line(
        "hash_like_share_of_occurrences",
        ratio(counts.hash_like_occurrences, counts.whole + counts.part),
    );
    out
}

/// `ferret-bench tokenize <corpus-dir>`.
pub(crate) fn run(dir: &Path) -> crate::Result<()> {
    let files = load(dir)?;
    print!("{}", report(&files, &counts(&files)));
    Ok(())
}

/// Whether a term looks like machine-made identity text rather than a word
/// or an identifier. Terms are lowercase, and never hold `-`, `+` or `/`: the
/// tokenizer splits a UUID at its hyphens and base64 at its symbols. Both
/// rules need a digit and a letter, so a plain number or word never counts.
///
/// - **Hex:** at least 12 hex digits — a digest, or a UUID's last group.
///   `deadbeef` is too short.
/// - **Base64:** at least 20 digits and letters with no two vowels in a row,
///   which words and identifiers nearly always have (`request`'s `ue`).
///
/// One-sided by design: a blob that slips past (a vowel pair, a short
/// fragment) only lowers the share.
pub(crate) fn is_hash_like(term: &[u8]) -> bool {
    let mixed = term.iter().any(u8::is_ascii_digit) && term.iter().any(u8::is_ascii_lowercase);
    let hex = term.len() >= 12 && term.iter().all(|&b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let base64 = term.len() >= 20
        && term
            .iter()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
        && !term.windows(2).any(|pair| vowel(pair[0]) && vowel(pair[1]));
    mixed && (hex || base64)
}

fn vowel(b: u8) -> bool {
    matches!(b, b'a' | b'e' | b'i' | b'o' | b'u')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::Fixture;

    #[test]
    fn counts_match_hand_computed_values() {
        let fixture = Fixture::new("tokenize");
        let corpus = fixture.out("corpus");
        fs::create_dir_all(corpus.join("docs")).unwrap();
        let files: [&[u8]; 4] = [
            // Whole runs only: fn and main, twice each.
            b"fn main() { fn main }\n",
            // Parts: parse, http, request, 2; and a, 1, b, 2, … f, 6 from
            // a hash-like 12-digit hex run.
            b"parseHTTPRequest2 a1b2c3d4e5f6\n",
            // Not ASCII: café_naïve, its parts café and naïve, and fn again.
            "café_naïve fn\n".as_bytes(),
            // One 65-byte run: capped to 64 `a`s, and not hash-like.
            &[b'a'; 65],
        ];
        for (name, bytes) in ["1", "2", "3", "4"].iter().zip(files) {
            fs::write(corpus.join("docs").join(name), bytes).unwrap();
        }
        let loaded = load(&corpus).unwrap();
        assert_eq!(loaded, files.map(<[u8]>::to_vec));
        let counted = counts(&loaded);

        // Every line but the timings, which vary run to run.
        let report: String = report(&loaded, &counted)
            .lines()
            .filter(|line| !line.starts_with("mb_s_"))
            .map(|line| format!("{line}\n"))
            .collect();
        assert_eq!(
            report,
            "files: 4\n\
             bytes: 134\n\
             ascii_files: 3\n\
             non_ascii_files: 1\n\
             whole_occurrences: 9\n\
             part_occurrences: 18\n\
             whole_per_byte: 6.7164e-2\n\
             part_per_byte: 1.3433e-1\n\
             bytes_per_whole (a): 14.8889\n\
             whole_pairs_per_whole (b): 0.7778\n\
             postings_multiplier (c): 3.4286\n\
             distinct_terms: 23\n\
             distinct_per_byte (f): 1.7164e-1\n\
             hapax_share: 8.6957e-1\n\
             over_64_share_of_distinct: 4.3478e-2\n\
             over_64_share_of_occurrences: 3.7037e-2\n\
             hash_like_share_of_distinct: 4.3478e-2\n\
             hash_like_share_of_occurrences: 3.7037e-2\n"
        );

        assert_eq!(
            counted,
            Counts {
                files: 4,
                bytes: 22 + 31 + 16 + 65,
                whole: 4 + 2 + 2 + 1,
                part: 4 + 12 + 2,
                over_cap: 1,
                // Per file, whole runs: 2, 2, 2, 1; with parts, file 2 adds
                // 15 (its two `2`s are one pair) and file 3 adds 2.
                whole_pairs: 7,
                pairs: 2 + 17 + 4 + 1,
                // fn, main, parsehttprequest2, parse, http, request, 2,
                // a1b2c3d4e5f6, its eleven other parts, café_naïve, café,
                // naïve, and the capped run.
                distinct: 23,
                // All but fn (3), main (2) and 2 (2).
                hapax: 20,
                distinct_over_cap: 1,
                hash_like: 1,
                hash_like_occurrences: 1,
            }
        );
    }

    /// The hash-like terms `text` tokenizes to.
    fn hash_like_terms(text: &str) -> Vec<String> {
        let mut found = Vec::new();
        tokenize(text.as_bytes(), &mut Scratch::default(), |token| {
            if is_hash_like(token.bytes) {
                found.push(String::from_utf8(token.bytes.to_vec()).unwrap());
            }
        });
        found
    }

    #[test]
    fn hash_like_named_examples() {
        let sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(hash_like_terms(sha256), [sha256]);
        // The tokenizer splits a UUID at its hyphens; the 12-digit last group
        // is the part long enough to count.
        assert_eq!(
            hash_like_terms("f47ac10b-58cc-4372-a567-0e02b2c3d479"),
            ["0e02b2c3d479"]
        );
        // Base64 of "1234567890abcdef", lower-cased as a term.
        assert_eq!(
            hash_like_terms("MTIzNDU2Nzg5MGFiY2RlZg=="),
            ["mtizndu2nzg5mgfiy2rlzg"]
        );

        let none: [&str; 0] = [];
        assert_eq!(hash_like_terms("parseHTTPRequest2"), none);
        assert_eq!(hash_like_terms("deadbeef"), none, "8 hex digits");
        assert_eq!(hash_like_terms("xmlHttpRequest"), none);
        assert_eq!(hash_like_terms("1696636800000"), none, "a number");
        assert_eq!(hash_like_terms("abcdefabcdefabcdef"), none, "no digit");
        assert_eq!(hash_like_terms("queryparamidentifier1"), none, "`ue`");
    }
}
