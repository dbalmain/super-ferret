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
        if self.rules.is_empty() {
            // Without overrides only hidden filtering applies: to the name and
            // the descendant directories above it, the last `max(depth, 1)`
            // components. Checking them in place keeps the index producer,
            // which asks once per catalog entry, off the allocator:
            // collecting every component cost 43% of its time on a
            // 300k-file tree (kdiag, 2026-10-20).
            return self.hidden
                || components_from_end(path)
                    .take(depth.max(1))
                    .all(|name| !name.starts_with(b"."));
        }
        with_components(path, |components| self.allows_components(components, depth))
    }

    fn allows_components(&self, components: &[&[u8]], depth: usize) -> bool {
        for end in components.len().saturating_sub(depth) + 1..components.len() {
            let matched = self.decision(&components[..end], true);
            if matched == Some(false)
                || (!self.hidden && components[end - 1].starts_with(b".") && matched != Some(true))
            {
                return false;
            }
        }
        self.decision(components, false).unwrap_or_else(|| {
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
        if self.rules.is_empty() {
            // As in `allows`: only the directory's own name can hide it.
            return self.hidden
                || !components_from_end(path)
                    .next()
                    .is_some_and(|name| name.starts_with(b"."));
        }
        with_components(path, |components| self.descend_components(components))
    }

    fn descend_components(&self, components: &[&[u8]]) -> bool {
        let matched = self.decision(components, true);
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

/// A path's components, last first, without empty or `.` components.
fn components_from_end(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.rsplit(|&b| b == b'/')
        .filter(|p| !p.is_empty() && *p != b".")
}

/// Calls `f` with a path's components, skipping empty and `.` components.
/// Paths up to 32 components deep are split on the stack; only deeper
/// ones allocate.
fn with_components<R>(path: &[u8], f: impl FnOnce(&[&[u8]]) -> R) -> R {
    const INLINE_COMPONENTS: usize = 32;
    let mut parts = path
        .split(|&b| b == b'/')
        .filter(|p| !p.is_empty() && *p != b".");
    let mut inline: [&[u8]; INLINE_COMPONENTS] = [&[]; INLINE_COMPONENTS];
    let mut len = 0;
    for part in parts.by_ref() {
        if len == INLINE_COMPONENTS {
            let mut spilled = inline.to_vec();
            spilled.push(part);
            spilled.extend(parts);
            return f(&spilled);
        }
        inline[len] = part;
        len += 1;
    }
    f(&inline[..len])
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

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Vec<Vec<u8>> {
        let mut paths: Vec<Vec<u8>> = [
            &b""[..],
            b".",
            b"/",
            b"a",
            b".a",
            b"./a",
            b"./.a",
            b"a/.b",
            b".a/b",
            b"a//b/",
            b"a/./.b/c",
            b"/abs/.h/x/y",
            b"x/y/.z",
            b"..",
            b"a/../b",
            b"x/\xff.h/y",
            b"x/.\xff/y",
            b"\xff\xfe/.q",
        ]
        .iter()
        .map(|p| p.to_vec())
        .collect();
        // Deeper than the 32 components split on the stack, with a hidden
        // component near each end.
        for hidden in [0, 3, 38, 39] {
            let deep: Vec<String> = (0..40)
                .map(|i| {
                    if i == hidden {
                        ".h".into()
                    } else {
                        format!("d{i}")
                    }
                })
                .collect();
            paths.push(deep.join("/").into_bytes());
        }
        paths
    }

    /// The no-rules fast paths answer exactly as the general paths do, for
    /// every depth up to past the path's length.
    #[test]
    fn no_rules_fast_path_matches_the_general_path() {
        for hidden in [false, true] {
            let selection = GrepSelection::default().with_hidden(hidden);
            for path in paths() {
                for depth in (0..=6).chain([39, 40, 41, 100]) {
                    let general = with_components(&path, |c| selection.allows_components(c, depth));
                    assert_eq!(
                        selection.allows(&path, depth),
                        general,
                        "allows {:?} depth {depth} hidden {hidden}",
                        path.escape_ascii().to_string(),
                    );
                    let general =
                        depth == 0 || with_components(&path, |c| selection.descend_components(c));
                    assert_eq!(
                        selection.descend(&path, depth),
                        general,
                        "descend {:?} depth {depth} hidden {hidden}",
                        path.escape_ascii().to_string(),
                    );
                }
            }
        }
    }

    /// A rule that never matches leaves the answer to hidden filtering, so a
    /// selection holding one takes the general path and must agree with the
    /// fast path of an empty selection.
    #[test]
    fn a_rule_that_never_matches_changes_nothing() {
        for hidden in [false, true] {
            let fast = GrepSelection::default().with_hidden(hidden);
            let mut ruled = fast.clone();
            ruled.push("!no-such-name").unwrap();
            for path in paths() {
                for depth in [0, 1, 2, 3, 40] {
                    assert_eq!(fast.allows(&path, depth), ruled.allows(&path, depth));
                    assert_eq!(fast.descend(&path, depth), ruled.descend(&path, depth));
                }
            }
        }
    }

    #[test]
    fn hidden_filtering_covers_only_the_walked_components() {
        let held = GrepSelection::default();
        assert!(held.allows(b".hidden/x", 1));
        assert!(!held.allows(b".hidden/x", 2));
        assert!(!held.allows(b"a/.x", 0));
        assert!(held.allows(b"", 0));
        assert!(held.descend(b".hidden", 0));
        assert!(!held.descend(b"a/.hidden", 1));
        assert!(held.descend(b".hidden/a", 1));
        let shown = GrepSelection::default().with_hidden(true);
        assert!(shown.allows(b".hidden/x", 2));
        assert!(shown.descend(b"a/.hidden", 1));
    }
}
