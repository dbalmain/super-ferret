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

use std::collections::BTreeSet;
use std::fmt;

use ferret_text::trigram_key;
use regex_syntax::hir::{Class, Hir, HirKind};

use crate::matcher::{Regex, RegexError};

mod profile;
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
        Ok(Grep {
            regex,
            trigrams,
            planning,
            line_local,
            lf_excluded: lf_excluded(&hir),
            filter,
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
    pub fn search(&self) -> Search {
        Search {
            regex: self.regex.clone(),
            filter: self.filter.clone(),
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
#[derive(Clone, Debug)]
pub struct Search {
    regex: Regex,
    filter: Option<Filter>,
}

/// [`Search`]'s required run and window.
#[derive(Clone, Debug)]
struct Filter {
    run: Regex,
    /// The bytes a match can be made of, by byte value; `None` for a
    /// pattern with assertions, whose window is the line.
    within: Option<Box<[bool; 256]>>,
}

impl Search {
    /// Offset of the first match's start in `text`.
    pub fn find(&self, text: &[u8]) -> Option<usize> {
        let Some(filter) = &self.filter else {
            return self.regex.find(text).map(|span| span.start);
        };
        let mut at = 0;
        while at <= text.len() {
            let hit = filter.run.find(&text[at..])?;
            let (hit, hit_end) = (at + hit.start, at + hit.end);
            let (start, end) = match &filter.within {
                None => (
                    memchr::memrchr(b'\n', &text[at..hit]).map_or(at, |i| at + i + 1),
                    memchr::memchr(b'\n', &text[hit_end..]).map_or(text.len(), |i| hit_end + i),
                ),
                Some(within) => (
                    text[at..hit]
                        .iter()
                        .rposition(|&b| !within[usize::from(b)])
                        .map_or(at, |i| at + i + 1),
                    text[hit_end..]
                        .iter()
                        .position(|&b| !within[usize::from(b)])
                        .map_or(text.len(), |i| hit_end + i),
                ),
            };
            if let Some(span) = self.regex.find(&text[start..end]) {
                return Some(start + span.start);
            }
            // `text[end]` is outside every window.
            at = end + 1;
        }
        None
    }

    /// Every match's byte span, for line rendering and match counts. The
    /// first-match screen does not narrow the haystack or assertion context.
    pub fn spans<'r, 't>(&'r self, text: &'t [u8]) -> crate::matcher::Spans<'r, 't> {
        self.regex.spans(text)
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
    let mut within = Box::new([false; 256]);
    let within = match_bytes(hir, &mut within).then(|| {
        within[usize::from(b'\n')] = false;
        within
    });
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
            hirs.iter().all(|hir| match_bytes(hir, within))
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

// ── limits ──

/// The simplification's caps (S3 § Simplification). `exact` and `set` are
/// Cox's `maxExact` and `maxSet`; `class` and `keys` are starting points M3
/// measures.
#[derive(Clone, Copy, Debug)]
struct Limits {
    /// A class with more members than this, after folding, is unknown.
    class: usize,
    /// An exact set with more strings than this is dumped into `match`.
    exact: usize,
    /// An exact set whose shortest string is longer than this is dumped.
    exact_len: usize,
    /// A prefix or suffix set is shortened until it holds at most this many.
    set: usize,
    /// A finished query is weakened until it holds at most this many keys.
    keys: usize,
    /// Bytes retained at each open concatenation edge.
    edge_len: usize,
}

const LIMITS: Limits = Limits {
    class: 4,
    exact: 7,
    exact_len: 4,
    set: 20,
    keys: 64,
    edge_len: 2,
};

/// How often each cap discarded information: the measurement M3 reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Trims {
    class: u32,
    exact: u32,
    set: u32,
    /// Keys the 64-key cap replaced with `All`.
    keys: u32,
}

// ── the fold ──

type Strings = BTreeSet<Vec<u8>>;

/// Cox's five facts for one HIR node. Invariant: every match starts with a
/// member of `prefix` and ends with a member of `suffix`, so either holds
/// `""` whenever nothing better is known.
#[derive(Clone, Debug)]
struct Info<Q = Trigrams> {
    emptyable: bool,
    /// The finite set of folded byte strings the node matches, or unknown.
    /// While known, its string facts are not yet in `matches`.
    exact: Option<Strings>,
    prefix: Strings,
    suffix: Strings,
    matches: Q,
}

/// The one folding rule: a byte folds as it does inside a key.
fn fold(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().map(|&b| trigram_key([b; 3])[0]).collect()
}

/// `AND` of every key in `s`; `All` for a string under three bytes, which
/// names no trigram.
fn string_trigrams(s: &[u8]) -> Trigrams {
    Trigrams::and(
        s.windows(3)
            .map(|w| Trigrams::Key(trigram_key([w[0], w[1], w[2]]))),
    )
}

/// `OR` over the set: any short member poisons it to `All`; the empty set is
/// `None`.
fn set_facts<Q: Facts>(set: &Strings) -> Q {
    Q::or(set.iter().map(|s| Q::string(s)))
}

fn single<Q: Facts>(set: Strings) -> Info<Q> {
    let emptyable = set.contains(&[][..]);
    Info {
        emptyable,
        exact: Some(set),
        prefix: Strings::new(),
        suffix: Strings::new(),
        matches: Q::all(),
    }
}

fn empty<Q: Facts>() -> Info<Q> {
    single(Strings::from([Vec::new()]))
}

/// A node about which nothing is known but whether it can be empty.
fn unknown<Q: Facts>(emptyable: bool) -> Info<Q> {
    Info {
        emptyable,
        exact: None,
        prefix: Strings::from([Vec::new()]),
        suffix: Strings::from([Vec::new()]),
        matches: Q::all(),
    }
}

fn product(left: &Strings, right: &Strings) -> Strings {
    left.iter()
        .flat_map(|l| right.iter().map(move |r| [&l[..], &r[..]].concat()))
        .collect()
}

impl<Q: Facts> Info<Q> {
    /// Makes `exact` unknown, saving its string facts into `matches` first, and
    /// its strings become the prefix and suffix sets.
    fn release(mut self) -> Info<Q> {
        if let Some(exact) = self.exact.take() {
            self.matches = Q::and([self.matches, set_facts(&exact)]);
            self.prefix = exact.clone();
            self.suffix = exact;
        }
        self
    }

    /// The members every match starts with: the exact set while known.
    fn starts(&self) -> &Strings {
        self.exact.as_ref().unwrap_or(&self.prefix)
    }

    fn ends(&self) -> &Strings {
        self.exact.as_ref().unwrap_or(&self.suffix)
    }
}

fn finish<Q: Facts>(info: Info<Q>) -> Q {
    match &info.exact {
        Some(exact) => Q::and([info.matches.clone(), set_facts(exact)]),
        None => info.matches,
    }
}

fn derive(hir: &Hir, limits: &Limits, trims: &mut Trims) -> Trigrams {
    cap_keys(finish(fold_node(hir, limits, trims)), limits.keys, trims)
}

fn fold_node<Q: Facts>(hir: &Hir, limits: &Limits, trims: &mut Trims) -> Info<Q> {
    let info = match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => empty(),
        HirKind::Literal(literal) => single(Strings::from([fold(&literal.0)])),
        HirKind::Class(class) => fold_class(class, limits, trims),
        HirKind::Capture(capture) => fold_node(&capture.sub, limits, trims),
        HirKind::Repetition(rep) => match (rep.min, rep.max) {
            // `e?` is `e|""` by the alternation rule; the `*` shortcut here
            // would cost the whole index (Cox; the course's `ab?ce`).
            (0, Some(1)) => {
                let sub = fold_node(&rep.sub, limits, trims);
                alternate(sub, empty(), limits, trims)
            }
            (0, _) => unknown(true),
            (min, max) => {
                let sub = fold_node(&rep.sub, limits, trims);
                repeat(sub, min, max, limits, trims)
            }
        },
        HirKind::Concat(parts) => {
            // Along a concatenation `matches` only gains conjuncts, so they
            // are joined once at the end: joining at every step is quadratic
            // in the pattern's length.
            let mut info = empty();
            let mut conjuncts = Vec::new();
            for part in parts {
                conjuncts.push(std::mem::replace(&mut info.matches, Q::all()));
                let next = fold_node(part, limits, trims);
                info = concat(info, next, limits, trims);
            }
            conjuncts.push(info.matches);
            info.matches = Q::and(conjuncts);
            info
        }
        HirKind::Alternation(parts) => {
            // The empty alternation matches nothing.
            let mut info = single(Strings::new());
            for part in parts {
                let next = fold_node(part, limits, trims);
                info = alternate(info, next, limits, trims);
            }
            info
        }
    };
    simplify(info, limits, trims)
}

/// A class consumes one unit: a raw byte for `Class::Bytes`, one scalar's
/// UTF-8 encoding for `Class::Unicode`. Folding merges at most an ASCII
/// letter pair, so more than twice the cap of raw members cannot fit.
fn fold_class<Q: Facts>(class: &Class, limits: &Limits, trims: &mut Trims) -> Info<Q> {
    let members: Option<Strings> = match class {
        Class::Bytes(set) => {
            let count: usize = set
                .ranges()
                .iter()
                .map(|r| usize::from(r.end() - r.start()) + 1)
                .sum();
            (count <= 2 * limits.class).then(|| {
                set.ranges()
                    .iter()
                    .flat_map(|r| r.start()..=r.end())
                    .map(|b| fold(&[b]))
                    .collect()
            })
        }
        Class::Unicode(set) => {
            let count: usize = set
                .ranges()
                .iter()
                .map(|r| (u32::from(r.end()) - u32::from(r.start())) as usize + 1)
                .sum();
            (count <= 2 * limits.class).then(|| {
                set.ranges()
                    .iter()
                    .flat_map(|r| r.start()..=r.end())
                    .map(|c| fold(c.encode_utf8(&mut [0; 4]).as_bytes()))
                    .collect()
            })
        }
    };
    match members {
        Some(members) if members.len() <= limits.class => single(members),
        _ => {
            trims.class += 1;
            unknown(false)
        }
    }
}

fn concat<Q: Facts>(left: Info<Q>, right: Info<Q>, limits: &Limits, trims: &mut Trims) -> Info<Q> {
    let emptyable = left.emptyable && right.emptyable;
    if let (Some(l), Some(r)) = (&left.exact, &right.exact) {
        let exact = product(l, r);
        return simplify(
            Info {
                emptyable,
                exact: Some(exact),
                prefix: Strings::new(),
                suffix: Strings::new(),
                matches: Q::and([left.matches, right.matches]),
            },
            limits,
            trims,
        );
    }
    // Each consumer retains enough edge bytes for its longest useful gram.
    // These strings are actual adjacent boundaries, never nonadjacent facts.
    let tails: Strings = left
        .ends()
        .iter()
        .map(|s| last_edge(s, limits.edge_len).to_vec())
        .collect();
    let heads: Strings = right
        .starts()
        .iter()
        .map(|s| first_edge(s, limits.edge_len).to_vec())
        .collect();
    let boundary = set_facts(&product(&tails, &heads));
    let prefix = match &left.exact {
        Some(exact) => product(exact, &right.prefix),
        None => left.prefix.clone(),
    };
    let suffix = match &right.exact {
        Some(exact) => product(&left.suffix, exact),
        None => right.suffix.clone(),
    };
    let (left, right) = (left.release(), right.release());
    simplify(
        Info {
            emptyable,
            exact: None,
            prefix,
            suffix,
            matches: Q::and([left.matches, right.matches, boundary]),
        },
        limits,
        trims,
    )
}

fn alternate<Q: Facts>(
    left: Info<Q>,
    right: Info<Q>,
    limits: &Limits,
    trims: &mut Trims,
) -> Info<Q> {
    let emptyable = left.emptyable || right.emptyable;
    let info = match (left.exact, right.exact) {
        (Some(l), Some(r)) => Info {
            emptyable,
            exact: Some(l.union(&r).cloned().collect()),
            prefix: Strings::new(),
            suffix: Strings::new(),
            matches: Q::or([left.matches, right.matches]),
        },
        (l, r) => {
            let left = Info { exact: l, ..left }.release();
            let right = Info { exact: r, ..right }.release();
            Info {
                emptyable,
                exact: None,
                prefix: left.prefix.union(&right.prefix).cloned().collect(),
                suffix: left.suffix.union(&right.suffix).cloned().collect(),
                matches: Q::or([left.matches, right.matches]),
            }
        }
    };
    simplify(info, limits, trims)
}

/// Copies past this add no key: once the exact set is unknown, a further
/// copy repeats the same `matches` and the same boundary trigrams.
const UNROLL: u32 = 8;

/// `sub{min,max}` with `min ≥ 1`: every match is `min` or more copies, so it
/// starts and ends with `sub` repeated `min` times. Unrolled as a
/// concatenation while that is exact; otherwise released.
fn repeat<Q: Facts>(
    sub: Info<Q>,
    min: u32,
    max: Option<u32>,
    limits: &Limits,
    trims: &mut Trims,
) -> Info<Q> {
    let copies = min.min(UNROLL);
    let mut info = sub.clone();
    for _ in 1..copies {
        info = concat(info, sub.clone(), limits, trims);
    }
    if max == Some(min) && copies == min {
        info
    } else {
        info.release()
    }
}

fn first_edge(s: &[u8], length: usize) -> &[u8] {
    &s[..s.len().min(length)]
}

fn last_edge(s: &[u8], length: usize) -> &[u8] {
    &s[s.len().saturating_sub(length)..]
}

/// Cox's order: save before you discard. An exact set over the caps has its
/// trigrams saved into `matches` before it is released.
///
/// Once `exact` is unknown, every prefix and suffix string's facts are
/// already in `matches`: each string came from a released exact set, or from
/// a concatenation whose boundary facts were saved. The shipped trigram fold
/// retains two bytes at each open edge; longer grams retain `edge_len` bytes
/// so later concatenations can recover their boundary facts. Shortening a set
/// to the cap then discards boundary keys only, which no saving could keep.
fn simplify<Q: Facts>(mut info: Info<Q>, limits: &Limits, trims: &mut Trims) -> Info<Q> {
    if let Some(exact) = &info.exact {
        let shortest = exact.iter().map(Vec::len).min().unwrap_or(0);
        if exact.len() > limits.exact || shortest > limits.exact_len {
            trims.exact += 1;
            info = info.release();
        }
    }
    if info.exact.is_none() {
        info.prefix = info
            .prefix
            .iter()
            .map(|s| first_edge(s, limits.edge_len).to_vec())
            .collect();
        info.suffix = info
            .suffix
            .iter()
            .map(|s| last_edge(s, limits.edge_len).to_vec())
            .collect();
        if info.prefix.len() > limits.set {
            trims.set += 1;
            info.prefix = shorten(
                &info.prefix,
                limits.set,
                |s| &s[..s.len() - 1],
                <[u8]>::starts_with,
            );
        }
        if info.suffix.len() > limits.set {
            trims.set += 1;
            info.suffix = shorten(&info.suffix, limits.set, |s| &s[1..], <[u8]>::ends_with);
        }
    }
    info
}

/// Cuts one unit off the longest strings until the set fits, dropping any
/// string another member already covers (`covers(s, shorter)`).
fn shorten(
    set: &Strings,
    max: usize,
    cut: fn(&[u8]) -> &[u8],
    covers: fn(&[u8], &[u8]) -> bool,
) -> Strings {
    let mut set = set.clone();
    while set.len() > max {
        let longest = set.iter().map(Vec::len).max().unwrap_or(0);
        let cut_set: Strings = set
            .iter()
            .map(|s| {
                if s.len() == longest {
                    cut(s).to_vec()
                } else {
                    s.clone()
                }
            })
            .collect();
        set = cut_set
            .iter()
            .filter(|s| !cut_set.iter().any(|t| t.len() < s.len() && covers(s, t)))
            .cloned()
            .collect();
    }
    set
}

/// Weakens `query` until it holds at most `max` distinct keys.
fn cap_keys(query: Trigrams, max: usize, trims: &mut Trims) -> Trigrams {
    let before = query.keys().len();
    if before <= max {
        return query;
    }
    let query = query.fit(max);
    trims.keys += u32::try_from(before - query.keys().len()).unwrap_or(u32::MAX);
    query
}
