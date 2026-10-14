//! The derivation against the real verifier and the real key function.
//!
//! A document's key set comes from [`ferret_text::trigram_key`] over its raw
//! bytes, and [`admits`] evaluates a tree against that set; neither knows
//! anything about regex syntax. Soundness is then the course's "never drop a
//! hit": whenever the compiled regex matches a document, the derived tree
//! admits it.

use std::collections::HashSet;

use ferret_text::trigram_key;

use super::{GrepError, GrepFlags, LIMITS, Limits, Trigrams, Trims};
use crate::Matcher;

// ── the oracle ──

fn doc_keys(doc: &[u8]) -> HashSet<[u8; 3]> {
    doc.windows(3)
        .map(|w| trigram_key([w[0], w[1], w[2]]))
        .collect()
}

fn admits(query: &Trigrams, keys: &HashSet<[u8; 3]>) -> bool {
    match query {
        Trigrams::All => true,
        Trigrams::None => false,
        Trigrams::Key(key) => keys.contains(key),
        Trigrams::And(parts) => parts.iter().all(|p| admits(p, keys)),
        Trigrams::Or(parts) => parts.iter().any(|p| admits(p, keys)),
    }
}

fn flags(case_insensitive: bool) -> GrepFlags {
    GrepFlags { case_insensitive }
}

fn derive(pattern: &str, case_insensitive: bool) -> Trigrams {
    derive_with(pattern, case_insensitive, &LIMITS).0
}

fn derive_with(pattern: &str, case_insensitive: bool, limits: &Limits) -> (Trigrams, Trims) {
    let mut trims = Trims::default();
    let query = flags(case_insensitive)
        .derive(pattern, limits, &mut trims)
        .unwrap();
    (query, trims)
}

/// The regex matches `doc`, and the derived query admits it.
fn assert_sound(pattern: &str, case_insensitive: bool, doc: &[u8]) {
    let grep = flags(case_insensitive).compile(pattern).unwrap();
    assert!(
        grep.regex().is_match(doc),
        "{pattern:?} should match {doc:?}"
    );
    assert!(
        admits(grep.trigrams(), &doc_keys(doc)),
        "{pattern:?} ci={case_insensitive}: {:?} excludes {doc:?}",
        grep.trigrams()
    );
}

fn key(s: &[u8]) -> Trigrams {
    Trigrams::Key([s[0], s[1], s[2]])
}

fn and_keys(keys: &[&[u8]]) -> Trigrams {
    Trigrams::and(keys.iter().map(|k| key(k)))
}

// ── precision, pinned (the course's index.json) ──

#[test]
fn the_course_cases_derive_their_exact_queries() {
    let cases: &[(&str, Trigrams)] = &[
        ("Search", and_keys(&[b"sea", b"ear", b"arc", b"rch"])),
        (
            "Google.*Search",
            and_keys(&[
                b"goo", b"oog", b"ogl", b"gle", b"sea", b"ear", b"arc", b"rch",
            ]),
        ),
        (
            "ab[cd]e",
            Trigrams::or([and_keys(&[b"abc", b"bce"]), and_keys(&[b"abd", b"bde"])]),
        ),
        ("a*", Trigrams::All),
        ("ab*", Trigrams::All),
        // The course pins `ab?ce`; S3.md's `ab?cef` misreads its fixture
        // table, where the next column (the hit file `f`) follows the
        // pattern. The `?` rule is the one under test either way.
        (
            "ab?ce",
            Trigrams::or([key(b"ace"), and_keys(&[b"abc", b"bce"])]),
        ),
        (
            "ab?cef",
            Trigrams::or([
                and_keys(&[b"ace", b"cef"]),
                and_keys(&[b"abc", b"bce", b"cef"]),
            ]),
        ),
        (
            "ab?cdef",
            Trigrams::or([
                and_keys(&[b"acd", b"cde", b"def"]),
                and_keys(&[b"abc", b"bcd", b"cde", b"def"]),
            ]),
        ),
    ];
    for (pattern, expect) in cases {
        assert_eq!(&derive(pattern, false), expect, "{pattern}");
    }
}

#[test]
fn a_missing_key_excludes_the_document() {
    let query = derive("banana", false);
    assert_eq!(query, and_keys(&[b"ban", b"ana", b"nan"]));
    for doc in [&b"Google Code Search"[..], b"banxna", b"anana nana"] {
        assert!(!admits(&query, &doc_keys(doc)), "{doc:?}");
    }
    assert!(admits(&query, &doc_keys(b"a BANANA split")));
}

#[test]
fn a_wide_class_falls_to_the_class_cap_and_keeps_the_literal() {
    for ci in [false, true] {
        let (query, trims) = derive_with("[A-Z_]+Error", ci, &LIMITS);
        assert_eq!(query, and_keys(&[b"err", b"rro", b"ror"]), "ci={ci}");
        assert!(trims.class >= 1);
    }
}

#[test]
fn short_patterns_derive_all() {
    for pattern in ["k", "a?", r"\b", "", "ab", "(?-u:\\xFF)a", "^$"] {
        assert_eq!(derive(pattern, true), Trigrams::All, "{pattern:?}");
    }
    assert_sound("k", true, b"k");
    assert_sound("k", true, "\u{212A}".as_bytes());
    assert_sound("", true, b"");
}

#[test]
fn an_empty_class_matches_nothing() {
    assert_eq!(derive(r"[^\x00-\x{10FFFF}]", false), Trigrams::None);
    assert_eq!(derive(r"(?-u:[^\x00-\xFF])x", false), Trigrams::None);
}

// ── discriminating cases ──

/// A wrong fold that lowercases the pattern's bytes, or folds them as ASCII
/// only, derives `kel AND …` and excludes the document spelled with the
/// Kelvin sign, which `(?i)` matches.
#[test]
fn case_insensitive_non_ascii_keeps_every_case_variant() {
    let kelvin = "\u{212A}".as_bytes();
    assert_eq!(kelvin, [0xE2, 0x84, 0xAA]);
    let query = derive("kelvin", true);
    let mut kelvin_el = kelvin.to_vec();
    kelvin_el.extend_from_slice(b"elvi");
    assert_eq!(
        query,
        Trigrams::and([
            Trigrams::or([
                and_keys(&[b"kel", b"elv", b"lvi"]),
                and_keys(&[
                    &kelvin_el[0..3],
                    &kelvin_el[1..4],
                    &kelvin_el[2..5],
                    b"elv",
                    b"lvi",
                ]),
            ]),
            key(b"vin"),
        ])
    );
    assert!(query.keys().contains(&[0xAA, b'e', b'l']));
    assert_sound("kelvin", true, "\u{212A}ELVIN".as_bytes());
    assert_sound("kelvin", true, b"KELVIN");
    // The long s folds to `s` under Unicode case folding; its bytes do not.
    assert_sound("class", true, "CLAſſ".as_bytes());
    // `É` is C3 89 and `é` C3 A9: an ASCII fold of the bytes joins neither.
    assert_sound("café", true, "CAFÉ".as_bytes());
    assert_sound("CAFÉ", true, "xcafé".as_bytes());
    // Case-sensitive, the Kelvin sign is not `k`.
    assert_eq!(
        derive("kelvin", false),
        and_keys(&[b"kel", b"elv", b"lvi", b"vin"])
    );
}

/// A class over raw bytes is raw bytes, not their UTF-8 encodings (Astra).
#[test]
fn a_byte_class_is_raw_bytes() {
    let query = derive(r"(?-u:[\x80-\x81])ab", true);
    assert_eq!(
        query,
        Trigrams::or([key(&[0x80, b'a', b'b']), key(&[0x81, b'a', b'b'])])
    );
    assert_sound(r"(?-u:[\x80-\x81])ab", true, &[0x80, 0x61, 0x62]);
    assert_sound(r"(?-u:\xFF)AB", true, &[b'z', 0xFF, b'a', b'b']);
    // Outside `(?-u)`, `\xFF` is U+00FF, matched as C3 BF.
    assert_sound(r"\xFFab", false, "\u{FF}ab".as_bytes());
}

/// A wrong `Or` that skips a branch with no trigrams (treating `All` as its
/// identity) derives the long branch's keys and drops `x`. Cox's rules are
/// coarse after it: the `Or` is `All`, and only the boundary keys with `z`
/// survive.
#[test]
fn an_alternative_without_trigrams_makes_the_or_all() {
    assert_eq!(derive("(abcdef|x)", false), Trigrams::All);
    assert_sound("(abcdef|x)", false, b"x");
    assert_eq!(
        derive("(abcdef|xy)z", false),
        Trigrams::or([key(b"efz"), key(b"xyz")])
    );
    assert_sound("(abcdef|xy)z", false, b"xyz");
}

/// A wrong repetition rule that keeps `sub`'s keys whenever it repeats would
/// demand `def` here, and `abcghi` has none.
#[test]
fn a_zero_minimum_repetition_contributes_no_keys() {
    for pattern in ["abc(def)*ghi", "abc(def){0,2}ghi", "abc(?:def){0,}ghi"] {
        assert_eq!(
            derive(pattern, false),
            and_keys(&[b"abc", b"ghi"]),
            "{pattern}"
        );
        assert_sound(pattern, false, b"abcghi");
    }
    // With a minimum of one, `sub`'s keys and both boundaries stay; `efd`
    // spans two copies and is not necessary.
    assert_eq!(
        derive("abc(def)+ghi", false),
        and_keys(&[b"abc", b"bcd", b"cde", b"def", b"efg", b"fgh", b"ghi"])
    );
    assert_sound("abc(def)+ghi", false, b"abcdefghi");
    assert_sound("abc(def)+ghi", false, b"abcdefdefghi");
}

// ── the caps ──

/// 80 letters with no repeated trigram: 78 keys, all in one `And`, so only
/// replacing `And` children with `All` can bring it under the cap.
#[test]
fn the_key_cap_shrinks_an_and_only_literal() {
    let mut literal = Vec::new();
    let mut seen = HashSet::new();
    let mut rng = Rng(0x5eed);
    while literal.len() < 80 {
        let byte = b'a' + rng.below(26) as u8;
        let n = literal.len();
        if n >= 2 && !seen.insert([literal[n - 2], literal[n - 1], byte]) {
            continue;
        }
        literal.push(byte);
    }
    let pattern = String::from_utf8(literal.clone()).unwrap();
    let all = doc_keys(&literal);
    assert_eq!(all.len(), 78);

    let (query, trims) = derive_with(&pattern, false, &LIMITS);
    let keys = query.keys();
    assert_eq!(keys.len(), LIMITS.keys);
    assert!(keys.iter().all(|k| all.contains(k)));
    assert!(matches!(query, Trigrams::And(_)));
    assert_eq!(trims.keys, 78 - 64);
    assert_sound(&pattern, false, &literal);
    let mut doc = b"\xff\n".to_vec();
    doc.extend_from_slice(&literal.to_ascii_uppercase());
    assert_sound(&pattern, true, &doc);

    // A long literal stays linear to derive, and keeps a full cap of keys
    // case-insensitively too, where `k` and `s` add `Or`s: dropping keys in
    // one batch once collapsed every `Or` and left no key at all.
    let long: String = (0..8_000)
        .map(|_| char::from(b'a' + rng.below(26) as u8))
        .collect();
    for ci in [false, true] {
        let (query, _) = derive_with(&long, ci, &LIMITS);
        assert_eq!(query.keys().len(), LIMITS.keys, "ci={ci}");
        let doc = if ci {
            long.to_ascii_uppercase()
        } else {
            long.clone()
        };
        assert_sound(&long, ci, doc.as_bytes());
    }
}

/// Deleting an alternative to fit the cap would exclude the documents that
/// only that alternative matches.
#[test]
fn the_key_cap_never_deletes_an_alternative() {
    let low = Limits { keys: 2, ..LIMITS };
    let (query, _) = derive_with("(longliteral|x)", false, &low);
    assert!(admits(&query, &doc_keys(b"x")));

    // At one key, one alternative's last key must go: the `Or` becomes
    // `All`, where deleting the alternative would leave `xyz` alone.
    let one = Limits { keys: 1, ..LIMITS };
    let (query, trims) = derive_with("(longliteral|xyz)", false, &one);
    assert!(query.keys().len() <= 1);
    assert!(trims.keys > 0);
    for doc in [&b"xyz"[..], b"longliteral"] {
        assert!(admits(&query, &doc_keys(doc)), "{query:?} excludes {doc:?}");
    }
}

#[test]
fn exact_sets_over_the_cap_save_their_trigrams_first() {
    // Eight strings: one over Cox's maxExact of 7.
    let (query, trims) = derive_with("[ab][cd][ef]x", false, &LIMITS);
    assert!(trims.exact >= 1);
    for doc in [&b"acex"[..], b"bdfx"] {
        assert!(admits(&query, &doc_keys(doc)));
    }
    assert!(!admits(&query, &doc_keys(b"acxx")));
}

// ── compile, then derive ──

#[test]
fn a_pattern_over_the_compile_limit_is_refused_and_never_derived() {
    let pattern = "a{100000000}";
    // It parses: the size limit is the compiler's, not the parser's.
    assert!(flags(true).parser().build().parse(pattern).is_ok());
    assert!(matches!(
        flags(true).compile(pattern),
        Err(GrepError::Pattern(_))
    ));
}

#[test]
fn a_syntax_error_is_the_patterns_and_a_byte_pattern_derives() {
    for pattern in ["(", "a{2,1}", "[z-a]", r"\p{Nope}"] {
        assert!(
            matches!(flags(true).compile(pattern), Err(GrepError::Pattern(_))),
            "{pattern}"
        );
    }
    // `utf8(false)`: the default parser would refuse this; both must accept.
    assert!(flags(true).compile(r"(?-u:\xFF)").is_ok());
    assert!(
        regex_syntax::ParserBuilder::new()
            .build()
            .parse(r"(?-u:\xFF)")
            .is_err()
    );
}

// ── agreement ──

/// The index's keys and the derivation's agree: a document's own bytes, as
/// a byte-for-byte pattern, derive a query its key set satisfies, and that
/// keeps every key of the slice. `trigram_key` is the only fold on either
/// side.
#[test]
fn a_document_satisfies_a_query_for_its_own_bytes() {
    let mut rng = Rng(0x0a11_ce5e);
    for _ in 0..500 {
        let doc: Vec<u8> = (0..4 + rng.below(40))
            .map(|_| *rng.pick(b"aAzZkK_\n\x80\xff\xe2\x84\xaa\xc3\x89"))
            .collect();
        let start = rng.below(doc.len() - 3);
        let slice = &doc[start..start + 3 + rng.below((doc.len() - start - 3).min(6) + 1)];
        let pattern: String = slice.iter().map(|b| format!(r"\x{b:02X}")).collect();
        for ci in [false, true] {
            let pattern = format!("(?-u:{pattern})");
            assert_sound(&pattern, ci, &doc);
            assert_eq!(
                derive(&pattern, ci).keys(),
                doc_keys(slice).into_iter().collect()
            );
        }
    }
}

// ── soundness, by property ──

/// xorshift64*: deterministic, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// A pattern node that can render itself and sample a string it matches.
enum Node {
    /// A literal character, rendered as itself or as an escape.
    Char(char),
    /// `\xNN` with NN ≥ 0x80 inside `(?-u)`: the raw byte.
    Byte(u8),
    /// A class's pattern text and members a sample draws from.
    Class(&'static str, &'static [&'static [u8]]),
    Dot,
    Look(&'static str),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Rep(Box<Node>, u32, Option<u32>, bool),
    Group(Group, Box<Node>),
}

#[derive(Clone, Copy)]
enum Group {
    Capture,
    CaseOn,
    CaseOff,
    Bytes,
}

#[derive(Clone, Copy)]
struct Context {
    case_insensitive: bool,
    unicode: bool,
}

impl Context {
    fn enter(self, group: Group) -> Context {
        match group {
            Group::Capture => self,
            Group::CaseOn => Context {
                case_insensitive: true,
                ..self
            },
            Group::CaseOff => Context {
                case_insensitive: false,
                ..self
            },
            Group::Bytes => Context {
                unicode: false,
                ..self
            },
        }
    }
}

const CHARS: &[char] = &[
    'a', 'b', 'k', 's', 'K', 'S', 'e', 'l', 'é', 'É', '\u{212A}', 'ſ', '1', '-', ' ',
];
const UNICODE_CLASSES: &[(&str, &[&[u8]])] = &[
    ("[a-c]", &[b"a", b"b", b"c"]),
    ("[kK]", &[b"k", b"K"]),
    ("[es]", &[b"e", b"s"]),
    (r"\d", &[b"0", b"7"]),
    (
        r"\w",
        &[b"a", b"_", b"9", "é".as_bytes(), "\u{212A}".as_bytes()],
    ),
    ("[A-Z_]", &[b"Q", b"_"]),
    ("[^a]", &[b"b", "é".as_bytes(), b"\n"]),
    ("[é\u{212A}]", &["é".as_bytes(), "\u{212A}".as_bytes()]),
    (r"\s", &[b" ", b"\n"]),
];
const BYTE_CLASSES: &[(&str, &[&[u8]])] = &[
    (r"[\x80-\x81]", &[b"\x80", b"\x81"]),
    ("[a-c]", &[b"a", b"c"]),
    (r"[^\x00-\x7F]", &[b"\x80", b"\xff", b"\xe2"]),
    ("[kK]", &[b"k"]),
];
const LOOKS: &[&str] = &["^", "$", r"\b", r"\B"];
const RAW_BYTES: &[u8] = &[0x80, 0x81, 0xAA, 0xE2, 0xFF, 0xC3];

fn gen_node(rng: &mut Rng, depth: u32, ctx: Context) -> Node {
    if depth == 0 || rng.below(3) == 0 {
        return gen_leaf(rng, ctx);
    }
    match rng.below(9) {
        0..=2 => Node::Concat(
            (0..2 + rng.below(4))
                .map(|_| gen_node(rng, depth - 1, ctx))
                .collect(),
        ),
        3 | 4 => Node::Alt(
            (0..2 + rng.below(2))
                .map(|_| {
                    if rng.below(6) == 0 {
                        Node::Concat(Vec::new())
                    } else {
                        gen_node(rng, depth - 1, ctx)
                    }
                })
                .collect(),
        ),
        5 | 6 => {
            let (min, max) = *rng.pick(&[
                (0, Some(1)),
                (0, None),
                (1, None),
                (2, Some(2)),
                (0, Some(3)),
                (1, Some(3)),
                (3, Some(3)),
                (2, None),
                (12, Some(12)),
            ]);
            let lazy = rng.below(4) == 0;
            Node::Rep(Box::new(gen_node(rng, depth - 1, ctx)), min, max, lazy)
        }
        _ => {
            let group = *rng.pick(&[Group::Capture, Group::CaseOn, Group::CaseOff, Group::Bytes]);
            Node::Group(group, Box::new(gen_node(rng, depth - 1, ctx.enter(group))))
        }
    }
}

fn gen_leaf(rng: &mut Rng, ctx: Context) -> Node {
    match rng.below(10) {
        0..=4 if ctx.unicode => Node::Char(*rng.pick(CHARS)),
        0..=2 => Node::Byte(*rng.pick(RAW_BYTES)),
        3 | 4 => Node::Char(*rng.pick(&['a', 'k', 'K', 's', 'b'])),
        5 | 6 => {
            let classes = if ctx.unicode {
                UNICODE_CLASSES
            } else {
                BYTE_CLASSES
            };
            let (text, members) = *rng.pick(classes);
            Node::Class(text, members)
        }
        7 => Node::Dot,
        8 => Node::Look(LOOKS[rng.below(LOOKS.len())]),
        // A literal run, long enough to reach the exact-set caps.
        _ => {
            let letters: &[char] = if ctx.unicode {
                &['a', 'b', 'c', 'k', 'e', 'l', 'v', 'i', 'n', 's', 'S', 'é']
            } else {
                &['a', 'b', 'k', 'K', 's']
            };
            Node::Concat(
                (0..3 + rng.below(6))
                    .map(|_| Node::Char(*rng.pick(letters)))
                    .collect(),
            )
        }
    }
}

fn render(node: &Node, rng: &mut Rng, out: &mut String) {
    match node {
        Node::Char(c) if c.is_ascii_alphanumeric() || !c.is_ascii() && rng.below(3) > 0 => {
            out.push(*c)
        }
        Node::Char(c) if c.is_ascii() => out.push_str(&regex_syntax::escape(&c.to_string())),
        Node::Char(c) => out.push_str(&format!(r"\x{{{:X}}}", u32::from(*c))),
        Node::Byte(b) => out.push_str(&format!(r"\x{b:02X}")),
        Node::Class(text, _) => out.push_str(text),
        Node::Dot => out.push('.'),
        Node::Look(text) => out.push_str(text),
        Node::Concat(parts) => parts.iter().for_each(|p| render(p, rng, out)),
        Node::Alt(parts) => {
            out.push_str("(?:");
            for (i, p) in parts.iter().enumerate() {
                if i > 0 {
                    out.push('|');
                }
                render(p, rng, out);
            }
            out.push(')');
        }
        Node::Rep(sub, min, max, lazy) => {
            out.push_str("(?:");
            render(sub, rng, out);
            out.push(')');
            match (min, max) {
                (0, Some(1)) => out.push('?'),
                (0, None) => out.push('*'),
                (1, None) => out.push('+'),
                (m, None) => out.push_str(&format!("{{{m},}}")),
                (m, Some(n)) if m == n => out.push_str(&format!("{{{m}}}")),
                (m, Some(n)) => out.push_str(&format!("{{{m},{n}}}")),
            }
            if *lazy {
                out.push('?');
            }
        }
        Node::Group(group, sub) => {
            out.push_str(match group {
                Group::Capture => "(",
                Group::CaseOn => "(?i:",
                Group::CaseOff => "(?-i:",
                Group::Bytes => "(?-u:",
            });
            render(sub, rng, out);
            out.push(')');
        }
    }
}

/// Spellings `(?i)` matches for `c`, as far as the generator needs them:
/// the Unicode simple case folds of the letters in [`CHARS`].
fn case_variants(c: char, ctx: Context) -> Vec<char> {
    if !ctx.case_insensitive {
        return vec![c];
    }
    let unicode: &[&[char]] = &[&['k', 'K', '\u{212A}'], &['s', 'S', 'ſ'], &['é', 'É']];
    match unicode.iter().find(|set| set.contains(&c)) {
        Some(set) if ctx.unicode => set.to_vec(),
        _ if c.is_ascii_alphabetic() => vec![c.to_ascii_lowercase(), c.to_ascii_uppercase()],
        _ => vec![c],
    }
}

fn sample(node: &Node, rng: &mut Rng, ctx: Context, out: &mut Vec<u8>) {
    match node {
        Node::Char(c) => {
            let variants = case_variants(*c, ctx);
            let c = *rng.pick(&variants);
            out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
        }
        Node::Byte(b) => out.push(*b),
        Node::Class(_, members) => {
            let member = *rng.pick(members);
            match std::str::from_utf8(member)
                .ok()
                .and_then(|s| s.chars().next())
            {
                Some(c) if ctx.unicode || c.is_ascii() => sample(&Node::Char(c), rng, ctx, out),
                _ => out.extend_from_slice(member),
            }
        }
        Node::Dot if ctx.unicode => {
            let c = *rng.pick(&['a', 'é', '\u{212A}', 'Z', ' ']);
            out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
        }
        Node::Dot => out.push(*rng.pick(&[b'a', 0x80, 0xFF, b' '])),
        Node::Look(_) => {}
        Node::Concat(parts) => parts.iter().for_each(|p| sample(p, rng, ctx, out)),
        Node::Alt(parts) => sample(rng.pick(parts), rng, ctx, out),
        Node::Rep(sub, min, max, _) => {
            let top = max.unwrap_or(min + 2);
            let n = *min + rng.below((top - min + 1) as usize) as u32;
            for _ in 0..n {
                sample(sub, rng, ctx, out);
            }
        }
        Node::Group(group, sub) => sample(sub, rng, ctx.enter(*group), out),
    }
}

/// Context bytes around a sample: pieces of the Kelvin sign's and other
/// encodings, so documents are often invalid UTF-8, plus newlines for the
/// multi-line anchors.
fn noise(rng: &mut Rng, out: &mut Vec<u8>) {
    const BYTES: &[u8] = &[
        b'a', b'b', b'k', b'K', b's', b'e', b'\n', b' ', 0x80, 0xFF, 0xE2, 0x84, 0xAA, 0xC5, 0xBF,
        0xC3, 0xA9,
    ];
    for _ in 0..rng.below(6) {
        out.push(*rng.pick(BYTES));
    }
}

#[derive(Debug, Default)]
struct Tally {
    patterns: u32,
    refused: u32,
    checked: u32,
    matched: u32,
    matched_invalid_utf8: u32,
    matched_non_ascii_fold: u32,
    matched_narrowed: u32,
    /// Documents the query excluded and the regex rejected: the query prunes.
    excluded: u32,
    /// Patterns whose [`super::Search`] looks for a required run first.
    filtered: u32,
    /// Texts such a search matched, and texts it rejected although the
    /// required run occurs (so a window was searched).
    filtered_matched: u32,
    filtered_windowed: u32,
}

/// Documents per pattern: two thirds hold a sample, the rest are noise.
/// Compiling dominates the cost in debug (about 1 ms a pattern), so each
/// pattern is checked against several documents.
const DOCS: u32 = 9;

fn run_property(patterns: u32, seed: u64) -> Tally {
    let mut rng = Rng(seed);
    let mut tally = Tally::default();
    for _ in 0..patterns {
        let case_insensitive = rng.below(3) > 0;
        let ctx = Context {
            case_insensitive,
            unicode: true,
        };
        let node = gen_node(&mut rng, 3, ctx);
        let mut pattern = String::new();
        render(&node, &mut rng, &mut pattern);
        tally.patterns += 1;
        let grep = match flags(case_insensitive).compile(&pattern) {
            Ok(grep) => grep,
            Err(GrepError::Pattern(_)) => {
                tally.refused += 1;
                continue;
            }
            Err(e @ GrepError::Derivation(_)) => panic!("{pattern:?}: {e}"),
        };
        let search = grep.search();
        tally.filtered += u32::from(search.filters());
        let mut docs = Vec::new();
        for doc_kind in 0..DOCS {
            let mut doc = Vec::new();
            noise(&mut rng, &mut doc);
            if doc_kind % 3 < 2 {
                sample(&node, &mut rng, ctx, &mut doc);
            }
            noise(&mut rng, &mut doc);
            check_search(&grep, &search, &doc, &mut tally);
            docs.push(doc.clone());
            tally.checked += 1;
            let admitted = admits(grep.trigrams(), &doc_keys(&doc));
            if !grep.regex().is_match(&doc) {
                tally.excluded += u32::from(!admitted);
                continue;
            }
            assert!(
                admitted,
                "unsound: {pattern:?} ci={case_insensitive} matches {doc:?} but {:?} excludes it",
                grep.trigrams()
            );
            tally.matched += 1;
            tally.matched_invalid_utf8 += u32::from(std::str::from_utf8(&doc).is_err());
            tally.matched_non_ascii_fold += u32::from(
                case_insensitive
                    && ["\u{212A}", "ſ", "É"]
                        .iter()
                        .any(|s| doc.windows(s.len()).any(|w| w == s.as_bytes())),
            );
            tally.matched_narrowed += u32::from(*grep.trigrams() != Trigrams::All);
        }
        let joined = join(&mut rng, &docs);
        check_search(&grep, &search, &joined, &mut tally);
        // Few generated patterns lack a literal prefix, which is when a
        // search filters; led by a large class, as in `(?i)[A-Z_]+worse`,
        // most do.
        let lead = LEADS[rng.below(LEADS.len())];
        if let Ok(led) = flags(case_insensitive).compile(&format!("{lead}(?:{pattern})")) {
            let search = led.search();
            tally.filtered += u32::from(search.filters());
            for doc in docs.iter().chain([&joined]) {
                check_search(&led, &search, doc, &mut tally);
            }
        }
    }
    tally
}

/// Large classes to lead a pattern with, without and with an assertion
/// (whose window is the line, not the word).
const LEADS: [&str; 4] = [
    r"[A-Za-z_]+",
    r"\b[a-z]+",
    r"[0-9a-z]*",
    r"(?:[[:alnum:]_]|-)+\B",
];

/// Every document in one text, so a search meets several windows and must
/// pick the first match among them; some on one line.
fn join(rng: &mut Rng, docs: &[Vec<u8>]) -> Vec<u8> {
    let mut joined = Vec::new();
    for doc in docs {
        joined.extend_from_slice(doc);
        joined.push(if rng.below(2) == 0 { b'\n' } else { b' ' });
    }
    joined
}

/// [`super::Search::find`] against the pattern over the whole text: the
/// same first match.
fn check_search(grep: &super::Grep, search: &super::Search, text: &[u8], tally: &mut Tally) {
    let expected = grep.regex().find(text).map(|m| m.start);
    assert_eq!(
        search.find(text),
        expected,
        "{:?} over {text:?}",
        grep.pattern()
    );
    if search.filters() {
        tally.filtered_matched += u32::from(expected.is_some());
        tally.filtered_windowed += u32::from(expected.is_none() && search.run_occurs(text));
    }
}

/// Floors that keep the property from passing vacuously: most generated
/// patterns compile, most samples match, and every feature the generator is
/// meant to cover shows up among the matches. Each floor is a share of the
/// base patterns generated, `n`, whatever it counts: compiled variants
/// (`filtered` counts led patterns too) or searched texts.
fn assert_covers(tally: &Tally) {
    let n = tally.patterns;
    assert!(tally.refused < n / 10, "{tally:?}");
    assert!(tally.matched > n, "{tally:?}");
    assert!(tally.matched_invalid_utf8 > n / 20, "{tally:?}");
    assert!(tally.matched_non_ascii_fold > n / 100, "{tally:?}");
    assert!(tally.matched_narrowed > n / 5, "{tally:?}");
    assert!(tally.excluded > n / 20, "{tally:?}");
    assert!(tally.filtered > n / 10, "{tally:?}");
    assert!(tally.filtered_matched > n / 5, "{tally:?}");
    assert!(tally.filtered_windowed > n / 5, "{tally:?}");
}

/// 3,400 patterns × 9 documents: 30,600 cases, inside the default suite's
/// budget in debug (about 16 s, most of it compiling the led patterns).
#[test]
fn every_match_satisfies_the_derived_query() {
    let tally = run_property(3_400, 0x9e37_79b9_7f4a_7c15);
    assert!(tally.checked >= 30_000, "{tally:?}");
    assert_covers(&tally);
}

/// 30,000 patterns, each against 9 documents.
#[test]
#[ignore = "long: about 45 s in debug; run with --release"]
fn every_match_satisfies_the_derived_query_long() {
    let tally = run_property(30_000, 0x2545_f491_4f6c_dd1d);
    assert_covers(&tally);
    println!("{tally:?}");
}

// ── coverage over the regex classes (M3's measurement) ──

/// An identifier-like word: camelCase syllables, sometimes a digit or `_`.
fn word(rng: &mut Rng, parts: usize) -> String {
    const SYLLABLES: &[&str] = &[
        "parse", "head", "er", "con", "fig", "load", "buf", "fer", "req", "uest", "han", "dle",
        "tok", "en", "map", "set", "err", "or", "num", "ber", "len", "file", "node", "id",
    ];
    let mut out = String::new();
    for i in 0..parts {
        let s = *rng.pick(SYLLABLES);
        if i > 0 && rng.below(2) == 0 {
            out.push_str(&s[..1].to_ascii_uppercase());
            out.push_str(&s[1..]);
        } else {
            out.push_str(s);
        }
    }
    match rng.below(6) {
        0 => out.push('_'),
        1 => out.push(char::from(b'0' + rng.below(10) as u8)),
        _ => {}
    }
    out
}

/// M5's regex classes (S3 § Bench plan), with synthetic words in place of
/// dictionary terms.
fn class_pattern(class: &str, rng: &mut Rng) -> String {
    let parts = 1 + rng.below(3);
    match class {
        "rare literal" => word(rng, 1 + parts),
        "identifier pattern" => format!(r"{}\w+{}", word(rng, 1), word(rng, 1)),
        "alternation" => format!("({}|{}|{})", word(rng, 2), word(rng, 2), word(rng, 2)),
        "broad" => format!(
            r"{}\d+",
            rng.pick(&["the", "ing", "ion", "ent", "and", "tio"])
        ),
        "code class" => format!("[A-Z_]+{}", word(rng, parts.min(2))),
        _ => (*rng.pick(&[
            r"\d{4}-\d{2}",
            r"[0-9a-f]{8}",
            r"\w+\.\w+",
            r"\d+",
            r"^\s*$",
        ]))
        .to_string(),
    }
}

const CLASSES: &[&str] = &[
    "rare literal",
    "identifier pattern",
    "alternation",
    "broad",
    "code class",
    "refused",
];

#[derive(Default)]
struct Coverage {
    patterns: u32,
    narrowed: u32,
    keys: Vec<usize>,
    trimmed: Trims,
}

fn coverage(class: &str, case_insensitive: bool) -> Coverage {
    let mut rng = Rng(0xc07e_2a9e_u64.wrapping_add(class.len() as u64));
    let mut out = Coverage::default();
    for _ in 0..500 {
        let pattern = class_pattern(class, &mut rng);
        let (query, trims) = derive_with(&pattern, case_insensitive, &LIMITS);
        out.patterns += 1;
        out.narrowed += u32::from(query != Trigrams::All);
        out.keys.push(query.keys().len());
        out.trimmed.class += u32::from(trims.class > 0);
        out.trimmed.exact += u32::from(trims.exact > 0);
        out.trimmed.set += u32::from(trims.set > 0);
        out.trimmed.keys += u32::from(trims.keys > 0);
    }
    out.keys.sort_unstable();
    out
}

/// Prints the coverage table (`--nocapture`), and holds each class to the
/// shape S3 expects of it: literals and alternations always narrow, the
/// refused class never does. The other classes narrow unless a part is
/// under three bytes (`er\w+id`).
#[test]
fn derivation_coverage_over_the_regex_classes() {
    println!(
        "| class | case | narrowed | keys median | keys p90 | keys max | class cap | exact cap | set cap | key cap |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    for class in CLASSES {
        for ci in [true, false] {
            let c = coverage(class, ci);
            let pct = |n: u32| format!("{:.0}%", 100.0 * f64::from(n) / f64::from(c.patterns));
            let at = |q: usize| c.keys[(c.keys.len() - 1) * q / 100];
            println!(
                "| {class} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                if ci { "insensitive" } else { "case:" },
                pct(c.narrowed),
                at(50),
                at(90),
                at(100),
                pct(c.trimmed.class),
                pct(c.trimmed.exact),
                pct(c.trimmed.set),
                pct(c.trimmed.keys),
            );
            match *class {
                "rare literal" | "alternation" => {
                    assert_eq!(c.narrowed, c.patterns, "{class} ci={ci}")
                }
                "refused" => assert_eq!(c.narrowed, 0, "{class} ci={ci}"),
                _ => {}
            }
        }
    }
}

// ── the search: required run, then windows ──

/// [`super::Search::find`] against the pattern over the whole text.
fn same_first_match(pattern: &str, case_insensitive: bool, text: &[u8]) {
    let grep = GrepFlags { case_insensitive }.compile(pattern).unwrap();
    same_first(&grep, text);
}

fn same_first(grep: &super::Grep, text: &[u8]) {
    let pattern = grep.pattern();
    assert_eq!(
        grep.search().find(text),
        grep.regex().find(text).map(|span| span.start),
        "{pattern} in {:?}",
        String::from_utf8_lossy(text)
    );
}

#[test]
fn only_patterns_without_a_literal_prefix_filter() {
    for (pattern, filters) in [
        (r"(?i)[A-Z_]+worse", true),
        (r"[A-Z_]+worse", true),
        (r"(?i)\b[A-Z_]+join\b", true),
        (r"(?i)defi\w+acro", false),
        (r"(?i)ion\d+", false),
        (r"(?i)(says|runtime|modern)", false),
        (r"(?i)[A-Z_]+ab", false),
        // Not line-local: `\s` matches a newline.
        (r"(?i)[A-Z_]+\s+worse", false),
    ] {
        let grep = GrepFlags::default().compile(pattern).unwrap();
        assert_eq!(grep.search().filters(), filters, "{pattern}");
    }
}

#[test]
fn the_run_keeps_every_case_variant_the_pattern_has() {
    // KELVIN SIGN folds with `k`, LATIN SMALL LETTER LONG S with `s`: a run
    // folded byte by byte in ASCII would miss both.
    for text in ["x x\u{212A}elvin\n", "a\nzz\u{212A}ELVIN"] {
        same_first_match(r"(?i)[a-z_]+kelvin", false, text.as_bytes());
        same_first_match(r"[a-z_]+kelvin", true, text.as_bytes());
        assert!(
            GrepFlags::default()
                .compile(r"(?i)[a-z_]+kelvin")
                .unwrap()
                .search()
                .find(text.as_bytes())
                .is_some()
        );
    }
    same_first_match(r"(?i)[a-z]+worse", false, "x xwor\u{17F}e".as_bytes());
    same_first_match(r"(?i)[a-zé]+join", false, "--éJOIN".as_bytes());
}

#[test]
fn a_window_without_a_match_moves_on_to_the_next() {
    for text in [
        "join foo xjoin",
        "join\njoin\n_join",
        "xjoinjoin",
        "jo in joi n_JOIN ajoin",
        "",
        "join",
    ] {
        same_first_match(r"(?i)[A-Z_]+join", false, text.as_bytes());
    }
}

#[test]
fn an_assertion_sees_the_whole_line_not_a_window() {
    // In "1xjoin" the `x` follows a word byte, so `\b` does not hold before
    // it. A window of the pattern's own bytes would start at `x` and see a
    // boundary there; the line does not.
    for text in ["1xjoin", "1xjoin xjoin", "a-xjoin", "xjoin1"] {
        same_first_match(r"(?i)\b[a-z]+join", false, text.as_bytes());
        same_first_match(r"(?i)[a-z]+join\b", false, text.as_bytes());
        same_first_match(r"(?im)^[a-z]+join$", false, text.as_bytes());
    }
    assert_eq!(
        GrepFlags::default()
            .compile(r"(?i)\b[a-z]+join")
            .unwrap()
            .search()
            .find(b"1xjoin"),
        None
    );
}

#[test]
fn the_search_finds_the_patterns_first_match_in_random_text() {
    let pieces: [&[u8]; 14] = [
        b"join",
        b"JOIN",
        b"jo",
        b"in",
        b"x",
        b"_",
        b" ",
        b"\n",
        b"1",
        b"-",
        "\u{212A}".as_bytes(),
        b"k",
        "é".as_bytes(),
        b"\xff",
    ];
    let patterns = [
        r"(?i)[A-Z_]+join",
        r"[a-z_]+join",
        r"(?i)\b[A-Z_]+join\b",
        r"(?i)[A-Z_]+jo[^\n]in",
        r"(?i)(?:[a-z]|é)+kjoin",
        r"(?i)[0-9]*[a-z]+(join|xjoin)_",
        r"(?i-u)[a-z_]+join",
        r"(?-u)[\x80-\xff]+join",
    ];
    let greps: Vec<_> = patterns
        .iter()
        .flat_map(|pattern| {
            [false, true]
                .map(|case_insensitive| GrepFlags { case_insensitive }.compile(pattern).unwrap())
        })
        .collect();
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = |below: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % below as u64) as usize
    };
    for _ in 0..4000 {
        let mut text = Vec::new();
        for _ in 0..next(24) {
            text.extend_from_slice(pieces[next(pieces.len())]);
        }
        for grep in &greps {
            same_first(grep, &text);
        }
    }
}
