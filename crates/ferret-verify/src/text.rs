//! [`Text`]: what a `text:ARG` content atom means, and [`TextMatcher`]:
//! whether a document's bytes hold it (docs/S2.md § What `text:ARG` means).
//!
//! The argument goes through the same tokenizer as documents. Its **units**
//! are, per run, the run's parts, or the whole run when it has none. A
//! document's units are formed the same way, in order, ignoring whatever
//! separates runs. The atom holds when:
//!
//! - the argument is one run, and the document emits its whole token, as a
//!   whole run or as a part (`text:serde`, `text:HttpRequest` in
//!   `parseHttpRequest`); or
//! - the argument's units occur contiguously among the document's units
//!   (`text:"request handler"` in `requestHandler`, `request_handler`,
//!   `RequestHandler` and `request, handler`).
//!
//! These are exactly the facts the index's postings can propose, which is
//! why the verifier is the specification: an uncovered document is
//! answered by this alone.
//!
//! `case:text:` compares the original bytes of each token's span instead of
//! its lowercase, so `case:text:Foo` holds for `Foo` and `FooBar`, not
//! `foo` or `FOO`. Tokens are never capped here: the verifier sees the
//! whole token, which is how a capped postings hit is settled.
//!
//! # Verifying without tokenizing the whole document
//!
//! Tokenizing runs at ~165 MB/s; a byte search runs at GB/s. So
//! [`TextMatcher::find`] tokenizes only **windows** around the places a
//! match could be, found by searching the bytes for a **needle** per unit,
//! and rejects a document lacking any needle without tokenizing it at all.
//! Its answer is exactly [`TextMatcher::find`]'s over the whole document
//! (tested against it), by this argument:
//!
//! 1. **Every match contains each unit's needle in its bytes.** A
//!    `case:text:` needle is the unit's original bytes, which the matching
//!    token's span equals. Otherwise the needle is the longest run of ASCII
//!    bytes in the unit's lowercase, searched ASCII-case-insensitively.
//!    Each character of a token's span lowercases on its own (Σ, the one
//!    exception, lowercases to σ or ς either way), to at least one
//!    character. An ASCII character lowercases to one ASCII byte, and no
//!    non-ASCII character outside [`ASCII_FROM_NON_ASCII`] lowercases to
//!    anything holding an ASCII byte. So in a document holding none of
//!    those, a run of ASCII bytes in a token comes from a run of ASCII
//!    characters in its span, equal up to ASCII case. A document holding
//!    one is tokenized whole. A unit with no ASCII has no needle; an
//!    argument whose units have none is verified by tokenizing whole.
//!    A single-run argument's whole token holds every unit's needle too:
//!    its parts' lowercases differ from the slices of its whole only at Σ.
//! 2. **A window holds every match touching its anchor.** A needle is
//!    word bytes only (`[A-Za-z0-9_]` and non-ASCII), so an occurrence lies
//!    inside one **segment**: a maximal stretch with no ASCII separator byte
//!    (an ASCII byte outside `[A-Za-z0-9_]`). A match of `n` units spans at
//!    most `n` runs, consecutive in the document, one of them in the
//!    anchor's segment. So the window is the anchor's segment widened by
//!    the `n - 1` nearest segments holding a run on each side, however much
//!    separator lies between: whatever the gap, it is walked, not guessed.
//! 3. **A window tokenizes as the document does.** Its edges are the
//!    document's edges or sit next to an ASCII separator byte. An ASCII
//!    byte always decodes as itself, so UTF-8 decoding resynchronises there,
//!    no run crosses it, and Final_Sigma looks only within a run. The
//!    window's units are therefore a contiguous stretch of the document's,
//!    and a match the window holds is a match of the document.
//!
//! Windows are tokenized in document order, merged where they overlap, and
//! the walk stops at its first match, which is therefore the document's
//! first.

use std::ops::{ControlFlow, Range};
use std::sync::LazyLock;

use ferret_text::{Kind, Scratch, Token, tokenize, tokenize_until};
use regex::bytes::Regex;

/// The non-ASCII characters whose lowercase, as `ferret-text` computes it,
/// holds an ASCII byte: `İ` (to `i` and a combining dot) and the Kelvin
/// sign (to `k`). A test derives the set by lowercasing every `char`
/// through the tokenizer.
pub(crate) const ASCII_FROM_NON_ASCII: [char; 2] = ['\u{130}', '\u{212A}'];

/// Matches the UTF-8 encoding of any [`ASCII_FROM_NON_ASCII`] character.
static UNSAFE: LazyLock<Regex> = LazyLock::new(|| {
    let alternatives: Vec<String> = ASCII_FROM_NON_ASCII
        .iter()
        .map(|c| {
            c.encode_utf8(&mut [0; 4])
                .bytes()
                .map(|b| format!("\\x{b:02X}"))
                .collect()
        })
        .collect();
    Regex::new(&format!("(?-u){}", alternatives.join("|"))).expect("a fixed pattern")
});

/// A parsed `text:ARG`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Text {
    /// The argument's bytes, which `Unit::raw` indexes.
    arg: Vec<u8>,
    case: bool,
    /// The one run's whole token, when the argument is a single run.
    whole: Option<Unit>,
    units: Vec<Unit>,
    /// Per unit, the index of the first unit equal to it: what the matcher
    /// compares, so a document token is compared once per distinct unit.
    ids: Vec<usize>,
    /// The Knuth–Morris–Pratt failure function over `ids`.
    fail: Vec<usize>,
    /// The needles; `None` when some unit has none to offer as the anchor
    /// (no ASCII, case-insensitive).
    needles: Option<Needles>,
}

/// Byte searches every match must satisfy (the module's point 1).
#[derive(Clone, Debug)]
struct Needles {
    /// The longest needle: every occurrence is a window's anchor.
    anchor: Regex,
    /// The other units' needles: a document lacking one cannot match.
    others: Vec<Regex>,
}

/// Compiled from the needles, so equal patterns are equal needles.
impl PartialEq for Needles {
    fn eq(&self, other: &Self) -> bool {
        fn patterns(n: &Needles) -> impl Iterator<Item = &str> {
            std::iter::once(n.anchor.as_str()).chain(n.others.iter().map(Regex::as_str))
        }
        patterns(self).eq(patterns(other))
    }
}

impl Eq for Needles {}

impl Needles {
    /// Per distinct unit, its needle, if it has one: its original bytes for
    /// `case:text:`, else the longest run of ASCII bytes in its lowercase.
    fn new(arg: &[u8], case: bool, units: &[Unit], ids: &[usize]) -> Option<Self> {
        let mut needles: Vec<&[u8]> = Vec::new();
        for (i, unit) in units.iter().enumerate() {
            if ids[i] != i {
                continue;
            }
            let needle = if case {
                &arg[unit.raw.clone()]
            } else {
                unit.token
                    .split(|b| !b.is_ascii())
                    .max_by_key(|run| run.len())
                    .unwrap_or_default()
            };
            if !needle.is_empty() && !needles.contains(&needle) {
                needles.push(needle);
            }
        }
        // Longest first: the anchor, likely the rarest.
        needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
        let compile = |needle: &[u8]| {
            let text = std::str::from_utf8(needle).expect("a token span is UTF-8");
            let flags = if case { "" } else { "(?i-u)" };
            Regex::new(&format!("{flags}{}", regex::escape(text))).expect("an escaped literal")
        };
        let (anchor, others) = needles.split_first()?;
        Some(Self {
            anchor: compile(anchor),
            others: others.iter().map(|n| compile(n)).collect(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Unit {
    /// Lowercased, as the tokenizer emits it.
    token: Vec<u8>,
    /// Its span in the argument.
    raw: Range<usize>,
}

impl Text {
    /// Parses `arg`; `case` asks for `case:text:`'s exact bytes. `None` when
    /// the argument holds no token at all (only punctuation or spaces).
    pub fn new(arg: &[u8], case: bool) -> Option<Self> {
        // Per run: its whole token, then its parts.
        let mut runs: Vec<(Unit, Vec<Unit>)> = Vec::new();
        tokenize(arg, &mut Scratch::default(), |token| {
            let unit = Unit {
                token: token.bytes.to_vec(),
                raw: token.run,
            };
            match (token.kind, runs.last_mut()) {
                (Kind::Part, Some((_, parts))) => parts.push(unit),
                _ => runs.push((unit, Vec::new())),
            }
        });
        let whole = match runs.as_slice() {
            [] => return None,
            [(whole, _)] => Some(whole.clone()),
            _ => None,
        };
        let units: Vec<Unit> = runs
            .into_iter()
            .flat_map(|(whole, parts)| if parts.is_empty() { vec![whole] } else { parts })
            .collect();
        let same = |a: &Unit, b: &Unit| {
            if case {
                arg[a.raw.clone()] == arg[b.raw.clone()]
            } else {
                a.token == b.token
            }
        };
        let ids: Vec<usize> = units
            .iter()
            .map(|u| units.iter().position(|v| same(u, v)).unwrap_or(0))
            .collect();
        let mut fail = vec![0; ids.len()];
        let mut k = 0;
        for i in 1..ids.len() {
            while k > 0 && ids[i] != ids[k] {
                k = fail[k - 1];
            }
            if ids[i] == ids[k] {
                k += 1;
            }
            fail[i] = k;
        }
        let needles = Needles::new(arg, case, &units, &ids);
        Some(Self {
            arg: arg.to_vec(),
            case,
            whole,
            units,
            ids,
            fail,
            needles,
        })
    }

    /// The single run's whole token, lowercased, when the argument is one
    /// run: a document emitting it holds the atom.
    pub fn whole(&self) -> Option<&[u8]> {
        self.whole.as_ref().map(|u| u.token.as_slice())
    }

    /// The units, lowercased, in order. A document holding the atom emits
    /// every one of them.
    pub fn units(&self) -> impl ExactSizeIterator<Item = &[u8]> {
        self.units.iter().map(|u| u.token.as_slice())
    }

    /// Whether this is `case:text:`, which postings cannot decide.
    pub fn is_case_sensitive(&self) -> bool {
        self.case
    }

    /// Whether `token`, from `doc`, is `unit`.
    fn is(&self, unit: &Unit, doc: &[u8], token: &Token<'_>) -> bool {
        if self.case {
            doc[token.run.clone()] == self.arg[unit.raw.clone()]
        } else {
            token.bytes == unit.token.as_slice()
        }
    }

    /// The id of the unit `token` is, or `None` when it is none of them.
    fn id(&self, doc: &[u8], token: &Token<'_>) -> Option<usize> {
        self.units
            .iter()
            .enumerate()
            .find(|&(i, unit)| self.ids[i] == i && self.is(unit, doc, token))
            .map(|(i, _)| i)
    }
}

/// Finds [`Text`] atoms in documents. Holds the tokenizer's scratch and the
/// match state, so matching allocates only while they grow.
#[derive(Debug, Default)]
pub struct TextMatcher {
    scratch: Scratch,
    /// The start offset of each of the last `units` document units, as a
    /// ring indexed by unit count.
    starts: Vec<usize>,
    stats: MatchStats,
}

/// What a [`TextMatcher`] has done, summed over its calls.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MatchStats {
    /// Documents matched against.
    pub documents: u64,
    /// Documents rejected by the byte search, without tokenizing.
    pub rejected: u64,
    /// Documents tokenized whole: the argument has no needle, or the
    /// document holds a character whose lowercase is ASCII.
    pub whole: u64,
    /// Bytes handed to the tokenizer, up to the token it stopped at.
    pub tokenized: u64,
}

impl TextMatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// The byte span of the first place `doc` holds `text`, or `None`.
    /// Tokenizes only windows around needle occurrences (module docs).
    pub fn find(&mut self, text: &Text, doc: &[u8]) -> Option<Range<usize>> {
        self.stats.documents += 1;
        let Some(needles) = &text.needles else {
            self.stats.whole += 1;
            return self.walk(text, doc, 0..doc.len());
        };
        if !text.case && UNSAFE.is_match(doc) {
            self.stats.whole += 1;
            return self.walk(text, doc, 0..doc.len());
        }
        if needles.others.iter().any(|n| !n.is_match(doc)) {
            self.stats.rejected += 1;
            return None;
        }
        let margin = text.units.len() - 1;
        let (mut from, mut window): (usize, Option<Range<usize>>) = (0, None);
        while let Some(hit) = needles.anchor.find_at(doc, from) {
            let segment = segment(doc, hit.start());
            from = segment.end;
            let next = widen(doc, segment, margin);
            match &mut window {
                Some(current) if next.start <= current.end => current.end = current.end.max(next.end),
                _ => {
                    if let Some(done) = window.replace(next)
                        && let Some(found) = self.walk(text, doc, done)
                    {
                        return Some(found);
                    }
                }
            }
        }
        match window {
            None => {
                self.stats.rejected += 1;
                None
            }
            Some(last) => self.walk(text, doc, last),
        }
    }

    pub fn is_match(&mut self, text: &Text, doc: &[u8]) -> bool {
        self.find(text, doc).is_some()
    }

    pub fn stats(&self) -> MatchStats {
        self.stats
    }

    /// Tokenizes `doc[range]`, whose edges are safe (module point 3), and
    /// returns its first match, in `doc`'s offsets.
    fn walk(&mut self, text: &Text, doc: &[u8], range: Range<usize>) -> Option<Range<usize>> {
        let base = range.start;
        let doc = &doc[range];
        let width = text.units.len();
        self.starts.clear();
        self.starts.resize(width, 0);
        let mut state = Walk {
            text,
            doc,
            starts: &mut self.starts,
            pending: None,
            matched: 0,
            fed: 0,
        };
        let found = match tokenize_until(doc, &mut self.scratch, |token| state.token(&token)) {
            ControlFlow::Break(found) => Some(found),
            ControlFlow::Continue(()) => state.flush().break_value(),
        };
        self.stats.tokenized += found.as_ref().map_or(doc.len(), |f| f.end) as u64;
        found.map(|f| base + f.start..base + f.end)
    }

    /// [`Self::find`] by tokenizing all of `doc`: the reference the windows
    /// must agree with.
    #[cfg(test)]
    fn find_whole(&mut self, text: &Text, doc: &[u8]) -> Option<Range<usize>> {
        self.walk(text, doc, 0..doc.len())
    }
}

/// An ASCII byte that separates runs: one outside `[A-Za-z0-9_]`.
fn separator(b: u8) -> bool {
    b.is_ascii() && !(b.is_ascii_alphanumeric() || b == b'_')
}

/// The segment holding `at`: the maximal range around it with no separator.
fn segment(doc: &[u8], at: usize) -> Range<usize> {
    let start = doc[..at].iter().rposition(|&b| separator(b)).map_or(0, |i| i + 1);
    let end = doc[at..].iter().position(|&b| separator(b)).map_or(doc.len(), |i| at + i);
    start..end
}

/// Whether a segment holds a run: a word byte, or an alphanumeric
/// character. A segment decodes alone as it does in place (module point 3).
fn holds_run(segment: &[u8]) -> bool {
    segment.iter().any(|&b| b.is_ascii_alphanumeric() || b == b'_')
        || segment
            .utf8_chunks()
            .any(|chunk| chunk.valid().chars().any(char::is_alphanumeric))
}

/// `segment` widened by the `margin` nearest run-holding segments on each
/// side (module point 2). A segment counted holds at least one run, which is
/// all the count needs; separators between are walked whatever their length.
fn widen(doc: &[u8], segment: Range<usize>, margin: usize) -> Range<usize> {
    let (mut start, mut need) = (segment.start, margin);
    while need > 0 && start > 0 {
        let end = doc[..start].iter().rposition(|&b| !separator(b)).map_or(0, |i| i + 1);
        start = doc[..end].iter().rposition(|&b| separator(b)).map_or(0, |i| i + 1);
        if start < end && holds_run(&doc[start..end]) {
            need -= 1;
        }
    }
    let (mut end, mut need) = (segment.end, margin);
    while need > 0 && end < doc.len() {
        let start = doc[end..].iter().position(|&b| !separator(b)).map_or(doc.len(), |i| end + i);
        end = doc[start..].iter().position(|&b| separator(b)).map_or(doc.len(), |i| start + i);
        if start < end && holds_run(&doc[start..end]) {
            need -= 1;
        }
    }
    start..end
}

/// One window's walk over its units.
struct Walk<'t, 'd, 's> {
    text: &'t Text,
    doc: &'d [u8],
    starts: &'s mut [usize],
    /// A whole run's unit, held until it is known whether parts follow; it
    /// is a unit only when none do.
    pending: Option<(Option<usize>, Range<usize>)>,
    /// Units of the argument matched so far, the KMP state.
    matched: usize,
    /// Document units fed so far.
    fed: usize,
}

impl Walk<'_, '_, '_> {
    /// Breaks with the match's span when `token` completes one.
    fn token(&mut self, token: &Token<'_>) -> ControlFlow<Range<usize>> {
        if let Some(whole) = &self.text.whole
            && self.text.is(whole, self.doc, token)
        {
            return ControlFlow::Break(token.run.clone());
        }
        match token.kind {
            Kind::Whole => {
                self.flush()?;
                self.pending = Some((self.text.id(self.doc, token), token.run.clone()));
                ControlFlow::Continue(())
            }
            Kind::Part => {
                self.pending = None;
                self.feed(self.text.id(self.doc, token), token.run.clone())
            }
        }
    }

    /// Feeds a pending whole run, which had no parts.
    fn flush(&mut self) -> ControlFlow<Range<usize>> {
        match self.pending.take() {
            Some((id, run)) => self.feed(id, run),
            None => ControlFlow::Continue(()),
        }
    }

    fn feed(&mut self, id: Option<usize>, run: Range<usize>) -> ControlFlow<Range<usize>> {
        let (ids, fail) = (&self.text.ids, &self.text.fail);
        let width = ids.len();
        self.starts[self.fed % width] = run.start;
        self.fed += 1;
        let Some(id) = id else {
            self.matched = 0;
            return ControlFlow::Continue(());
        };
        while self.matched > 0 && ids[self.matched] != id {
            self.matched = fail[self.matched - 1];
        }
        if ids[self.matched] == id {
            self.matched += 1;
        }
        if self.matched == width {
            return ControlFlow::Break(self.starts[(self.fed - width) % width]..run.end);
        }
        ControlFlow::Continue(())
    }
}

#[cfg(test)]
mod tests;
