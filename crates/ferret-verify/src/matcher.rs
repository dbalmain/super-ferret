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
            rebased: false,
        }
    }

    /// Multiline search-region iteration. Each resumed search sees the
    /// remaining suffix as its haystack, as rg's multiline search does.
    /// Counting and rendering must re-match with `spans` and their own bounds.
    pub fn rebased_spans<'r, 't>(&'r self, bytes: &'t [u8]) -> Spans<'r, 't> {
        Spans {
            rebased: true,
            ..self.spans(bytes)
        }
    }

    /// Capture spans retain the supplied haystack's assertion context.
    pub fn captures_at<'t>(&self, bytes: &'t [u8], at: usize) -> Option<Captures<'t>> {
        self.0.captures_at(bytes, at).map(Captures)
    }

    /// Resolve replacement references against this compiled pattern.
    pub fn replacement(&self, template: &str) -> Replacement {
        Replacement::new(template, |name| {
            self.0.capture_names().position(|candidate| candidate == Some(name))
        })
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

/// Captures from one match; engine types stay inside the verifier.
 pub struct Captures<'t>(regex::bytes::Captures<'t>);

impl Captures<'_> {
    /// A participating group's bytes; group zero is the complete match.
    pub fn get(&self, index: usize) -> Option<&[u8]> {
        self.0.get(index).map(|capture| capture.as_bytes())
    }
}

/// Parsed rg replacement syntax. References are resolved once, while bytes
/// are expanded from each match's original haystack, including invalid UTF-8.
#[derive(Clone, Debug)]
pub struct Replacement {
    parts: Vec<ReplacementPart>,
}

#[derive(Clone, Debug)]
enum ReplacementPart {
    Literal(Vec<u8>),
    Capture(usize),
}

impl Replacement {
    fn new(template: &str, named: impl Fn(&str) -> Option<usize>) -> Self {
        let bytes = template.as_bytes();
        let mut parts = Vec::new();
        let mut literal = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            if bytes[at] != b'$' {
                literal.push(bytes[at]);
                at += 1;
                continue;
            }
            if bytes.get(at + 1) == Some(&b'$') {
                literal.push(b'$');
                at += 2;
                continue;
            }
            let braced = bytes.get(at + 1) == Some(&b'{');
            let start = at + if braced { 2 } else { 1 };
            let mut end = start;
            while bytes.get(end).is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_') {
                end += 1;
            }
            if end == start || (braced && bytes.get(end) != Some(&b'}')) {
                literal.push(b'$');
                at += 1;
                continue;
            }
            if !literal.is_empty() {
                parts.push(ReplacementPart::Literal(std::mem::take(&mut literal)));
            }
            // The reference consists entirely of ASCII bytes.
            let name = &template[start..end];
            let index = if name.bytes().all(|byte| byte.is_ascii_digit()) {
                name.parse().ok()
            } else {
                named(name)
            };
            if let Some(index) = index {
                parts.push(ReplacementPart::Capture(index));
            }
            at = end + usize::from(braced);
        }
        if !literal.is_empty() {
            parts.push(ReplacementPart::Literal(literal));
        }
        Self { parts }
    }

    /// Emit literal/captured byte pieces without accumulating expanded output.
    /// The caller controls allocation, cancellation and output-fault handling.
    pub fn expand<E>(
        &self,
        captures: &Captures<'_>,
        mut emit: impl FnMut(&[u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        for part in &self.parts {
            match part {
                ReplacementPart::Literal(bytes) => emit(bytes)?,
                ReplacementPart::Capture(index) => {
                    if let Some(bytes) = captures.get(*index) {
                        emit(bytes)?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Shared byte-span iteration. Ordinary output/count spans retain assertion
/// context; multiline search explicitly opts into rebasing resumed searches.
pub struct Spans<'r, 't> {
    regex: &'r Regex,
    bytes: &'t [u8],
    next: Option<usize>,
    previous_end: Option<usize>,
    rebased: bool,
}

impl Iterator for Spans<'_, '_> {
    type Item = std::ops::Range<usize>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let from = self.next?;
            if from > self.bytes.len() || (self.rebased && from == self.bytes.len()) {
                self.next = None;
                return None;
            }
            let found = if self.rebased {
                self.regex
                    .0
                    .find(&self.bytes[from..])
                    .map(|m| m.start() + from..m.end() + from)
            } else {
                self.regex.0.find_at(self.bytes, from).map(|m| m.range())
            };
            let Some(span) = found else {
                self.next = None;
                return None;
            };
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
    fn multiline_search_rebases_assertions_but_count_iteration_keeps_context() {
        let regex = Regex::new(r"\A", false).unwrap();
        assert_eq!(
            regex.rebased_spans(b"ab\n").collect::<Vec<_>>(),
            [0..0, 1..1, 2..2]
        );
        assert_eq!(
            regex.spans(b"ab\n").collect::<Vec<_>>(),
            std::iter::once(0..0).collect::<Vec<_>>()
        );
        let regex = Regex::new("a*", false).unwrap();
        assert_eq!(
            regex.rebased_spans(b"abc\n").collect::<Vec<_>>(),
            [0..1, 2..2, 3..3]
        );
        assert!(regex.rebased_spans(b"").next().is_none());
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
