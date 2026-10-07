//! The tokenizer: alphanumeric-and-underscore runs, lowercased, plus the
//! camelCase / TitleCase / snake / digit-boundary parts (DECISIONS.md D9).
//!
//! Versioned, because segments record the version that wrote them. The one
//! place tokens are defined: indexing and query parsing both call it.
//!
//! Knows nothing about files or ids.

/// Change when the token contract changes; indexes must rebuild on a change.
pub const TOKENIZER_VERSION: u32 = 1;

/// D9: whole alphanumeric/underscore runs, then identifier parts, lowercased.
/// Invalid UTF-8 bytes separate runs. Duplicates are allowed; index builders
/// deduplicate within a name before writing its postings.
pub fn tokens(bytes: &[u8], mut emit: impl FnMut(&[u8])) {
    let text = String::from_utf8_lossy(bytes);
    for run in text
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
    {
        let whole = run.to_lowercase();
        emit(whole.as_bytes());
        let chars: Vec<_> = run.char_indices().collect();
        let mut start = 0;
        for i in 0..chars.len() {
            let (at, current) = chars[i];
            let previous = i.checked_sub(1).map(|j| chars[j].1);
            let next = chars.get(i + 1).map(|&(_, c)| c);
            let boundary = current == '_'
                || previous == Some('_')
                || previous.is_some_and(|p| {
                    p.is_numeric() != current.is_numeric()
                        || p.is_lowercase() && current.is_uppercase()
                        || p.is_uppercase()
                            && current.is_uppercase()
                            && next.is_some_and(char::is_lowercase)
                });
            if boundary {
                part(&run[start..at], &whole, &mut emit);
                start = at;
            }
            if current == '_' {
                start = at + current.len_utf8();
            }
        }
        part(&run[start..], &whole, &mut emit);
    }
}

fn part(text: &str, whole: &str, emit: &mut impl FnMut(&[u8])) {
    if !text.is_empty() {
        let part = text.to_lowercase();
        if part != whole {
            emit(part.as_bytes());
        }
    }
}

/// Normalises one explicit token. Separators are not accepted as part of a
/// token query; a whole snake-case identifier remains a valid token.
pub fn normalize_token(bytes: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(bytes).ok()?;
    (!text.is_empty() && text.chars().all(|c| c.is_alphanumeric() || c == '_'))
        .then(|| text.to_lowercase().into_bytes())
}

pub fn has_token(bytes: &[u8], token: &[u8]) -> bool {
    let mut found = false;
    tokens(bytes, |value| found |= value == token);
    found
}
