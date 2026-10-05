//! Walking configured roots.
//!
//! [`walk`] and [`walk_parallel`] visit one root and report each entry
//! [`ferret_policy`] decides, with the `lstat` fields the catalog will store.
//! The root is opened by the path the user named; every operation below it is
//! relative to a directory handle, so a name swapped for a symlink cannot
//! redirect the walk (D21).
//!
//! [`index`] turns walks into a published catalog generation: it takes the
//! writer lock, walks the roots that need it with a visitor that carries,
//! sniffs and hashes content on the worker (D26, D31, D33), retains checked
//! old scopes under typed coverage faults, and commits trustworthy changes.
//! This crate knows nothing about queries or index formats.

mod coverage;
mod index;
mod observe;
pub mod reconcile;
mod refresh;
mod walk;
pub mod watch;

pub use index::{
    Counts, CoverageContext, CoverageFault, IndexError, IndexOptions, Published, Refresh, Report,
    RootChange, index, index_change, recrawl, session_change,
};
pub use observe::ContentFault;
pub use refresh::{
    RefreshOutcome, RefreshReason, RefreshReport, RefreshRequest, RefreshScope, RenameHint, refresh,
};

pub use walk::{
    Boundary, Decided, Event, EventVisitor, FaultContext, IoOp, Stat, WalkOptions, WorkTree,
    WorkTreeKind, default_workers, walk, walk_parallel,
};

#[cfg(test)]
mod tests;
