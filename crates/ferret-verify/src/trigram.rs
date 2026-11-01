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
use std::collections::BTreeSet;
use std::fmt;

use ferret_text::trigram_key;
use regex_syntax::hir::{Class, Hir, HirKind};

use crate::matcher::{Regex, RegexError};

mod fold;
mod profile;
use fold::{Info, LIMITS, Limits, Trims, cap_keys, derive, finish, fold_node, string_trigrams};
pub use profile::LineFlags;

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

/// [`Search`]'s required run and window.
#[derive(Clone, Debug)]
struct Filter {
    run: Regex,
    /// Every byte a consuming node can admit. Full-span searches keep a
    /// whole line when assertions are present; line selection adds context
    /// around this narrower window instead.
    within: Box<Window>,
}

#[derive(Clone, Debug)]
struct Window {
    bytes: [bool; 256],
    assertions: bool,
}

impl Filter {
    fn start(&self, text: &[u8], at: usize, hit_start: usize) -> usize {
        text[at..hit_start]
            .iter()
            .rposition(|&b| !self.within.bytes[usize::from(b)])
            .map_or(at, |i| at + i + 1)
    }

    fn window(&self, text: &[u8], at: usize, hit: std::ops::Range<usize>) -> (usize, usize) {
        #[cfg(test)]
        WINDOW_SCANNED.with(|scanned| scanned.set(scanned.get() + text.len() - hit.end));
        (
            text[at..hit.start]
                .iter()
                .rposition(|&b| !self.within.bytes[usize::from(b)])
                .map_or(at, |i| at + i + 1),
            text[hit.end..]
                .iter()
                .position(|&b| !self.within.bytes[usize::from(b)])
                .map_or(text.len(), |i| hit.end + i),
        )
    }
}

#[cfg(test)]
thread_local! {
    static WINDOW_SCANNED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_window_scanned() {
    WINDOW_SCANNED.with(|scanned| scanned.set(0));
}

#[cfg(test)]
fn window_scanned() -> usize {
    WINDOW_SCANNED.with(std::cell::Cell::get)
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
            let (start, end) = if filter.within.assertions {
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

/// [`Search`]'s filter for a line-local pattern; `None` when the engine
/// finds the pattern's matches by a literal prefix of its own (a finite
/// prefix set), or when there is no run of three or more.
fn filter(hir: &Hir) -> Option<Filter> {
    use regex_syntax::hir::literal::{ExtractKind, Extractor};
    if Extractor::new()
        .kind(ExtractKind::Prefix)
        .extract(hir)
        .is_finite()
    {
        return None;
    }
    let run = required_run(hir)?;
    // Printing a HIR gives a pattern that parses back to it; it is already
    // case folded, so it compiles with the builder's defaults.
    let run = Regex::build(&regex::bytes::RegexBuilder::new(&run.to_string())).ok()?;
    let mut within = Box::new(Window {
        bytes: [false; 256],
        assertions: false,
    });
    within.assertions = !match_bytes(hir, &mut within.bytes);
    within.bytes[usize::from(b'\n')] = false;
    Some(Filter { run, within })
}

/// Marks in `within` every byte a match of `hir` can contain; false when
/// `hir` has an assertion, whose answer depends on bytes beside the match.
fn match_bytes(hir: &Hir, within: &mut [bool; 256]) -> bool {
    let mut mark = |from: u32, to: u32| {
        for b in from..=to.min(0xFF) {
            within[b as usize] = true;
        }
    };
    match hir.kind() {
        HirKind::Empty => true,
        HirKind::Literal(literal) => {
            for &b in literal.0.iter() {
                within[usize::from(b)] = true;
            }
            true
        }
        // A character above ASCII is two or more bytes, each above it.
        HirKind::Class(Class::Unicode(class)) => {
            for range in class.ranges() {
                let (from, to) = (u32::from(range.start()), u32::from(range.end()));
                if from < 0x80 {
                    mark(from, to.min(0x7F));
                }
                if to >= 0x80 {
                    mark(0x80, 0xFF);
                }
            }
            true
        }
        HirKind::Class(Class::Bytes(class)) => {
            for range in class.ranges() {
                mark(u32::from(range.start()), u32::from(range.end()));
            }
            true
        }
        HirKind::Look(_) => false,
        HirKind::Repetition(repetition) => match_bytes(&repetition.sub, within),
        HirKind::Capture(capture) => match_bytes(&capture.sub, within),
        HirKind::Concat(hirs) | HirKind::Alternation(hirs) => {
            // Assertions require context, but every consuming node still
            // contributes to the selection window's byte set.
            let mut plain = true;
            for hir in hirs {
                if !match_bytes(hir, within) {
                    plain = false;
                }
            }
            plain
        }
    }
}

/// The longest run of consecutive literals and small classes that every
/// match of `hir` contains: at least three long (bytes of a literal, one per
/// class), and short of the whole pattern.
fn required_run(hir: &Hir) -> Option<Hir> {
    Some(run_in(hir)?.1)
}

/// [`required_run`], with its length.
fn run_in(hir: &Hir) -> Option<(usize, Hir)> {
    match hir.kind() {
        HirKind::Capture(capture) => run_in(&capture.sub),
        HirKind::Repetition(repetition) if repetition.min > 0 => run_in(&repetition.sub),
        HirKind::Concat(hirs) => {
            let (mut best, mut length, mut start, mut sum) = (0..0, 0, 0, 0);
            for (i, child) in hirs.iter().enumerate() {
                match literal_length(child) {
                    None => (start, sum) = (i + 1, 0),
                    Some(n) => {
                        sum += n;
                        if sum > length {
                            (best, length) = (start..i + 1, sum);
                        }
                    }
                }
            }
            let own = (length >= 3 && best.len() < hirs.len())
                .then(|| (length, Hir::concat(hirs[best].to_vec())));
            // A run inside a child is required too.
            own.into_iter()
                .chain(hirs.iter().filter_map(run_in))
                .max_by_key(|(length, _)| *length)
        }
        _ => None,
    }
}

/// How many bytes a literal matches, or 1 for a class of
/// at most four characters (the case variants of one); `None` otherwise.
fn literal_length(hir: &Hir) -> Option<usize> {
    let small = |sizes: &mut dyn Iterator<Item = u32>| (sizes.sum::<u32>() <= 4).then_some(1);
    match hir.kind() {
        HirKind::Literal(literal) => Some(literal.0.len()),
        HirKind::Class(Class::Unicode(class)) => small(
            &mut class
                .ranges()
                .iter()
                .map(|r| u32::from(r.end()) - u32::from(r.start()) + 1),
        ),
        HirKind::Class(Class::Bytes(class)) => small(
            &mut class
                .ranges()
                .iter()
                .map(|r| u32::from(r.end()) - u32::from(r.start()) + 1),
        ),
        HirKind::Capture(capture) => literal_length(&capture.sub),
        _ => None,
    }
}

// Line/absolute anchors supply no proof excluding LF. Word assertions do;
// every consuming node and every alternative must conservatively exclude LF.
fn lf_excluded(hir: &Hir) -> bool {
    use regex_syntax::hir::Look;
    match hir.kind() {
        HirKind::Look(look) => !matches!(
            look,
            Look::Start | Look::End | Look::StartLF | Look::EndLF | Look::StartCRLF | Look::EndCRLF
        ),
        HirKind::Capture(capture) => lf_excluded(&capture.sub),
        HirKind::Repetition(repetition) => lf_excluded(&repetition.sub),
        HirKind::Concat(parts) | HirKind::Alternation(parts) => parts.iter().all(lf_excluded),
        _ => line_local(hir),
    }
}

/// [`Grep::is_line_local`] of a parsed pattern.
fn line_local(hir: &Hir) -> bool {
    use regex_syntax::hir::Look;
    match hir.kind() {
        HirKind::Empty => true,
        HirKind::Literal(literal) => !literal.0.contains(&b'\n'),
        HirKind::Class(Class::Unicode(class)) => !class
            .ranges()
            .iter()
            .any(|range| range.start() <= '\n' && '\n' <= range.end()),
        HirKind::Class(Class::Bytes(class)) => !class
            .ranges()
            .iter()
            .any(|range| range.start() <= b'\n' && b'\n' <= range.end()),
        HirKind::Look(look) => !matches!(
            look,
            Look::Start | Look::End | Look::StartCRLF | Look::EndCRLF
        ),
        HirKind::Repetition(repetition) => line_local(&repetition.sub),
        HirKind::Capture(capture) => line_local(&capture.sub),
        HirKind::Concat(hirs) | HirKind::Alternation(hirs) => hirs.iter().all(line_local),
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

// ── the query tree ──

/// What every match of a regex contains: necessary, never sufficient.
///
/// The derivation builds it through constructors that flatten, sort,
/// deduplicate and apply the identities, so in a derived tree `And` and `Or`
/// hold at least two distinct children, never `All` or `None`, and the root
/// holds at most 64 distinct keys.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Trigrams {
    /// No constraint: every document is a candidate.
    All,
    /// No document can match.
    None,
    /// Folded by [`ferret_text::trigram_key`]'s rule.
    Key([u8; 3]),
    And(Vec<Trigrams>),
    Or(Vec<Trigrams>),
}

impl Trigrams {
    fn and(parts: impl IntoIterator<Item = Trigrams>) -> Trigrams {
        let mut flat = Vec::new();
        for part in parts {
            match part {
                Trigrams::All => {}
                Trigrams::None => return Trigrams::None,
                Trigrams::And(inner) => flat.extend(inner),
                other => flat.push(other),
            }
        }
        Trigrams::collect(flat, Trigrams::All, Trigrams::And)
    }

    fn or(parts: impl IntoIterator<Item = Trigrams>) -> Trigrams {
        let mut flat = Vec::new();
        for part in parts {
            match part {
                Trigrams::None => {}
                Trigrams::All => return Trigrams::All,
                Trigrams::Or(inner) => flat.extend(inner),
                other => flat.push(other),
            }
        }
        Trigrams::collect(flat, Trigrams::None, Trigrams::Or)
    }

    fn collect(
        mut flat: Vec<Trigrams>,
        identity: Trigrams,
        node: fn(Vec<Trigrams>) -> Trigrams,
    ) -> Trigrams {
        flat.sort();
        flat.dedup();
        match flat.len() {
            0 => identity,
            1 => flat.pop().unwrap_or(identity),
            _ => node(flat),
        }
    }

    /// The tree within the 64-key cap, using dictionary frequencies: common
    /// bare keys are discarded first. Alternatives are weakened, never
    /// deleted. A tree within the cap is returned unchanged.
    pub fn fit_by_frequency(
        &self,
        frequencies: &std::collections::BTreeMap<[u8; 3], u64>,
    ) -> Trigrams {
        if self.keys().len() <= LIMITS.keys {
            return self.clone();
        }
        self.clone().fit_by(LIMITS.keys, &|key| {
            frequencies.get(&key).copied().unwrap_or(u64::MAX)
        })
    }

    /// Every distinct key in the tree.
    pub fn keys(&self) -> BTreeSet<[u8; 3]> {
        let mut keys = BTreeSet::new();
        self.collect_keys(&mut keys);
        keys
    }

    fn collect_keys(&self, keys: &mut BTreeSet<[u8; 3]>) {
        match self {
            Trigrams::All | Trigrams::None => {}
            Trigrams::Key(key) => {
                keys.insert(*key);
            }
            Trigrams::And(parts) | Trigrams::Or(parts) => {
                parts.iter().for_each(|p| p.collect_keys(keys));
            }
        }
    }

    /// The tree weakened to at most `budget` keys (counted with
    /// repeats, so distinct keys are fewer still) by replacing subtrees with
    /// `All`, which only admits more documents. An `And` keeps what fits:
    /// its bare keys first, the largest first (M5 orders by df instead),
    /// then its other children, smallest first, each fitted to what is left.
    /// An `Or` must keep every alternative, so each is fitted to an equal
    /// share; a share of zero makes that alternative, and so the `Or`, `All`.
    /// An alternative is never deleted.
    fn fit(self, budget: usize) -> Trigrams {
        self.fit_by(budget, &|_| 0)
    }

    fn fit_by(self, budget: usize, frequency: &dyn Fn([u8; 3]) -> u64) -> Trigrams {
        match self {
            Trigrams::Key(_) if budget == 0 => Trigrams::All,
            Trigrams::And(mut parts) => {
                parts.sort_by_cached_key(|p| match p {
                    Trigrams::Key(key) => (0, frequency(*key), std::cmp::Reverse(*key), 0),
                    other => (1, 0, std::cmp::Reverse([0; 3]), other.weight()),
                });
                let mut left = budget;
                let mut kept = Vec::new();
                for part in parts {
                    let part = part.fit_by(left, frequency);
                    left -= part.weight();
                    kept.push(part);
                }
                Trigrams::and(kept)
            }
            Trigrams::Or(parts) => {
                let share = budget / parts.len();
                Trigrams::or(parts.into_iter().map(|p| p.fit_by(share, frequency)))
            }
            other if other.weight() <= budget => other,
            _ => Trigrams::All,
        }
    }

    /// Keys counted with repeats: an upper bound on distinct keys.
    fn weight(&self) -> usize {
        match self {
            Trigrams::All | Trigrams::None => 0,
            Trigrams::Key(_) => 1,
            Trigrams::And(parts) | Trigrams::Or(parts) => parts.iter().map(Trigrams::weight).sum(),
        }
    }
}

/// Folded byte strings that every regex match must satisfy. An alternative
/// with no useful literal remains `All`; no branch may be dropped.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RequiredLiterals {
    All,
    None,
    Literal(Vec<u8>),
    And(Vec<RequiredLiterals>),
    Or(Vec<RequiredLiterals>),
}

/// The fold saves strings at exactly the same points for both consumers.
/// Trigrams retain their shipped simplification and key budget.
trait Facts: Clone {
    fn all() -> Self;
    fn and(parts: impl IntoIterator<Item = Self>) -> Self;
    fn or(parts: impl IntoIterator<Item = Self>) -> Self;
    fn string(bytes: &[u8]) -> Self;
}

impl Facts for Trigrams {
    fn all() -> Self {
        Self::All
    }
    fn and(parts: impl IntoIterator<Item = Self>) -> Self {
        Self::and(parts)
    }
    fn or(parts: impl IntoIterator<Item = Self>) -> Self {
        Self::or(parts)
    }
    fn string(bytes: &[u8]) -> Self {
        string_trigrams(bytes)
    }
}

impl Facts for RequiredLiterals {
    fn all() -> Self {
        Self::All
    }
    fn and(parts: impl IntoIterator<Item = Self>) -> Self {
        let mut flat = Vec::new();
        for part in parts {
            match part {
                Self::All => {}
                Self::None => return Self::None,
                Self::And(inner) => flat.extend(inner),
                other => flat.push(other),
            }
        }
        Self::collect(flat, Self::All, Self::And)
    }
    fn or(parts: impl IntoIterator<Item = Self>) -> Self {
        let mut flat = Vec::new();
        for part in parts {
            match part {
                Self::None => {}
                Self::All => return Self::All,
                Self::Or(inner) => flat.extend(inner),
                other => flat.push(other),
            }
        }
        Self::collect(flat, Self::None, Self::Or)
    }
    fn string(bytes: &[u8]) -> Self {
        if bytes.len() < 3 {
            Self::All
        } else {
            Self::Literal(bytes.to_vec())
        }
    }
}

impl RequiredLiterals {
    fn collect(mut flat: Vec<Self>, identity: Self, node: fn(Vec<Self>) -> Self) -> Self {
        flat.sort();
        flat.dedup();
        match flat.len() {
            0 => identity,
            1 => flat.pop().unwrap_or(identity),
            _ => node(flat),
        }
    }
}
