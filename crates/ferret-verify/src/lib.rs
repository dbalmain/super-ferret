//! The verifier: re-read a candidate's bytes and match a query atom against
//! them, so every answer is exact whatever structure proposed it.
//!
//! A file whose `(size, mtime)` no longer matches the catalog is reported as
//! stale, never matched against.
//!
//! Knows nothing about how candidates were found.
