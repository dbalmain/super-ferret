//! Substring constraints for folded required literals. The literal facts
//! preserve ASCII-folded bytes but not original case, so unit boundaries
//! cannot safely drive exact or prefix term lookups.

/// The shortest substring worth a dictionary pass.
pub(super) const MIN_SUBSTRING: usize = 4;

/// The lowercased word runs of `literal`, each of which lies inside one
/// whole-run term of any document containing the literal in any case: a
/// whole run is lowercased character by character, so its term contains the
/// lowercase of every piece of it. Sigma is left out, the one character whose
/// lowercase depends on its neighbours. A term capped at 64 bytes, or dropped
/// by T1, can still lose it: the caller admits those documents separately.
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
        assert_eq!(
            substrings(b"deserializeledger"),
            vec![v("deserializeledger")]
        );
        assert_eq!(substrings(b"abc"), Vec::<Vec<u8>>::new());
        assert_eq!(substrings(b"\xffabcd"), Vec::<Vec<u8>>::new());
        assert_eq!(
            substrings("Kelvin\u{212a}x".as_bytes()),
            vec![v("kelvinkx")]
        );
    }

    /// A document holding the literal in any case and any context has a
    /// whole-run term containing each substring, unless its token passes
    /// the cap (the fallback's case).
    #[test]
    fn every_substring_is_inside_a_term_of_any_containing_document() {
        let literals = ["cachevalues", "a.config_value(", "x9y_z", "École_ab"];
        let neighbours = ["", " ", "q", "Q", "7", "_", "é", "."];
        for literal in literals {
            for spelling in [
                literal.to_string(),
                literal.to_uppercase(),
                literal.to_lowercase(),
            ] {
                for left in neighbours {
                    for right in neighbours {
                        let doc = format!("{left}{spelling}{right}");
                        let mut terms = Vec::new();
                        ferret_text::tokenize(doc.as_bytes(), &mut Default::default(), |t| {
                            terms.push(ferret_text::cap(t.bytes).to_vec());
                        });
                        for needle in substrings(literal.as_bytes()) {
                            assert!(
                                terms
                                    .iter()
                                    .any(|t| t.windows(needle.len()).any(|w| w == needle)),
                                "{needle:?} not in {doc:?}"
                            );
                        }
                    }
                }
            }
        }
    }
}
