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

use std::ops::Range;

use ferret_text::{Kind, Scratch, Token, tokenize};

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
        Some(Self {
            arg: arg.to_vec(),
            case,
            whole,
            units,
            ids,
            fail,
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
}

impl TextMatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// The byte span of the first place `doc` holds `text`, or `None`.
    pub fn find(&mut self, text: &Text, doc: &[u8]) -> Option<Range<usize>> {
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
            found: None,
        };
        // Tokenizing cannot stop early, so the walk ignores tokens once it
        // has an answer.
        tokenize(doc, &mut self.scratch, |token| state.token(&token));
        state.flush();
        state.found
    }

    pub fn is_match(&mut self, text: &Text, doc: &[u8]) -> bool {
        self.find(text, doc).is_some()
    }
}

/// One document's walk over its units.
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
    found: Option<Range<usize>>,
}

impl Walk<'_, '_, '_> {
    fn token(&mut self, token: &Token<'_>) {
        if self.found.is_some() {
            return;
        }
        if let Some(whole) = &self.text.whole
            && self.text.is(whole, self.doc, token)
        {
            self.found = Some(token.run.clone());
            return;
        }
        match token.kind {
            Kind::Whole => {
                self.flush();
                self.pending = Some((self.text.id(self.doc, token), token.run.clone()));
            }
            Kind::Part => {
                self.pending = None;
                self.feed(self.text.id(self.doc, token), token.run.clone());
            }
        }
    }

    /// Feeds a pending whole run, which had no parts.
    fn flush(&mut self) {
        if let Some((id, run)) = self.pending.take() {
            self.feed(id, run);
        }
    }

    fn feed(&mut self, id: Option<usize>, run: Range<usize>) {
        if self.found.is_some() {
            return;
        }
        let (ids, fail) = (&self.text.ids, &self.text.fail);
        let width = ids.len();
        self.starts[self.fed % width] = run.start;
        self.fed += 1;
        let Some(id) = id else {
            self.matched = 0;
            return;
        };
        while self.matched > 0 && ids[self.matched] != id {
            self.matched = fail[self.matched - 1];
        }
        if ids[self.matched] == id {
            self.matched += 1;
        }
        if self.matched == width {
            self.found = Some(self.starts[(self.fed - width) % width]..run.end);
        }
    }
}

#[cfg(test)]
mod tests;
