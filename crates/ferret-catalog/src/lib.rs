//! Names, inodes and documents: the only mutable state in Super Ferret.
//!
//! `(parent inode, name) → inode → document → content hash`, with dense ids
//! assigned here (DECISIONS.md D4, D5). Ignored names carry type tags and have
//! no inode row. A rename, move or duplicate changes
//! this crate's tables and nothing else. Also owns the contiguous name heap
//! that filename search scans (D14), and document liveness.
//!
//! Knows nothing about tokens, postings, the walker, or content hashing: the
//! crawler fills [`Batch`]es and hands over finished hashes.
//!
//! - [`batch`]: the rows a walk worker produces.
//! - `build`: merges batches and carried roots into tables (D29, D30, D31).
//! - `format`: the snapshot file, its encoder and its validating decoder.
//! - [`read`]: [`Catalog`], one opened generation, and its accessors.
//! - [`transaction`]: checkpoint writer: lock, carry-over, commit (D26, D32).
//! - [`log`]: durable changes, published prefixes and lazy family checks.

pub mod batch;
mod budget;
mod build;
mod compact;
mod format;
mod generation;
mod input;
mod lock;
pub mod log;
mod migrate;
mod overlay;
mod packed;
mod publication;
pub mod read;
mod resident_names;
mod session;
pub mod transaction;

#[cfg(test)]
mod tests;

pub use batch::{Batch, Content, DirToken, Stat, WorkTreeKind};
pub use budget::{BudgetUsage, CompactionLimits};
pub use build::BuildError;
pub use format::{DecodeError, Section};
pub use generation::{Generation, Handle, RetryFromCurrent};
pub use input::{InputBudget, InputLimits, InputUsage};
pub use read::{
    Catalog, Contents, Entry, Inode, Kind, Kinds, Name, NameReader, NameRuns, OpenError, RUN,
    Resolved, Target, WorkTree,
};
pub use resident_names::{PackedNameLists, PackedStrings, ResidentNames};
pub use session::WriterSession;
pub use transaction::{BeginError, CommitError, KeepError, Transaction};

/// A content hash: the first 128 bits of BLAKE3, computed by the crawler.
pub type Hash = [u8; 16];

/// An inode row in a checkpoint epoch. Dense at its base, renumbered by
/// checkpoint publication (D52). Retained references use [`Handle`].
/// Base directories form a prefix; log-created directories can follow files.
/// Use the effective live iterators and kind checks (D52).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InoId(pub u32);

/// A name edge in a checkpoint epoch. Retained references use [`Handle`].
/// Dense at the base, renumbered by checkpoint publication, and in
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

/// BLAKE3-128 used by checkpoint heads, section payloads and manifests.
/// Exposed for offline format tools and the benchmark driver; no third-party
/// hash types cross the catalog boundary.
pub fn checkpoint_checksum(bytes: &[u8]) -> Hash {
    generation::checksum(bytes)
}
