//! Gitignore pattern-list compilation and last-match-wins dispatch.
//!
//! `pattern` owns parsing and matching one pattern. This module keeps pattern
//! positions while partitioning common shapes into fast lookup buckets. Tests
//! use Git 2.54 only as a black-box oracle; no Git or third-party matcher source
//! or tests are used.

mod pattern;

use std::collections::HashMap;
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
    literals: HashMap<Vec<u8>, Vec<FastMatch>>,
    extensions: HashMap<Vec<u8>, Vec<FastMatch>>,
    basename_general: Vec<Pattern>,
    anchored_any: Vec<Pattern>,
    anchored_by_first: HashMap<u8, Vec<Pattern>>,
}

#[derive(Clone, Copy, Debug)]
struct FastMatch {
    index: usize,
    result: Match,
    directory_only: bool,
}

impl Gitignore {
    /// Compiles all valid lines and returns invalid ones separately. A bad line
    /// never prevents another line in the same file from applying.
    pub(crate) fn compile(text: &str) -> (Self, Vec<LineError>) {
        let mut matcher = Self::default();
        let mut errors = Vec::new();
        for (index, original) in text.lines().enumerate() {
            match Pattern::compile(index, original) {
                Ok(Some(pattern)) => matcher.push(pattern),
                Ok(None) => {}
                Err(detail) => errors.push(LineError {
                    line: index + 1,
                    pattern: original.to_owned(),
                    detail,
                }),
            }
        }
        (matcher, errors)
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
        if let Some(dot) = basename.iter().rposition(|byte| *byte == b'.')
            && let Some(candidate) = self
                .extensions
                .get(&basename[dot..])
                .and_then(|entries| latest_fast(entries, is_dir))
            && best.is_none_or(|current| candidate.index > current.index)
        {
            best = Some(candidate);
        }

        let prefixed = bytes
            .first()
            .and_then(|first| self.anchored_by_first.get(first))
            .map_or(&[][..], Vec::as_slice);
        let groups = [
            self.basename_general.as_slice(),
            self.anchored_any.as_slice(),
            prefixed,
        ];
        let mut positions = groups.map(<[Pattern]>::len);
        while let Some(group) = positions
            .iter()
            .enumerate()
            .filter(|(_, position)| **position != 0)
            .max_by_key(|(group, position)| groups[*group][**position - 1].index)
            .map(|(group, _)| group)
        {
            positions[group] -= 1;
            let pattern = &groups[group][positions[group]];
            if best.is_some_and(|candidate| pattern.index < candidate.index) {
                break;
            }
            if pattern.matches(bytes, basename, is_dir) {
                return pattern.result;
            }
        }
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
        } else if let Some(extension) = pattern.simple_extension() {
            self.extensions.entry(extension).or_default().push(fast);
        } else if pattern.basename_only {
            self.basename_general.push(pattern);
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

fn latest_fast(entries: &[FastMatch], is_dir: bool) -> Option<FastMatch> {
    entries
        .iter()
        .rev()
        .copied()
        .find(|entry| !entry.directory_only || is_dir)
}

#[cfg(test)]
mod tests;
