//! Re-inclusion beneath an excluded directory: the one place the policy goes
//! beyond gitignore semantics (D13).
//!
//! Git never walks into an excluded directory, so nothing beneath it can be
//! re-included. A `.ferretignore` `!` pattern can, but only if it is
//! *anchored* (it has a `/` before its last character and does not start with
//! `**`): only then is the set of directories it could reach bounded, so the
//! walker never has to open every `node_modules/` on the chance that `!*.pdf`
//! matches inside. This module answers "could this anchored pattern match
//! something strictly beneath directory `d`?", using the same parsed pattern
//! and component semantics as direct matching, so `!/target/*/x.html` reaches
//! `target/debug/` just as `!/target/doc/**` reaches `target/doc/`.
//!
//! Seam: `rules` builds one [`Reinclude`] per qualifying line of a
//! `.ferretignore` and asks it about excluded directories.

use std::path::Path;

use crate::gitignore::Pattern;

/// One anchored `!` pattern.
#[derive(Clone, Debug)]
pub(crate) struct Reinclude {
    pattern: Pattern,
}

impl Reinclude {
    pub(crate) fn from_pattern(pattern: Pattern) -> Option<Self> {
        pattern.is_anchored_reinclude().then_some(Self { pattern })
    }

    /// Parses one `.ferretignore` line. `None` when the line is not a `!`
    /// pattern, is not anchored beneath a directory, or does not compile (the
    /// caller has already reported the bad line).
    #[cfg(test)]
    fn parse(line: &str) -> Option<Self> {
        let pattern = Pattern::compile(0, line).ok()??;
        pattern.is_anchored_reinclude().then_some(Self { pattern })
    }

    /// True when this pattern could match something strictly beneath `dir`,
    /// given relative to the `.ferretignore` that holds the pattern.
    pub(crate) fn reaches_below(&self, dir: &Path) -> bool {
        self.pattern.reaches_below(dir)
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
            ("!/a/***/b", true),
            ("!/a\\/**/b", true),
            ("!*.pdf", false),
            ("!node_modules/", false),
            ("!**/foo/x", false),
            ("!***/foo/x", false),
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
            ("!/a/***/b", "a/x/y/z", true),
            ("!/a\\/**/b", "a/x/y/z", true),
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
