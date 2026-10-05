//! Exclusion policy: which entries of a directory walk are skipped,
//! catalogued, or content-indexed.
//!
//! Owns the precedence of `.ferretignore`, `.gitignore` and the user's global
//! ignore file (DECISIONS.md D13), the default contents setup writes to that
//! file ([`DEFAULT_IGNORE`]), and the size cap and binary check that separate
//! *catalogued* from *content-indexed*.
//!
//! Rule compilation is pure; `GitInputs` discovers config through git. The
//! crawler (`ferret-crawl`) walks the tree, reads ignore files and each file's
//! head, and asks this crate what to do: [`DirRules::root`] once per configured
//! root, [`DirRules::enter`] or [`DirRules::traverse`] per directory it walks
//! into, [`DirRules::decide`] per entry, and [`sniff`] per file it would index.
//! Knows nothing about the catalog or the index. Tested against the golden
//! corpus in `tests/golden/`.
//!
//! Seams: `rules` holds the precedence order as one list of rules per
//! directory (D19), including the one extension over gitignore semantics
//! (re-including beneath an excluded directory); `lists` interns those lists
//! so each distinct one is compiled once. Pattern syntax and matching are
//! implemented in this crate (D16), behind this API so callers do not depend
//! on matcher details.

mod inputs;
pub use inputs::GitInputs;

mod gitignore;
mod lists;
mod rules;

use std::fmt;
use std::path::PathBuf;

pub use rules::DirRules;

/// What setup writes to a new global ignore file: the M1 list, measured on
/// Dave's tree (research M1), in sections a user can comment out. After that
/// the file is the user's; nothing here applies these patterns by itself.
/// `result` has no slash: Nix's `result` is a symlink.
pub const DEFAULT_IGNORE: &str = "\
# ferret's global ignore file: gitignore syntax, applied under every root,
# below any .ferretignore or .gitignore. Delete a line, or add `!pattern`
# after it, to index what it excludes.

# Version control
.git/

# Dependencies
node_modules/
vendor/
.venv/

# Build output
target/
dist/
build/
.next/
# Nix build output: a symlink, so no trailing slash.
result

# Caches and environments
.cache/
__pycache__/
.direnv/
";

/// How many leading bytes of a file the crawler reads for [`sniff`].
pub const SNIFF_LEN: usize = 8192;

/// The version of [`sniff`]'s rule. Bump it whenever `sniff` could classify
/// some file differently: the catalog then refreshes every root instead of
/// carrying old classifications forward (D37).
pub const SNIFFER_VERSION: u32 = 1;

/// Default [`Config::size_cap`]: 8 MiB. A judgement, not a measurement.
pub const DEFAULT_SIZE_CAP: u64 = 8 << 20;

/// Policy settings that are not ignore patterns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Files larger than this many bytes are catalogued, not content-indexed.
    /// A file of exactly this size is indexed.
    pub size_cap: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            size_cap: DEFAULT_SIZE_CAP,
        }
    }
}

/// The ignore-related contents of one directory, as read by the crawler.
///
/// Contents are text: the crawler converts non-UTF-8 bytes lossily. A field
/// left `None` means the file is absent.
#[derive(Clone, Copy, Debug, Default)]
pub struct IgnoreFiles<'a> {
    /// Contents of `.ferretignore` in this directory.
    pub ferretignore: Option<&'a str>,
    /// Contents of `.gitignore` in this directory. Has no effect unless this
    /// directory is inside a git work tree.
    pub gitignore: Option<&'a str>,
    /// True when this directory contains an entry named `.git`: it starts a
    /// work tree, and inherited `.gitignore` rules stop applying (as in git
    /// and ripgrep).
    pub git_root: bool,
    /// Contents of `.git/info/exclude`; read only when `git_root` is true.
    pub git_exclude: Option<&'a str>,
}

/// What a directory entry is, from `lstat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    /// A directory.
    Dir,
    /// A regular file of `size` bytes.
    File {
        /// Length in bytes.
        size: u64,
    },
    /// A symbolic link. Catalogued, never followed.
    Symlink,
    /// A socket, FIFO or device. Catalogued without content when included.
    Other,
}

/// What the crawler does with one entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Ignored: retain only its name and type; a directory is not walked into.
    Skip,
    /// Directory: catalogue it, read its ignore files and walk into it with
    /// [`DirRules::enter`].
    Descend,
    /// Directory that is excluded, but beneath which a `!` pattern in a
    /// `.ferretignore` could re-include something. Walk into it with
    /// [`DirRules::traverse`], without reading its ignore files (D13:
    /// an excluded directory's ignore files are never read).
    Traverse,
    /// File or symlink: catalogue its name and metadata, do not index content.
    Catalog(Reason),
    /// File: catalogue it and index its content, unless [`sniff`] says binary.
    Index,
}

/// Why an entry is catalogued without its content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// Larger than [`Config::size_cap`].
    TooLarge,
    /// A symlink; its target is not followed.
    Symlink,
    /// A FIFO, socket or device; never read as content.
    Special,
}

/// What a file's head says about its content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Content {
    /// Index it.
    Text,
    /// Catalogue it only.
    Binary,
}

/// Classifies a file by its first bytes (up to [`SNIFF_LEN`]; fewer for a
/// short file). A NUL byte means binary, as in git and ripgrep; any other
/// bytes, UTF-8 or not, are text. Hazard: UTF-16 text contains NULs and is
/// therefore binary here.
pub fn sniff(head: &[u8]) -> Content {
    if head.contains(&0) {
        Content::Binary
    } else {
        Content::Text
    }
}

/// Which ignore source a [`PatternError`] came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IgnoreFile {
    /// A `.ferretignore`, by absolute path.
    Ferret(PathBuf),
    /// A `.gitignore`, by absolute path.
    Git(PathBuf),
    /// A `.git/info/exclude`, by absolute path.
    GitExclude(PathBuf),
    /// The user's global ignore file.
    Global,
}

/// An ignore file the policy could not fully use. Non-fatal: as in git, a bad
/// line is dropped and the rest of its file still applies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternError {
    /// One line is not a valid glob; it was dropped.
    Line {
        /// Where it came from.
        file: IgnoreFile,
        /// 1-based line number.
        line: usize,
        /// The line as written.
        pattern: String,
        /// The matcher's reason.
        detail: String,
    },
}

impl fmt::Display for IgnoreFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ferret(path) | Self::Git(path) | Self::GitExclude(path) => {
                write!(f, "{}", path.display())
            }
            Self::Global => f.write_str("global ignore file"),
        }
    }
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Line {
                file,
                line,
                pattern,
                detail,
            } => write!(f, "{file}:{line}: `{pattern}`: {detail}"),
        }
    }
}

impl std::error::Error for PatternError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_classifies_by_nul() {
        let cases: &[(&[u8], Content)] = &[
            (b"", Content::Text),
            (b"plain ascii\n", Content::Text),
            ("caf\u{e9} \u{2014} utf-8\n".as_bytes(), Content::Text),
            (b"caf\xe9 latin-1\n", Content::Text),
            (b"\x7fELF\x02\x01\x01\x00", Content::Binary),
            (b"text then \0 nul", Content::Binary),
            (b"h\0i\0", Content::Binary), // UTF-16LE "hi"
        ];
        for &(head, want) in cases {
            assert_eq!(sniff(head), want, "{head:?}");
        }
    }
}
