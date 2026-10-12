//! T1 (DECISIONS.md D65 update — token policy, answered C): base64-like runs
//! leave the content term index. [`tokenize_terms`] is what the term index
//! holds; [`base64_runs`] is the detector it and the query planner share, so
//! the two cannot disagree about what a blob is. Trigrams never consult it,
//! so `grep:` and regex still see every byte.
//!
//! The detector was measured on branch `wt/tokens` (docs/TOKENS.md § T1
//! detector): on `$HOME` it halves codec 1's term index for 2.6% more follow
//! CPU. Its false suppressions are identifiers built from short capitalised
//! words and digits (about 4% of reviewed code runs) and SVG path data.
//! Changing anything here changes what an index holds: bump
//! [`TERM_VERSION`](crate::TERM_VERSION).

use std::ops::Range;

use crate::{Scratch, Token, tokenize};

/// The shortest run, excluding padding, that [`base64_runs`] reports.
pub const BASE64_MIN: usize = 20;

/// [`tokenize`], less every token whose run overlaps a [`base64_runs`] span:
/// the tokens the content term index holds. Order and spellings are
/// otherwise unchanged. A whole run that reaches into a blob is dropped
/// while its parts outside the blob are kept (`key_<blob>` keeps `key`).
pub fn tokenize_terms(bytes: &[u8], scratch: &mut Scratch, mut emit: impl FnMut(Token<'_>)) {
    let mut spans = std::mem::take(&mut scratch.spans);
    spans.clear();
    base64_runs(bytes, |run| spans.push(run));
    if spans.is_empty() {
        scratch.spans = spans;
        return tokenize(bytes, scratch, emit);
    }
    let mut at = 0;
    // Token starts never decrease (a whole run, then its parts left to right,
    // then the next run), and the spans are sorted and disjoint, so a span
    // ending at or before one token's start overlaps no later token.
    tokenize(bytes, scratch, |token| {
        while spans
            .get(at)
            .is_some_and(|span| span.end <= token.run.start)
        {
            at += 1;
        }
        if spans.get(at).is_none_or(|span| span.start >= token.run.end) {
            emit(token);
        }
    });
    scratch.spans = spans;
}

/// Whether [`tokenize_terms`] drops any of `bytes`' tokens: whether `bytes`
/// holds a [`base64_runs`] span. Every span holds a letter or digit, so it
/// always overlaps a token. The query planner asks this of a `text:`
/// argument to decide whether the term index can answer it.
pub fn has_base64_run(bytes: &[u8]) -> bool {
    let mut found = false;
    base64_runs(bytes, |_| found = true);
    found
}

fn base64_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'-' | b'_')
}

/// Visits, in order, each maximal run of the base64 and base64url alphabets,
/// with up to two `=` of padding, that looks random rather than written.
/// The spans are disjoint.
///
/// First, written segments are trimmed off both ends of the run. Segments are
/// split at `-`, `_` and `/`; a segment is written if it is empty, short
/// lowercase (`sha512-…`, `forms/d/…`), or fails the randomness test below
/// regardless of length (`LazyRender-mkD3tyWn`, `_taskRow_sU7PM_taskRow`).
/// What remains is reported if it has at least [`BASE64_MIN`] characters
/// before the padding, is not all hex digits, and looks random:
///
/// - at least one digit, and upper and lower case each a tenth of it;
/// - case changes between at least a quarter of adjacent letter pairs (random
///   text: half);
/// - lowercase letters in runs averaging at most 2.6 (random text: about 1.7;
///   camelCase words: 3 or more);
/// - of the lowercase runs two or more long, fewer than 60% hold a vowel
///   (random text: about 45%; `MapViewOfFile3FromApp`: 80%). Not applied to
///   fewer than two such runs.
pub fn base64_runs(bytes: &[u8], mut emit: impl FnMut(Range<usize>)) {
    let mut at = 0;
    while at < bytes.len() {
        if !base64_byte(bytes[at]) {
            at += 1;
            continue;
        }
        let start = at;
        while at < bytes.len() && base64_byte(bytes[at]) {
            at += 1;
        }
        let (start, body_end) = trim_written(bytes, start..at);
        let mut end = body_end;
        if body_end == at {
            while end < bytes.len() && end - at < 2 && bytes[end] == b'=' {
                end += 1;
            }
        }
        let body = &bytes[start..body_end];
        if body.len() >= BASE64_MIN && !hex(body) && random_looking(body) {
            emit(start..end);
        }
        at = at.max(end);
    }
}

fn joiner(b: u8) -> bool {
    matches!(b, b'-' | b'_' | b'/')
}

/// The run without its written edge segments.
fn trim_written(bytes: &[u8], run: Range<usize>) -> (usize, usize) {
    let (mut start, mut end) = (run.start, run.end);
    while let Some(len) = bytes[start..end].iter().position(|&b| joiner(b)) {
        if !written(&bytes[start..start + len]) {
            break;
        }
        start += len + 1;
    }
    while let Some(len) = bytes[start..end].iter().rev().position(|&b| joiner(b)) {
        if !written(&bytes[end - len..end]) {
            break;
        }
        end -= len + 1;
    }
    (start, end.max(start))
}

fn written(segment: &[u8]) -> bool {
    segment.len() <= 16
        && segment
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        || !random_looking(segment)
}

fn hex(run: &[u8]) -> bool {
    let run = run.strip_prefix(b"0x").unwrap_or(run);
    run.iter().all(u8::is_ascii_hexdigit)
}

fn random_looking(run: &[u8]) -> bool {
    let (mut upper, mut lower, mut digit) = (0, 0, 0);
    let (mut pairs, mut flips) = (0, 0);
    let (mut lower_runs, mut long_runs, mut voweled) = (0, 0, 0);
    let (mut run_len, mut run_vowel) = (0, false);
    let mut previous = 0u8;
    for &b in run.iter().chain([&0]) {
        if b.is_ascii_lowercase() {
            lower += 1;
            lower_runs += usize::from(run_len == 0);
            run_len += 1;
            run_vowel |= matches!(b, b'a' | b'e' | b'i' | b'o' | b'u' | b'y');
        } else {
            if run_len >= 2 {
                long_runs += 1;
                voweled += usize::from(run_vowel);
            }
            (run_len, run_vowel) = (0, false);
            upper += usize::from(b.is_ascii_uppercase());
            digit += usize::from(b.is_ascii_digit());
        }
        if b.is_ascii_alphabetic() && previous.is_ascii_alphabetic() {
            pairs += 1;
            flips += usize::from(b.is_ascii_uppercase() != previous.is_ascii_uppercase());
        }
        previous = b;
    }
    let tenth = run.len().div_ceil(10);
    digit > 0
        && upper >= tenth
        && lower >= tenth
        && flips * 4 >= pairs
        && lower * 10 <= lower_runs * 26
        && (long_runs < 2 || voweled * 10 < long_runs * 6)
}

#[cfg(test)]
mod tests;
