//! The catalogued directories a content walk must enter. A grep over a large
//! operand with few candidates otherwise lists every catalogued name below
//! it, at about 0.7 µs a name: 38 ms of a 52 ms answer on a 57k-name tree
//! that held three candidates (2026-10-30).

use super::groups::Candidates;
use ferret_catalog::{Catalog, Contents, InoId, NameId, OpenError, Section, Target};
use std::os::unix::ffi::OsStrExt;

/// Directories whose subtree holds a name the walk can yield a job for.
/// Every other catalogued directory is left unlisted. Their files would each
/// be dropped by [`Filter`](super::groups::Filter) as a non-candidate, and
/// they hold no directory listed live, so skipping them changes only which
/// files count as searched (D99), which the caller settles separately.
pub(super) struct Holders {
    directories: Vec<bool>,
}

/// A directory with no name row: a root.
const NO_PARENT: u32 = u32::MAX;

impl Holders {
    /// One pass over the catalog's names, a run at a time. Holds the
    /// directory of each candidate document's names; of each file the index
    /// cannot vouch for (oversized, or binary under `text`), which is
    /// searched live; and each directory or root whose contents are not
    /// catalogued, which the walk lists live or reports as unreadable. Then
    /// each held directory's ancestors. `Ok(None)` when cancelled.
    pub fn new(
        catalog: &Catalog,
        candidates: &dyn Candidates,
        text: bool,
        stopped: &dyn Fn() -> bool,
    ) -> Result<Option<Self>, OpenError> {
        catalog.load(&[
            Section::Names,
            Section::Entries,
            Section::Roots,
            Section::Doc,
            Section::Size,
            Section::States,
        ])?;
        let inodes = catalog.next_inode().0 as usize;
        // Each directory's parent, read from its name row as the pass goes:
        // marking then follows memory, not a name lookup a level.
        let mut parents = vec![NO_PARENT; inodes];
        let mut held = Vec::new();
        for (seen, (_, name)) in catalog.name_reader().runs_from(NameId(0)).enumerate() {
            if (seen + 1).is_multiple_of(ferret_index::DocSet::CHECK_EVERY) && stopped() {
                return Ok(None);
            }
            let Target::Inode(inode) = name.target() else {
                continue;
            };
            if let Some(doc) = catalog.doc(inode) {
                if candidates.contains(doc.0, stopped) {
                    held.push(name.parent);
                }
            } else if catalog.is_directory(inode) {
                if let Some(parent) = parents.get_mut(inode.0 as usize) {
                    *parent = name.parent.0;
                }
                if catalog.contents(Target::Inode(inode)) != Some(Contents::Catalogued) {
                    held.push(inode);
                }
            } else if catalog.size(inode) > ferret_policy::DEFAULT_SIZE_CAP
                || text && catalog.state(inode) == ferret_catalog::ContentState::Binary
            {
                held.push(name.parent);
            }
        }
        // A nested root has no name row in the outer root (D34): the walk
        // reaches it from the directory holding its path. One whose contents
        // are not catalogued is held itself, as a directory's would be.
        for (root, path) in catalog.roots() {
            if catalog.contents(Target::Inode(root)) != Some(Contents::Catalogued) {
                held.push(root);
            }
            let parent = std::path::Path::new(std::ffi::OsStr::from_bytes(path)).parent();
            if let Some(ferret_catalog::Resolved {
                target: Target::Inode(parent),
                remainder: b"",
                ..
            }) = parent.and_then(|parent| catalog.resolve(parent.as_os_str().as_bytes()))
                && let Some(slot) = parents.get_mut(root.0 as usize)
            {
                *slot = parent.0;
            }
        }
        let mut directories = vec![false; inodes];
        for mut directory in held {
            // Stops at the first directory already held: its ancestors are.
            while let Some(slot) = directories.get_mut(directory.0 as usize)
                && !*slot
            {
                *slot = true;
                directory = InoId(parents[directory.0 as usize]);
            }
        }
        Ok(Some(Self { directories }))
    }

    /// Whether the walk must list `directory`.
    pub fn holds(&self, directory: InoId) -> bool {
        self.directories
            .get(directory.0 as usize)
            .copied()
            .unwrap_or(true)
    }
}
