//! Typed walker faults to checked protection boundaries and D26 opaque denials.
//! This runs only after all workers finish, so a late fault overrides
//! observations on other workers. Reconcile retains old subtrees by stopping
//! sweeps, never copying descendants.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use ferret_catalog::batch::DirectoryObservation;
use ferret_catalog::{Batch, Catalog, DirToken, InoId, Kind, Transaction, WriterSession};

use crate::IoOp;
use crate::index::{CoverageContext, CoverageFault};

#[derive(Default)]
pub(crate) struct Protection {
    pub directories: BTreeSet<u32>,
    pub edges: BTreeSet<u32>,
    pub tokens: BTreeSet<DirToken>,
    pub opaque: BTreeSet<DirToken>,
    // Covered observations: these do not block global version transitions.
    pub denied: BTreeSet<DirToken>,
    pub markers: BTreeSet<u32>,
}
impl Protection {
    pub fn is_empty(&self) -> bool {
        self.directories.is_empty() && self.edges.is_empty() && self.opaque.is_empty()
    }
}

/// D26: only directory EACCES is a covered opaque observation, not retention.
pub(crate) fn directory_denied(fault: &CoverageFault) -> bool {
    matches!(fault.op, IoOp::OpenDir | IoOp::List | IoOp::Reopen)
        && fault.error.raw_os_error() == Some(rustix::io::Errno::ACCESS.raw_os_error())
}

fn denied_tokens(
    dirs: &BTreeMap<DirToken, DirectoryObservation<'_>>,
    faults: &[CoverageFault],
) -> Option<BTreeSet<DirToken>> {
    faults
        .iter()
        .filter(|f| directory_denied(f))
        .map(|fault| match &fault.context {
            CoverageContext::Root => dirs
                .values()
                .find(|d| d.parent.is_none() && d.name == fault.root.as_os_str().as_bytes())
                .map(|d| d.token),
            CoverageContext::Directory(token) => dirs.contains_key(token).then_some(*token),
            CoverageContext::Child { parent, name } if fault.op == IoOp::OpenDir => dirs
                .values()
                .find(|d| d.parent == Some(*parent) && d.name == name)
                .map(|d| d.token),
            _ => None,
        })
        .collect()
}

/// Faults strictly below a denied boundary belong to its discarded prefix.
/// Rule uncertainty at that directory itself or an ancestor still protects;
/// unknown operations/contexts still block instead of acquiring a scope here.
pub(crate) fn discard_denied_prefix_faults(
    batches: &[Batch],
    faults: &mut Vec<CoverageFault>,
) -> Option<()> {
    if !faults.iter().any(directory_denied) {
        return Some(());
    }
    let dirs: BTreeMap<_, _> = batches
        .iter()
        .flat_map(Batch::directories)
        .map(|d| (d.token, d))
        .collect();
    let denied = denied_tokens(&dirs, faults)?;
    faults.retain(|fault| {
        if directory_denied(fault) {
            return true;
        }
        let (mut token, leaf) = match (&fault.context, fault.op) {
            (
                CoverageContext::Directory(token),
                IoOp::OpenDir | IoOp::List | IoOp::Reopen | IoOp::ReadIgnore | IoOp::ProbeGit,
            ) => (*token, false),
            (
                CoverageContext::Child { parent, .. },
                IoOp::OpenDir | IoOp::Lstat | IoOp::Readlink,
            ) => (*parent, true),
            (CoverageContext::Child { parent, .. }, IoOp::ReadIgnore | IoOp::ProbeGit) => {
                (*parent, false)
            }
            _ => return true,
        };
        let start = token;
        let mut visited = BTreeSet::new();
        while visited.insert(token) {
            if denied.contains(&token) && (leaf || token != start) {
                return false;
            }
            let Some(parent) = dirs.get(&token).and_then(|d| d.parent) else {
                break;
            };
            token = parent;
        }
        true
    });
    Some(())
}

/// Initial indexing has no old namespace to sweep. Replay only trustworthy
/// observations through real batches, dropping denied prefixes across workers.
/// This fault-only path preserves transaction publication and needs no format
/// or checkpoint-builder exception.
pub(crate) fn opaque_checkpoint(
    txn: &Transaction,
    batches: &[Batch],
    faults: &[CoverageFault],
) -> Option<Vec<Batch>> {
    let dirs: BTreeMap<_, _> = batches
        .iter()
        .flat_map(Batch::directories)
        .map(|d| (d.token, d))
        .collect();
    let denied = denied_tokens(&dirs, faults)?;
    if denied.is_empty() {
        return None;
    }
    let mut batch = txn.batch();
    let mapped = replay_directories(&mut batch, batches, &dirs, &denied)?;
    replay_entries(&mut batch, batches, &denied, &mapped);
    batch.finish_observations();
    Some(vec![batch])
}

/// Replays the directories in `dirs` root first, mapping each token to its
/// new token. A denied directory is replayed without its entry count or
/// children.
fn replay_directories(
    batch: &mut Batch,
    batches: &[Batch],
    dirs: &BTreeMap<DirToken, DirectoryObservation<'_>>,
    denied: &BTreeSet<DirToken>,
) -> Option<BTreeMap<DirToken, DirToken>> {
    let counts: BTreeMap<_, _> = batches.iter().flat_map(Batch::entry_counts).collect();
    let mut children: BTreeMap<_, Vec<_>> = BTreeMap::new();
    let mut queue = VecDeque::new();
    for d in dirs.values() {
        if let Some(parent) = d.parent {
            children.entry(parent).or_default().push(d.token);
        } else {
            queue.push_back(d.token);
        }
    }
    let mut mapped = BTreeMap::new();
    while let Some(token) = queue.pop_front() {
        let d = dirs.get(&token)?;
        let next = match d.parent {
            None => batch.root(d.name, *d.stat),
            Some(parent) if d.traversed => {
                batch
                    .revealing(d.revealed)
                    .traversed_dir(*mapped.get(&parent)?, d.name, *d.stat)
            }
            Some(parent) => batch
                .revealing(d.revealed)
                .dir(*mapped.get(&parent)?, d.name, *d.stat),
        };
        mapped.insert(token, next);
        if !denied.contains(&token) {
            if let Some(&count) = counts.get(&token) {
                batch.entry_count(next, count);
            }
            queue.extend(children.remove(&token).unwrap_or_default());
        }
    }
    Some(mapped)
}

/// Replays every file, ignored entry and work tree whose parent was replayed
/// and is not denied.
fn replay_entries(
    batch: &mut Batch,
    batches: &[Batch],
    denied: &BTreeSet<DirToken>,
    mapped: &BTreeMap<DirToken, DirToken>,
) {
    for old in batches {
        for f in (0..old.file_count()).map(|i| old.file_observation(i)) {
            if !denied.contains(&f.parent)
                && let Some(&parent) = mapped.get(&f.parent)
            {
                if let Some(target) = f.target {
                    batch
                        .revealing(f.revealed)
                        .symlink(parent, f.name, f.stat, target);
                } else {
                    batch
                        .revealing(f.revealed)
                        .file(parent, f.name, f.stat, f.content);
                }
            }
        }
        for (parent, name, kind) in old.ignored_entries() {
            if !denied.contains(&parent)
                && let Some(&next) = mapped.get(&parent)
            {
                batch.ignored(next, name, kind);
            }
        }
        for (token, kind, path, identity) in old.work_tree_observations() {
            if !denied.contains(&token)
                && let Some(&next) = mapped.get(&token)
            {
                batch.work_tree(next, kind, path, identity);
            }
        }
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
        let id = (|| {
            let d = *self.dirs.get(&token)?;
            if let Some(parent) = d.parent {
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
            }
        })();
        self.active.remove(&token);
        self.resolved.insert(token, id);
        id
    }
    fn anchored_directory(&mut self, token: DirToken) -> Option<InoId> {
        let id = self.directory(token)?;
        let mut current = token;
        let mut visited = BTreeSet::new();
        while visited.insert(current) {
            let d = *self.dirs.get(&current)?;
            let Some(parent) = d.parent else {
                return Some(id);
            };
            let child = self.directory(current)?;
            let p = self.directory(parent)?;
            if self.old.lookup(p, d.name).map(|n| self.old.name(n).child) != Some(child) {
                return None;
            }
            current = parent;
        }
        None
    }
    fn old_root(&mut self, mut token: DirToken) -> Option<(DirToken, InoId)> {
        let mut visited = BTreeSet::new();
        while visited.insert(token) {
            match self.dirs.get(&token)?.parent {
                Some(parent) => token = parent,
                None => return Some((token, self.directory(token)?)),
            }
        }
        None
    }
    fn mark(&mut self, token: DirToken, id: InoId) {
        self.out.directories.insert(id.0);
        self.out.markers.insert(id.0);
        self.out.tokens.insert(token);
    }
    fn protect(&mut self, mut token: DirToken) -> Option<()> {
        let mut visited = BTreeSet::new();
        while visited.insert(token) {
            if let Some(id) = self.anchored_directory(token) {
                self.mark(token, id);
                return Some(());
            }
            // A relocated old directory cannot retain its old incoming edge
            // while its former ancestors are swept. Protect the checked owner
            // root rather than attaching old children to the new occurrence.
            if let Some((root_token, root)) = self.old_root(token) {
                let d = *self.dirs.get(&token)?;
                if self
                    .session
                    .directory_identity(root, (d.stat.dev, d.stat.ino))
                    .is_some()
                {
                    self.mark(root_token, root);
                    return Some(());
                }
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
            && let Some(p) = self.anchored_directory(parent)
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
            if self.directory(token).is_some()
                || self.old_root(token).is_some_and(|(_, root)| {
                    let d = self.dirs[&token];
                    self.session
                        .directory_identity(root, (d.stat.dev, d.stat.ino))
                        .is_some()
                })
            {
                return self.protect(token);
            }
            let Some(p) = self.anchored_directory(parent) else {
                return self.protect(parent);
            };
            if self.old.lookup(p, name).is_none() {
                self.out.opaque.insert(token);
                return Some(());
            }
        }
        self.protect(parent)
    }
    fn child_edge(&mut self, parent: DirToken, name: &[u8]) -> Option<()> {
        let Some(p) = self.anchored_directory(parent) else {
            return self.protect(parent);
        };
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

    /// Protects what one fault's typed context names; None when the fault
    /// has no trustworthy anchor.
    fn record(&mut self, fault: &CoverageFault) -> Option<()> {
        match (&fault.context, fault.op) {
            (CoverageContext::Root, IoOp::OpenDir | IoOp::Lstat | IoOp::List) => {
                let id = self
                    .old
                    .roots()
                    .find(|(_, path)| *path == fault.root.as_os_str().as_bytes())?
                    .0;
                if let Some(token) = self
                    .dirs
                    .values()
                    .find(|d| d.parent.is_none() && d.name == fault.root.as_os_str().as_bytes())
                    .map(|d| d.token)
                {
                    if self.directory(token) != Some(id) {
                        return None;
                    }
                    self.out.tokens.insert(token);
                }
                self.out.directories.insert(id.0);
                self.out.markers.insert(id.0);
            }
            (
                CoverageContext::Directory(token),
                IoOp::List | IoOp::OpenDir | IoOp::Reopen | IoOp::ReadIgnore | IoOp::ProbeGit,
            ) => {
                // A new unreadable child directory is opaque, without inventing
                // an old subtree; a replaced one protects its old parent.
                if matches!(fault.op, IoOp::List | IoOp::OpenDir | IoOp::Reopen)
                    && let Some(d) = self.dirs.get(token).copied()
                    && let Some(parent) = d.parent
                {
                    self.child_directory(parent, d.name)?;
                } else {
                    self.protect(*token)?;
                }
            }
            (CoverageContext::Child { parent, name }, IoOp::OpenDir) => {
                self.child_directory(*parent, name)?;
            }
            (CoverageContext::Child { parent, name }, IoOp::Lstat | IoOp::Readlink) => {
                self.child_edge(*parent, name)?;
            }
            (CoverageContext::Child { parent, .. }, IoOp::ReadIgnore | IoOp::ProbeGit) => {
                self.protect(*parent)?;
            }
            _ => return None,
        }
        Some(())
    }

    /// Overlapping scopes collapse by old graph ancestry, independent of
    /// worker event order and without walking any protected descendant.
    fn collapse(&mut self) {
        let old = &self.old;
        let directories = self
            .out
            .directories
            .iter()
            .copied()
            .filter(|&id| outermost(old, &self.out.directories, InoId(id)))
            .collect();
        self.out.directories = directories;
        let out = &mut self.out;
        out.markers
            .retain(|&id| outermost(old, &out.directories, InoId(id)));
        out.edges.retain(|&id| {
            let p = old.name(ferret_catalog::NameId(id)).parent;
            !out.directories.contains(&p.0) && outermost(old, &out.directories, p)
        });
    }

    /// A new opaque scope beneath an old protected scope is discarded along
    /// with the rest of that ancestor's observations, not counted
    /// separately.
    fn keep_unprotected_opaque(&mut self) -> Option<()> {
        let mut opaque = BTreeSet::new();
        for token in self.out.opaque.iter().copied().collect::<Vec<_>>() {
            let mut parent = self.dirs.get(&token)?.parent;
            let mut protected = false;
            let mut visited = BTreeSet::new();
            while let Some(p) = parent {
                if !visited.insert(p) {
                    return None;
                }
                if self.out.tokens.contains(&p)
                    || self
                        .directory(p)
                        .is_some_and(|id| self.out.directories.contains(&id.0))
                {
                    protected = true;
                    break;
                }
                parent = self.dirs.get(&p)?.parent;
            }
            if !protected {
                opaque.insert(token);
            }
        }
        self.out.opaque = opaque;
        Some(())
    }

    /// Root-boundary edits are normalised plan paths. Compare them only with
    /// paths derived from checked graph IDs, never use paths to guess a
    /// scope. A disjoint root edit does not invalidate an unchanged
    /// protected subtree.
    fn refuse_root_edits(&mut self, changed: &[&PathBuf]) -> Option<()> {
        let mut path = Vec::new();
        for &id in &self.out.directories {
            path.clear();
            self.old.dir_path(InoId(id), &mut path);
            let scope = std::path::Path::new(std::ffi::OsStr::from_bytes(&path));
            if changed.iter().any(|root| root.starts_with(scope)) {
                return None;
            }
        }
        for token in self.out.opaque.iter().copied().collect::<Vec<_>>() {
            let d = *self.dirs.get(&token)?;
            let parent = self.directory(d.parent?)?;
            path.clear();
            self.old.dir_path(parent, &mut path);
            let scope = PathBuf::from(std::ffi::OsStr::from_bytes(&path))
                .join(std::ffi::OsStr::from_bytes(d.name));
            if changed.iter().any(|root| root.starts_with(&scope)) {
                return None;
            }
        }
        Some(())
    }
}

/// Whether no proper ancestor of `id` in the old graph is in `set`.
fn outermost(old: &Catalog, set: &BTreeSet<u32>, mut id: InoId) -> bool {
    while let Some(name) = old.dir_name(id) {
        id = old.name(name).parent;
        if set.contains(&id.0) {
            return false;
        }
    }
    true
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
    r.out.denied = denied_tokens(&r.dirs, faults)?;
    for fault in faults {
        if directory_denied(fault) {
            continue;
        }
        r.record(fault)?;
    }
    r.collapse();
    r.keep_unprotected_opaque()?;
    r.refuse_root_edits(&changed)?;
    Some(r.out)
}

/// Replays complete rewalk observations and checked retained boundaries through
/// the full builder. Numeric ids select old rows only; new tokens own all
/// edges. None refuses unrepresentable coverage before publishing anything.
pub(crate) fn checkpoint_observations(
    session: &WriterSession,
    batches: Vec<Batch>,
    protection: &Protection,
    configured: &[PathBuf],
) -> Option<Vec<Batch>> {
    if protection.is_empty() && protection.denied.is_empty() {
        return Some(batches);
    }
    let old = session.view();
    let dirs: BTreeMap<_, _> = batches
        .iter()
        .flat_map(Batch::directories)
        .map(|d| (d.token, d))
        .collect();
    let mut children: BTreeMap<_, Vec<_>> = BTreeMap::new();
    let mut queue = VecDeque::new();
    for d in dirs.values() {
        if let Some(parent) = d.parent {
            children.entry(parent).or_default().push(d.token);
        } else {
            queue.push_back(d.token);
        }
    }
    let mut resolver = Resolver {
        session,
        old: old.clone(),
        dirs,
        resolved: BTreeMap::new(),
        active: BTreeSet::new(),
        out: Protection::default(),
    };
    let mut replay = Replay {
        old: &old,
        protection,
        carried: session.checkpoint_batch(0),
        live: BTreeSet::new(),
        coverage: BTreeMap::new(),
        overridden: BTreeMap::new(),
        represented: BTreeSet::new(),
    };
    while let Some(token) = queue.pop_front() {
        if replay.visit(&mut resolver, token)? {
            queue.extend(children.remove(&token).unwrap_or_default());
        }
    }
    replay.fill_roots(&resolver.dirs, configured)?;
    replay.check_represented()?;
    drop(resolver);
    drop(children);
    Some(replay.apply(batches))
}

/// A checkpoint replay's state: the batch of old rows it carries, and per new
/// token whether it is live, its coverage, and the old names it overrides.
struct Replay<'a> {
    old: &'a Catalog,
    protection: &'a Protection,
    carried: Batch,
    live: BTreeSet<DirToken>,
    coverage: BTreeMap<DirToken, Option<u64>>,
    overridden: BTreeMap<DirToken, BTreeSet<Vec<u8>>>,
    represented: BTreeSet<u32>,
}

impl Replay<'_> {
    /// Replays directory `token`: copies a protected subtree whole, records
    /// an opaque directory's coverage, or carries its protected old edges.
    /// `Some(true)` when its children are to be replayed too; `None` refuses.
    fn visit(&mut self, resolver: &mut Resolver<'_>, token: DirToken) -> Option<bool> {
        let (old, protection) = (self.old, self.protection);
        let d = *resolver.dirs.get(&token)?;
        let parent = match d.parent {
            Some(p) => self.live.contains(&p).then_some(Some(p))?,
            None => None,
        };
        if d.parent.is_some_and(|p| {
            self.overridden
                .get(&p)
                .is_some_and(|names| names.contains(d.name))
        }) {
            return Some(false);
        }
        if protection.tokens.contains(&token) {
            let id = resolver.directory(token)?;
            copy_subtree(
                old,
                &mut self.carried,
                id,
                parent,
                d.name,
                d.revealed,
                &protection.markers,
            )?;
            self.represented.insert(id.0);
            return Some(false);
        }
        self.live.insert(token);
        let opaque = protection.denied.contains(&token) || protection.opaque.contains(&token);
        if opaque {
            self.coverage.insert(token, None);
            return Some(false);
        }
        if let Some(id) = resolver.directory(token) {
            if protection.markers.contains(&id.0) {
                self.coverage.insert(token, retained_sequence(old, id));
            }
            for name in old.children(id) {
                let edge = old.name(name);
                let retained_dir = matches!(edge.target(), ferret_catalog::Target::Inode(child) if old.is_directory(child) && protection.directories.contains(&child.0));
                if protection.edges.contains(&name.0) || retained_dir {
                    self.overridden
                        .entry(token)
                        .or_default()
                        .insert(edge.bytes.to_vec());
                    copy_edge(old, &mut self.carried, token, name, &protection.markers)?;
                    self.represented.insert(edge.child.0);
                }
            }
        }
        Some(true)
    }

    /// A failed root open can have no token. Only a checked old configured
    /// root is allowed to fill that absence; new unanchored roots abort.
    fn fill_roots(
        &mut self,
        dirs: &BTreeMap<DirToken, DirectoryObservation<'_>>,
        configured: &[PathBuf],
    ) -> Option<()> {
        for root in configured {
            if !dirs
                .values()
                .any(|d| d.parent.is_none() && d.name == root.as_os_str().as_bytes())
            {
                let (id, path) = self
                    .old
                    .roots()
                    .find(|(_, path)| *path == root.as_os_str().as_bytes())?;
                if !self.protection.directories.contains(&id.0) {
                    return None;
                }
                copy_subtree(
                    self.old,
                    &mut self.carried,
                    id,
                    None,
                    path,
                    false,
                    &self.protection.markers,
                )?;
                self.represented.insert(id.0);
            }
        }
        Some(())
    }

    /// Every protected directory has a represented ancestor, or is one.
    fn check_represented(&self) -> Option<()> {
        for &id in &self.protection.directories {
            let mut dir = InoId(id);
            while !self.represented.contains(&dir.0) {
                dir = self.old.name(self.old.dir_name(dir)?).parent;
            }
        }
        Some(())
    }

    /// Maps the replay onto `batches`, then appends the carried batch when it
    /// holds anything.
    fn apply(self, mut batches: Vec<Batch>) -> Vec<Batch> {
        let Self {
            protection,
            mut carried,
            live,
            coverage,
            overridden,
            ..
        } = self;
        let mut mapping: BTreeMap<_, _> = batches
            .iter()
            .flat_map(|batch| batch.checkpoint_tokens(|token| live.contains(&token)))
            .collect();
        mapping.extend(carried.checkpoint_tokens(|_| true));
        for batch in &mut batches {
            batch.checkpoint_coverage(&coverage);
            batch.retain_checkpoint(&mapping, |parent, name| {
                !protection.denied.contains(&parent)
                    && !protection.opaque.contains(&parent)
                    && !overridden
                        .get(&parent)
                        .is_some_and(|names| names.contains(name))
            });
        }
        carried.retain_checkpoint(&mapping, |_, _| true);
        carried.seal();
        if carried.directories().next().is_some()
            || carried.file_count() > 0
            || carried.ignored_entries().next().is_some()
        {
            batches.push(carried);
        }
        batches
    }
}

fn retained_sequence(old: &Catalog, id: InoId) -> Option<u64> {
    old.retained_at(id)
        .or_else(|| old.entry_count(id).map(|_| old.generation().sequence))
}

fn copy_subtree(
    old: &Catalog,
    batch: &mut Batch,
    id: InoId,
    parent: Option<DirToken>,
    name: &[u8],
    revealed: bool,
    markers: &BTreeSet<u32>,
) -> Option<()> {
    // The legacy full builder cannot express an independently suppressed old
    // directory. Refuse that rare protected case instead of changing D29.
    if old.is_traversed(id) != old.is_search_suppressed(id) {
        return None;
    }
    let stat = old.inode(id).stat;
    let token = match parent {
        None => batch.root(name, stat),
        Some(p) if old.is_traversed(id) => batch.revealing(revealed).traversed_dir(p, name, stat),
        Some(p) => batch.revealing(revealed).dir(p, name, stat),
    };
    if markers.contains(&id.0) {
        if let Some(seq) = retained_sequence(old, id) {
            batch.retained_at(token, Some(seq));
        }
    } else {
        if let Some(count) = old.entry_count(id) {
            batch.entry_count(token, count);
        }
        if let Some(seq) = old.retained_at(id) {
            batch.retained_at(token, Some(seq));
        }
    }
    if let Some(wt) = old.work_tree(id) {
        batch.work_tree(token, wt.kind, wt.common_dir, wt.common_id);
    }
    for name in old.children(id) {
        copy_edge(old, batch, token, name, markers)?;
    }
    Some(())
}

fn copy_edge(
    old: &Catalog,
    batch: &mut Batch,
    parent: DirToken,
    name: ferret_catalog::NameId,
    markers: &BTreeSet<u32>,
) -> Option<()> {
    use ferret_catalog::{Content, ContentState, Target};
    let edge = old.name(name);
    let Target::Inode(id) = edge.target() else {
        if let Target::Ignored(kind) = edge.target() {
            batch.ignored(parent, edge.bytes, kind);
        }
        return Some(());
    };
    if old.is_directory(id) {
        let revealed = old.is_revealed(name);
        return copy_subtree(old, batch, id, Some(parent), edge.bytes, revealed, markers);
    }
    let inode = old.inode(id);
    if old.kind(id) == Kind::Symlink {
        batch.retained_file(
            parent,
            edge.bytes,
            inode.stat,
            Content::Unindexed,
            Some(old.link_target(id)?),
            old.is_revealed(name),
        );
    } else {
        let content = match inode.state {
            ContentState::Unindexed => Content::Unindexed,
            ContentState::Binary => Content::Binary,
            ContentState::Fault => Content::Fault,
            ContentState::Hashed => Content::Hashed(old.doc_hash(inode.doc?)?),
        };
        batch.retained_file(
            parent,
            edge.bytes,
            inode.stat,
            content,
            None,
            old.is_revealed(name),
        );
    }
    Some(())
}
