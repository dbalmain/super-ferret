//! The fold: Cox's five facts over folded byte strings, derived node by
//! node from the HIR and simplified under [`Limits`].

use ferret_text::trigram_key;

use super::*;

// ── limits ──

/// The simplification's caps (S3 § Simplification). `exact` and `set` are
/// Cox's `maxExact` and `maxSet`; `class` and `keys` are starting points M3
/// measures.
#[derive(Clone, Copy, Debug)]
pub(super) struct Limits {
    /// A class with more members than this, after folding, is unknown.
    pub(super) class: usize,
    /// An exact set with more strings than this is dumped into `match`.
    pub(super) exact: usize,
    /// An exact set whose shortest string is longer than this is dumped.
    pub(super) exact_len: usize,
    /// A prefix or suffix set is shortened until it holds at most this many.
    pub(super) set: usize,
    /// A finished query is weakened until it holds at most this many keys.
    pub(super) keys: usize,
    /// Bytes retained at each open concatenation edge.
    pub(super) edge_len: usize,
}

pub(super) const LIMITS: Limits = Limits {
    class: 4,
    exact: 7,
    exact_len: 4,
    set: 20,
    keys: 64,
    edge_len: 2,
};

/// How often each cap discarded information: the measurement M3 reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Trims {
    pub(super) class: u32,
    pub(super) exact: u32,
    pub(super) set: u32,
    /// Keys the 64-key cap replaced with `All`.
    pub(super) keys: u32,
}

// ── the fold ──

type Strings = BTreeSet<Vec<u8>>;

/// Cox's five facts for one HIR node. Invariant: every match starts with a
/// member of `prefix` and ends with a member of `suffix`, so either holds
/// `""` whenever nothing better is known.
#[derive(Clone, Debug)]
pub(super) struct Info<Q = Trigrams> {
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
pub(super) fn string_trigrams(s: &[u8]) -> Trigrams {
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

pub(super) fn finish<Q: Facts>(info: Info<Q>) -> Q {
    match &info.exact {
        Some(exact) => Q::and([info.matches.clone(), set_facts(exact)]),
        None => info.matches,
    }
}

pub(super) fn derive(hir: &Hir, limits: &Limits, trims: &mut Trims) -> Trigrams {
    cap_keys(finish(fold_node(hir, limits, trims)), limits.keys, trims)
}

pub(super) fn fold_node<Q: Facts>(hir: &Hir, limits: &Limits, trims: &mut Trims) -> Info<Q> {
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
pub(super) fn cap_keys(query: Trigrams, max: usize, trims: &mut Trims) -> Trigrams {
    let before = query.keys().len();
    if before <= max {
        return query;
    }
    let query = query.fit(max);
    trims.keys += u32::try_from(before - query.keys().len()).unwrap_or(u32::MAX);
    query
}
