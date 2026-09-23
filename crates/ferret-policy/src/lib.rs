//! Exclusion policy: which entries of a directory walk are skipped,
//! catalogued, or content-indexed.
//!
//! Owns the precedence of `.ferretignore`, `.gitignore`, the user's global
//! ignore file and the built-in defaults (DECISIONS.md D13), plus the size cap
//! and binary check that separate *catalogued* from *content-indexed*.
//!
//! Pure: no file-system I/O. The crawler (`ferret-crawl`) walks the tree,
//! reads ignore files and each file's head, and asks this crate what to do:
//! [`DirRules::root`] once per configured root, [`DirRules::enter`] or
//! [`DirRules::traverse`] per directory it walks into, [`DirRules::decide`]
//! per entry, and [`sniff`] per file it would index. Knows nothing about the
//! catalog or the index. Tested against the golden corpus in `tests/golden/`.
//!
//! Seams: `rules` holds the precedence chain, `reinclude` the one extension
//! over gitignore semantics (re-including beneath an excluded directory).
//! Pattern syntax and matching are the `ignore` crate's (D13 option A), kept
//! behind this API so an own matcher can replace it without callers noticing.

mod reinclude;
mod rules;

use std::fmt;
use std::path::PathBuf;

pub use rules::DirRules;

/// How many leading bytes of a file the crawler reads for [`sniff`].
pub const SNIFF_LEN: usize = 8192;

/// Default [`Config::size_cap`]: 8 MiB. A judgement, not a measurement.
pub const DEFAULT_SIZE_CAP: u64 = 8 << 20;

/// Policy settings that are not ignore patterns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Files larger than this many bytes are catalogued, not content-indexed.
    /// A file of exactly this size is indexed.
    pub size_cap: u64,
    /// Whether the built-in default exclusions (`node_modules/`, `target/`, …)
    /// apply. They sit below every ignore file, so `!pat` anywhere overrides
    /// them either way.
    pub defaults: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            size_cap: DEFAULT_SIZE_CAP,
            defaults: true,
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
    /// work tree, and the enclosing work tree's `.gitignore` files stop
    /// applying (as in git and ripgrep).
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
    /// A socket, FIFO or device. Always skipped.
    Other,
}

/// What the crawler does with one entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Not catalogued; for a directory, not walked into.
    Skip,
    /// Directory: catalogue it, read its ignore files and walk into it with
    /// [`DirRules::enter`].
    Descend,
    /// Directory that is excluded, but beneath which a `!` pattern in a
    /// `.ferretignore` could re-include something. Not catalogued; walk into
    /// it with [`DirRules::traverse`], without reading its ignore files (D13:
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
    /// The built-in defaults. An error here is a bug in this crate.
    Builtin,
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
    /// The file's patterns could not be compiled together; none of it applies.
    File {
        /// Where it came from.
        file: IgnoreFile,
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
            Self::Builtin => f.write_str("built-in defaults"),
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
            Self::File { file, detail } => write!(f, "{file}: {detail}"),
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
