//! Walking configured roots.
//!
//! [`walk`] and [`walk_parallel`] visit one root and report each entry
//! [`ferret_policy`] decides, with the `lstat` fields the catalog will store.
//! The root is opened by the path the user named; every operation below it is
//! relative to a directory handle, so a name swapped for a symlink cannot
//! redirect the walk (D21). Change detection against the catalog and content
//! hashing are later slices (DESIGN.md § Policy and crawl); this crate still
//! knows nothing about queries or index formats.

mod walk;

pub use walk::{
    Boundary, Decided, Event, EventVisitor, FaultContext, IoOp, Stat, WalkOptions, WorkTree,
    WorkTreeKind, default_workers, walk, walk_parallel,
};

#[cfg(test)]
mod tests;
