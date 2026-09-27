//! [`Matcher`]: the narrow interface every byte matcher is used through, so
//! the regex engine behind [`Regex`] stays replaceable (D17 A).

use std::fmt;

/// Something that decides whether a byte string matches.
pub trait Matcher {
    /// Whether `bytes` match anywhere (a regex is unanchored unless its
    /// pattern anchors it).
    fn is_match(&self, bytes: &[u8]) -> bool;
}

/// A compiled regular expression over bytes, in the `regex` crate's syntax.
/// Names are bytes, not text, so a pattern matches invalid UTF-8 byte for
/// byte; `.` still matches one UTF-8 character where there is one.
#[derive(Clone, Debug)]
pub struct Regex(regex::bytes::Regex);

/// A pattern that does not compile. Carries the engine's message, which
/// points at the offending part of the pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegexError(pub String);

impl fmt::Display for RegexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RegexError {}

impl Regex {
    /// Compiles `pattern`; `fold` makes it case-insensitive.
    pub fn new(pattern: &str, fold: bool) -> Result<Regex, RegexError> {
        regex::bytes::RegexBuilder::new(pattern)
            .case_insensitive(fold)
            .build()
            .map(Regex)
            .map_err(|e| RegexError(e.to_string()))
    }
}

impl Matcher for Regex {
    fn is_match(&self, bytes: &[u8]) -> bool {
        self.0.is_match(bytes)
    }
}
