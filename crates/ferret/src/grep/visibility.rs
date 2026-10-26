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
        let relative = entry
            .path()
            .strip_prefix(self.cwd)
            .unwrap_or(entry.path())
            .as_os_str()
            .as_bytes();
        let visit = match kind {
            FileKind::Directory => {
                let descend = self.max_depth.is_none_or(|limit| entry.depth() < limit)
                    && (rg
                        || (self.selection.descend(relative, entry.depth())
                            && !self.selection.hides_stripped(path, relative, entry.depth())));
                if !descend {
                    return Ok(Visit::Skip);
                }
                Visit::Descend
            }
            // Each directory above the file passed `descend` when walked, so
            // only the file itself is judged: depth 0 skips its ancestors.
            FileKind::File
                if rg
                    || (self.selection.allows(relative, 0)
                        && !self.selection.hides_stripped(path, relative, entry.depth())) =>
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
