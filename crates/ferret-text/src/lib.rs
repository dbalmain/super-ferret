//! The tokenizer: alphanumeric-and-underscore runs, lowercased, plus the
//! camelCase / TitleCase / snake / digit-boundary parts (DECISIONS.md D9).
//!
//! Versioned, because segments record the version that wrote them. The one
//! place tokens are defined: indexing and query parsing both call it.
//! [`tokenize`] is the streaming form the content index uses; [`tokens`] and
//! [`has_token`] wrap it for D54's name terms. The implementation it replaced
//! is the test oracle in `tests.rs`, and the token sequence must stay
//! byte-identical to it while [`TOKENIZER_VERSION`] is 1.
//!
//! Knows nothing about files or ids.

use std::collections::BTreeMap;
use std::ops::Range;

/// Change when the token contract changes; indexes must rebuild on a change.
pub const TOKENIZER_VERSION: u32 = 1;

/// The longest term the content index stores; see [`cap`].
pub const MAX_TOKEN_BYTES: usize = 64;

/// One token, borrowed from the [`Scratch`] until `emit` returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token<'a> {
    /// Lowercased, non-empty, valid UTF-8.
    pub bytes: &'a [u8],
    /// Where the token's text sits in the input: the whole run for a
    /// [`Kind::Whole`], the part's own span for a [`Kind::Part`]. Lowercasing
    /// `input[run]` as a string yields `bytes`.
    pub run: Range<usize>,
    pub kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A whole run, emitted before its parts.
    Whole,
    /// An identifier part that differs from its whole run.
    Part,
}

/// Buffers [`tokenize`] reuses across calls, so tokenizing allocates only
/// while they grow.
#[derive(Debug, Default)]
pub struct Scratch {
    whole: String,
    part: String,
    parts: Vec<Range<usize>>,
    casing: BTreeMap<char, Casing>,
}

/// D9: whole alphanumeric/underscore runs, then identifier parts, lowercased.
/// Invalid UTF-8 bytes separate runs. Duplicates are allowed; index builders
/// deduplicate within a name before writing its postings.
///
/// ASCII runs take a byte-table path; a run containing any other alphanumeric
/// character takes the per-`char` path. Tokens are not capped: the content
/// indexer applies [`cap`] itself, because D54's name terms must match whole
/// runs of any length.
pub fn tokenize(bytes: &[u8], scratch: &mut Scratch, mut emit: impl FnMut(Token<'_>)) {
    let mut base = 0;
    for chunk in bytes.utf8_chunks() {
        let text = chunk.valid();
        tokenize_valid(text, base, scratch, &mut emit);
        base += text.len() + chunk.invalid().len();
    }
}

/// [`tokenize`]'s token bytes alone, with a scratch of its own.
pub fn tokens(bytes: &[u8], mut emit: impl FnMut(&[u8])) {
    tokenize(bytes, &mut Scratch::default(), |token| emit(token.bytes));
}

/// Normalises one explicit token. Separators are not accepted as part of a
/// token query; a whole snake-case identifier remains a valid token.
pub fn normalize_token(bytes: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(bytes).ok()?;
    (!text.is_empty() && text.chars().all(|c| c.is_alphanumeric() || c == '_'))
        .then(|| text.to_lowercase().into_bytes())
}

pub fn has_token(bytes: &[u8], token: &[u8]) -> bool {
    let mut found = false;
    tokens(bytes, |value| found |= value == token);
    found
}

/// The longest prefix of `token` of at most [`MAX_TOKEN_BYTES`] that ends on a
/// UTF-8 character boundary.
pub fn cap(token: &[u8]) -> &[u8] {
    let mut end = token.len().min(MAX_TOKEN_BYTES);
    while end > 0 && token.get(end).is_some_and(|&b| b & 0xc0 == 0x80) {
        end -= 1;
    }
    &token[..end]
}

// ── Character shapes ──

// What the part rule asks of a character, as bit flags so that one `splits`
// serves both paths. A character can carry several: `ⅰ` (U+2170) is numeric
// and lowercase.
const LOWER: u8 = 1;
const UPPER: u8 = 2;
const NUMERIC: u8 = 4;
const UNDERSCORE: u8 = 8;
/// An ASCII byte that continues a run.
const WORD: u8 = 16;
/// A byte ≥ 0x80, whose character decides whether a run continues.
const HIGH: u8 = 32;

const ASCII: [u8; 256] = {
    let mut table = [0; 256];
    let mut b = 0;
    while b < 256 {
        table[b] = match b as u8 {
            b'a'..=b'z' => LOWER | WORD,
            b'A'..=b'Z' => UPPER | WORD,
            b'0'..=b'9' => NUMERIC | WORD,
            b'_' => UNDERSCORE | WORD,
            0x80..=0xff => HIGH,
            _ => 0,
        };
        b += 1;
    }
    table
};

fn shape(c: char) -> u8 {
    u8::from(c.is_lowercase())
        | u8::from(c.is_uppercase()) << 1
        | u8::from(c.is_numeric()) << 2
        | u8::from(c == '_') << 3
}

/// Whether a part ends before `current`. `previous` and `next` are 0 at the
/// run's edges. That is safe at the start too: a split before the first
/// character closes only an empty part.
fn splits(previous: u8, current: u8, next: u8) -> bool {
    (previous | current) & UNDERSCORE != 0
        || (previous ^ current) & NUMERIC != 0
        || previous & LOWER != 0 && current & UPPER != 0
        || previous & current & UPPER != 0 && next & LOWER != 0
}

/// Records the part `start..end` of a run `len` bytes long, if it is to be
/// emitted. A part spanning the whole run is the only one whose lowercase
/// equals the whole's: any shorter part drops a character, and every character
/// lowercases to at least one byte.
fn push_part(parts: &mut Vec<Range<usize>>, start: usize, end: usize, len: usize) {
    if start < end && end - start < len {
        parts.push(start..end);
    }
}

// ── Runs ──

fn tokenize_valid(
    text: &str,
    base: usize,
    scratch: &mut Scratch,
    emit: &mut impl FnMut(Token<'_>),
) {
    let bytes = text.as_bytes();
    let mut at = 0;
    while let Some(&b) = bytes.get(at) {
        let flags = ASCII[usize::from(b)];
        if flags & HIGH != 0 {
            let Some(c) = text[at..].chars().next() else {
                break;
            };
            if !c.is_alphanumeric() {
                at += c.len_utf8();
                continue;
            }
        } else if flags & WORD == 0 {
            at += 1;
            continue;
        }
        at = match ascii_run(text, at, scratch) {
            Some(end) => {
                emit_ascii(scratch, base + at, end - at, emit);
                end
            }
            None => {
                let end = text[at..]
                    .char_indices()
                    .find(|&(_, c)| !c.is_alphanumeric() && c != '_')
                    .map_or(text.len(), |(len, _)| at + len);
                char_run(&text[at..end], base + at, scratch, emit);
                end
            }
        };
    }
}

/// Lowercases the ASCII run starting at `start` into `scratch.whole` and
/// records its parts, in one pass. Returns the run's end, or `None` if the
/// run reaches a non-ASCII alphanumeric and needs the `char` path.
fn ascii_run(text: &str, start: usize, scratch: &mut Scratch) -> Option<usize> {
    let bytes = text.as_bytes();
    scratch.whole.clear();
    scratch.parts.clear();
    let mut part_start = 0;
    let mut previous = 0;
    let mut at = start;
    while let Some(&b) = bytes.get(at) {
        let current = ASCII[usize::from(b)];
        if current & WORD == 0 {
            if current & HIGH != 0 && text[at..].chars().next().is_some_and(char::is_alphanumeric) {
                return None;
            }
            break;
        }
        // A HIGH next byte either starts an alphanumeric, and the run is redone
        // on the char path, or ends the run, where the char path's next is
        // none. Neither is lowercase, so 0 stands for both.
        let next = bytes.get(at + 1).map_or(0, |&b| ASCII[usize::from(b)]);
        let offset = at - start;
        if splits(previous, current, next) {
            push_part(&mut scratch.parts, part_start, offset, usize::MAX);
            part_start = offset;
        }
        if current & UNDERSCORE != 0 {
            part_start = offset + 1;
        }
        scratch.whole.push(char::from(b.to_ascii_lowercase()));
        previous = current;
        at += 1;
    }
    push_part(&mut scratch.parts, part_start, at - start, at - start);
    Some(at)
}

fn emit_ascii(scratch: &Scratch, at: usize, len: usize, emit: &mut impl FnMut(Token<'_>)) {
    let whole = scratch.whole.as_bytes();
    emit(Token {
        bytes: whole,
        run: at..at + len,
        kind: Kind::Whole,
    });
    for part in &scratch.parts {
        emit(Token {
            bytes: &whole[part.clone()],
            run: at + part.start..at + part.end,
            kind: Kind::Part,
        });
    }
}

/// A run containing a non-ASCII alphanumeric. Each part is lowercased on its
/// own rather than sliced from the whole, because a final sigma depends on the
/// text around it: `ΑΣΒc` lowercases to `ασβc`, but its part `ΑΣ` to `ας`.
fn char_run(run: &str, at: usize, scratch: &mut Scratch, emit: &mut impl FnMut(Token<'_>)) {
    let Scratch {
        whole,
        part,
        parts,
        casing,
    } = scratch;
    whole.clear();
    parts.clear();
    lowercase_into(run, whole, casing);
    let mut part_start = 0;
    let mut previous = 0;
    let mut chars = run.char_indices().peekable();
    while let Some((offset, c)) = chars.next() {
        let current = shape(c);
        let next = chars.peek().map_or(0, |&(_, c)| shape(c));
        if splits(previous, current, next) {
            push_part(parts, part_start, offset, usize::MAX);
            part_start = offset;
        }
        if c == '_' {
            part_start = offset + 1;
        }
        previous = current;
    }
    push_part(parts, part_start, run.len(), run.len());
    emit(Token {
        bytes: whole.as_bytes(),
        run: at..at + run.len(),
        kind: Kind::Whole,
    });
    for range in parts.iter() {
        part.clear();
        lowercase_into(&run[range.clone()], part, casing);
        emit(Token {
            bytes: part.as_bytes(),
            run: at + range.start..at + range.end,
            kind: Kind::Part,
        });
    }
}

// ── Lowercasing ──

/// `str::to_lowercase`, appended to `out`. That is `char::to_lowercase` per
/// character except for `Σ`, which becomes `ς` at the end of a word (Unicode's
/// Final_Sigma condition) and `σ` elsewhere.
fn lowercase_into(text: &str, out: &mut String, casing: &mut BTreeMap<char, Casing>) {
    for (at, c) in text.char_indices() {
        if c == 'Σ' {
            let before = cased_after_ignorable(text[..at].chars().rev(), casing);
            let after = cased_after_ignorable(text[at + 'Σ'.len_utf8()..].chars(), casing);
            out.push(if before && !after { 'ς' } else { 'σ' });
        } else {
            out.extend(c.to_lowercase());
        }
    }
}

/// The two Unicode properties Final_Sigma reads. A case-ignorable character is
/// skipped whether or not it is also cased, so that overlap never matters.
#[derive(Clone, Copy, Debug)]
enum Casing {
    Ignorable,
    Cased,
    Other,
}

/// Final_Sigma's scan: skip case-ignorable characters, then is the next one
/// cased?
fn cased_after_ignorable(
    chars: impl Iterator<Item = char>,
    casing: &mut BTreeMap<char, Casing>,
) -> bool {
    for c in chars {
        match *casing.entry(c).or_insert_with(|| probe_casing(c)) {
            Casing::Ignorable => {}
            Casing::Cased => return true,
            Casing::Other => return false,
        }
    }
    false
}

/// Reads a character's Final_Sigma properties back through `str::to_lowercase`
/// itself, since std does not expose them (`core::unicode` is unstable). With
/// nothing after it, `Σ` ends a word exactly when the first non-ignorable
/// character before it is cased. So `cΣ` lowercases to end in `ς` iff `c` is
/// cased and not ignorable, and `AcΣ` iff `c` is ignorable or cased. This
/// allocates, so [`Scratch`] keeps the answer.
fn probe_casing(c: char) -> Casing {
    let ends_word = |prefix: &str| format!("{prefix}{c}Σ").to_lowercase().ends_with('ς');
    if ends_word("") {
        Casing::Cased
    } else if ends_word("A") {
        Casing::Ignorable
    } else {
        Casing::Other
    }
}

#[cfg(test)]
mod tests;
