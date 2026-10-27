//! Which walked entries a grep walk visits: depth, then the `-g`/`-t`/hidden
//! selection, then live policy (rg's rules for an uncovered tree, D13 under
//! `--live`). Content search and `--files` share it; what each does with a
//! selected file is its own.

use ferret_query::find::Entry;
use std::ffi::OsStr;

use super::*;

pub(super) enum Visit {
    Skip,
    Descend,
    File,
}

/// An entry that could not be selected, and the path its diagnostic names:
/// the entry's own, or its directory's when that directory's rules failed
/// to load at this, its first entry ([`live::Policy::reach`]).
#[derive(Debug)]
pub(super) struct Refusal {
    pub path: PathBuf,
    pub error: io::Error,
}

pub(super) struct Visibility<'a> {
    pub selection: &'a GrepSelection,
    /// Under `--rg`, the catalog a covered walk lists, which records the
    /// hidden names a repository ignore file's `!` line reveals (D127). Needs
    /// [`Section::Revealed`](ferret_catalog::Section::Revealed). `None` in
    /// ferret mode, where a `!` line does not override hidden selection.
    pub revealed: Option<&'a ferret_catalog::Catalog>,
    pub cwd: &'a Path,
    pub max_depth: Option<usize>,
    /// Content search: D13's size and binary rules apply to files.
    pub content: bool,
}

impl Visibility<'_> {
    /// The visit for an entry. A refusal (the entry's type, or live policy)
    /// is the caller's to report; the entry is then skipped. An entry past
    /// `--max-depth` is skipped before its type is read, as rg never visits
    /// it.
    pub fn select(
        &self,
        entry: &Entry,
        mut policy: Option<&mut live::Policy>,
        stopped: &dyn Fn() -> bool,
    ) -> Result<Visit, Refusal> {
        if let Some(policy) = policy.as_deref_mut() {
            policy.reach(entry, stopped).map_err(|error| Refusal {
                path: entry.path().parent().unwrap_or(entry.path()).to_owned(),
                error,
            })?;
        }
        self.visit(entry, policy, stopped).map_err(|error| Refusal {
            path: entry.path().to_owned(),
            error,
        })
    }

    fn visit(
        &self,
        entry: &Entry,
        policy: Option<&mut live::Policy>,
        stopped: &dyn Fn() -> bool,
    ) -> io::Result<Visit> {
        if self.max_depth.is_some_and(|limit| entry.depth() > limit) {
            return Ok(Visit::Skip);
        }
        let kind = entry.kind()?;
        let rg = policy.as_ref().is_some_and(|policy| policy.rg());
        let path = entry.path().as_os_str().as_bytes();
        let relative = relative_to(entry.path(), self.cwd).as_os_str().as_bytes();
        // rg shows a hidden name that an ignore file's `!` line reveals, as
        // the live walk does through `Policy::selects`. A catalogued entry
        // has no rules at hand; the crawl recorded the answer (D127). Only a
        // dot name can be revealed, so others skip the lookup.
        let revealed = !rg
            && entry.name().starts_with(b".")
            && self.revealed.is_some_and(|catalog| {
                entry
                    .catalog_name()
                    .is_some_and(|name| catalog.is_revealed(name))
            });
        let directory = kind == FileKind::Directory;
        let selected = |plain: &dyn Fn() -> bool| {
            if revealed {
                self.selection
                    .selects(path, relative, entry.depth(), directory, true)
            } else {
                plain()
            }
        };
        let visit = match kind {
            FileKind::Directory => {
                let descend = self.max_depth.is_none_or(|limit| entry.depth() < limit)
                    && (rg
                        || selected(&|| {
                            self.selection.descend(relative, entry.depth())
                                && !self.selection.hides_stripped(path, relative, entry.depth())
                        }));
                if !descend {
                    return Ok(Visit::Skip);
                }
                Visit::Descend
            }
            FileKind::File
                if rg
                    || selected(&|| {
                        self.selection.allows(relative)
                            && !self.selection.hides_stripped(path, relative, entry.depth())
                    }) =>
            {
                Visit::File
            }
            _ => return Ok(Visit::Skip),
        };
        let Some(policy) = policy else {
            return Ok(visit);
        };
        let allowed = if rg {
            policy.selects(entry, self.selection, self.cwd, self.content, stopped)?
        } else {
            policy.accepts(entry, self.content, stopped)?
        };
        Ok(if allowed { visit } else { Visit::Skip })
    }
}

/// `path` under the absolute `cwd`, as `Path::strip_prefix` gives it, else
/// `path` itself. Every walked entry asks, so a plain spelling (no empty or
/// `.` component) is compared as bytes; any other takes the component-wise
/// strip. Only an absolute path can lie under the absolute cwd.
pub(super) fn relative_to<'p>(path: &'p Path, cwd: &Path) -> &'p Path {
    if !path.is_absolute() {
        return path;
    }
    let (bytes, base) = (path.as_os_str().as_bytes(), cwd.as_os_str().as_bytes());
    if plain(base) {
        if let Some(rest) = bytes.strip_prefix(base)
            && (base == b"/" || rest.is_empty() || rest[0] == b'/')
        {
            let start = rest.iter().position(|&b| b != b'/').unwrap_or(rest.len());
            let rest = &rest[start..];
            if plain_relative(rest) {
                return Path::new(OsStr::from_bytes(rest));
            }
        } else if plain(&bytes[..base.len().min(bytes.len())]) {
            // The components that would have to equal cwd's are spelled
            // plainly and differ from it as bytes.
            return path;
        }
    }
    path.strip_prefix(cwd).unwrap_or(path)
}

/// No empty or `.` component, and no trailing separator.
fn plain(bytes: &[u8]) -> bool {
    bytes == b"/"
        || !(bytes.ends_with(b"/")
            || bytes.ends_with(b"/.")
            || bytes.windows(2).any(|pair| pair == b"//")
            || bytes.windows(3).any(|triple| triple == b"/./"))
}

fn plain_relative(bytes: &[u8]) -> bool {
    bytes.is_empty() || !(bytes == b"." || bytes.starts_with(b"./")) && plain(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_to_agrees_with_strip_prefix() {
        let paths = [
            "rel/x", "/", "/a", "/a/", "/a/b", "/a/b/", "/a/b/c", "/a/bc", "/a/b//c", "/a//b/c",
            "/a/./b/c", "/a/b/./c", "/a/b/.", "/a/b/c/.", "/a/b/..", "//a/b/c", "/./a/b",
            "/a/b/c/", "/ab/c", "/a/b\\c",
        ];
        for cwd in ["/", "/a", "/a/b", "/a/", "/a//b", "/a/./b"] {
            for path in paths {
                let (path, cwd) = (Path::new(path), Path::new(cwd));
                let expected = if path.is_absolute() {
                    path.strip_prefix(cwd).unwrap_or(path)
                } else {
                    path
                };
                assert_eq!(relative_to(path, cwd), expected, "{path:?} under {cwd:?}");
                // `Path` equality ignores some spelling; the bytes must agree.
                assert_eq!(
                    relative_to(path, cwd).as_os_str(),
                    expected.as_os_str(),
                    "{path:?} under {cwd:?}"
                );
            }
        }
    }
}
