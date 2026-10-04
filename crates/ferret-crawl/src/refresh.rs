//! Resident requests are hints about final disk state, never delete commands.
//! Generation checking precedes all scope-id access. The crawl owns scope
//! expansion; catalog owns the durable delta and the next pinned view.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use ferret_catalog::log::ChangeSet;
use ferret_catalog::{Catalog, Generation, InoId, RetryFromCurrent, WriterSession};

use crate::{IndexError, IndexOptions, Refresh, Report};

/// A scope in `RefreshRequest::expected_generation`'s inode namespace.
#[derive(Clone, Debug)]
pub enum RefreshScope {
    /// Observe the final state of one basename and refresh its parent's count.
    Entry { parent: InoId, basename: Vec<u8> },
    /// Refresh this directory's affected subtree.
    Directory(InoId),
    /// Refresh a configured root, retaining its nested-root boundaries.
    Root(PathBuf),
}

/// A move cookie suggests two observations; neither endpoint proves identity.
#[derive(Clone, Debug)]
pub struct RenameHint {
    pub old_parent: InoId,
    pub old_name: Vec<u8>,
    pub new_parent: InoId,
    pub new_name: Vec<u8>,
}

/// Why the caller requests a refresh. No watcher is implemented here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshReason {
    Burst,
    Overflow,
    PolicyChange,
    Backstop,
}

/// Coalesced hints, checked against a complete expected generation.
#[derive(Clone, Debug)]
pub struct RefreshRequest {
    pub expected_generation: Generation,
    pub scopes: Vec<RefreshScope>,
    pub rename_hints: Vec<RenameHint>,
    pub reason: RefreshReason,
}

/// Publication shape. Only a same-epoch committed result carries a delta.
#[derive(Debug)]
pub enum RefreshOutcome {
    Unchanged,
    Committed {
        changes: ChangeSet,
    },
    /// The caller must re-resolve every id against the returned current view.
    RetryFromCurrent(RetryFromCurrent),
    /// Reserved for M7's checkpoint publication; epoch caches must be cleared.
    Checkpointed,
}

/// The host can adopt `view` directly, without reopening the published log.
pub struct RefreshReport {
    /// Source generation for a committed delta; adoption checks this before
    /// ids.
    pub base_generation: Generation,
    pub outcome: RefreshOutcome,
    pub view: Catalog,
    pub report: Report,
}

fn containing_root(view: &Catalog, mut id: InoId) -> Result<PathBuf, IndexError> {
    if !view.is_live_inode(id) || !view.is_directory(id) {
        return Err(IndexError::BadScope(id));
    }
    while let Some(name) = view.dir_name(id) {
        id = view.name(name).parent;
    }
    view.roots()
        .find(|(root, _)| *root == id)
        .map(|(_, path)| PathBuf::from(OsStr::from_bytes(path)))
        .ok_or(IndexError::BadScope(id))
}

/// Root-relative complete scopes and the ancestors needed to reach them.
/// Promotion happens before child jobs are dispatched. Paths choose work;
/// the real opened handles and observation tokens still prove identities.
pub(crate) struct Selection {
    paths: RwLock<BTreeSet<PathBuf>>,
    ancestors: RwLock<BTreeSet<PathBuf>>,
}
impl Selection {
    fn new(paths: BTreeSet<PathBuf>) -> Self {
        let ancestors = paths
            .iter()
            .flat_map(|p| p.ancestors().map(Path::to_owned))
            .collect();
        Self {
            paths: RwLock::new(paths),
            ancestors: RwLock::new(ancestors),
        }
    }
    pub(crate) fn includes(&self, path: &Path) -> bool {
        let paths = self
            .paths
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.ancestors
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(path)
            || path.ancestors().any(|p| paths.contains(p))
    }
    pub(crate) fn promote(&self, path: &Path) {
        self.paths
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(path.to_owned());
        self.ancestors
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(path.ancestors().map(Path::to_owned));
    }
}

fn relative_scope(view: &Catalog, id: InoId) -> Result<(PathBuf, PathBuf), IndexError> {
    let root = containing_root(view, id)?;
    let mut path = Vec::new();
    view.dir_path(id, &mut path);
    let path = Path::new(OsStr::from_bytes(&path));
    let relative = path
        .strip_prefix(&root)
        .map_err(|_| IndexError::BadScope(id))?
        .to_owned();
    Ok((root, relative))
}

fn basename_valid(name: &[u8]) -> bool {
    !name.is_empty() && name != b"." && name != b".." && !name.contains(&b'/') && !name.contains(&0)
}
fn entry_scope(
    view: &Catalog,
    parent: InoId,
    name: &[u8],
) -> Result<(PathBuf, PathBuf), IndexError> {
    if !basename_valid(name) {
        return Err(IndexError::BadEntry(name.to_vec()));
    }
    let (root, mut relative) = relative_scope(view, parent)?;
    if matches!(name, b".ferretignore" | b".gitignore" | b".git") {
        return Ok((root, relative));
    }
    if relative.components().any(|c| c.as_os_str() == ".git") {
        // info/exclude changes affect the containing work tree, not its gitdir.
        let mut owner = parent;
        while view.work_tree(owner).is_none() {
            let Some(name) = view.dir_name(owner) else {
                break;
            };
            owner = view.name(name).parent;
        }
        return relative_scope(view, owner);
    }
    relative.push(OsStr::from_bytes(name));
    Ok((root, relative))
}

/// Observes final disk state under the session's writer lock. Stale requests
/// return before any request id is interpreted, including after compaction
/// with an unchanged sequence. Overflow discards hints and performs a complete
/// backstop recrawl. Policy/sniffer changes expand through the recrawl plan.
pub fn refresh(
    session: &mut WriterSession,
    request: RefreshRequest,
    options: &IndexOptions,
) -> Result<RefreshReport, IndexError> {
    let view = session.view();
    if let Err(stale) = view.generation().check(request.expected_generation) {
        return Ok(RefreshReport {
            base_generation: request.expected_generation,
            outcome: RefreshOutcome::RetryFromCurrent(stale),
            view,
            report: Report::default(),
        });
    }
    let roots: Vec<_> = view
        .roots()
        .map(|(_, p)| PathBuf::from(OsStr::from_bytes(p)))
        .collect();
    let mut selected: BTreeMap<PathBuf, BTreeSet<PathBuf>> = BTreeMap::new();
    if request.reason == RefreshReason::Burst {
        for scope in &request.scopes {
            let (root, relative) = match scope {
                RefreshScope::Entry { parent, basename } => entry_scope(&view, *parent, basename)?,
                RefreshScope::Directory(id) => relative_scope(&view, *id)?,
                RefreshScope::Root(path) => (path.clone(), PathBuf::new()),
            };
            selected.entry(root).or_default().insert(relative);
        }
        for hint in &request.rename_hints {
            for (parent, name) in [
                (hint.old_parent, &hint.old_name),
                (hint.new_parent, &hint.new_name),
            ] {
                if let Ok((root, relative)) = entry_scope(&view, parent, name) {
                    selected.entry(root).or_default().insert(relative);
                }
            }
        }
    }
    let named: Vec<_> = selected.keys().cloned().collect();
    let selections = selected
        .into_iter()
        .map(|(root, paths)| (root, Arc::new(Selection::new(paths))))
        .collect();
    let scope = match request.reason {
        RefreshReason::Overflow | RefreshReason::PolicyChange | RefreshReason::Backstop => {
            Refresh::All
        }
        RefreshReason::Burst => Refresh::Only(&named),
    };
    let (report, changes) =
        crate::index::recrawl_scoped(session, &roots, scope, options, selections)?;
    Ok(RefreshReport {
        base_generation: request.expected_generation,
        outcome: if changes.records.is_empty() {
            RefreshOutcome::Unchanged
        } else {
            RefreshOutcome::Committed { changes }
        },
        view: session.view(),
        report,
    })
}
