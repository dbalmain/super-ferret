//! GNU find dialect translation, derived from the pinned binary. Matching is
//! over C-locale bytes and covers the entire path. Ordinary patterns use the
//! regex crate; backreferences use the bounded executor in `backtrack`.

use super::{Matcher, Regex, RegexError};

mod backtrack;
pub use backtrack::MatchLimit;

/// The regular expression syntaxes accepted by GNU find.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Dialect {
    /// GNU's default: escaped groups and alternation, unescaped + and ?.
    #[default]
    Emacs,
    /// POSIX basic, grep, sed and ed.
    Basic,
    /// Grep basic (a pattern newline is alternation).
    Grep,
    /// POSIX minimal basic (no escaped +, ? or alternation).
    MinimalBasic,
    /// POSIX extended, egrep and posix-egrep.
    Extended,
    /// Egrep and POSIX egrep (a pattern newline is alternation).
    Egrep,
    /// Historical awk (intervals are literals).
    Awk,
    /// POSIX awk (interval operators, no GNU word assertions).
    PosixAwk,
    /// GNU awk (intervals and GNU word assertions).
    GnuAwk,
}

impl Dialect {
    /// Resolves a `-regextype` operand. Unknown names are not aliases.
    pub fn from_name(name: &[u8]) -> Option<Self> {
        Some(match name {
            b"emacs" | b"findutils-default" => Self::Emacs,
            b"posix-basic" | b"sed" | b"ed" => Self::Basic,
            b"grep" => Self::Grep,
            b"egrep" | b"posix-egrep" => Self::Egrep,
            b"posix-minimal-basic" => Self::MinimalBasic,
            b"posix-extended" => Self::Extended,
            b"awk" => Self::Awk,
            b"posix-awk" => Self::PosixAwk,
            b"gnu-awk" => Self::GnuAwk,
            _ => return None,
        })
    }
}

/// A whole-path, byte-oriented GNU dialect regex.
#[derive(Clone, Debug)]
pub struct FindRegex {
    translated: String,
    engine: Engine,
}

#[derive(Clone, Debug)]
enum Engine {
    Linear(Regex),
    Backtrack(backtrack::Program),
}

impl PartialEq for FindRegex {
    fn eq(&self, other: &Self) -> bool {
        self.translated == other.translated
    }
}
impl Eq for FindRegex {}

impl FindRegex {
    /// Translates and compiles a GNU pattern over C-locale bytes.
    pub fn new(pattern: &[u8], dialect: Dialect, fold: bool) -> Result<Self, RegexError> {
        let (translated, has_reference) = translate(pattern, dialect, fold)?;
        let engine = if has_reference {
            Engine::Backtrack(backtrack::Program::new(&translated)?)
        } else {
            Engine::Linear(Regex::new(&translated, false)?)
        };
        Ok(Self { translated, engine })
    }

    /// Matches the entire path. A backreference search that exhausts its step
    /// budget returns an error; callers executing find should report it.
    pub fn try_is_match(&self, bytes: &[u8]) -> Result<bool, MatchLimit> {
        match &self.engine {
            Engine::Linear(regex) => Ok(regex.is_match(bytes)),
            Engine::Backtrack(program) => program.is_match(bytes),
        }
    }
}

impl Matcher for FindRegex {
    fn is_match(&self, bytes: &[u8]) -> bool {
        // The infallible trait treats an unfinished search as a non-match.
        // Find uses try_is_match so budget exhaustion is never silent there.
        self.try_is_match(bytes).unwrap_or(false)
    }
}

fn literal(out: &mut String, byte: u8) {
    use std::fmt::Write;
    // Hex escapes prevent Rust regex extensions from leaking into GNU syntax.
    let _ = write!(out, "\\x{byte:02x}");
}

fn translate(pattern: &[u8], dialect: Dialect, fold: bool) -> Result<(String, bool), RegexError> {
    let basic = matches!(
        dialect,
        Dialect::Emacs | Dialect::Basic | Dialect::Grep | Dialect::MinimalBasic
    );
    let mut out = String::from(match (fold, dialect == Dialect::Emacs) {
        (true, true) => "(?i-u)\\A(?:",
        (false, true) => "(?-u)\\A(?:",
        (true, false) => "(?is-u)\\A(?:",
        (false, false) => "(?s-u)\\A(?:",
    });
    let mut at = 0;
    let mut branch_start = true;
    let mut can_repeat = false;
    let mut atom_start = out.len();
    let mut repeated = false;
    let mut groups = Vec::new();
    let mut group_count = 0;
    let mut closed = [false; 9];
    let mut has_reference = false;
    while let Some(&byte) = pattern.get(at) {
        if byte == b'[' {
            atom_start = out.len();
            can_repeat = true;
            repeated = false;
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
            b'\n' => matches!(dialect, Dialect::Grep | Dialect::Egrep),
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
        if escaped && matches!(byte, b'1'..=b'9') && dialect != Dialect::Awk {
            if !closed[usize::from(byte - b'1')] {
                return Err(RegexError("invalid backreference".into()));
            }
            atom_start = out.len();
            out.push('\\');
            out.push(byte as char);
            has_reference = true;
            can_repeat = true;
            repeated = false;
        } else if operator && matches!(byte, b'*' | b'+' | b'?' | b'{') {
            if !can_repeat {
                if matches!(dialect, Dialect::Extended | Dialect::PosixAwk)
                    || (byte == b'{' && dialect == Dialect::Basic)
                {
                    return Err(RegexError("invalid preceding regular expression".into()));
                }
                if dialect != Dialect::Egrep {
                    atom_start = out.len();
                    literal(&mut out, byte);
                    can_repeat = true;
                }
                repeated = false;
            } else {
                let repetition = if byte == b'{' {
                    interval(pattern, at, basic, dialect)?
                } else {
                    Some((String::from(byte as char), at + 1))
                };
                if let Some((repetition, next)) = repetition {
                    if repeated && dialect == Dialect::Basic {
                        return Err(RegexError("invalid preceding regular expression".into()));
                    }
                    // GNU repetition has no lazy/possessive suffixes. Nest
                    // successive operators instead of leaking Rust's syntax.
                    if repeated {
                        out.insert_str(atom_start, "(?:");
                        out.push(')');
                    }
                    out.push_str(&repetition);
                    at = next;
                    repeated = true;
                    branch_start = false;
                    continue;
                }
                atom_start = out.len();
                literal(&mut out, byte);
                repeated = false;
            }
        } else if operator {
            match byte {
                b'(' => {
                    group_count += 1;
                    groups.push((group_count, out.len()));
                    can_repeat = false;
                }
                b')' => {
                    let (number, start) = groups
                        .pop()
                        .ok_or_else(|| RegexError("unmatched group closer".into()))?;
                    if number <= 9 {
                        closed[number - 1] = true;
                    }
                    atom_start = start;
                    can_repeat = true;
                }
                b'|' | b'\n' | b'^' | b'$' => can_repeat = false,
                _ => {
                    atom_start = out.len();
                    can_repeat = true;
                }
            }
            repeated = false;
            if byte == b'}' {
                literal(&mut out, byte);
            } else {
                out.push(if byte == b'\n' { '|' } else { byte as char });
            }
        } else if escaped
            && matches!(byte, b'w' | b'W' | b'b' | b'B')
            && !matches!(dialect, Dialect::Awk | Dialect::PosixAwk)
        {
            atom_start = out.len();
            can_repeat = matches!(byte, b'w' | b'W');
            repeated = false;
            out.push('\\');
            out.push(byte as char);
        } else if escaped
            && matches!(byte, b'<' | b'>')
            && !matches!(dialect, Dialect::Awk | Dialect::PosixAwk)
        {
            return Err(RegexError(
                "GNU word-start/end assertions are not supported".into(),
            ));
        } else {
            atom_start = out.len();
            can_repeat = true;
            repeated = false;
            literal(&mut out, byte);
        }
        branch_start = operator && matches!(byte, b'(' | b'|' | b'\n');
        at += 1;
    }
    if !groups.is_empty() {
        return Err(RegexError("unclosed group".into()));
    }
    out.push_str(")\\z");
    Ok((out, has_reference))
}

// Malformed intervals are literal in the permissive extended dialects, but
// reversed/oversized numeric bounds and an empty interval always diagnose.
fn interval(
    pattern: &[u8],
    at: usize,
    basic: bool,
    dialect: Dialect,
) -> Result<Option<(String, usize)>, RegexError> {
    let close: &[u8] = if basic { b"\\}" } else { b"}" };
    let tail = &pattern[at + 1..];
    let strict = basic || dialect == Dialect::Extended;
    let Some(end) = tail.windows(close.len()).position(|pair| pair == close) else {
        return if strict {
            Err(RegexError("unmatched interval opener".into()))
        } else {
            Ok(None)
        };
    };
    let content = &tail[..end];
    if content.is_empty() {
        return Err(RegexError("empty interval".into()));
    }
    let fields: Vec<_> = content.split(|byte| *byte == b',').collect();
    if fields.len() > 2
        || fields
            .iter()
            .any(|field| field.iter().any(|byte| !byte.is_ascii_digit()))
    {
        return if strict {
            Err(RegexError("invalid interval content".into()))
        } else {
            Ok(None)
        };
    }
    let number = |field: &[u8]| -> Result<usize, RegexError> {
        let value = field
            .iter()
            .try_fold(0usize, |value, byte| {
                value.checked_mul(10)?.checked_add(usize::from(byte - b'0'))
            })
            .ok_or_else(|| RegexError("interval bound too large".into()))?;
        if value > 32767 {
            return Err(RegexError("interval bound too large".into()));
        }
        Ok(value)
    };
    let min = number(fields[0])?;
    let max = if fields.len() == 1 {
        Some(min)
    } else if fields[1].is_empty() {
        None
    } else {
        Some(number(fields[1])?)
    };
    if max.is_some_and(|max| max < min) {
        return Err(RegexError("reversed interval bounds".into()));
    }
    let repetition = match max {
        Some(max) if min == max => format!("{{{min}}}"),
        Some(max) => format!("{{{min},{max}}}"),
        None => format!("{{{min},}}"),
    };
    Ok(Some((repetition, at + end + close.len() + 1)))
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
        if byte == b'[' && pattern.get(at + 1).is_some_and(|byte| b".=".contains(byte)) {
            let delimiter = pattern[at + 1];
            let end = pattern[at + 2..]
                .windows(2)
                .position(|pair| pair == [delimiter, b']'])
                .ok_or_else(|| RegexError("unclosed collating symbol".into()))?
                + at
                + 2;
            let symbol = &pattern[at + 2..end];
            if symbol.len() != 1 || !symbol[0].is_ascii() {
                return Err(RegexError(
                    "only single-byte C-locale collating symbols are supported".into(),
                ));
            }
            literal(out, symbol[0]);
            at = end + 2;
            continue;
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
        if byte == b'\\' && matches!(dialect, Dialect::Awk | Dialect::PosixAwk | Dialect::GnuAwk) {
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
