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
use ferret_verify::Text;

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
        let lookup = |term: &[u8]| -> Result<Option<Candidates>, ReadError> {
            for source in pinned.sources() {
                if let Some(found) = source.read(&Atom::Term(cap(term)))? {
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

    /// What the verifier checks a Maybe document against.
    pub fn text(&self) -> &Text {
        &self.text
    }

    /// The atom's cursor tree over `pinned`, uncovered documents included
    /// as Maybe. Liveness is not applied: the planner filters the whole
    /// content tree once ([`Pinned::top`]).
    pub fn cursor<'a>(&'a self, pinned: &'a Pinned<'_>) -> Cursor<'a> {
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
            Some(whole) => Cursor::or(vec![whole, units()], pinned.live().bound()),
            None => units(),
        };
        pinned.atom(tree)
    }
}
