//! Re-inclusion beneath an excluded directory: the one place the policy goes
//! beyond gitignore semantics (D13).
//!
//! Git never walks into an excluded directory, so nothing beneath it can be
//! re-included. A `.ferretignore` `!` pattern can, but only if it is
//! *anchored* (it has a `/` before its last character and does not start with
//! `**`): only then is the set of directories it could reach bounded, so the
//! walker never has to open every `node_modules/` on the chance that `!*.pdf`
//! matches inside. This module answers "could this anchored pattern match
//! something strictly beneath directory `d`?", using the `ignore` crate's own
//! glob semantics component by component, so `!/target/*/x.html` reaches
//! `target/debug/` just as `!/target/doc/**` reaches `target/doc/`.
//!
//! Seam: `rules` builds one [`Reinclude`] per qualifying line of a
//! `.ferretignore` and asks it about excluded directories.

use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// One anchored `!` pattern, split into per-depth directory prefixes.
#[derive(Clone, Debug)]
pub(crate) struct Reinclude {
    /// `prefixes[i]` matches a directory whose first `i + 1` components could
    /// lead to a match: the pattern's first `i + 1` components, anchored.
    prefixes: Vec<Gitignore>,
    /// Index of the first `**` component, past which any depth could match.
    open_at: Option<usize>,
    /// Number of components in the pattern.
    len: usize,
}

impl Reinclude {
    /// Parses one `.ferretignore` line. `None` when the line is not a `!`
    /// pattern, is not anchored beneath a directory, or does not compile (the
    /// caller has already reported the bad line).
    pub(crate) fn parse(line: &str) -> Option<Self> {
        let pattern = line.strip_prefix('!')?.trim_end();
        let parts: Vec<&str> = pattern.split('/').filter(|p| !p.is_empty()).collect();
        // One component is either unanchored (`!*.pdf`) or names the
        // directory itself (`!/target/`); a leading `**` is unanchored too.
        // Neither reaches below an excluded directory.
        if parts.len() < 2 || parts[0] == "**" {
            return None;
        }
        let open_at = parts.iter().position(|p| *p == "**");
        let depth = open_at.unwrap_or(parts.len() - 1);
        let prefixes = (1..=depth)
            .map(|d| {
                let mut builder = GitignoreBuilder::new(".");
                builder.add_line(None, &format!("/{}", parts[..d].join("/")))?;
                builder.build()
            })
            .collect::<Result<_, _>>()
            .ok()?;
        Some(Self {
            prefixes,
            open_at,
            len: parts.len(),
        })
    }

    /// True when this pattern could match something strictly beneath `dir`,
    /// given relative to the `.ferretignore` that holds the pattern.
    pub(crate) fn reaches_below(&self, dir: &Path) -> bool {
        let depth = dir.components().count();
        let (probe, at) = match self.open_at {
            Some(open) if depth >= open => (dir.components().take(open).collect(), open),
            _ if depth < self.len => (PathBuf::from(dir), depth),
            _ => return false,
        };
        at > 0
            && self
                .prefixes
                .get(at - 1)
                .is_some_and(|p| p.matched(&probe, true).is_ignore())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_anchored_bang_patterns_qualify() {
        let cases = [
            ("!/target/doc/**", true),
            ("!target/doc/**", true),
            ("!/target/", false),
            ("!/a/**/b", true),
            ("!*.pdf", false),
            ("!node_modules/", false),
            ("!**/foo/x", false),
            ("target/doc/**", false),
            ("\\!/target/doc", false),
        ];
        for (line, want) in cases {
            assert_eq!(Reinclude::parse(line).is_some(), want, "{line}");
        }
    }

    #[test]
    fn reaches_below_follows_the_pattern_components() {
        let cases = [
            ("!/target/doc/**", "target", true),
            ("!/target/doc/**", "target/doc", true),
            ("!/target/doc/**", "target/debug", false),
            ("!/target/doc/**", "other", false),
            ("!/target/doc/x.html", "target/doc", true),
            // The pattern names `target/doc/x.html` itself, not something below it.
            ("!/target/doc/x.html", "target/doc/x.html", false),
            ("!/target/*/x.html", "target/debug", true),
            ("!/target/*/x.html", "target/debug/deps", false),
            ("!/a/**/b", "a/x/y/z", true),
            ("!/a/**/b", "c/x", false),
        ];
        for (line, dir, want) in cases {
            let reinclude = Reinclude::parse(line).unwrap();
            assert_eq!(
                reinclude.reaches_below(Path::new(dir)),
                want,
                "{line} {dir}"
            );
        }
    }
}
