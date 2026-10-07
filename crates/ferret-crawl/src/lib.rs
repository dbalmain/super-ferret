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

pub mod content;
mod coverage;
mod index;
mod observe;
pub mod reconcile;
mod refresh;
mod walk;
pub mod watch;

pub use content::Documents;
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

/// Lowers the calling Linux index thread alone. Intake must be spawned before
/// lowering its parent; query and socket threads retain their normal priority.
pub fn lower_index_priority() -> std::io::Result<()> {
    rustix::process::setpriority_process(Some(rustix::thread::gettid()), 19).map_err(Into::into)
}
/// Additional checkpoint space available to this uid, excluding reserved
/// blocks.
pub fn available_disk(path: &std::path::Path) -> std::io::Result<u64> {
    let stat = rustix::fs::statvfs(path)?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}
