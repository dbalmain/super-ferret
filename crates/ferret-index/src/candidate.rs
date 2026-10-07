//! The candidate seam (docs/S2.md § The candidate seam; D6): how a query
//! asks the index for documents without seeing any structure's format.
//!
//! An [`Atom`] is a byte string at the index level. A [`Source`] is one
//! structure of a pinned view; [`Source::read`] answers an atom with
//! [`Candidates`], read and owned, whose [`Estimate`] the planner orders by
//! and whose [`Cursor`]s and [`Probe`]s borrow them. [`Pinned`] adds the two
//! bitmaps every query needs, live and uncovered, and the arms that apply
//! them.
//!
//! A new structure (S3's trigram postings, per-document filters) is a
//! `Source` variant, a `Candidates` variant and their arms; the planner
//! does not change.

use crate::cursor::{Certainty, Cursor, Or, Probe};
use crate::live::DocSet;
use crate::postings;
use crate::segment::ReadError;
use crate::store::View;

/// One content atom at the index level: byte strings, never paths.
/// Query-level atoms (a phrase, a regex) compile into trees of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Atom<'q> {
    /// One ferret-text token, normalised and capped (`ferret_text::cap`).
    Term(&'q [u8]),
    /// Three raw bytes. No S2 source answers it; S3's do.
    Trigram([u8; 3]),
}

/// What a source's answer for an atom costs, for ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Estimate {
    /// Upper bound on documents the cursor yields (Lucene's `cost()`).
    pub docs: u64,
    /// Whether every yielded document is Yes.
    pub exact: bool,
    /// Whether the answer can enumerate its documents. A per-document
    /// filter cannot; it can only be probed.
    pub enumerable: bool,
    /// Bytes read to answer it.
    pub bytes: u64,
}

/// One structure of a pinned index view.
#[derive(Clone, Copy, Debug)]
pub enum Source<'v> {
    /// S2: term postings over one manifest's segments.
    Postings(&'v View),
    // S3: TrigramPostings(..), DocFilter(..)
}

impl Source<'_> {
    /// The source's answer for `atom`, read now; `None` when this source
    /// does not answer atoms of that kind.
    pub fn read(&self, atom: &Atom) -> Result<Option<Candidates>, ReadError> {
        match (self, atom) {
            (Self::Postings(view), Atom::Term(term)) => {
                let segments = view.segments().iter().map(|s| &**s);
                Ok(Some(Candidates::Postings(postings::Term::read(
                    segments, term,
                )?)))
            }
            (Self::Postings(_), Atom::Trigram(_)) => Ok(None),
        }
    }
}

/// A source's answer for one atom, owned by the query; cursors borrow it.
#[derive(Debug)]
pub enum Candidates {
    Postings(postings::Term),
}

impl Candidates {
    pub fn estimate(&self) -> Estimate {
        match self {
            Self::Postings(term) => Estimate {
                docs: term.docs(),
                exact: true,
                enumerable: true,
                bytes: term.bytes(),
            },
        }
    }

    /// Ascending DocIds. Only for an enumerable estimate.
    pub fn cursor(&self) -> Cursor<'_> {
        match self {
            Self::Postings(term) => Cursor::Postings(Box::new(term.cursor())),
        }
    }

    /// Membership by id, for an answer that is not the driver.
    pub fn probe(&self) -> Probe<'_> {
        match self {
            Self::Postings(_) => Probe::Cursor(self.cursor()),
        }
    }
}

/// One query's view of the index: a pinned [`View`] and the live set of
/// the catalog view pinned with it (docs/S2.md § Liveness, coverage and the
/// cursor tree a query sees).
pub struct Pinned<'a> {
    view: &'a View,
    live: &'a DocSet,
    uncovered: DocSet,
}

impl<'a> Pinned<'a> {
    /// `live` is the catalog view's live documents; its bound is the view's
    /// `next_doc`.
    pub fn new(view: &'a View, live: &'a DocSet) -> Self {
        let uncovered = DocSet::new(live.bound(), view.uncovered(live));
        Self {
            view,
            live,
            uncovered,
        }
    }

    /// Every structure the view holds; S2 has one.
    pub fn sources(&self) -> [Source<'a>; 1] {
        [Source::Postings(self.view)]
    }

    pub fn live(&self) -> &'a DocSet {
        self.live
    }

    /// Live documents the index has not tokenized: Maybe for every atom.
    pub fn uncovered(&self) -> &DocSet {
        &self.uncovered
    }

    /// One content atom's tree, with the uncovered documents added as
    /// Maybe. With full coverage the tree is returned as it is.
    pub fn atom<'c>(&'c self, tree: Cursor<'c>) -> Cursor<'c>
    where
        'a: 'c,
    {
        if self.uncovered.is_empty() {
            return tree;
        }
        let uncovered = Cursor::bits(&self.uncovered, Certainty::Maybe);
        match tree {
            Cursor::Empty => uncovered,
            // Not `Cursor::or`: the uncovered set is already a bitmap, and a
            // first build's would otherwise materialise every atom.
            tree => Cursor::Or(Or::new(vec![tree, uncovered])),
        }
    }

    /// Live documents for which `negated` is not Yes: a NOT with nothing
    /// enumerable beside it.
    pub fn not<'c>(&'c self, negated: Cursor<'c>) -> Cursor<'c>
    where
        'a: 'c,
    {
        Cursor::and_not(Cursor::bits(self.live, Certainty::Yes), negated)
    }

    /// The query's whole content tree, restricted to live documents. Applied
    /// once, at the top, rather than per atom.
    pub fn top<'c>(&'c self, tree: Cursor<'c>) -> Cursor<'c>
    where
        'a: 'c,
    {
        Cursor::filter(tree, Probe::Cursor(Cursor::bits(self.live, Certainty::Yes)))
    }
}
