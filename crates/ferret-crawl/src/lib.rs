//! Walking configured roots.
//!
//! [`walk`] visits one root and reports each entry [`ferret_policy`] decides,
//! with the `lstat` fields the catalog will store. Change detection against
//! the catalog and content hashing are later slices (DESIGN.md § Policy and
//! crawl); this crate still knows nothing about queries or index formats.

mod walk;

pub use walk::{Decided, Event, Stat, walk};

#[cfg(test)]
mod tests;
