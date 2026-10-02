//! GNU find dialect translation, derived from the pinned binary. Matching is
//! over C-locale bytes and covers the entire path. Backreferences are refused
//! because the linear-time regex executor cannot represent them.

use super::{Matcher, Regex, RegexError};

/// The regular expression syntaxes accepted by GNU find.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Dialect {
    /// GNU's default: escaped groups and alternation, unescaped + and ?.
    #[default]
    Emacs,
    /// POSIX basic, grep, sed and ed.
    Basic,
    /// POSIX minimal basic (no escaped +, ? or alternation).
    MinimalBasic,
    /// POSIX extended, egrep and posix-egrep.
    Extended,
    /// Historical awk (intervals are literals).
    Awk,
    /// POSIX awk and GNU awk (interval operators).
    PosixAwk,
}

impl Dialect {
    /// Resolves a `-regextype` operand. Unknown names are not aliases.
    pub fn from_name(name: &[u8]) -> Option<Self> {
        Some(match name {
            b"emacs" | b"findutils-default" => Self::Emacs,
            b"posix-basic" | b"grep" | b"sed" | b"ed" => Self::Basic,
            b"posix-minimal-basic" => Self::MinimalBasic,
            b"posix-extended" | b"posix-egrep" | b"egrep" => Self::Extended,
            b"awk" => Self::Awk,
            b"posix-awk" | b"gnu-awk" => Self::PosixAwk,
            _ => return None,
        })
    }
}

/// A whole-path, byte-oriented GNU dialect regex.
#[derive(Clone, Debug)]
pub struct FindRegex {
    translated: String,
    regex: Regex,
}

impl PartialEq for FindRegex {
    fn eq(&self, other: &Self) -> bool {
        self.translated == other.translated
    }
}
impl Eq for FindRegex {}

impl FindRegex {
    /// Translates and compiles a GNU pattern. Unsupported backreferences and
    /// GNU word-start/end assertions return a diagnostic instead of guessing.
    pub fn new(pattern: &[u8], dialect: Dialect, fold: bool) -> Result<Self, RegexError> {
        let translated = translate(pattern, dialect, fold)?;
        let regex = Regex::new(&translated, false)?;
        Ok(Self { translated, regex })
    }
}

impl Matcher for FindRegex {
    fn is_match(&self, bytes: &[u8]) -> bool {
        self.regex.is_match(bytes)
    }
}

fn literal(out: &mut String, byte: u8) {
    use std::fmt::Write;
    // Hex escapes prevent Rust regex extensions from leaking into GNU syntax.
    let _ = write!(out, "\\x{byte:02x}");
}

fn translate(pattern: &[u8], dialect: Dialect, fold: bool) -> Result<String, RegexError> {
    let basic = matches!(
        dialect,
        Dialect::Emacs | Dialect::Basic | Dialect::MinimalBasic
    );
    let mut out = String::from(if fold {
        "(?is-u)\\A(?:"
    } else {
        "(?s-u)\\A(?:"
    });
    let mut at = 0;
    let mut branch_start = true;
    while let Some(&byte) = pattern.get(at) {
        if byte == b'[' {
            at = bracket(pattern, at, dialect, &mut out)?;
            branch_start = false;
            continue;
        }
        let escaped = byte == b'\\';
        let byte = if escaped {
            at += 1;
            *pattern
                .get(at)
                .ok_or_else(|| RegexError("trailing backslash in regex".into()))?
        } else {
            byte
        };
        let operator = match byte {
            b'(' | b')' => escaped == basic,
            b'|' => {
                if basic {
                    escaped && dialect != Dialect::MinimalBasic
                } else {
                    !escaped
                }
            }
            b'+' | b'?' => {
                if dialect == Dialect::Emacs || !basic {
                    !escaped
                } else {
                    escaped && dialect != Dialect::MinimalBasic
                }
            }
            b'{' | b'}' => dialect != Dialect::Awk && escaped == basic,
            b'*' | b'.' => !escaped,
            b'^' => !escaped && (!basic || branch_start),
            b'$' => {
                !escaped
                    && (!basic
                        || at + 1 == pattern.len()
                        || pattern[at + 1..].starts_with(b"\\)")
                        || pattern[at + 1..].starts_with(b"\\|"))
            }
            _ => false,
        };
        if escaped && byte.is_ascii_digit() && byte != b'0' && dialect != Dialect::Awk {
            return Err(RegexError(
                "backreferences are not supported by the find regex executor".into(),
            ));
        }
        if operator && byte == b'{' {
            let tail = &pattern[at + 1..];
            let length = tail
                .iter()
                .take_while(|b| b.is_ascii_digit() || **b == b',')
                .count();
            let close: &[u8] = if basic { b"\\}" } else { b"}" };
            if length > 0 && tail[length..].starts_with(close) {
                out.push('{');
                out.push_str(
                    std::str::from_utf8(&tail[..length]).map_err(|e| RegexError(e.to_string()))?,
                );
                out.push('}');
                at += length + close.len() + 1;
                branch_start = false;
                continue;
            }
            literal(&mut out, byte);
        } else if operator {
            out.push(byte as char);
        } else if escaped
            && matches!(byte, b'w' | b'W' | b'b' | b'B')
            && !matches!(dialect, Dialect::Awk | Dialect::PosixAwk)
        {
            out.push('\\');
            out.push(byte as char);
        } else if escaped
            && matches!(byte, b'<' | b'>')
            && !matches!(dialect, Dialect::Awk | Dialect::PosixAwk)
        {
            return Err(RegexError(
                "GNU word-start/end assertions are not supported".into(),
            ));
        } else if escaped
            && matches!(dialect, Dialect::Awk | Dialect::PosixAwk)
            && b"abfnrtv".contains(&byte)
        {
            literal(
                &mut out,
                match byte {
                    b'a' => 7,
                    b'b' => 8,
                    b'f' => 12,
                    b'n' => 10,
                    b'r' => 13,
                    b't' => 9,
                    _ => 11,
                },
            );
        } else {
            literal(&mut out, byte);
        }
        branch_start = operator && matches!(byte, b'(' | b'|');
        at += 1;
    }
    out.push_str(")\\z");
    Ok(out)
}

fn bracket(
    pattern: &[u8],
    start: usize,
    dialect: Dialect,
    out: &mut String,
) -> Result<usize, RegexError> {
    out.push('[');
    let mut at = start + 1;
    if pattern.get(at) == Some(&b'^') {
        out.push('^');
        at += 1;
    }
    if pattern.get(at) == Some(&b']') {
        literal(out, b']');
        at += 1;
    }
    while let Some(&byte) = pattern.get(at) {
        if byte == b']' {
            out.push(']');
            return Ok(at + 1);
        }
        if byte == b'[' && pattern.get(at + 1) == Some(&b':') {
            let end = pattern[at + 2..]
                .windows(2)
                .position(|pair| pair == b":]")
                .ok_or_else(|| RegexError("unclosed POSIX class".into()))?
                + at
                + 4;
            out.push_str(
                std::str::from_utf8(&pattern[at..end])
                    .map_err(|error| RegexError(error.to_string()))?,
            );
            at = end;
            continue;
        }
        if byte == b'\\' && matches!(dialect, Dialect::Awk | Dialect::PosixAwk) {
            at += 1;
            let byte = *pattern
                .get(at)
                .ok_or_else(|| RegexError("unclosed bracket".into()))?;
            literal(out, byte);
        } else if byte == b'-' {
            out.push('-');
        } else {
            literal(out, byte);
        }
        at += 1;
    }
    Err(RegexError("unclosed bracket expression".into()))
}

#[cfg(test)]
mod tests;
