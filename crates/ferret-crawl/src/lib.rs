//! Walking roots, `statx`, change detection against the catalog, hashing.
//!
//! Descends only where `ferret-policy` allows; compares `(size, mtime, ctime)`
//! with the catalog row and re-reads and re-hashes only what changed
//! (DESIGN.md § The catalog).
//!
//! Knows nothing about queries or index formats.
