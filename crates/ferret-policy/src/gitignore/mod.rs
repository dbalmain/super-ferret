//! Gitignore line parsing and a last-match-wins index over basename patterns.
//!
//! `pattern` owns parsing one pattern, matching one component, and stepping
//! an anchored pattern through directories. This module parses a file's lines
//! and indexes one directory's basename rules, keeping their positions while
//! partitioning common shapes into fast lookup buckets. Tests
//! use Git 2.54 only as a black-box oracle; no Git or third-party matcher
//! source or tests are used.

mod fxhash;
mod pattern;

pub(crate) use fxhash::FxHashMap;
pub(crate) use pattern::{Pattern, RuleText};

/// What the last matching rule says about an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Match {
    Ignore,
    Whitelist,
}

/// One invalid line, omitted from the rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LineError {
    pub(crate) line: usize,
    pub(crate) pattern: String,
    pub(crate) detail: String,
}

/// Parses every line of one ignore file, in source order, and returns the
/// invalid ones separately. A bad line never prevents another line in the
/// same file from applying.
pub(crate) fn parse(text: &str) -> (Vec<Pattern>, Vec<LineError>) {
    let mut patterns = Vec::new();
    let mut errors = Vec::new();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (index, line) in text.split('\n').enumerate() {
        let original = line.strip_suffix('\r').unwrap_or(line);
        let original = original.split('\0').next().unwrap_or(original);
        match Pattern::compile(index, original) {
            Ok(Some(pattern)) => patterns.push(pattern),
            Ok(None) => {}
            Err(detail) => errors.push(LineError {
                line: index + 1,
                pattern: original.to_owned(),
                detail,
            }),
        }
    }
    (patterns, errors)
}

/// Basename patterns indexed for last-match-wins. Pattern positions let fast
/// buckets and general patterns jointly implement it: each bucket is appended
/// in position order, scanned backwards, and cut off below the best position
/// another bucket already found.
#[derive(Clone, Debug, Default)]
pub(crate) struct Gitignore {
    literals: FxHashMap<Vec<u8>, Vec<FastMatch>>,
    extensions: FxHashMap<Vec<u8>, Vec<FastMatch>>,
    prefixes: Vec<ByteFastMatch>,
    suffixes: Vec<ByteFastMatch>,
    contains: Vec<ByteFastMatch>,
    fixed_suffixes: Vec<FixedSuffixMatch>,
    general: Vec<Pattern>,
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
    /// Indexes basename patterns. Last-match order is by each pattern's
    /// index, so callers may pass them in any order.
    pub(crate) fn from_patterns(patterns: impl IntoIterator<Item = Pattern>) -> Self {
        let mut patterns: Vec<Pattern> = patterns.into_iter().collect();
        patterns.sort_by_key(|pattern| pattern.index);
        let mut matcher = Self::default();
        for pattern in patterns {
            matcher.push(pattern);
        }
        matcher
    }

    /// The index and action of the last pattern that matches entry `name`.
    pub(crate) fn best(&self, name: &[u8], is_dir: bool) -> Option<(usize, Match)> {
        let mut best = self
            .literals
            .get(name)
            .and_then(|entries| latest_fast(entries, is_dir));
        if let Some(dot) = name.iter().rposition(|byte| *byte == b'.')
            && let Some(candidate) = self
                .extensions
                .get(&name[dot..])
                .and_then(|entries| latest_fast(entries, is_dir))
            && best.is_none_or(|current| candidate.index > current.index)
        {
            best = Some(candidate);
        }
        scan_byte_fast(&self.prefixes, name, is_dir, &mut best, |name, bytes| {
            name.starts_with(bytes)
        });
        scan_byte_fast(&self.suffixes, name, is_dir, &mut best, |name, bytes| {
            name.ends_with(bytes)
        });
        scan_byte_fast(&self.contains, name, is_dir, &mut best, |name, bytes| {
            name.windows(bytes.len()).any(|window| window == bytes)
        });
        scan_fixed_suffixes(&self.fixed_suffixes, name, is_dir, &mut best);
        scan_patterns(&self.general, name, is_dir, &mut best);
        best.map(|candidate| (candidate.index, candidate.result))
    }

    fn push(&mut self, pattern: Pattern) {
        debug_assert!(pattern.basename_only);
        let fast = FastMatch {
            index: pattern.index,
            result: pattern.result,
            directory_only: pattern.directory_only,
        };
        if let Some(literal) = pattern.literal_basename() {
            self.literals.entry(literal).or_default().push(fast);
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
        } else {
            self.general.push(pattern);
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

fn scan_patterns(patterns: &[Pattern], name: &[u8], is_dir: bool, best: &mut Option<FastMatch>) {
    for pattern in patterns.iter().rev() {
        if best.is_some_and(|candidate| pattern.index < candidate.index) {
            break;
        }
        if pattern.matches_name(name, is_dir) {
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
