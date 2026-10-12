//! Which term-index lookups a required literal licenses, for a
//! case-sensitive regex. A document containing the literal's exact bytes
//! tokenizes them the same way whatever surrounds them, except near the
//! edges: the tokenizer's split rule reads one character either side. So a
//! span of the literal is a safe unit when the product tokenizer yields it,
//! in every one-character context, as:
//!
//! - **exact**: a token whose run is exactly that span, looked up whole;
//! - **prefix**: a token whose run starts at the span and covers it, looked
//!   up by prefix enumeration.
//!
//! The contexts stand for every character shape the split rule and the
//! run rule distinguish, so checking them is checking every document.
//! Case-insensitive matching has no safe unit: the document's case decides
//! the camelCase splits, and lowercasing the literal loses them.

use ferret_text::{MAX_TOKEN_BYTES, tokenize};

/// One-character contexts, one per shape: none, a separator, invalid UTF-8,
/// ASCII lower/upper/digit/underscore, and non-ASCII lower, upper, caseless
/// letter, digit and lowercase numeral (`ⅰ` is both).
const CONTEXTS: [&[u8]; 12] = [
    b"",
    b" ",
    b"\xff",
    b"a",
    b"A",
    b"0",
    b"_",
    "é".as_bytes(),
    "É".as_bytes(),
    "中".as_bytes(),
    "٣".as_bytes(),
    "ⅰ".as_bytes(),
];

/// The shortest prefix worth enumerating; shorter ones name most of the
/// dictionary.
pub(super) const MIN_PREFIX: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Unit {
    Exact(Vec<u8>),
    Prefix(Vec<u8>),
}

/// The safe units of `literal`, a fact from a case-sensitive regex. Exact
/// units first; a prefix unit only where no exact unit starts.
pub(super) fn units(literal: &[u8]) -> Vec<Unit> {
    if std::str::from_utf8(literal).is_err() {
        return Vec::new();
    }
    // span start -> (token bytes, whether every context had it exactly)
    let mut exact: Option<Vec<(usize, usize, Vec<u8>)>> = None;
    // span start -> shortest covered end over contexts
    let mut prefix: Option<Vec<Option<usize>>> = None;
    for left in CONTEXTS {
        for right in CONTEXTS {
            let mut text = left.to_vec();
            text.extend_from_slice(literal);
            text.extend_from_slice(right);
            let (lo, hi) = (left.len(), left.len() + literal.len());
            let mut here = Vec::new();
            let mut covers = vec![None; literal.len()];
            tokenize(&text, &mut Default::default(), |token| {
                let run = token.run.clone();
                if run.start >= lo && run.end <= hi {
                    here.push((run.start - lo, run.end - lo, token.bytes.to_vec()));
                }
                if run.start >= lo && run.start < hi {
                    let end = run.end.min(hi) - lo;
                    let slot: &mut Option<usize> = &mut covers[run.start - lo];
                    // The token that starts here and reaches furthest.
                    *slot = Some(slot.map_or(end, |e: usize| e.max(end)));
                }
            });
            exact = Some(match exact {
                None => here,
                Some(previous) => previous.into_iter().filter(|e| here.contains(e)).collect(),
            });
            prefix = Some(match prefix {
                None => covers,
                Some(previous) => previous
                    .into_iter()
                    .zip(covers)
                    .map(|(a, b)| Some(a?.min(b?)))
                    .collect(),
            });
        }
    }
    let mut exact = exact.unwrap_or_default();
    exact.sort();
    exact.dedup();
    let mut out: Vec<Unit> = exact
        .iter()
        .filter(|(_, _, bytes)| ferret_text::exact_under_cap(bytes))
        .map(|(_, _, bytes)| Unit::Exact(bytes.clone()))
        .collect();
    for (start, end) in prefix
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .filter_map(|(start, end)| Some((start, end?)))
    {
        if exact.iter().any(|&(s, _, _)| s == start) {
            continue;
        }
        let span = &literal[start..end];
        // Every prefix token here lowercases the same bytes; ASCII only, so
        // lowercasing the span is the token's prefix byte for byte.
        if span.len() >= MIN_PREFIX && span.len() < MAX_TOKEN_BYTES - 3 && span.is_ascii() {
            out.push(Unit::Prefix(span.to_ascii_lowercase()));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The shortest substring worth a dictionary pass.
pub(super) const MIN_SUBSTRING: usize = 4;

/// The lowercased word runs of `literal`, each of which lies inside one
/// whole-run term of any document containing the literal in any case: a
/// whole run is lowercased character by character, so its term contains
/// the lowercase of every piece of it. Sound case-insensitive as well as
/// case-sensitive. Sigma is left out, the one character whose lowercase
/// depends on its neighbours. A term capped at 64 bytes, or dropped by T1,
/// can still lose it: the caller admits those documents separately.
pub(super) fn substrings(literal: &[u8]) -> Vec<Vec<u8>> {
    let Ok(text) = std::str::from_utf8(literal) else {
        return Vec::new();
    };
    let mut out: Vec<Vec<u8>> = text
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|run| !run.contains(['Σ', 'σ', 'ς']))
        .map(|run| run.to_lowercase().into_bytes())
        .filter(|run| run.len() >= MIN_SUBSTRING)
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substrings_are_lowercased_word_runs() {
        let v = |s: &str| s.as_bytes().to_vec();
        assert_eq!(substrings(b"a.Config_Value("), vec![v("config_value")]);
        assert_eq!(substrings(b"deserializeledger"), vec![v("deserializeledger")]);
        assert_eq!(substrings(b"abc"), Vec::<Vec<u8>>::new());
        assert_eq!(substrings(b"\xffabcd"), Vec::<Vec<u8>>::new());
        assert_eq!(substrings("Kelvin\u{212a}x".as_bytes()), vec![v("kelvinkx")]);
    }

    /// A document holding the literal in any case and any context has a
    /// whole-run term containing each substring, unless its token passes
    /// the cap (the fallback's case).
    #[test]
    fn every_substring_is_inside_a_term_of_any_containing_document() {
        let literals = ["cachevalues", "a.config_value(", "x9y_z", "École_ab"];
        let neighbours = ["", " ", "q", "Q", "7", "_", "é", "."];
        for literal in literals {
            for spelling in [literal.to_string(), literal.to_uppercase(), literal.to_lowercase()] {
                for left in neighbours {
                    for right in neighbours {
                        let doc = format!("{left}{spelling}{right}");
                        let mut terms = Vec::new();
                        tokenize(doc.as_bytes(), &mut Default::default(), |t| {
                            terms.push(ferret_text::cap(t.bytes).to_vec());
                        });
                        for needle in substrings(literal.as_bytes()) {
                            assert!(
                                terms.iter().any(|t| t.windows(needle.len()).any(|w| w == needle)),
                                "{needle:?} not in {doc:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    fn exact(s: &str) -> Unit {
        Unit::Exact(s.as_bytes().to_vec())
    }
    fn prefix(s: &str) -> Unit {
        Unit::Prefix(s.as_bytes().to_vec())
    }

    #[test]
    fn edges_are_dropped_and_the_last_unit_is_a_prefix() {
        // The brief's example: the first unit may be a suffix in the
        // document, so only `ledger` survives, and only as a prefix.
        assert_eq!(units(b"deserialize_ledger"), vec![prefix("ledger")]);
        // A single run has no safe unit: it may sit inside a longer token.
        assert_eq!(units(b"deserializeledger"), vec![]);
        assert_eq!(units(b"foo_bar_baz"), vec![exact("bar"), prefix("baz")]);
    }

    #[test]
    fn separators_inside_the_literal_fix_whole_runs() {
        assert_eq!(
            units(b"a.config_value("),
            vec![exact("config"), exact("config_value"), exact("value")]
        );
    }

    #[test]
    fn camel_case_is_split_only_where_the_literal_decides_it() {
        // Inside the literal, `Ledger` is a part either way.
        assert_eq!(
            units(b"parseLedgerEntry"),
            vec![exact("ledger"), prefix("entry")]
        );
        // `HTM` followed by a lowercase letter would split before `M`, so the
        // last unit shrinks to what every context agrees on.
        assert_eq!(units(b"parseHTM"), vec![]);
        assert_eq!(units(b"parse_HTMLX"), vec![prefix("html")]);
    }

    #[test]
    fn digits_split_runs() {
        assert_eq!(units(b"abc123def"), vec![exact("123"), prefix("def")]);
    }

    /// The discriminating check: every unit is found in a document made of
    /// the literal and arbitrary neighbours, through the real tokenizer.
    #[test]
    fn every_unit_is_a_token_of_any_document_containing_the_literal() {
        let literals = [
            "deserialize_ledger",
            "foo_bar_baz",
            "parseLedgerEntry",
            "parse_HTMLX",
            "abc123def",
            "x.y_zz.wwww",
            "__init__",
            "ÉcoleNormale_x",
        ];
        let neighbours = ["", " ", "q", "Q", "QQ", "Qq", "7", "_", "é", "Éa", "aB"];
        for literal in literals {
            for unit in units(literal.as_bytes()) {
                for left in neighbours {
                    for right in neighbours {
                        let doc = format!("{left}{literal}{right}");
                        let mut found = false;
                        tokenize(doc.as_bytes(), &mut Default::default(), |t| {
                            found |= match &unit {
                                Unit::Exact(u) => t.bytes == u.as_slice(),
                                Unit::Prefix(u) => t.bytes.starts_with(u),
                            };
                        });
                        assert!(found, "{unit:?} of {literal} missing from {doc:?}");
                    }
                }
            }
        }
    }
}
