//! Content atoms over the index's candidate seam: [`TextAtom`] compiles a
//! `text:ARG` into a cursor tree (docs/S2.md § What `text:ARG` means,
//! § Planning order step 2).
//!
//! The meaning is [`ferret_verify::Text`]'s, which also verifies; this
//! module only says which index answers can stand for it:
//!
//! ```text
//! one token, no parts     term(whole)                                    Yes
//! one run with parts      Or(term(whole) Yes, Maybe(And(term(part)…)))
//! several runs            Maybe(And(term(unit)…))
//! ```
//!
//! A term is looked up capped (`ferret_text::cap`) and is Yes only when no
//! longer token could share its cap (`ferret_text::exact_under_cap`), and
//! never under `case:text:`, which postings cannot see. Every tree then
//! gains the uncovered documents as Maybe ([`Pinned::atom`]).
//!
//! Reading and building are two steps because cursors borrow what was read:
//! [`TextAtom::read`] does the I/O, [`TextAtom::cursor`] none.

use ferret_index::{Atom, Candidates, Certainty, Cursor, Pinned, ReadError};
use ferret_text::{cap, exact_under_cap};
use crate::GrepAtom;

use ferret_verify::{Grep, Matcher, Text, TextMatcher};

/// A `text:ARG` atom with every term it needs read from the index.
#[derive(Debug)]
pub struct TextAtom {
    text: Text,
    /// The whole token's answer, when the argument is one run. `None`
    /// inside: no source answers terms.
    whole: Option<Option<Candidates>>,
    /// The distinct units' answers, when there are several.
    units: Vec<Option<Candidates>>,
}

impl TextAtom {
    /// Reads the atom's terms from the first of `pinned`'s sources that
    /// answers terms (S2 has one).
    pub fn read(text: Text, pinned: &Pinned<'_>) -> Result<Self, ReadError> {
        Self::read_until(text, pinned, &|| false)
    }

    /// Like `read`, checking host cancellation at every term and segment.
    pub fn read_until(
        text: Text,
        pinned: &Pinned<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self, ReadError> {
        if cancelled() {
            return Err(ReadError::Cancelled);
        }
        let lookup = |term: &[u8]| -> Result<Option<Candidates>, ReadError> {
            for source in pinned.sources() {
                if let Some(found) = source.read_until(&Atom::Term(cap(term)), cancelled)? {
                    return Ok(Some(found));
                }
            }
            Ok(None)
        };
        let whole = text.whole().map(lookup).transpose()?;
        // One token with no parts: its only unit is the whole token.
        let term_only = text.units().len() == 1 && text.units().next() == text.whole();
        let mut distinct: Vec<&[u8]> = text.units().collect();
        distinct.sort_unstable();
        distinct.dedup();
        let units = if term_only {
            Vec::new()
        } else {
            distinct.into_iter().map(lookup).collect::<Result<_, _>>()?
        };
        Ok(Self { text, whole, units })
    }

    /// The documents [`TextAtom::cursor`] may yield, uncovered ones
    /// included, and whether every one it yields from the index is Yes:
    /// read from the dictionaries alone, never a postings list, so that
    /// the planner can choose before it reads one. Dead documents count;
    /// it is an upper bound.
    pub fn estimate(text: &Text, pinned: &Pinned<'_>) -> Result<(u64, Certainty), ReadError> {
        Self::estimate_until(text, pinned, &|| false)
    }

    /// Like `estimate`, checking cancellation for every unit and segment.
    pub fn estimate_until(
        text: &Text,
        pinned: &Pinned<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(u64, Certainty), ReadError> {
        if cancelled() {
            return Err(ReadError::Cancelled);
        }
        let mut sources_exact = true;
        let mut df = |term: &[u8]| -> Result<u64, ReadError> {
            if cancelled() {
                return Err(ReadError::Cancelled);
            }
            for source in pinned.sources() {
                if let Some(found) = source.estimate_until(&Atom::Term(cap(term)), cancelled)? {
                    sources_exact &= found.exact;
                    return Ok(found.docs);
                }
            }
            Ok(u64::from(pinned.live().len()))
        };
        let whole = text.whole().map(&mut df).transpose()?;
        let term_only = text.units().len() == 1 && text.units().next() == text.whole();
        let units = if term_only {
            None
        } else {
            let mut least = None;
            for unit in text.units() {
                let n = df(unit)?;
                least = Some(least.map_or(n, |m: u64| m.min(n)));
            }
            least
        };
        let docs = whole.unwrap_or(0) + units.unwrap_or(0);
        let exact = sources_exact && term_only
            && pinned.sources().next().is_some()
            && text
                .whole()
                .is_some_and(|w| !text.is_case_sensitive() && exact_under_cap(w));
        let certainty = if exact {
            Certainty::Yes
        } else {
            Certainty::Maybe
        };
        Ok((docs + u64::from(pinned.uncovered().len()), certainty))
    }

    /// What the verifier checks a Maybe document against.
    pub fn text(&self) -> &Text {
        &self.text
    }

    /// The atom's cursor tree over `pinned`, uncovered documents included
    /// as Maybe. Liveness is not applied: the planner filters the whole
    /// content tree once ([`Pinned::top`]).
    pub fn cursor<'a>(&'a self, pinned: &'a Pinned<'_>) -> Cursor<'a> {
        self.cursor_until(pinned, &|| false)
            .unwrap_or(Cursor::Empty)
    }

    /// Like `cursor`, checking cancellation during union materialisation.
    pub fn cursor_until<'a>(
        &'a self,
        pinned: &'a Pinned<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Cursor<'a>, ReadError> {
        if cancelled() {
            return Err(ReadError::Cancelled);
        }
        // A term no source answers could hold anywhere.
        let term = |found: &'a Option<Candidates>| match found {
            Some(candidates) => candidates.cursor(),
            None => Cursor::bits(pinned.live(), Certainty::Maybe),
        };
        let exact = |whole: &[u8]| !self.text.is_case_sensitive() && exact_under_cap(whole);
        let whole = match (&self.whole, self.text.whole()) {
            (Some(found), Some(token)) => Some(if exact(token) {
                term(found)
            } else {
                Cursor::maybe(term(found))
            }),
            _ => None,
        };
        let units = || Cursor::maybe(Cursor::and(self.units.iter().map(term).collect()));
        let tree = match whole {
            Some(whole) if self.units.is_empty() => whole,
            Some(whole) => {
                Cursor::or_until(vec![whole, units()], pinned.live().bound(), cancelled)?
            }
            None => units(),
        };
        Ok(pinned.atom(tree))
    }
}

/// A compiled content predicate. The verifier owns each predicate's meaning.
#[derive(Clone, Debug)]
pub enum ContentAtom {
    Text(Text),
    Grep(Grep),
}

impl ContentAtom {
    pub(crate) fn estimate_until(&self, pinned: &Pinned<'_>, cancelled: &dyn Fn() -> bool) -> Result<(u64, Certainty), ReadError> {
        match self {
            Self::Text(text) => TextAtom::estimate_until(text, pinned, cancelled),
            Self::Grep(grep) => Ok((GrepAtom::estimate_until(grep, pinned, cancelled)?, Certainty::Maybe)),
        }
    }

    pub(crate) fn read_until(&self, pinned: &Pinned<'_>, cancelled: &dyn Fn() -> bool) -> Result<ReadAtom, ReadError> {
        match self {
            Self::Text(text) => TextAtom::read_until(text.clone(), pinned, cancelled).map(ReadAtom::Text),
            Self::Grep(grep) => GrepAtom::read_until(grep.clone(), pinned, cancelled).map(ReadAtom::Grep),
        }
    }

    pub(crate) fn is_match(&self, matcher: &mut TextMatcher, bytes: &[u8]) -> bool {
        match self {
            Self::Text(text) => matcher.is_match(text, bytes),
            Self::Grep(grep) => grep.is_match(bytes),
        }
    }
}

pub(crate) enum ReadAtom {
    Text(TextAtom),
    Grep(GrepAtom),
}

impl ReadAtom {
    pub(crate) fn cursor_until<'a>(&'a self, pinned: &'a Pinned<'_>, cancelled: &dyn Fn() -> bool) -> Result<Cursor<'a>, ReadError> {
        match self {
            Self::Text(atom) => atom.cursor_until(pinned, cancelled),
            Self::Grep(atom) => atom.cursor_until(pinned, cancelled),
        }
    }
}
