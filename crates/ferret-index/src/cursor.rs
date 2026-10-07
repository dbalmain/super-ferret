//! [`Cursor`] and [`Probe`]: ascending DocIds, each with a [`Certainty`],
//! and the AND / OR / NOT algebra over them (docs/S2.md § The Rust shape).
//!
//! The composite arms combine certainties under Kleene's three-valued
//! logic, with `Maybe < Yes` and absence as No:
//!
//! ```text
//! And      every child yields d          min of the children's: Yes∧Maybe = Maybe
//! Or       some child yields d           max of those that do:  Yes∨Maybe = Yes
//! AndNot   a yields d, b is not Yes      a's when b says No; Maybe when b says Maybe
//!                                         (NOT Yes = No, NOT Maybe = Maybe)
//! Filter   a yields d, p is not No       as And
//! Maybe    the child yields d            Maybe
//! ```
//!
//! Every arm keeps intpack's `next_geq` contract (Lucene's `advance`): the
//! cursor stays on the document it returned, a target at or below it
//! returns it again, and targets never decrease.

use std::borrow::Cow;

use crate::live::DocSet;
use crate::postings;
use crate::segment::ReadError;

/// How sure a source is that a yielded document holds the atom. Absence
/// from a cursor is No. Ordered so that AND is `min` and OR is `max`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Certainty {
    /// It may; the verifier decides.
    Maybe,
    /// It certainly does.
    Yes,
}

/// An `Or` wider than this many children is materialised into bitmaps
/// (S2.md § Planning order; a starting point for M4b's measurement).
pub const OR_WIDTH: usize = 64;

/// An `Or` whose children's summed cost exceeds `bound / OR_DENSITY` is
/// materialised into bitmaps: past that, two bitmaps of `bound` bits are
/// cheaper than merging the children on every step.
pub const OR_DENSITY: u32 = 32;

/// Ascending DocIds, each with a certainty.
pub enum Cursor<'a> {
    /// One term's postings, concatenated across segments: always Yes.
    /// Boxed because intpack's cursor carries a decoded 128-id block.
    Postings(Box<postings::Cursor<'a>>),
    /// A bitmap: liveness, uncovered documents, a materialised union.
    Bits(Bits<'a>),
    /// Leapfrog over the children, cheapest first; it drives.
    And(Box<[Cursor<'a>]>),
    Or(Or<'a>),
    AndNot(Box<(Cursor<'a>, Probe<'a>)>),
    Filter(Box<(Cursor<'a>, Probe<'a>)>),
    /// The child's documents, each downgraded to Maybe: a phrase's terms,
    /// which hold without the phrase necessarily holding.
    Maybe(Box<Cursor<'a>>),
    Empty,
}

impl<'a> Cursor<'a> {
    /// The members of `docs`, each with `certainty`.
    pub fn bits(docs: &'a DocSet, certainty: Certainty) -> Self {
        Self::Bits(Bits {
            docs: Cow::Borrowed(docs),
            certainty,
            floor: 0,
        })
    }

    /// Documents every child yields. An empty child makes the whole empty;
    /// so does an empty list, which has no driver: a caller that means
    /// "every document" passes the live bitmap.
    pub fn and(children: Vec<Cursor<'a>>) -> Self {
        let mut children = children;
        if children.is_empty() || children.iter().any(|c| matches!(c, Self::Empty)) {
            return Self::Empty;
        }
        if children.len() == 1 {
            return children.swap_remove(0);
        }
        children.sort_by_key(Cursor::cost);
        Self::And(children.into_boxed_slice())
    }

    /// Documents some child yields. `bound` is the view's `next_doc`, which
    /// sizes a materialised union; a document at or past it is dropped
    /// there, as it cannot be live.
    pub fn or(children: Vec<Cursor<'a>>, bound: u32) -> Self {
        Self::or_until(children, bound, &|| false).unwrap_or(Self::Empty)
    }

    /// Like `or`, checking cancellation while materialising a wide/dense union.
    pub fn or_until(
        children: Vec<Cursor<'a>>,
        bound: u32,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self, ReadError> {
        if cancelled() {
            return Err(ReadError::Cancelled);
        }
        let mut children: Vec<_> = children
            .into_iter()
            .filter(|c| !matches!(c, Self::Empty))
            .collect();
        match children.len() {
            0 => return Ok(Self::Empty),
            1 => return Ok(children.swap_remove(0)),
            _ => {}
        }
        let cost = children
            .iter()
            .map(Cursor::cost)
            .fold(0u64, u64::saturating_add);
        if children.len() > OR_WIDTH || cost > u64::from(bound / OR_DENSITY) {
            return materialise(children, bound, cancelled);
        }
        Ok(Self::Or(Or::new(children)))
    }

    /// Documents `positive` yields whose `negated` is not Yes.
    pub fn and_not(positive: Cursor<'a>, negated: Cursor<'a>) -> Self {
        match (positive, negated) {
            (Self::Empty, _) => Self::Empty,
            (positive, Self::Empty) => positive,
            (positive, negated) => Self::AndNot(Box::new((positive, Probe::Cursor(negated)))),
        }
    }

    /// Documents `driver` yields that `probe` does not rule out.
    pub fn filter(driver: Cursor<'a>, probe: Probe<'a>) -> Self {
        match driver {
            Self::Empty => Self::Empty,
            driver => Self::Filter(Box::new((driver, probe))),
        }
    }

    /// The child's documents, each as Maybe.
    pub fn maybe(child: Cursor<'a>) -> Self {
        match child {
            Self::Empty => Self::Empty,
            Self::Maybe(_) => child,
            child => Self::Maybe(Box::new(child)),
        }
    }

    /// The smallest document `>= target` at or after the current one, and
    /// its certainty.
    pub fn next_geq(&mut self, target: u32) -> Option<(u32, Certainty)> {
        match self {
            Self::Postings(cursor) => cursor.next_geq(target).map(|doc| (doc, Certainty::Yes)),
            Self::Bits(bits) => bits.next_geq(target),
            Self::And(children) => leapfrog(children, target),
            Self::Or(or) => or.next_geq(target),
            Self::AndNot(pair) => {
                let (positive, negated) = &mut **pair;
                let mut target = target;
                loop {
                    let (doc, certainty) = positive.next_geq(target)?;
                    match negated.test(doc) {
                        None => return Some((doc, certainty)),
                        Some(Certainty::Maybe) => return Some((doc, Certainty::Maybe)),
                        Some(Certainty::Yes) => target = doc.checked_add(1)?,
                    }
                }
            }
            Self::Filter(pair) => {
                let (driver, probe) = &mut **pair;
                let mut target = target;
                loop {
                    let (doc, certainty) = driver.next_geq(target)?;
                    match probe.test(doc) {
                        Some(other) => return Some((doc, certainty.min(other))),
                        None => target = doc.checked_add(1)?,
                    }
                }
            }
            Self::Maybe(child) => child
                .next_geq(target)
                .map(|(doc, _)| (doc, Certainty::Maybe)),
            Self::Empty => None,
        }
    }

    /// An upper bound on the documents the cursor yields (Lucene's
    /// `cost()`), for ordering an And's children and sizing an Or.
    pub fn cost(&self) -> u64 {
        match self {
            Self::Postings(cursor) => cursor.cost(),
            Self::Bits(bits) => u64::from(bits.docs.len()),
            Self::And(children) => children.iter().map(Cursor::cost).min().unwrap_or(0),
            Self::Or(or) => or
                .children
                .iter()
                .map(Cursor::cost)
                .fold(0, u64::saturating_add),
            Self::AndNot(pair) | Self::Filter(pair) => pair.0.cost(),
            Self::Maybe(child) => child.cost(),
            Self::Empty => 0,
        }
    }
}

/// Leapfrog: the first child drives, every other must land on its
/// document, and any that overshoots becomes the new target.
fn leapfrog(children: &mut [Cursor<'_>], target: u32) -> Option<(u32, Certainty)> {
    let (driver, rest) = children.split_first_mut()?;
    let mut target = target;
    'driver: loop {
        let (doc, mut certainty) = driver.next_geq(target)?;
        for child in rest.iter_mut() {
            let (other, other_certainty) = child.next_geq(doc)?;
            if other > doc {
                target = other;
                continue 'driver;
            }
            certainty = certainty.min(other_certainty);
        }
        return Some((doc, certainty));
    }
}

/// Drains `children` into two bitmaps, Yes and Maybe, under one `Or`.
fn materialise<'a>(
    children: Vec<Cursor<'a>>,
    bound: u32,
    cancelled: &dyn Fn() -> bool,
) -> Result<Cursor<'a>, ReadError> {
    let (mut yes, mut maybe) = (Vec::new(), Vec::new());
    for mut child in children {
        let mut target = 0;
        loop {
            if cancelled() {
                return Err(ReadError::Cancelled);
            }
            let Some((doc, certainty)) = child.next_geq(target) else {
                break;
            };
            match certainty {
                Certainty::Yes => yes.push(doc),
                Certainty::Maybe => maybe.push(doc),
            }
            let Some(next) = doc.checked_add(1) else {
                break;
            };
            target = next;
        }
    }
    let owned = |docs: Vec<u32>, certainty| {
        if cancelled() {
            return Err(ReadError::Cancelled);
        }
        let docs = DocSet::new(bound, docs.into_iter().take_while(|_| !cancelled()));
        if cancelled() {
            return Err(ReadError::Cancelled);
        }
        Ok(Cursor::Bits(Bits {
            docs: Cow::Owned(docs),
            certainty,
            floor: 0,
        }))
    };
    Ok(Cursor::Or(Or::new(vec![
        owned(yes, Certainty::Yes)?,
        owned(maybe, Certainty::Maybe)?,
    ])))
}

/// A bitmap's members, all with one certainty.
pub struct Bits<'a> {
    docs: Cow<'a, DocSet>,
    certainty: Certainty,
    /// The document last returned: a lower target returns it again.
    floor: u32,
}

impl Bits<'_> {
    fn next_geq(&mut self, target: u32) -> Option<(u32, Certainty)> {
        let doc = self.docs.next_geq(target.max(self.floor))?;
        self.floor = doc;
        Some((doc, self.certainty))
    }
}

/// A union, merged by scanning each child's current document. Wider unions
/// are materialised (see [`Cursor::or`]), so the scan is over at most
/// [`OR_WIDTH`] children.
pub struct Or<'a> {
    children: Box<[Cursor<'a>]>,
    heads: Box<[Head]>,
}

/// Where one child of an `Or` stands.
#[derive(Clone, Copy)]
enum Head {
    /// Not yet asked.
    Fresh,
    At(u32, Certainty),
    Exhausted,
}

impl<'a> Or<'a> {
    pub(crate) fn new(children: Vec<Cursor<'a>>) -> Self {
        let heads = vec![Head::Fresh; children.len()].into_boxed_slice();
        Self {
            children: children.into_boxed_slice(),
            heads,
        }
    }

    fn next_geq(&mut self, target: u32) -> Option<(u32, Certainty)> {
        let mut best: Option<(u32, Certainty)> = None;
        for (child, head) in self.children.iter_mut().zip(self.heads.iter_mut()) {
            let behind = match *head {
                Head::Fresh => true,
                Head::At(doc, _) => doc < target,
                Head::Exhausted => false,
            };
            if behind {
                *head = child
                    .next_geq(target)
                    .map_or(Head::Exhausted, |(doc, certainty)| Head::At(doc, certainty));
            }
            if let Head::At(doc, certainty) = *head {
                best = match best {
                    Some((low, _)) if doc > low => best,
                    Some((low, other)) if doc == low => Some((low, other.max(certainty))),
                    _ => Some((doc, certainty)),
                };
            }
        }
        best
    }
}

/// Answers "is `doc` in this atom?" with No (`None`), Maybe or Yes, for
/// non-decreasing `doc`.
pub enum Probe<'a> {
    /// A cursor, probed by `next_geq`.
    Cursor(Cursor<'a>),
    // S3: DocFilter(docfilter::Probe<'a>),
}

impl Probe<'_> {
    pub fn test(&mut self, doc: u32) -> Option<Certainty> {
        match self {
            Self::Cursor(cursor) => match cursor.next_geq(doc)? {
                (found, certainty) if found == doc => Some(certainty),
                _ => None,
            },
        }
    }
}

#[cfg(test)]
mod tests;
