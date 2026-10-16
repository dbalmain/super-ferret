//! Ripgrep-style override globs and hidden filtering over ferret's selected
//! universe. Component matching shares the policy matcher; brace alternatives
//! are expanded at compilation, independently of ignore-file syntax.

use crate::gitignore::Pattern;

/// An override glob that cannot be compiled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrepGlobError {
    Alternatives,
    Nesting,
    UnopenedAlternation,
    UnclosedAlternation,
    UnclosedClass,
    DanglingEscape,
    Pattern(String),
}

impl std::fmt::Display for GrepGlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Alternatives => f.write_str("too many glob alternatives"),
            Self::Nesting => f.write_str("glob nesting limit exceeded"),
            Self::UnopenedAlternation => f.write_str("unopened glob alternation"),
            Self::UnclosedAlternation => f.write_str("unclosed glob alternation"),
            Self::UnclosedClass => f.write_str("unclosed glob character class"),
            Self::DanglingEscape => f.write_str("dangling glob escape"),
            Self::Pattern(detail) => f.write_str(detail),
        }
    }
}

impl std::error::Error for GrepGlobError {}

/// Ordered command-line overrides. A positive rule admits held hidden names,
/// but cannot restore a name excluded from the catalog by ferret policy.
#[derive(Clone, Debug, Default)]
pub struct GrepSelection {
    rules: Vec<(bool, Vec<Pattern>)>,
    positive: bool,
    hidden: bool,
}

impl GrepSelection {
    /// Reveals held hidden names without overriding exclusions or globs.
    pub fn with_hidden(mut self, hidden: bool) -> Self {
        self.hidden = hidden;
        self
    }

    /// Adds one `-g` operand. Leading `!` excludes; the last matching rule
    /// wins. Invalid globs fail before any content is searched.
    pub fn push(&mut self, glob: &str) -> Result<(), GrepGlobError> {
        let (include, body) = glob
            .strip_prefix('!')
            .map_or((true, glob), |body| (false, body));
        if body.is_empty() || body.starts_with('#') {
            return Ok(());
        }
        validate(body)?;
        let mut expanded = Vec::new();
        braces(body, &mut expanded)?;
        let patterns = expanded
            .into_iter()
            .filter_map(|body| {
                // The outer override already consumed its one negation marker.
                let body = if body.starts_with('!') {
                    format!("\\{body}")
                } else {
                    body
                };
                Pattern::compile_override(&body).transpose()
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(GrepGlobError::Pattern)?;
        self.positive |= include && !patterns.is_empty();
        self.rules.push((include, patterns));
        Ok(())
    }

    /// Whether a file path relative to cwd is selected. `depth` is the walk
    /// depth below its explicit start; that start and its ancestors bypass
    /// overrides and hidden filtering. Descendant directories must each pass
    /// before a file can be selected.
    pub fn allows(&self, path: &[u8], depth: usize) -> bool {
        let components: Vec<_> = path
            .split(|&b| b == b'/')
            .filter(|p| !p.is_empty() && *p != b".")
            .collect();
        for end in components.len().saturating_sub(depth) + 1..components.len() {
            let matched = self.decision(&components[..end], true);
            if matched == Some(false)
                || (!self.hidden && components[end - 1].starts_with(b".") && matched != Some(true))
            {
                return false;
            }
        }
        self.decision(&components, false).unwrap_or_else(|| {
            !self.positive
                && (self.hidden || !components.last().is_some_and(|p| p.starts_with(b".")))
        })
    }

    /// Prunes excluded or held-hidden directories before a source descends.
    /// The explicit directory start itself bypasses these filters.
    pub fn descend(&self, path: &[u8], depth: usize) -> bool {
        if depth == 0 {
            return true;
        }
        let components: Vec<_> = path
            .split(|&b| b == b'/')
            .filter(|p| !p.is_empty() && *p != b".")
            .collect();
        let matched = self.decision(&components, true);
        matched != Some(false)
            && (matched == Some(true)
                || self.hidden
                || !components.last().is_some_and(|p| p.starts_with(b".")))
    }

    fn decision(&self, components: &[&[u8]], directory: bool) -> Option<bool> {
        self.rules.iter().rev().find_map(|(include, alternatives)| {
            alternatives
                .iter()
                .any(|p| p.matches_override(components, directory))
                .then_some(*include)
        })
    }
}

fn braces(pattern: &str, out: &mut Vec<String>) -> Result<(), GrepGlobError> {
    if out.len() >= 1024 {
        return Err(GrepGlobError::Alternatives);
    }
    let mut escaped = false;
    let mut skip_until = 0;
    let mut start = None;
    let mut depth = 0;
    let mut comma = Vec::new();
    for (at, ch) in pattern.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if at < skip_until {
            continue;
        }
        if ch == '[' {
            skip_until = class_end(pattern.as_bytes(), at)?;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '{' => {
                if depth >= 64 {
                    return Err(GrepGlobError::Nesting);
                }
                if depth == 0 {
                    start = Some(at);
                }
                depth += 1;
            }
            ',' if depth == 1 => comma.push(at),
            '}' => {
                if depth == 0 {
                    return Err(GrepGlobError::UnopenedAlternation);
                }
                depth -= 1;
                if depth == 0 {
                    let start = start.ok_or(GrepGlobError::UnopenedAlternation)?;
                    let mut left = start + 1;
                    comma.push(at);
                    for right in comma {
                        braces(
                            &format!(
                                "{}{}{}",
                                &pattern[..start],
                                &pattern[left..right],
                                &pattern[at + 1..]
                            ),
                            out,
                        )?;
                        left = right + 1;
                    }
                    return Ok(());
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(GrepGlobError::UnclosedAlternation);
    }
    out.push(pattern.into());
    Ok(())
}

fn validate(pattern: &str) -> Result<(), GrepGlobError> {
    let mut at = 0;
    let bytes = pattern.as_bytes();
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => {
                at += 2;
                if at > bytes.len() {
                    return Err(GrepGlobError::DanglingEscape);
                }
            }
            b'[' => at = class_end(bytes, at)?,
            _ => at += 1,
        }
    }
    Ok(())
}

fn class_end(bytes: &[u8], start: usize) -> Result<usize, GrepGlobError> {
    let mut at = start + 1;
    if bytes.get(at).is_some_and(|b| *b == b'!' || *b == b'^') {
        at += 1;
    }
    if bytes.get(at) == Some(&b']') {
        at += 1;
    }
    while let Some(&byte) = bytes.get(at) {
        match byte {
            b'\\' => at += 2,
            b']' => return Ok(at + 1),
            _ => at += 1,
        }
    }
    Err(GrepGlobError::UnclosedClass)
}
