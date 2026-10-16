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
        Regex::build(regex::bytes::RegexBuilder::new(pattern).case_insensitive(fold))
    }

    /// The pattern that compiled into this matcher.
    pub fn pattern(&self) -> &str {
        self.0.as_str()
    }

    /// Byte span of the first match, over the whole supplied document.
    pub fn find(&self, bytes: &[u8]) -> Option<std::ops::Range<usize>> {
        self.0.find(bytes).map(|m| m.range())
    }

    /// Nonoverlapping byte spans, retaining the complete haystack for
    /// assertions. Empty spans advance one byte, including inside UTF-8; an
    /// empty span at the previous consuming match's end is suppressed.
    pub fn spans<'r, 't>(&'r self, bytes: &'t [u8]) -> Spans<'r, 't> {
        self.spans_at(bytes, 0)
    }

    /// Iterate with assertion context intact, starting at a byte offset.
    pub fn spans_at<'r, 't>(&'r self, bytes: &'t [u8], at: usize) -> Spans<'r, 't> {
        Spans {
            regex: self,
            bytes,
            next: Some(at),
            previous_end: None,
        }
    }

    /// Compiles with every setting already chosen (`GrepFlags`).
    pub(crate) fn build(builder: &regex::bytes::RegexBuilder) -> Result<Regex, RegexError> {
        builder
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

/// Shared byte-span iteration for grep output and counting. No haystack slicing
/// changes anchor or word-boundary context when advancing the search position.
pub struct Spans<'r, 't> {
    regex: &'r Regex,
    bytes: &'t [u8],
    next: Option<usize>,
    previous_end: Option<usize>,
}

impl Iterator for Spans<'_, '_> {
    type Item = std::ops::Range<usize>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let from = self.next?;
            if from > self.bytes.len() {
                self.next = None;
                return None;
            }
            let Some(found) = self.regex.0.find_at(self.bytes, from) else {
                self.next = None;
                return None;
            };
            let span = found.range();
            self.next = if span.is_empty() {
                span.end.checked_add(1)
            } else {
                Some(span.end)
            };
            if span.is_empty() && self.previous_end == Some(span.end) {
                continue;
            }
            self.previous_end = Some(span.end);
            return Some(span);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // M2: scalar-consuming matches and byte-wise empty advancement coexist.
    #[test]
    fn empty_spans_visit_every_byte_and_the_terminal_position() {
        for pattern in ["", "(?u:)", "(?-u:)"] {
            let regex = Regex::new(pattern, false).unwrap();
            assert_eq!(
                regex.spans("é界x".as_bytes()).collect::<Vec<_>>(),
                (0..=6).map(|at| at..at).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn consuming_spans_suppress_the_adjacent_empty_but_keep_later_empties() {
        let regex = Regex::new("a*", false).unwrap();
        assert_eq!(regex.spans(b"abc").collect::<Vec<_>>(), [0..1, 2..2, 3..3]);
        let regex = Regex::new(".+?", false).unwrap();
        assert_eq!(
            regex.spans("é界x".as_bytes()).collect::<Vec<_>>(),
            [0..2, 2..5, 5..6]
        );
    }

    #[test]
    fn iteration_preserves_assertion_context_and_terminates() {
        for (pattern, expected) in [
            (r"\A|\z", vec![0..0, 3..3]),
            (r"\b", vec![0..0, 3..3]),
            ("$", std::iter::once(3..3).collect()),
        ] {
            let regex = Regex::new(pattern, false).unwrap();
            let mut spans = regex.spans(b"abc");
            assert_eq!(spans.by_ref().collect::<Vec<_>>(), expected);
            assert_eq!(spans.next(), None);
        }
    }
}
