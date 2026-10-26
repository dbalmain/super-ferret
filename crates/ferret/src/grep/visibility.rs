//! Which walked entries a grep walk visits: depth, then the `-g`/`-t`/hidden
//! selection, then live policy (rg's rules for an uncovered tree, D13 under
//! `--live`). Content search and `--files` share it; what each does with a
//! selected file is its own.

use ferret_query::find::Entry;

use super::*;

pub(super) enum Visit {
    Skip,
    Descend,
    File,
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
    /// The visit for an entry. An error (the entry's type, or live policy)
    /// is the caller's to report; the entry is then skipped. An entry past
    /// `--max-depth` is skipped before its type is read, as rg never visits
    /// it.
    pub fn select(
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
        // Only an absolute path can lie under the absolute cwd; skipping the
        // component-wise strip for the usual relative path saves a parse per
        // entry.
        let relative = if entry.path().is_absolute() {
            entry.path().strip_prefix(self.cwd).unwrap_or(entry.path())
        } else {
            entry.path()
        }
        .as_os_str()
        .as_bytes();
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
