//! Gitignore pattern-list compilation and last-match-wins dispatch.
//!
//! `pattern` owns parsing and matching one pattern. This module keeps pattern
//! positions while partitioning common shapes into fast lookup buckets. Tests
//! use Git 2.54 only as a black-box oracle; no Git or third-party matcher source
//! or tests are used.

mod pattern;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

pub(crate) use pattern::Pattern;

/// The result of matching one path against one ignore file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Match {
    None,
    Ignore,
    Whitelist,
}

/// One invalid line, omitted from the compiled matcher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LineError {
    pub(crate) line: usize,
    pub(crate) pattern: String,
    pub(crate) detail: String,
}

/// A compiled ignore file. Pattern positions let fast buckets and general
/// patterns jointly implement last-match-wins.
#[derive(Clone, Debug, Default)]
pub(crate) struct Gitignore {
    // Each bucket is appended in source order. Matching scans it backwards and
    // compares its newest result with the best index found in other buckets.
    literals: HashMap<Vec<u8>, Vec<FastMatch>>,
    paths: BTreeMap<u16, BTreeMap<Vec<u8>, Vec<FastMatch>>>,
    extensions: HashMap<Vec<u8>, Vec<FastMatch>>,
    prefixes: Vec<ByteFastMatch>,
    suffixes: Vec<ByteFastMatch>,
    contains: Vec<ByteFastMatch>,
    fixed_suffixes: Vec<FixedSuffixMatch>,
    basename_general: Vec<Pattern>,
    anchored_any: Vec<Pattern>,
    anchored_by_prefix2: HashMap<u16, Vec<Pattern>>,
    anchored_by_first: HashMap<u8, Vec<Pattern>>,
}

#[derive(Clone, Copy, Debug)]
struct FastMatch {
    index: usize,
    result: Match,
    directory_only: bool,
}

#[derive(Clone, Debug)]
struct ByteFastMatch {
    bytes: Vec<u8>,
    action: FastMatch,
}

#[derive(Clone, Debug)]
struct FixedSuffixMatch {
    pattern: Pattern,
    width: usize,
}

impl Gitignore {
    /// Compiles all valid lines and returns invalid ones separately. A bad line
    /// never prevents another line in the same file from applying.
    pub(crate) fn compile(text: &str) -> (Self, Vec<LineError>) {
        let (matcher, errors, _) = Self::compile_inner(text, false);
        (matcher, errors)
    }

    /// Compiles each normalized source line once. The returned patterns let
    /// policy derive traversal reincludes from the same parse as matching.
    pub(crate) fn compile_patterns(text: &str) -> (Self, Vec<LineError>, Vec<Pattern>) {
        Self::compile_inner(text, true)
    }

    fn compile_inner(text: &str, retain_patterns: bool) -> (Self, Vec<LineError>, Vec<Pattern>) {
        let mut matcher = Self::default();
        let mut errors = Vec::new();
        let mut patterns = Vec::new();
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        for (index, line) in text.split('\n').enumerate() {
            let original = line.strip_suffix('\r').unwrap_or(line);
            let original = original.split('\0').next().unwrap_or(original);
            match Pattern::compile(index, original) {
                Ok(Some(pattern)) => {
                    if retain_patterns {
                        patterns.push(pattern.clone());
                    }
                    matcher.push(pattern);
                }
                Ok(None) => {}
                Err(detail) => errors.push(LineError {
                    line: index + 1,
                    pattern: original.to_owned(),
                    detail,
                }),
            }
        }
        (matcher, errors, patterns)
    }

    /// Returns the last matching line's action for `path` itself. Exclusion by
    /// a matching parent directory is deliberately the walker's responsibility.
    pub(crate) fn matched(&self, path: &Path, is_dir: bool) -> Match {
        let bytes = path.as_os_str().as_encoded_bytes();
        let basename = bytes
            .iter()
            .rposition(|byte| *byte == b'/')
            .map_or(bytes, |slash| &bytes[slash + 1..]);

        let mut best = self
            .literals
            .get(basename)
            .and_then(|entries| latest_fast(entries, is_dir));
        if let Some(candidate) = bytes
            .first()
            .map(|first| u16::from_ne_bytes([*first, bytes.get(1).copied().unwrap_or(0)]))
            .and_then(|prefix| self.paths.get(&prefix))
            .and_then(|paths| paths.get(bytes))
            .and_then(|entries| latest_fast(entries, is_dir))
            && best.is_none_or(|current| candidate.index > current.index)
        {
            best = Some(candidate);
        }
        if let Some(dot) = basename.iter().rposition(|byte| *byte == b'.')
            && let Some(candidate) = self
                .extensions
                .get(&basename[dot..])
                .and_then(|entries| latest_fast(entries, is_dir))
            && best.is_none_or(|current| candidate.index > current.index)
        {
            best = Some(candidate);
        }
        scan_byte_fast(
            &self.prefixes,
            basename,
            is_dir,
            &mut best,
            |name, bytes| name.starts_with(bytes),
        );
        scan_byte_fast(
            &self.suffixes,
            basename,
            is_dir,
            &mut best,
            |name, bytes| name.ends_with(bytes),
        );
        scan_byte_fast(
            &self.contains,
            basename,
            is_dir,
            &mut best,
            |name, bytes| name.windows(bytes.len()).any(|window| window == bytes),
        );
        scan_fixed_suffixes(&self.fixed_suffixes, basename, is_dir, &mut best);
        let prefixed = bytes
            .first()
            .and_then(|first| self.anchored_by_first.get(first))
            .map_or(&[][..], Vec::as_slice);
        let prefix2 = bytes
            .get(..2)
            .map(|prefix| u16::from_ne_bytes([prefix[0], prefix[1]]))
            .and_then(|prefix| self.anchored_by_prefix2.get(&prefix))
            .map_or(&[][..], Vec::as_slice);
        scan_patterns(&self.basename_general, bytes, basename, is_dir, &mut best);
        scan_patterns(&self.anchored_any, bytes, basename, is_dir, &mut best);
        scan_patterns(prefixed, bytes, basename, is_dir, &mut best);
        scan_patterns(prefix2, bytes, basename, is_dir, &mut best);
        best.map_or(Match::None, |candidate| candidate.result)
    }

    fn push(&mut self, pattern: Pattern) {
        let fast = FastMatch {
            index: pattern.index,
            result: pattern.result,
            directory_only: pattern.directory_only,
        };
        if let Some(literal) = pattern.literal_basename() {
            self.literals.entry(literal).or_default().push(fast);
        } else if let Some(path) = pattern.literal_path() {
            let prefix = u16::from_ne_bytes([path[0], path.get(1).copied().unwrap_or(0)]);
            self.paths
                .entry(prefix)
                .or_default()
                .entry(path)
                .or_default()
                .push(fast);
        } else if let Some(extension) = pattern.simple_extension() {
            self.extensions.entry(extension).or_default().push(fast);
        } else if let Some(prefix) = pattern.basename_prefix() {
            self.prefixes.push(ByteFastMatch {
                bytes: prefix,
                action: fast,
            });
        } else if let Some(suffix) = pattern.basename_suffix() {
            self.suffixes.push(ByteFastMatch {
                bytes: suffix,
                action: fast,
            });
        } else if let Some(needle) = pattern.basename_contains() {
            self.contains.push(ByteFastMatch {
                bytes: needle,
                action: fast,
            });
        } else if let Some(width) = pattern.fixed_basename_suffix_width() {
            self.fixed_suffixes
                .push(FixedSuffixMatch { pattern, width });
        } else if pattern.basename_only {
            self.basename_general.push(pattern);
        } else if let Some(prefix) = pattern.first_literal_prefix2() {
            self.anchored_by_prefix2
                .entry(prefix)
                .or_default()
                .push(pattern);
        } else if let Some(first) = pattern.first_literal_byte() {
            self.anchored_by_first
                .entry(first)
                .or_default()
                .push(pattern);
        } else {
            self.anchored_any.push(pattern);
        }
    }
}

fn scan_fixed_suffixes(
    patterns: &[FixedSuffixMatch],
    basename: &[u8],
    is_dir: bool,
    best: &mut Option<FastMatch>,
) {
    for fixed in patterns.iter().rev() {
        let pattern = &fixed.pattern;
        if best.is_some_and(|candidate| pattern.index < candidate.index) {
            break;
        }
        if pattern.matches_fixed_basename_suffix(basename, is_dir, fixed.width) {
            *best = Some(FastMatch {
                index: pattern.index,
                result: pattern.result,
                directory_only: pattern.directory_only,
            });
            break;
        }
    }
}

fn scan_byte_fast(
    patterns: &[ByteFastMatch],
    basename: &[u8],
    is_dir: bool,
    best: &mut Option<FastMatch>,
    matches: impl Fn(&[u8], &[u8]) -> bool,
) {
    for pattern in patterns.iter().rev() {
        if best.is_some_and(|candidate| pattern.action.index < candidate.index) {
            break;
        }
        if (!pattern.action.directory_only || is_dir) && matches(basename, &pattern.bytes) {
            *best = Some(pattern.action);
            break;
        }
    }
}

fn scan_patterns(
    patterns: &[Pattern],
    path: &[u8],
    basename: &[u8],
    is_dir: bool,
    best: &mut Option<FastMatch>,
) {
    for pattern in patterns.iter().rev() {
        if best.is_some_and(|candidate| pattern.index < candidate.index) {
            break;
        }
        if pattern.matches(path, basename, is_dir) {
            *best = Some(FastMatch {
                index: pattern.index,
                result: pattern.result,
                directory_only: pattern.directory_only,
            });
            break;
        }
    }
}

fn latest_fast(entries: &[FastMatch], is_dir: bool) -> Option<FastMatch> {
    entries
        .iter()
        .rev()
        .copied()
        .find(|entry| !entry.directory_only || is_dir)
}

#[cfg(test)]
mod tests;
