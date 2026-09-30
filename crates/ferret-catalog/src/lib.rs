//! Names, inodes and documents: the only mutable state in Super Ferret.
//!
//! `(parent inode, name) → inode → document → content hash`, with dense ids
//! assigned here (DECISIONS.md D4, D5). A rename, move or duplicate changes
//! this crate's tables and nothing else. Also owns the contiguous name heap
//! that filename search scans (D14), and document liveness.
//!
//! Knows nothing about tokens, postings, the walker, or hashing: the crawler
//! fills [`Batch`]es and hands over finished hashes.
//!
//! - [`batch`]: the rows a walk worker produces.
//! - `build`: merges batches and carried roots into tables (D29, D30, D31).
//! - `format`: the snapshot file, its encoder and its validating decoder.
//! - [`read`]: [`Catalog`], one opened generation, and its accessors.
//! - [`transaction`]: the single writer: lock, carry-over, commit (D26, D32).

pub mod batch;
mod build;
mod format;
mod packed;
pub mod read;
pub mod transaction;

#[cfg(test)]
mod tests;

pub use batch::{Batch, Content, DirToken, Stat, WorkTreeKind};
pub use build::BuildError;
pub use format::{DecodeError, Section};
pub use read::{Catalog, Inode, Kind, Kinds, Name, NameReader, NameRuns, OpenError, WorkTree};
pub use transaction::{BeginError, CommitError, KeepError, Transaction};

/// A content hash: the first 128 bits of BLAKE3, computed by the crawler.
pub type Hash = [u8; 16];

/// An inode row. Dense in each generation, and renumbered by every commit
/// (D27): never store one outside the catalog. Directories come first, so
/// `0..Catalog::dir_count()` are exactly the directories (D30).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InoId(pub u32);

/// A name edge. Dense in each generation, renumbered by every commit, and in
/// (parent, name) order, which is also name-heap order (D28).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NameId(pub u32);

/// A document: one distinct content. Stable across commits and never reused
/// (D4, D36 B); the id space has holes where documents died.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocId(pub u32);

/// What the last observation learnt about an inode's content (D37). Current
/// policy still decides eligibility each run; this records only what was
/// found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentState {
    /// Not sent to the index: a directory, a symlink, or a file the policy
    /// catalogues only (over the size cap).
    Unindexed = 0,
    /// Sent to the index, and the sniffer said binary.
    Binary = 1,
    /// Hashed; the inode has a document.
    Hashed = 2,
    /// Could not be read, or changed while it was read (D26, D33).
    Fault = 3,
}

impl ContentState {
    fn from_bits(bits: u8) -> Self {
        match bits & 3 {
            0 => Self::Unindexed,
            1 => Self::Binary,
            2 => Self::Hashed,
            _ => Self::Fault,
        }
    }
}
