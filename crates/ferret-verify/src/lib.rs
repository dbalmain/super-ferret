//! The verifier: match a query atom against bytes, so every answer is exact
//! whatever proposed it.
//!
//! - [`scan`]: [`Finder`], the substring scanner with ASCII case folding that
//!   filename search runs over the name heap (D41), and that verifying a file's
//!   content will reuse.
//! - [`matcher`]: [`Matcher`], the narrow interface to the regex engine (D17),
//!   and [`Regex`].
//! - [`text`]: [`Text`], a `text:ARG` content atom, and [`TextMatcher`], the
//!   part-sequence phrase matcher over a document's tokens (S2).
//! - `dialect`: [`FindRegex`], GNU find syntax over whole-path bytes, with
//!   bounded backreference matching and fallible budget reporting.
//! - [`toolchain`]: the ledger for the scanner's AVX2 arm (D11).
//!
//! A file whose `(size, mtime)` no longer matches the catalog is reported as
//! stale, never matched against (from S3, when content is verified).
//!
//! Knows nothing about how candidates were found.

mod dialect;
pub mod matcher;
pub mod scan;
pub mod text;
pub mod toolchain;

#[cfg(test)]
mod tests;

pub use dialect::{Dialect, FindRegex, MatchLimit};
pub use matcher::{Matcher, Regex, RegexError};
pub use scan::{Arm, Finder};
pub use text::{Text, TextMatcher};
