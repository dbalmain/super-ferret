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
//! about regex syntax; `ferret-text` owns the key function.

use std::collections::BTreeSet;
use std::fmt;

use ferret_text::trigram_key;
use regex_syntax::hir::{Class, Hir, HirKind};

use crate::matcher::{Regex, RegexError};

#[cfg(test)]
mod tests;

// ── flags ──

/// The one place a content regex's flags are set (D67). Both the verifier's
/// builder and the derivation's parser are built from one settings table, so
/// the two parses see the same HIR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrepFlags {
    /// Default true; false under `case:`.
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
        Ok(Grep {
            regex,
            trigrams,
            planning,
        })
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
    /// The verifier refused the pattern: the user's answer.
    Pattern(RegexError),
    /// The verifier compiled the pattern and the derivation's parse refused
    /// it. Compiling includes that parse, so this is a bug in the flags.
    Derivation(String),
}

impl fmt::Display for GrepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
}

const LIMITS: Limits = Limits {
    class: 4,
    exact: 7,
    exact_len: 4,
    set: 20,
    keys: 64,
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
struct Info {
    emptyable: bool,
    /// The finite set of folded byte strings the node matches, or unknown.
    /// While known, its trigrams are not yet in `matches`.
    exact: Option<Strings>,
    prefix: Strings,
    suffix: Strings,
    matches: Trigrams,
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
fn set_trigrams(set: &Strings) -> Trigrams {
    Trigrams::or(set.iter().map(|s| string_trigrams(s)))
}

fn single(set: Strings) -> Info {
    let emptyable = set.contains(&[][..]);
    Info {
        emptyable,
        exact: Some(set),
        prefix: Strings::new(),
        suffix: Strings::new(),
        matches: Trigrams::All,
    }
}

fn empty() -> Info {
    single(Strings::from([Vec::new()]))
}

/// A node about which nothing is known but whether it can be empty.
fn unknown(emptyable: bool) -> Info {
    Info {
        emptyable,
        exact: None,
        prefix: Strings::from([Vec::new()]),
        suffix: Strings::from([Vec::new()]),
        matches: Trigrams::All,
    }
}

fn product(left: &Strings, right: &Strings) -> Strings {
    left.iter()
        .flat_map(|l| right.iter().map(move |r| [&l[..], &r[..]].concat()))
        .collect()
}

impl Info {
    /// Makes `exact` unknown, saving its trigrams into `matches` first, and
    /// its strings become the prefix and suffix sets.
    fn release(mut self) -> Info {
        if let Some(exact) = self.exact.take() {
            self.matches = Trigrams::and([self.matches, set_trigrams(&exact)]);
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

fn derive(hir: &Hir, limits: &Limits, trims: &mut Trims) -> Trigrams {
    let info = fold_node(hir, limits, trims);
    let query = match &info.exact {
        Some(exact) => Trigrams::and([info.matches.clone(), set_trigrams(exact)]),
        None => info.matches,
    };
    cap_keys(query, limits.keys, trims)
}

fn fold_node(hir: &Hir, limits: &Limits, trims: &mut Trims) -> Info {
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
                conjuncts.push(std::mem::replace(&mut info.matches, Trigrams::All));
                let next = fold_node(part, limits, trims);
                info = concat(info, next, limits, trims);
            }
            conjuncts.push(info.matches);
            info.matches = Trigrams::and(conjuncts);
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
fn fold_class(class: &Class, limits: &Limits, trims: &mut Trims) -> Info {
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

fn concat(left: Info, right: Info, limits: &Limits, trims: &mut Trims) -> Info {
    let emptyable = left.emptyable && right.emptyable;
    if let (Some(l), Some(r)) = (&left.exact, &right.exact) {
        let exact = product(l, r);
        return simplify(
            Info {
                emptyable,
                exact: Some(exact),
                prefix: Strings::new(),
                suffix: Strings::new(),
                matches: Trigrams::and([left.matches, right.matches]),
            },
            limits,
            trims,
        );
    }
    // Only trigrams spanning the boundary are new: two bytes from each side
    // are enough to form them, and each side's own trigrams are already in
    // its `matches` once released.
    let tails: Strings = left.ends().iter().map(|s| last_two(s).to_vec()).collect();
    let heads: Strings = right
        .starts()
        .iter()
        .map(|s| first_two(s).to_vec())
        .collect();
    let boundary = set_trigrams(&product(&tails, &heads));
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
            matches: Trigrams::and([left.matches, right.matches, boundary]),
        },
        limits,
        trims,
    )
}

fn alternate(left: Info, right: Info, limits: &Limits, trims: &mut Trims) -> Info {
    let emptyable = left.emptyable || right.emptyable;
    let info = match (left.exact, right.exact) {
        (Some(l), Some(r)) => Info {
            emptyable,
            exact: Some(l.union(&r).cloned().collect()),
            prefix: Strings::new(),
            suffix: Strings::new(),
            matches: Trigrams::or([left.matches, right.matches]),
        },
        (l, r) => {
            let left = Info { exact: l, ..left }.release();
            let right = Info { exact: r, ..right }.release();
            Info {
                emptyable,
                exact: None,
                prefix: left.prefix.union(&right.prefix).cloned().collect(),
                suffix: left.suffix.union(&right.suffix).cloned().collect(),
                matches: Trigrams::or([left.matches, right.matches]),
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
fn repeat(sub: Info, min: u32, max: Option<u32>, limits: &Limits, trims: &mut Trims) -> Info {
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

fn first_two(s: &[u8]) -> &[u8] {
    &s[..s.len().min(2)]
}

fn last_two(s: &[u8]) -> &[u8] {
    &s[s.len().saturating_sub(2)..]
}

/// Cox's order: save before you discard. An exact set over the caps has its
/// trigrams saved into `matches` before it is released.
///
/// Once `exact` is unknown, every prefix and suffix string's own trigrams are
/// already in `matches`: each string came from a released exact set, or from
/// a concatenation whose boundary trigrams were saved. Only the two bytes at
/// the open end can still form a new key, across a later boundary, so the
/// sets keep just those. Shortening a set to the cap then discards boundary
/// keys only, which no saving could keep.
fn simplify(mut info: Info, limits: &Limits, trims: &mut Trims) -> Info {
    if let Some(exact) = &info.exact {
        let shortest = exact.iter().map(Vec::len).min().unwrap_or(0);
        if exact.len() > limits.exact || shortest > limits.exact_len {
            trims.exact += 1;
            info = info.release();
        }
    }
    if info.exact.is_none() {
        info.prefix = info.prefix.iter().map(|s| first_two(s).to_vec()).collect();
        info.suffix = info.suffix.iter().map(|s| last_two(s).to_vec()).collect();
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
