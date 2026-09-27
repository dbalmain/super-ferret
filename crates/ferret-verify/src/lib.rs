//! The verifier: match a query atom against bytes, so every answer is exact
//! whatever proposed it.
//!
//! - [`scan`]: [`Finder`], the substring scanner with ASCII case folding that
//!   filename search runs over the name heap (D41), and that verifying a file's
//!   content will reuse.
//! - [`matcher`]: [`Matcher`], the narrow interface to the regex engine (D17),
//!   and [`Regex`].
//! - [`toolchain`]: the ledger for the scanner's AVX2 arm (D11).
//!
//! A file whose `(size, mtime)` no longer matches the catalog is reported as
//! stale, never matched against (from S3, when content is verified).
//!
//! Knows nothing about how candidates were found.

pub mod matcher;
pub mod scan;
pub mod toolchain;

#[cfg(test)]
mod tests;

pub use matcher::{Matcher, Regex, RegexError};
pub use scan::{Arm, Finder};
