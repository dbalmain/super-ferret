//! A content regex's trigram query: what every match must contain (S3 § Regex
//! to trigram query).
//!
//! [`GrepFlags`] is the one place a content regex's flags are set, so the
//! verifier's [`Regex`] and the derivation's parse cannot disagree.
//! [`GrepFlags::compile`] compiles the verifier first and derives second; the
//! result's [`Trigrams`] tree is necessary, never sufficient, and every
//! document the regex matches satisfies it.
//!
//! The derivation folds `regex-syntax`'s HIR into Cox's five facts over byte
//! strings, folded byte by byte by [`ferret_text::trigram_key`]'s rule, so
//! the keys mean exactly what the index's keys mean.
//!
//! Seams: `ferret-query` receives the finished [`Trigrams`] and knows nothing
//! about regex syntax; `ferret-text` owns the key function. `ferret-bench`
//! uses [`GrepFlags::required_literals`] to measure longer grams through
//! the same fold, while shipped trigrams retain their original limits.

use std::borrow::Cow;
use std::fmt;

use regex_syntax::hir::{Hir, HirKind};

use crate::matcher::{Regex, RegexError};

mod fold;
mod prefilter;
mod profile;
mod tree;
use fold::{Info, LIMITS, Limits, Trims, cap_keys, derive, finish, fold_node};
use prefilter::{Filter, filter, lf_excluded, line_local};
pub use profile::LineFlags;
pub use tree::{RequiredLiterals, Trigrams};

#[cfg(test)]
mod tests;

// ── flags ──

/// The one place a content regex's flags are set (D67). Both the verifier's
/// builder and the derivation's parser are built from one settings table, so
/// the two parses see the same HIR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrepFlags {
    /// Builder default true; `ferret search grep:` passes false (D67 A2).
    /// Inline pattern flags override this setting.
    pub case_insensitive: bool,
}

impl Default for GrepFlags {
    fn default() -> Self {
        GrepFlags {
            case_insensitive: true,
        }
    }
}

/// Every syntax setting, set explicitly on both builders. The values that
/// are not D67's choices are the `regex` crate's defaults, restated so a
/// change of default upstream cannot split the two parses.
struct Settings {
    unicode: bool,
    case_insensitive: bool,
    multi_line: bool,
    dot_matches_new_line: bool,
    crlf: bool,
    line_terminator: u8,
    swap_greed: bool,
    ignore_whitespace: bool,
    octal: bool,
    nest_limit: u32,
}

impl GrepFlags {
    fn settings(&self) -> Settings {
        Settings {
            unicode: true,
            case_insensitive: self.case_insensitive,
            multi_line: true,
            dot_matches_new_line: false,
            crlf: false,
            line_terminator: b'\n',
            swap_greed: false,
            ignore_whitespace: false,
            octal: false,
            nest_limit: 250,
        }
    }

    /// The derivation's parser. `utf8(false)` because `regex::bytes`
    /// translates that way: a pattern such as `(?-u:\xFF)` matches invalid
    /// UTF-8, and the default UTF-8-only translation would refuse it.
    fn parser(&self) -> regex_syntax::ParserBuilder {
        let s = self.settings();
        let mut parser = regex_syntax::ParserBuilder::new();
        parser
            .utf8(false)
            .unicode(s.unicode)
            .case_insensitive(s.case_insensitive)
            .multi_line(s.multi_line)
            .dot_matches_new_line(s.dot_matches_new_line)
            .crlf(s.crlf)
            .line_terminator(s.line_terminator)
            .swap_greed(s.swap_greed)
            .ignore_whitespace(s.ignore_whitespace)
            .octal(s.octal)
            .nest_limit(s.nest_limit);
        parser
    }

    /// The verifier's builder. Its size limit stays the `regex` default:
    /// compiling, not parsing, decides whether a pattern is executable.
    fn regex(&self, pattern: &str) -> regex::bytes::RegexBuilder {
        let s = self.settings();
        let mut builder = regex::bytes::RegexBuilder::new(pattern);
        builder
            .unicode(s.unicode)
            .case_insensitive(s.case_insensitive)
            .multi_line(s.multi_line)
            .dot_matches_new_line(s.dot_matches_new_line)
            .crlf(s.crlf)
            .line_terminator(s.line_terminator)
            .swap_greed(s.swap_greed)
            .ignore_whitespace(s.ignore_whitespace)
            .octal(s.octal)
            .nest_limit(s.nest_limit);
        builder
    }

    /// Compiles the verifier, then derives the trigram query.
    ///
    /// A pattern that does not compile — bad syntax, or a program over the
    /// size limit such as `a{100000000}` — is [`GrepError::Pattern`], and
    /// nothing is derived. A compiled pattern the derivation cannot parse is
    /// [`GrepError::Derivation`]: a bug, never a silent [`Trigrams::All`].
    pub fn compile(&self, pattern: &str) -> Result<Grep, GrepError> {
        let regex = Regex::build(&self.regex(pattern)).map_err(GrepError::Pattern)?;
        let limits = Limits {
            keys: usize::MAX,
            ..LIMITS
        };
        let planning = self.derive(pattern, &limits, &mut Trims::default())?;
        let trigrams = cap_keys(planning.clone(), LIMITS.keys, &mut Trims::default());
        let hir = self
            .parser()
            .build()
            .parse(pattern)
            .map_err(|e| GrepError::Derivation(e.to_string()))?;
        let line_local = line_local(&hir);
        let filter = if line_local { filter(&hir) } else { None };
        let literal = match hir.kind() {
            HirKind::Literal(literal) if line_local => Some(Box::new(
                memchr::memmem::Finder::new(&literal.0).into_owned(),
            )),
            _ => None,
        };
        Ok(Grep {
            regex,
            trigrams,
            planning,
            line_local,
            lf_excluded: lf_excluded(&hir),
            filter,
            literal,
        })
    }

    /// Compiles grep's line profile. Literal LF nodes are errors (classes
    /// containing LF are harmless because execution excludes terminators).
    /// The necessary query still describes the original indexed bytes when
    /// the executor strips an initial UTF-8 BOM.
    pub fn compile_lines(&self, pattern: &str) -> Result<Grep, GrepError> {
        let grep = self.compile(pattern)?;
        let hir = self
            .parser()
            .build()
            .parse(pattern)
            .map_err(|e| GrepError::Derivation(e.to_string()))?;
        fn literal_lf(hir: &Hir) -> bool {
            match hir.kind() {
                HirKind::Literal(literal) => literal.0.contains(&b'\n'),
                HirKind::Capture(capture) => literal_lf(&capture.sub),
                HirKind::Repetition(repetition) => literal_lf(&repetition.sub),
                HirKind::Concat(parts) | HirKind::Alternation(parts) => {
                    parts.iter().any(literal_lf)
                }
                _ => false,
            }
        }
        if pattern.contains('\n') || literal_lf(&hir) {
            return Err(GrepError::LineFeed);
        }
        Ok(grep)
    }

    /// Compile a line profile's OR patterns after resolving fixed-string mode.
    /// Pattern arguments remain intact; only the eventual pattern-file caller
    /// splits its input into lines. Scoped alternatives keep inline flags
    /// local.
    pub fn compile_line_patterns(
        &self,
        patterns: &[String],
        fixed: bool,
    ) -> Result<Grep, GrepError> {
        self.compile_line_profile(
            patterns,
            LineFlags {
                fixed,
                ..LineFlags::default()
            },
        )
    }

    /// Experimental required literals, using the same Cox fold and flags as
    /// the shipped trigram derivation. Byte strings are ASCII-folded; the
    /// AND/OR tree is necessary, never sufficient. Longer edge facts allow
    /// bench callers to extract grams without a second regex parser.
    pub fn required_literals(&self, pattern: &str) -> Result<RequiredLiterals, GrepError> {
        self.compile(pattern)?;
        let hir = self
            .parser()
            .build()
            .parse(pattern)
            .map_err(|e| GrepError::Derivation(e.to_string()))?;
        let limits = Limits {
            exact_len: 32,
            edge_len: 31,
            ..LIMITS
        };
        let info: Info<RequiredLiterals> = fold_node(&hir, &limits, &mut Trims::default());
        Ok(finish(info))
    }

    fn derive(
        &self,
        pattern: &str,
        limits: &Limits,
        trims: &mut Trims,
    ) -> Result<Trigrams, GrepError> {
        let hir = self
            .parser()
            .build()
            .parse(pattern)
            .map_err(|e| GrepError::Derivation(e.to_string()))?;
        Ok(derive(&hir, limits, trims))
    }
}

/// A compiled content regex and the trigram query its matches satisfy.
#[derive(Clone, Debug)]
pub struct Grep {
    regex: Regex,
    trigrams: Trigrams,
    planning: Trigrams,
    /// Every match lies within one line, and finding it needs nothing
    /// beyond that line ([`Grep::is_line_local`]).
    line_local: bool,
    lf_excluded: bool,
    /// A required run of the pattern, searched first to find where a match
    /// can be ([`Search`]); only for line-local patterns whose matches the
    /// engine cannot find by a literal prefix.
    filter: Option<Filter>,
    /// The whole pattern when it is one line-local literal, whose first
    /// occurrence is its first match, so finding it needs no engine call.
    literal: Option<Box<memchr::memmem::Finder<'static>>>,
}

impl crate::Matcher for Grep {
    fn is_match(&self, bytes: &[u8]) -> bool {
        crate::Matcher::is_match(&self.regex, bytes)
    }
}

impl Grep {
    /// First match's byte span in the whole document.
    pub fn find(&self, bytes: &[u8]) -> Option<std::ops::Range<usize>> {
        self.regex.find(bytes)
    }

    /// Whether the pattern can be searched a run of whole lines at a time.
    ///
    /// True when no match can contain a `\n` byte and the only assertions
    /// are `^`, `$` (multi-line, so at a `\n`) and word boundaries. Then a
    /// position matches in a document exactly when it matches in any run of
    /// whole lines around it that stops short of the next `\n`: every
    /// assertion sees a `\n` or an end of text on each side, and both read
    /// the same to it. So the first match of the document is the first
    /// match of the first such run that has one, searched in order. `\A`,
    /// `\z` and the CRLF forms of `^` and `$` (inline `R`) see a run's ends
    /// differently, and rule it out, as do classes such as `\s` and `[^x]`,
    /// which match `\n`.
    pub fn is_line_local(&self) -> bool {
        self.line_local
    }

    /// Conservative LF exclusion from the resolved HIR. Line/absolute
    /// assertions give no exclusion proof; word assertions do. Impossible
    /// alternatives are not discarded.
    /// This is a counting/execution profile, not language reachability.
    pub fn excludes_lf(&self) -> bool {
        self.lf_excluded
    }

    /// A copy of what finds the first match, for one thread's own use
    /// (its engine caches are then its own).
    pub fn search(&self) -> Search<'static> {
        Search {
            regex: Cow::Owned(self.regex.clone()),
            filter: self.filter.clone().map(Cow::Owned),
            literal: self.literal.as_deref().cloned().map(Cow::Owned),
        }
    }

    /// Reuse this grep's cache pool across inputs instead of creating cold
    /// caches for each file. The regex pool supplies independent concurrent
    /// searches with their own caches; owned searches remain available for
    /// long-lived worker-local use.
    pub fn borrowed_search(&self) -> Search<'_> {
        Search {
            regex: Cow::Borrowed(&self.regex),
            filter: self.filter.as_ref().map(Cow::Borrowed),
            literal: self.literal.as_deref().map(Cow::Borrowed),
        }
    }

    /// The verifier: the only judge of a match.
    pub fn regex(&self) -> &Regex {
        &self.regex
    }

    /// Original pattern, for query construction without exposing engine types.
    pub fn pattern(&self) -> &str {
        self.regex.pattern()
    }

    /// The uncapped derivation, so a planner can rank keys before weakening.
    pub fn planning_trigrams(&self) -> &Trigrams {
        &self.planning
    }

    /// Applies the 64-key cap using dictionary frequencies; common bare keys
    /// are discarded first. Alternatives are weakened, never deleted.
    pub fn trigrams_by_frequency(
        &self,
        frequencies: &std::collections::BTreeMap<[u8; 3], u64>,
    ) -> Trigrams {
        self.planning.fit_by_frequency(frequencies)
    }

    /// What every match contains.
    pub fn trigrams(&self) -> &Trigrams {
        &self.trigrams
    }
}

/// A [`Grep`]'s search for its first match.
///
/// Patterns that start with a large class, such as `(?i)[A-Z_]+worse`, give
/// the engine no literal prefix to look for, and it runs its automaton over
/// every byte: about 10 s of CPU on the bench corpus where `(?i)worse`
/// alone costs 1 s. When the pattern is line-local and has a required run
/// of literals or small classes, the search looks for that run first, and
/// runs the pattern only over the window around each occurrence, in order.
///
/// A window is the longest stretch around the occurrence of bytes a match
/// can be made of. For a pattern without assertions that is the bytes its
/// literals and classes can match (any byte above ASCII when a class has a
/// character there), so `(?i)[A-Z_]+worse` searches the word holding each
/// `worse`, not its line. With an assertion (`^`, `$`, `\b`), whose answer
/// depends on the bytes beside a match, the window is the line.
///
/// This finds the same first match as the pattern over the whole text.
/// Every match contains an occurrence of the run and lies within one
/// window, so a window without one has no match, and windows are searched
/// in order. Within a window the pattern finds what it finds in the text:
/// with no assertions a match depends only on its own bytes, all of them
/// in the window; with assertions the window is a line, and a line-local
/// pattern matches within a line as it does within the text around it
/// ([`Grep::is_line_local`]). The run is cut from the parsed pattern itself,
/// after case folding, so it accepts every case variant the pattern does
/// (`k` and the Kelvin sign, `s` and the long s).
///
/// Line selection additionally uses consuming-byte windows with original left
/// context and four right-context bytes. Its end-position bound prevents added
/// context, including an artificial EOF, from supplying a selected match.
#[derive(Clone, Debug)]
pub struct Search<'a> {
    regex: Cow<'a, Regex>,
    filter: Option<Cow<'a, Filter>>,
    literal: Option<Cow<'a, memchr::memmem::Finder<'static>>>,
}

impl Search<'_> {
    /// Uses the same regex and span iterator without the required-run windows.
    #[cfg(test)]
    fn without_windowing(&self) -> Search<'_> {
        Search {
            regex: Cow::Borrowed(&self.regex),
            filter: None,
            literal: None,
        }
    }

    /// Offset of the first match's start in `text`.
    pub fn find(&self, text: &[u8]) -> Option<usize> {
        if let Some(literal) = &self.literal {
            return literal.find(text);
        }
        let Some(filter) = &self.filter else {
            return self.regex.find(text).map(|span| span.start);
        };
        let mut at = 0;
        while at <= text.len() {
            let hit = filter.run.find(&text[at..])?;
            let hit = at + hit.start..at + hit.end;
            let (start, end) = if filter.assertions() {
                (
                    memchr::memrchr(b'\n', &text[at..hit.start]).map_or(at, |i| at + i + 1),
                    memchr::memchr(b'\n', &text[hit.end..]).map_or(text.len(), |i| hit.end + i),
                )
            } else {
                filter.window(text, at, hit)
            };
            if let Some(span) = self.regex.find(&text[start..end]) {
                return Some(start + span.start);
            }
            // `text[end]` is outside every window.
            at = end + 1;
        }
        None
    }

    /// A position on the first matching line, possibly at its LF or end.
    /// Requires [`Grep::is_line_local`]. Consuming-byte windows keep original
    /// left context and four right-context bytes for LF/Unicode word
    /// assertions. An occurrence ending in that added context cannot select
    /// the window. Rendering and counts still search the complete original
    /// line.
    pub fn matching_line(&self, text: &[u8]) -> Option<usize> {
        if let Some(literal) = &self.literal {
            return literal.find(text);
        }
        let Some(filter) = &self.filter else {
            return self.regex.shortest_match(text);
        };
        let mut at = 0;
        while at <= text.len() {
            let hit = filter.run.find(&text[at..])?;
            let (start, end) = filter.window(text, at, at + hit.start..at + hit.end);
            // The supported assertions inspect at most one UTF-8 scalar on
            // either side. Keep the left haystack intact and include enough
            // right context even when a scalar is invalid or four bytes wide.
            let context = end.saturating_add(4).min(text.len());
            if self
                .regex
                .shortest_match_at(&text[..context], start)
                .is_some_and(|found| found <= end)
            {
                return Some(start);
            }
            at = end + 1;
        }
        None
    }

    /// Every match's byte span, for line rendering and match counts: the
    /// spans [`Regex::spans`] gives over `text`.
    ///
    /// With a filter, each match is searched for only in the window around
    /// the next occurrence of the required run, with the left haystack intact
    /// and four bytes of right context, as [`Search::matching_line`] does.
    /// Searching from each match to the end of `text` instead is quadratic in
    /// a long line's matches, and with Unicode word boundaries over non-ASCII
    /// text the engine's slow fallback runs over all of that.
    pub fn spans<'r, 't>(&'r self, text: &'t [u8]) -> Spans<'r, 't> {
        if let Some(literal) = &self.literal {
            return Spans(Windows::Literal {
                literal,
                text,
                at: 0,
            });
        }
        match &self.filter {
            None => Spans(Windows::Whole(self.regex.spans(text))),
            Some(filter) => Spans(Windows::Filtered {
                regex: &self.regex,
                filter,
                text,
                at: 0,
                window_end: 0,
            }),
        }
    }

    /// Whether the search looks for a required run before the pattern.
    pub fn filters(&self) -> bool {
        self.filter.is_some()
    }

    /// Whether the required run occurs in `text`: a window is searched.
    #[cfg(test)]
    fn run_occurs(&self, text: &[u8]) -> bool {
        self.filter
            .as_ref()
            .is_some_and(|filter| filter.run.find(text).is_some())
    }
}

/// [`Search::spans`]'s iterator.
#[derive(Debug)]
pub struct Spans<'r, 't>(Windows<'r, 't>);

#[derive(Debug)]
enum Windows<'r, 't> {
    /// No filter: the engine searches the rest of the text for each match.
    Whole(crate::matcher::Spans<'r, 't>),
    /// The pattern is a literal: its occurrences, from `at`, never overlap.
    Literal {
        literal: &'r memchr::memmem::Finder<'static>,
        text: &'t [u8],
        at: usize,
    },
    /// The windows around the required run, in order, from `at`.
    Filtered {
        regex: &'r Regex,
        filter: &'r Filter,
        text: &'t [u8],
        at: usize,
        window_end: usize,
    },
}

impl Iterator for Spans<'_, '_> {
    type Item = std::ops::Range<usize>;

    fn next(&mut self) -> Option<Self::Item> {
        let (regex, filter, text, at, window_end) = match &mut self.0 {
            Windows::Whole(spans) => return spans.next(),
            Windows::Literal { literal, text, at } => {
                let start = *at + literal.find(text.get(*at..)?)?;
                *at = start + literal.needle().len();
                return Some(start..*at);
            }
            Windows::Filtered {
                regex,
                filter,
                text,
                at,
                window_end,
            } => (*regex, *filter, *text, at, window_end),
        };
        // Every match contains an occurrence of the run and lies within one
        // window, whose end byte no match contains; a match is never empty,
        // as the run is at least three long. So the next match is the first
        // one found in a window, searched in order.
        while *at <= text.len() {
            let Some(hit) = filter.run.find(&text[*at..]) else {
                *at = text.len() + 1;
                return None;
            };
            let hit = *at + hit.start..*at + hit.end;
            let (start, end) = if hit.end <= *window_end {
                (filter.start(text, *at, hit.start), *window_end)
            } else {
                let window = filter.window(text, *at, hit.clone());
                *window_end = window.1;
                window
            };
            let context = end.saturating_add(4).min(text.len());
            match regex.find_at(&text[..context], start) {
                // A span reaching into the added context may owe its match to
                // the artificial end of text.
                Some(span) if span.end <= end => {
                    *at = span.end;
                    return Some(span);
                }
                _ => *at = end + 1,
            }
        }
        None
    }
}

/// What every case-insensitive occurrence of `literal` contains, uncapped:
/// the derivation of the escaped literal under the default flags, so Unicode
/// case folding (`k` and the Kelvin sign) is the verifier's. `All` should
/// the derivation refuse, which only admits more documents.
pub(crate) fn literal_trigrams(literal: &str) -> Trigrams {
    let limits = Limits {
        keys: usize::MAX,
        ..LIMITS
    };
    GrepFlags::default()
        .derive(
            &regex_syntax::escape(literal),
            &limits,
            &mut Trims::default(),
        )
        .unwrap_or(Trigrams::All)
}

/// [`Trigrams::and`], for the crate's other derivations.
pub(crate) fn all_of(parts: impl IntoIterator<Item = Trigrams>) -> Trigrams {
    Trigrams::and(parts)
}

/// Why [`GrepFlags::compile`] failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrepError {
    /// Line mode cannot execute a literal LF.
    LineFeed,
    /// The verifier refused the pattern: the user's answer.
    Pattern(RegexError),
    /// The verifier compiled the pattern and the derivation's parse refused
    /// it. Compiling includes that parse, so this is a bug in the flags.
    Derivation(String),
}

impl fmt::Display for GrepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GrepError::LineFeed => f.write_str("literal newline requires multiline mode (M7)"),
            GrepError::Pattern(e) => e.fmt(f),
            GrepError::Derivation(e) => write!(
                f,
                "bug: the trigram derivation refused a compiled pattern: {e}"
            ),
        }
    }
}

impl std::error::Error for GrepError {}
