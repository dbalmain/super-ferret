//! Typed walker faults to checked protection boundaries. This runs only after
//! all workers finish, so a late fault overrides observations on other workers.
//! Reconcile retains old subtrees by stopping sweeps, never copying
//! descendants.
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use ferret_catalog::batch::DirectoryObservation;
use ferret_catalog::{Batch, Catalog, DirToken, InoId, Kind, WriterSession};

use crate::IoOp;
use crate::index::{CoverageContext, CoverageFault};

#[derive(Default)]
pub(crate) struct Protection {
    pub directories: BTreeSet<u32>,
    pub edges: BTreeSet<u32>,
    pub tokens: BTreeSet<DirToken>,
    pub opaque: BTreeSet<DirToken>,
    pub markers: BTreeSet<u32>,
}
impl Protection {
    pub fn is_empty(&self) -> bool {
        self.directories.is_empty() && self.edges.is_empty() && self.opaque.is_empty()
    }
}

struct Resolver<'a> {
    session: &'a WriterSession,
    old: Catalog,
    dirs: BTreeMap<DirToken, DirectoryObservation<'a>>,
    resolved: BTreeMap<DirToken, Option<InoId>>,
    active: BTreeSet<DirToken>,
    out: Protection,
}
impl Resolver<'_> {
    fn directory(&mut self, token: DirToken) -> Option<InoId> {
        if let Some(id) = self.resolved.get(&token) {
            return *id;
        }
        if !self.active.insert(token) {
            return None;
        }
        let d = *self.dirs.get(&token)?;
        let id = if let Some(parent) = d.parent {
            let p = self.directory(parent)?;
            match self.old.lookup(p, d.name).map(|n| self.old.name(n).child) {
                Some(id) => (self.old.is_live_inode(id)
                    && self.old.is_directory(id)
                    && self.old.identity(id) == (d.stat.dev, d.stat.ino))
                    .then_some(id),
                None => {
                    let mut root = p;
                    while let Some(name) = self.old.dir_name(root) {
                        root = self.old.name(name).parent;
                    }
                    self.session
                        .directory_identity(root, (d.stat.dev, d.stat.ino))
                }
            }
        } else {
            self.old
                .roots()
                .find(|(_, path)| *path == d.name)
                .map(|(id, _)| id)
                .filter(|&id| self.old.identity(id) == (d.stat.dev, d.stat.ino))
        };
        self.active.remove(&token);
        self.resolved.insert(token, id);
        id
    }
    fn protect(&mut self, mut token: DirToken) -> Option<()> {
        let mut visited = BTreeSet::new();
        while visited.insert(token) {
            if let Some(id) = self.directory(token) {
                self.out.directories.insert(id.0);
                self.out.markers.insert(id.0);
                self.out.tokens.insert(token);
                return Some(());
            }
            // Replaced directories cannot carry their old subtrees. Only an
            // unchanged ancestor can anchor retention; new roots have none.
            token = self.dirs.get(&token)?.parent?;
        }
        None
    }
    fn child_directory(&mut self, parent: DirToken, name: &[u8]) -> Option<()> {
        let token = self
            .dirs
            .values()
            .find(|d| d.parent == Some(parent) && d.name == name)
            .map(|d| d.token);
        if token.is_none()
            && let Some(p) = self.directory(parent)
            && let Some(edge) = self.old.lookup(p, name)
        {
            let child = self.old.name(edge).child;
            if self.old.is_live_inode(child) && self.old.is_directory(child) {
                self.out.directories.insert(child.0);
                self.out.markers.insert(child.0);
                return Some(());
            }
        }
        if let Some(token) = token {
            if self.directory(token).is_some() {
                return self.protect(token);
            }
            let p = self.directory(parent)?;
            if self.old.lookup(p, name).is_none() {
                self.out.opaque.insert(token);
                return Some(());
            }
        }
        self.protect(parent)
    }
    fn child_edge(&mut self, parent: DirToken, name: &[u8]) -> Option<()> {
        let p = self.directory(parent)?;
        let Some(edge) = self.old.lookup(p, name) else {
            return self.protect(parent);
        };
        let child = self.old.name(edge).child;
        if self.old.is_live_inode(child) && self.old.kind(child) == Kind::Dir {
            self.out.directories.insert(child.0);
            self.out.markers.insert(child.0);
            if let Some(token) = self
                .dirs
                .values()
                .find(|d| d.parent == Some(parent) && d.name == name)
                .map(|d| d.token)
            {
                // A Decided directory with another identity is a replacement.
                if self.directory(token) != Some(child) {
                    return self.protect(parent);
                }
                self.out.tokens.insert(token);
            }
        } else {
            self.out.edges.insert(edge.0);
            // Non-directory rows have no coverage column. Mark the parent as
            // stale while retaining only this edge; trustworthy siblings
            // update.
            self.out.markers.insert(p.0);
        }
        Some(())
    }
}

/// None means the fault has no trustworthy anchor or the root/policy transition
/// cannot represent retention. Inputs are typed contexts, never pathname
/// scopes.
pub(crate) fn resolve(
    session: &WriterSession,
    batches: &[Batch],
    faults: &[CoverageFault],
    configured: &[PathBuf],
) -> Option<Protection> {
    if faults.is_empty()
        && !batches
            .iter()
            .flat_map(Batch::directories)
            .any(|d| d.retained_at.is_some())
    {
        return Some(Protection::default());
    }
    let old = session.view();
    if batches.iter().any(|b| {
        b.observation_generation()
            .is_some_and(|g| g != old.generation())
    }) {
        return None;
    }
    let previous: Vec<_> = old
        .roots()
        .map(|(_, p)| PathBuf::from(std::ffi::OsStr::from_bytes(p)))
        .collect();
    let changed: Vec<_> = previous
        .iter()
        .filter(|p| !configured.contains(p))
        .chain(configured.iter().filter(|p| !previous.contains(p)))
        .collect();
    let mut r = Resolver {
        session,
        old,
        dirs: batches
            .iter()
            .flat_map(Batch::directories)
            .map(|d| (d.token, d))
            .collect(),
        resolved: BTreeMap::new(),
        active: BTreeSet::new(),
        out: Protection::default(),
    };
    let retained: Vec<_> = r
        .dirs
        .values()
        .filter(|d| d.retained_at.is_some())
        .map(|d| d.token)
        .collect();
    for token in retained {
        r.protect(token)?;
    }
    for fault in faults {
        match (&fault.context, fault.op) {
            (CoverageContext::Root, IoOp::OpenDir | IoOp::Lstat | IoOp::List) => {
                let id = r
                    .old
                    .roots()
                    .find(|(_, path)| *path == fault.root.as_os_str().as_bytes())?
                    .0;
                if let Some(token) = r
                    .dirs
                    .values()
                    .find(|d| d.parent.is_none() && d.name == fault.root.as_os_str().as_bytes())
                    .map(|d| d.token)
                {
                    if r.directory(token) != Some(id) {
                        return None;
                    }
                    r.out.tokens.insert(token);
                }
                r.out.directories.insert(id.0);
                r.out.markers.insert(id.0);
            }
            (
                CoverageContext::Directory(token),
                IoOp::List | IoOp::OpenDir | IoOp::Reopen | IoOp::ReadIgnore | IoOp::ProbeGit,
            ) => {
                // A new unreadable child directory is opaque, without inventing
                // an old subtree; a replaced one protects its old parent.
                if matches!(fault.op, IoOp::List | IoOp::OpenDir | IoOp::Reopen)
                    && let Some(d) = r.dirs.get(token).copied()
                    && let Some(parent) = d.parent
                {
                    r.child_directory(parent, d.name)?;
                } else {
                    r.protect(*token)?;
                }
            }
            (CoverageContext::Child { parent, name }, IoOp::OpenDir) => {
                r.child_directory(*parent, name)?;
            }
            (CoverageContext::Child { parent, name }, IoOp::Lstat | IoOp::Readlink) => {
                r.child_edge(*parent, name)?;
            }
            (CoverageContext::Child { parent, .. }, IoOp::ReadIgnore | IoOp::ProbeGit) => {
                r.protect(*parent)?;
            }
            _ => return None,
        }
    }
    // Overlapping scopes collapse by old graph ancestry, independent of worker
    // event order and without walking any protected descendant.
    let outermost = |set: &BTreeSet<u32>, mut id: InoId| {
        while let Some(name) = r.old.dir_name(id) {
            id = r.old.name(name).parent;
            if set.contains(&id.0) {
                return false;
            }
        }
        true
    };
    let directories = r
        .out
        .directories
        .iter()
        .copied()
        .filter(|&id| outermost(&r.out.directories, InoId(id)))
        .collect();
    r.out.directories = directories;
    r.out
        .markers
        .retain(|&id| outermost(&r.out.directories, InoId(id)));
    r.out.edges.retain(|&id| {
        let p = r.old.name(ferret_catalog::NameId(id)).parent;
        !r.out.directories.contains(&p.0) && outermost(&r.out.directories, p)
    });
    // Root-boundary edits are normalised plan paths. Compare them only with
    // paths derived from checked graph IDs, never use paths to guess a scope.
    // A disjoint root edit does not invalidate an unchanged protected subtree.
    let mut path = Vec::new();
    for &id in &r.out.directories {
        path.clear();
        r.old.dir_path(InoId(id), &mut path);
        let scope = std::path::Path::new(std::ffi::OsStr::from_bytes(&path));
        if changed.iter().any(|root| root.starts_with(scope)) {
            return None;
        }
    }
    for token in r.out.opaque.iter().copied().collect::<Vec<_>>() {
        let d = *r.dirs.get(&token)?;
        let parent = r.directory(d.parent?)?;
        path.clear();
        r.old.dir_path(parent, &mut path);
        let scope = PathBuf::from(std::ffi::OsStr::from_bytes(&path))
            .join(std::ffi::OsStr::from_bytes(d.name));
        if changed.iter().any(|root| root.starts_with(&scope)) {
            return None;
        }
    }
    Some(r.out)
}
