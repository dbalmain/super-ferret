//! Resident requests are hints about final disk state, never delete commands.
//! Generation checking precedes all scope-id access. The crawl owns scope
//! expansion; catalog owns the durable delta and the next pinned view.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

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
            outcome: RefreshOutcome::RetryFromCurrent(stale),
            view,
            report: Report::default(),
        });
    }
    let roots: Vec<_> = view
        .roots()
        .map(|(_, p)| PathBuf::from(OsStr::from_bytes(p)))
        .collect();
    let mut selected = BTreeSet::new();
    if request.reason == RefreshReason::Burst {
        for scope in &request.scopes {
            selected.insert(match scope {
                RefreshScope::Entry { parent, .. } => containing_root(&view, *parent)?,
                RefreshScope::Directory(id) => containing_root(&view, *id)?,
                RefreshScope::Root(path) => path.clone(),
            });
        }
        for hint in &request.rename_hints {
            for parent in [hint.old_parent, hint.new_parent] {
                if let Ok(root) = containing_root(&view, parent) {
                    selected.insert(root);
                }
            }
        }
    }
    let selected: Vec<_> = selected.into_iter().collect();
    let scope = match request.reason {
        RefreshReason::Overflow | RefreshReason::PolicyChange | RefreshReason::Backstop => {
            Refresh::All
        }
        RefreshReason::Burst => Refresh::Only(&selected),
    };
    let (report, changes) = crate::index::recrawl_with_changes(session, &roots, scope, options)?;
    Ok(RefreshReport {
        outcome: if changes.records.is_empty() {
            RefreshOutcome::Unchanged
        } else {
            RefreshOutcome::Committed { changes }
        },
        view: session.view(),
        report,
    })
}
