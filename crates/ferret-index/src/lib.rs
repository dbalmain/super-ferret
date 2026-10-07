//! Index structures over document ids: postings, filters, trigram
//! structures — each a source of *candidate* documents for a query atom,
//! with an exactness flag and a cost estimate (DECISIONS.md D6).
//!
//! Knows doc ids and byte strings, never documents' files, paths or inodes,
//! so it stays reusable outside desktop search. The only files it opens are
//! its own: segments and the manifest, in a directory the host names. The
//! planner sees these structures only through the candidate-source interface,
//! never their formats.
//!
//! S2 M2 adds the first structure, one immutable [`segment`] of term
//! postings over a contiguous DocId range (docs/S2.md § Postings layout on
//! intpack). A DocId here is a plain `u32`, the catalog's `DocId.0`.

//!
//! S2 M3 makes segments an index that follows the catalog: [`store`]'s
//! [`IndexWriter`] appends segments by DocId range under a [`manifest`],
//! with liveness supplied by the host as a [`live::DocSet`], and merges
//! adjacent segments by size level.
//!
//! S2 M4a adds the read side: [`candidate`]'s seam ([`Atom`], [`Source`],
//! [`Candidates`], [`Pinned`]), [`postings`]' term cursor across segments,
//! and [`cursor`]'s certainty algebra.

pub mod candidate;
pub mod cursor;
pub mod live;
pub mod manifest;
mod merge;
pub mod postings;
pub mod segment;
pub mod store;

pub use candidate::{Atom, Candidates, Estimate, Pinned, Source};
pub use cursor::{Advance, Certainty, Cursor, Probe};
pub use live::DocSet;
pub use manifest::{Manifest, SegmentEntry};
pub use segment::ReadError;
pub use store::{
    BUFFER, Budget, CatalogView, Error, Fault, Followed, IndexWriter, Merged, Reader, Stopped, View,
};
