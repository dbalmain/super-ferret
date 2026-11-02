//! Whole-walk observations to a sparse final change set. Directory tokens are
//! resolved against the effective graph, never a renumbered candidate snapshot.
//! Only covered portions of refreshed roots are swept; kept roots remain in
//! the pinned view. Typed protection scopes stop sweeps at checked old
//! boundaries and discard every worker's new observations below them before
//! alias grouping.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

mod finish;

use ferret_catalog::batch::{DirectoryObservation, FileObservation};
use ferret_catalog::log::{ChangeSet, Error, Record};
use ferret_catalog::{
    Batch, Catalog, Content, DecodeError, DirToken, DocId, Hash, InoId, Kind, NameId, Stat, Target,
    WriterSession,
};

/// Prepares complete configured-root observations for a full checkpoint.
/// Fault locators are checked against the current published view. Returns the
/// optional consumed batches and protected-scope count; None means retention
/// cannot safely represent the observations or a global version transition.
/// Scoped or partial inputs must never be supplied to this full-root seam.
pub fn checkpoint_batches(
    session: &WriterSession,
    batches: Vec<Batch>,
    faults: &[crate::CoverageFault],
    configured: &[PathBuf],
    sniffer: u32,
    policy: Hash,
) -> (Option<Vec<Batch>>, usize) {
    let Some(protection) = crate::coverage::resolve(session, &batches, faults, configured) else {
        return (None, 0);
    };
    let scopes = protection.directories.len() + protection.edges.len() + protection.opaque.len();
    let old = session.view();
    if !protection.is_empty() && (old.policy() != policy || old.sniffer_version() != sniffer) {
        return (None, scopes);
    }
    (
        crate::coverage::checkpoint_observations(session, batches, &protection, configured),
        scopes,
    )
}

struct Seen(Vec<u64>);
impl Seen {
    fn new(bound: u32) -> Self {
        Self(vec![0; (bound as usize).div_ceil(64)])
    }
    fn insert(&mut self, id: u32) {
        self.0[id as usize / 64] |= 1 << (id % 64);
    }
    fn extend(&mut self, words: &[u64]) {
        for (to, from) in self.0.iter_mut().zip(words) {
            *to |= from;
        }
    }
    fn contains(&self, id: u32) -> bool {
        self.0
            .get(id as usize / 64)
            .is_some_and(|word| word & (1 << (id % 64)) != 0)
    }
}

struct Directory<'a> {
    observation: DirectoryObservation<'a>,
    children: Vec<usize>,
    included: bool,
    id: Option<u32>,
    root: u32,
    entries: Option<u32>,
    blocked: bool,
    opaque: bool,
    denied: bool,
}

#[derive(Clone, Copy)]
enum File {
    Full(u32, u32),
    Reused(u32, u32),
}
impl File {
    fn observation(self, batches: &[Batch]) -> FileObservation<'_> {
        match self {
            Self::Full(b, i) => batches[b as usize].file_observation(i as usize),
            Self::Reused(b, i) => batches[b as usize].reused_name_observation(NameId(i)),
        }
    }
}

#[derive(Default)]
struct Tokens(BTreeMap<u32, Vec<usize>>);
impl Tokens {
    fn insert(&mut self, token: DirToken, index: usize) -> bool {
        let (batch, row) = token.coordinates();
        let rows = self.0.entry(batch).or_default();
        rows.resize(rows.len().max(row as usize + 1), usize::MAX);
        let before = std::mem::replace(&mut rows[row as usize], index);
        before == usize::MAX
    }
    fn get(&self, token: &DirToken) -> Option<&usize> {
        let (batch, row) = token.coordinates();
        self.0
            .get(&batch)?
            .get(row as usize)
            .filter(|&&i| i != usize::MAX)
    }
}
impl std::ops::Index<&DirToken> for Tokens {
    type Output = usize;
    fn index(&self, token: &DirToken) -> &usize {
        let (batch, row) = token.coordinates();
        &self.0[&batch][row as usize]
    }
}

struct Edge {
    parent: u32,
    child: u32,
    name: Vec<u8>,
    revealed: bool,
}

struct Final<'a> {
    session: &'a WriterSession,
    budget: std::sync::Arc<ferret_catalog::InputBudget>,
    old: Catalog,
    changes: ChangeSet,
    names: Seen,
    inodes: Seen,
    puts: BTreeMap<u32, Record>,
    pending: Vec<Edge>,
    removed: BTreeSet<u32>,
    references: BTreeMap<u32, i64>,
    document_refs: BTreeMap<u32, i64>,
    new_docs: BTreeMap<Hash, u32>,
}

fn invalid(reason: &'static str) -> Error {
    Error::Invalid(DecodeError::Corrupt(reason))
}
fn allocate(counter: &mut u32, limit: u32) -> Result<u32, Error> {
    let id = *counter;
    *counter = id
        .checked_add(1)
        .filter(|n| *n < limit)
        .ok_or_else(|| invalid("recrawl allocation exhausted"))?;
    Ok(id)
}
fn ignored(kind: Kind) -> u32 {
    u32::MAX - 1 - kind as u32
}

/// Reconciles completed batches through the same producer used by `index`.
/// `None` requests checkpoint fallback for unclassified incomplete coverage.
/// The caller holds the session lock throughout planning, walking and
/// publication. Refreshed/dropped paths must be the already widened root plan;
/// batches must contain the refreshed roots. `Batch::retained_at` marks an
/// explicitly protected old directory occurrence; an unanchored marker is
/// rejected.
pub fn changes(
    session: &WriterSession,
    batches: &[Batch],
    refreshed: &[PathBuf],
    dropped: &[PathBuf],
    policy: Hash,
    sniffer: u32,
) -> Result<Option<ChangeSet>, Error> {
    let old = session.view();
    let mut configured: Vec<_> = old
        .roots()
        .map(|(_, p)| PathBuf::from(std::ffi::OsStr::from_bytes(p)))
        .collect();
    configured.retain(|p| !dropped.contains(p));
    configured.extend(refreshed.iter().cloned());
    configured.sort();
    configured.dedup();
    let protection = crate::coverage::resolve(session, batches, &[], &configured)
        .ok_or_else(|| invalid("unresolvable retained observation"))?;
    with_protection(
        session,
        batches,
        refreshed,
        dropped,
        policy,
        sniffer,
        &protection,
    )
}

pub(crate) fn with_protection(
    session: &WriterSession,
    batches: &[Batch],
    refreshed: &[PathBuf],
    dropped: &[PathBuf],
    policy: Hash,
    sniffer: u32,
    protection: &crate::coverage::Protection,
) -> Result<Option<ChangeSet>, Error> {
    let budget = std::sync::Arc::new(ferret_catalog::InputBudget::new(session.input_limits()));
    with_budget(
        session,
        batches,
        refreshed,
        dropped,
        policy,
        sniffer,
        (protection, budget),
    )
}

pub(crate) fn with_budget(
    session: &WriterSession,
    batches: &[Batch],
    refreshed: &[PathBuf],
    dropped: &[PathBuf],
    policy: Hash,
    sniffer: u32,
    guarded: (
        &crate::coverage::Protection,
        std::sync::Arc<ferret_catalog::InputBudget>,
    ),
) -> Result<Option<ChangeSet>, Error> {
    let (protection, budget) = guarded;
    if batches.iter().any(Batch::overflowed) {
        return Err(invalid("recrawl batch overflow"));
    }
    let old = session.view();
    if batches.iter().any(|batch| {
        batch
            .observation_generation()
            .is_some_and(|g| g != old.generation())
    }) {
        return Err(invalid("stale observation generation"));
    }
    let Some(mut tree) = Tree::observe(batches, protection)? else {
        return Ok(None);
    };
    if !protection.is_empty() && (old.policy() != policy || old.sniffer_version() != sniffer) {
        return Err(invalid("version transition under protection"));
    }
    tree.include(batches, protection)?;
    let mut out = Final::new(session, budget, old);
    out.retain(protection);
    out.place_directories(&mut tree, refreshed, protection)?;
    let kept = out.preserved_files(&tree, batches)?;
    out.place_files(&tree, batches, protection)?;
    out.ignored_entries(&tree, batches, protection)?;
    out.sweep(refreshed, dropped, protection, &kept)?;
    out.finish_edges()?;
    out.directory_records(&tree, protection)?;
    out.work_trees(&tree, batches)?;
    out.finish_inodes()?;
    if out.old.policy() != policy || out.old.sniffer_version() != sniffer {
        push(
            &out.budget,
            &mut out.changes.records,
            Record::PolicyPut { hash: policy },
        )?;
    }
    out.changes.records.sort_unstable_by_key(record_key);
    Ok(Some(out.changes))
}

/// The walk's directory observations, linked to their parents and marked
/// for protection and inclusion.
struct Tree<'a> {
    dirs: Vec<Directory<'a>>,
    tokens: Tokens,
}

impl<'a> Tree<'a> {
    /// Links the observations and spreads protection down from protected
    /// scopes. `None` when an unprotected directory has no listing.
    fn observe(
        batches: &'a [Batch],
        protection: &crate::coverage::Protection,
    ) -> Result<Option<Self>, Error> {
        let mut dirs: Vec<_> = batches
            .iter()
            .flat_map(Batch::directories)
            .map(|observation| Directory {
                included: !observation.traversed || observation.parent.is_none(),
                observation,
                children: Vec::new(),
                id: None,
                root: 0,
                entries: None,
                blocked: protection.tokens.contains(&observation.token),
                opaque: protection.opaque.contains(&observation.token)
                    || protection.denied.contains(&observation.token),
                denied: protection.denied.contains(&observation.token),
            })
            .collect();
        let mut tokens = Tokens::default();
        for (i, d) in dirs.iter().enumerate() {
            if !tokens.insert(d.observation.token, i) {
                return Err(invalid("duplicate directory token"));
            }
        }
        for i in 0..dirs.len() {
            if let Some(parent) = dirs[i].observation.parent {
                let p = *tokens
                    .get(&parent)
                    .ok_or_else(|| invalid("missing directory token"))?;
                dirs[p].children.push(i);
            }
        }
        for (token, count) in batches.iter().flat_map(Batch::entry_counts) {
            let i = *tokens.get(&token).ok_or_else(|| invalid("listing token"))?;
            dirs[i].entries = Some(count);
        }
        // D26 discards the whole denied prefix. Its included descendants must
        // not keep Traverse ancestors alive after failed re-inclusion (D29).
        if !protection.denied.is_empty() {
            let mut denied: Vec<_> = dirs
                .iter()
                .enumerate()
                .filter(|(_, d)| d.denied)
                .map(|(i, _)| i)
                .collect();
            while let Some(i) = denied.pop() {
                for child in dirs[i].children.clone() {
                    if !dirs[child].denied {
                        dirs[child].denied = true;
                        denied.push(child);
                    }
                }
            }
        }
        let mut protected: Vec<_> = dirs
            .iter()
            .enumerate()
            .filter(|(_, d)| d.blocked || d.opaque)
            .map(|(i, _)| i)
            .collect();
        while let Some(i) = protected.pop() {
            for child in dirs[i].children.clone() {
                if !dirs[child].blocked {
                    dirs[child].blocked = true;
                    protected.push(child);
                }
            }
        }
        if dirs
            .iter()
            .any(|d| d.entries.is_none() && !d.blocked && !d.opaque)
        {
            return Ok(None);
        }
        Ok(Some(Self { dirs, tokens }))
    }

    /// Marks the directories that survive: retained scopes, parents of
    /// observed rows, and every ancestor of those.
    fn include(
        &mut self,
        batches: &[Batch],
        protection: &crate::coverage::Protection,
    ) -> Result<(), Error> {
        let Self { dirs, tokens } = self;
        // Retained old scopes still prove their old re-inclusion ancestry
        // (D29).
        for d in dirs.iter_mut() {
            if d.observation
                .token
                .previous_directory()
                .is_some_and(|id| protection.directories.contains(&id.0))
            {
                d.included = true;
            }
        }
        // D29: only ancestors of an included row survive a Traverse
        // observation.
        for batch in batches {
            for i in 0..batch.file_count() {
                let parent = batch.file_observation(i).parent;
                let p = *tokens
                    .get(&parent)
                    .ok_or_else(|| invalid("file parent token"))?;
                if !dirs[p].denied {
                    dirs[p].included = true;
                }
            }
            for parent in batch.reused_directories().chain(batch.preserved_parents()) {
                let p = *tokens
                    .get(&parent)
                    .ok_or_else(|| invalid("reused parent token"))?;
                if !dirs[p].denied {
                    dirs[p].included = true;
                }
            }
        }
        let seeds: Vec<_> = dirs
            .iter()
            .enumerate()
            .filter(|(_, d)| d.included && (!d.denied || !d.blocked))
            .map(|(i, _)| i)
            .collect();
        for mut i in seeds {
            loop {
                dirs[i].included = true;
                let Some(parent) = dirs[i].observation.parent else {
                    break;
                };
                let p = tokens[&parent];
                if dirs[p].included {
                    break;
                }
                i = p;
            }
        }
        Ok(())
    }
}

impl<'a> Final<'a> {
    fn new(
        session: &'a WriterSession,
        budget: std::sync::Arc<ferret_catalog::InputBudget>,
        old: Catalog,
    ) -> Self {
        Self {
            budget,
            changes: ChangeSet {
                counters: [old.next_inode().0, old.next_name().0, old.next_doc().0],
                counts: [
                    old.inode_count(),
                    old.name_count(),
                    old.dir_count(),
                    old.doc_count(),
                ],
                records: Vec::new(),
            },
            names: Seen::new(old.next_name().0),
            inodes: Seen::new(old.next_inode().0),
            old,
            session,
            puts: BTreeMap::new(),
            pending: Vec::new(),
            removed: BTreeSet::new(),
            references: BTreeMap::new(),
            document_refs: BTreeMap::new(),
            new_docs: BTreeMap::new(),
        }
    }

    /// Retains the checked protection boundaries' own rows.
    fn retain(&mut self, protection: &crate::coverage::Protection) {
        // Retain only checked boundary IDs. Descendants stay untouched because
        // sweeping stops before iterating that boundary's old child range.
        for &id in &protection.directories {
            self.inodes.insert(id);
            if let Some(name) = self.old.dir_name(InoId(id)) {
                self.names.insert(name.0);
            }
        }
        for &id in &protection.edges {
            self.names.insert(id);
            if let Target::Inode(child) = self.old.name(NameId(id)).target() {
                self.inodes.insert(child.0);
            }
        }
    }

    /// Places every directory breadth-first from the observed roots, reusing
    /// old IDs where the identity continues.
    fn place_directories(
        &mut self,
        tree: &mut Tree<'_>,
        refreshed: &[PathBuf],
        protection: &crate::coverage::Protection,
    ) -> Result<(), Error> {
        let Tree { dirs, tokens } = tree;
        let mut roots: Vec<_> = dirs
            .iter()
            .enumerate()
            .filter(|(_, d)| d.observation.parent.is_none())
            .map(|(i, _)| i)
            .collect();
        roots.sort_unstable_by_key(|&i| dirs[i].observation.name);
        let observed_roots: Vec<_> = roots.iter().map(|&i| dirs[i].observation.name).collect();
        self.check_roots(&observed_roots, refreshed, protection)?;
        let mut queue: VecDeque<_> = roots.into();
        let mut visited = 0;
        while let Some(i) = queue.pop_front() {
            visited += 1;
            let mut children = std::mem::take(&mut dirs[i].children);
            children.sort_unstable_by_key(|&c| dirs[c].observation.name);
            queue.extend(children);
            if dirs[i].blocked {
                continue;
            }
            let d = dirs[i].observation;
            let parent = d.parent.map(|token| tokens[&token]);
            let parent_id = parent.and_then(|p| dirs[p].id);
            if parent.is_some() && parent_id.is_none() {
                continue;
            }
            if !dirs[i].included && (!dirs[i].opaque || dirs[i].denied) {
                if let Some(parent) = parent_id {
                    self.edge(parent, ignored(Kind::Dir), d.name, false)?;
                }
                continue;
            }
            let root = parent.map_or_else(
                || {
                    self.old
                        .roots()
                        .find(|(_, path)| *path == d.name)
                        .map(|(id, _)| id.0)
                },
                |p| Some(dirs[p].root),
            );
            let hinted = d
                .token
                .previous_directory()
                .filter(|&id| !self.inodes.contains(id.0));
            let continuing = hinted
                .or_else(|| self.continuing_directory(&d, parent_id, root))
                .filter(|id| !self.inodes.contains(id.0));
            let id = match continuing {
                Some(id) => id.0,
                None => allocate(&mut self.changes.counters[0], u32::MAX - 16)?,
            };
            dirs[i].id = Some(id);
            dirs[i].root = parent.map_or(id, |p| dirs[p].root);
            self.observe_inode(id, Kind::Dir, *d.stat, Content::Unindexed)?;
            self.name_directory(&d, id, parent_id, hinted.is_some(), continuing.is_some())?;
        }
        // A disconnected/cyclic token graph cannot reach publication.
        if visited != dirs.len() {
            return Err(invalid("unreachable directory observation"));
        }
        Ok(())
    }

    /// Fails unless the observed roots are the refreshed roots, less the
    /// protected ones the crawl did not observe.
    fn check_roots(
        &self,
        observed_roots: &[&[u8]],
        refreshed: &[PathBuf],
        protection: &crate::coverage::Protection,
    ) -> Result<(), Error> {
        if observed_roots
            != refreshed
                .iter()
                .filter(|p| {
                    !self.old.roots().any(|(id, path)| {
                        path == p.as_os_str().as_bytes()
                            && protection.directories.contains(&id.0)
                            && !observed_roots.contains(&path)
                    })
                })
                .map(|p| p.as_os_str().as_bytes())
                .collect::<Vec<_>>()
        {
            return Err(invalid("recrawl root observations"));
        }
        Ok(())
    }

    /// The old directory `d` continues, when its token gives no hint: the
    /// same identity under the same name in its parent, or anywhere under
    /// its root; for a root, the root itself.
    fn continuing_directory(
        &self,
        d: &DirectoryObservation<'_>,
        parent_id: Option<u32>,
        root: Option<u32>,
    ) -> Option<InoId> {
        let identity = (d.stat.dev, d.stat.ino);
        let Some(parent) = parent_id else {
            return root
                .map(InoId)
                .filter(|&id| self.old.identity(id) == identity);
        };
        self.old
            .is_live_inode(InoId(parent))
            .then(|| self.old.lookup(InoId(parent), d.name))
            .flatten()
            .map(|id| self.old.name(id).child)
            .filter(|&id| {
                self.old.is_live_inode(id)
                    && self.old.is_directory(id)
                    && self.old.identity(id) == identity
            })
            .or_else(|| root.and_then(|r| self.session.directory_identity(InoId(r), identity)))
    }

    /// Names directory `id` in its parent, keeping a hinted directory's
    /// unchanged name, or puts it as a new root.
    fn name_directory(
        &mut self,
        d: &DirectoryObservation<'_>,
        id: u32,
        parent_id: Option<u32>,
        hinted: bool,
        continuing: bool,
    ) -> Result<(), Error> {
        if let Some(parent) = parent_id {
            let same_edge = hinted
                .then(|| self.old.dir_name(InoId(id)))
                .flatten()
                .filter(|&name| {
                    let edge = self.old.name(name);
                    edge.parent.0 == parent && edge.bytes == d.name
                });
            if let Some(name) = same_edge {
                self.keep(name, d.revealed)?;
            } else {
                self.edge(parent, id, d.name, d.revealed)?;
            }
        } else if !continuing {
            push(
                &self.budget,
                &mut self.changes.records,
                Record::RootPut {
                    id,
                    path: d.name.to_vec(),
                },
            )?;
        }
        Ok(())
    }

    /// Keeps untouched scopes' rows, returning the kept directories.
    fn preserved_files(
        &mut self,
        tree: &Tree<'_>,
        batches: &[Batch],
    ) -> Result<BTreeSet<u32>, Error> {
        let Tree { dirs, tokens } = tree;
        let mut kept = BTreeSet::new();
        for batch in batches {
            for (token, name) in batch.preserved_files() {
                let d = &dirs[tokens[&token]];
                if d.blocked || d.opaque {
                    continue;
                }
                if d.id != token.previous_directory().map(|id| id.0) {
                    return Err(invalid("untouched scope parent changed"));
                }
                self.names.insert(name.0);
                if let Target::Inode(child) = self.old.name(name).target() {
                    self.inodes.insert(child.0);
                    if self.old.is_directory(child) {
                        kept.insert(child.0);
                    }
                }
            }
        }
        Ok(kept)
    }

    /// Groups file observations by identity and places each group's inode and
    /// edges.
    fn place_files(
        &mut self,
        tree: &Tree<'_>,
        batches: &[Batch],
        protection: &crate::coverage::Protection,
    ) -> Result<(), Error> {
        let Tree { dirs, tokens } = tree;
        let mut files = self.observed_files(tree, batches, protection);
        self.reuse_files(tree, batches, protection, &mut files)?;
        let file = |loc: File| loc.observation(batches);
        files.sort_unstable_by(|&a, &b| {
            let a = file(a);
            let b = file(b);
            (a.stat.dev, a.stat.ino)
                .cmp(&(b.stat.dev, b.stat.ino))
                .then_with(|| {
                    (dirs[tokens[&a.parent]].id, a.name).cmp(&(dirs[tokens[&b.parent]].id, b.name))
                })
        });
        let same = |a: File, b: File| {
            let (a, b) = (file(a).stat, file(b).stat);
            (a.dev, a.ino) == (b.dev, b.ino)
        };
        for group in files.chunk_by(|&a, &b| same(a, b)) {
            self.place_inode(tree, batches, group)?;
        }
        Ok(())
    }

    /// The batches' file observations, less those under a blocked or opaque
    /// directory or behind a protected name edge.
    fn observed_files(
        &self,
        tree: &Tree<'_>,
        batches: &[Batch],
        protection: &crate::coverage::Protection,
    ) -> Vec<File> {
        let Tree { dirs, tokens } = tree;
        batches
            .iter()
            .enumerate()
            .flat_map(|(b, batch)| {
                (0..batch.file_count()).map(move |f| File::Full(b as u32, f as u32))
            })
            .filter(|&loc| {
                let f = loc.observation(batches);
                let dir = &dirs[tokens[&f.parent]];
                if dir.blocked || dir.opaque {
                    return false;
                }
                if !protection.edges.is_empty()
                    && let Some(id) = dir.id
                    && self.old.is_live_inode(InoId(id))
                    && let Some(name) = self.old.lookup(InoId(id), f.name)
                    && protection.edges.contains(&name.0)
                {
                    return false;
                }
                true
            })
            .collect()
    }

    /// Carries forward each reused file whose directory continues and which is
    /// no alias of an observed file; adds the rest to `files` to place.
    fn reuse_files(
        &mut self,
        tree: &Tree<'_>,
        batches: &[Batch],
        protection: &crate::coverage::Protection,
        files: &mut Vec<File>,
    ) -> Result<(), Error> {
        let Tree { dirs, tokens } = tree;
        // An unmatched observation may be another alias of a compacted row,
        // even with st_nlink == 1 (bind mounts). Expand that residue's
        // equal aliases so conflicts and canonical first-observation
        // selection remain unchanged.
        let mut aliases = Seen::new(self.old.next_inode().0);
        for &loc in files.iter() {
            let st = loc.observation(batches).stat;
            if let Some(id) = self.session.identity((st.dev, st.ino)) {
                aliases.insert(id.0);
            }
        }
        for (b, batch) in batches.iter().enumerate() {
            let mut parent = None;
            let mut continuing = false;
            let mut active = false;
            if files.is_empty()
                && protection.edges.is_empty()
                && batch.reused_parents().all(|token| {
                    let d = &dirs[tokens[&token]];
                    !d.blocked && !d.opaque && d.id == token.previous_directory().map(|id| id.0)
                })
            {
                let (names, inodes) = batch.reused_words();
                self.names.extend(names);
                self.inodes.extend(inodes);
                continue;
            }
            for (token, name) in batch.reused_files() {
                if parent != Some(token) {
                    parent = Some(token);
                    let d = &dirs[tokens[&token]];
                    active = !d.blocked && !d.opaque;
                    continuing = d.id == token.previous_directory().map(|id| id.0);
                }
                if !active || protection.edges.contains(&name.0) {
                    continue;
                }
                let child = self.old.name(name).child;
                if continuing && !aliases.contains(child.0) {
                    self.names.insert(name.0);
                    self.inodes.insert(child.0);
                } else {
                    self.budget.charge(1, std::mem::size_of::<File>())?;
                    files.push(File::Reused(b as u32, name.0));
                }
            }
        }
        Ok(())
    }

    /// Places one identity's inode, its link target, and an edge per name.
    fn place_inode(
        &mut self,
        tree: &Tree<'_>,
        batches: &[Batch],
        group: &[File],
    ) -> Result<(), Error> {
        let Tree { dirs, tokens } = tree;
        let first = group[0].observation(batches);
        let kind = Kind::from_mode(first.stat.mode);
        let id = match self
            .session
            .identity((first.stat.dev, first.stat.ino))
            .filter(|&id| self.old.kind(id) == kind)
        {
            Some(id) => id.0,
            None => allocate(&mut self.changes.counters[0], u32::MAX - 16)?,
        };
        let conflict = group.iter().any(|&loc| {
            let other = loc.observation(batches);
            other.stat != first.stat
                || other.content != first.content
                || other.target != first.target
        });
        self.observe_inode(
            id,
            kind,
            first.stat,
            if conflict {
                Content::Fault
            } else {
                first.content
            },
        )?;
        if kind == Kind::Symlink
            && (!self.old.is_live_inode(InoId(id))
                || self.old.link_target(InoId(id)) != first.target)
        {
            push(
                &self.budget,
                &mut self.changes.records,
                Record::LinkPut {
                    id,
                    target: first.target.unwrap_or_default().to_vec(),
                },
            )?;
        }
        for &loc in group {
            let f = loc.observation(batches);
            let parent = dirs[tokens[&f.parent]]
                .id
                .ok_or_else(|| invalid("collapsed file parent"))?;
            self.edge(parent, id, f.name, f.revealed)?;
        }
        Ok(())
    }

    fn ignored_entries(
        &mut self,
        tree: &Tree<'_>,
        batches: &[Batch],
        protection: &crate::coverage::Protection,
    ) -> Result<(), Error> {
        let Tree { dirs, tokens } = tree;
        for batch in batches {
            for (parent, name, kind) in batch.ignored_entries() {
                let d = &dirs[tokens[&parent]];
                if d.blocked || d.opaque {
                    continue;
                }
                if !protection.edges.is_empty()
                    && let Some(id) = d.id
                    && self.old.is_live_inode(InoId(id))
                    && let Some(edge) = self.old.lookup(InoId(id), name)
                    && protection.edges.contains(&edge.0)
                {
                    continue;
                }
                if let Some(parent) = dirs[tokens[&parent]].id {
                    self.edge(parent, ignored(kind), name, false)?;
                }
            }
        }
        Ok(())
    }

    /// Removes the refreshed and dropped roots' names that were not seen again.
    fn sweep(
        &mut self,
        refreshed: &[PathBuf],
        dropped: &[PathBuf],
        protection: &crate::coverage::Protection,
        kept: &BTreeSet<u32>,
    ) -> Result<(), Error> {
        let swept: Vec<_> = self
            .old
            .roots()
            .filter(|(_, path)| {
                refreshed
                    .iter()
                    .chain(dropped)
                    .any(|p| p.as_os_str().as_bytes() == *path)
            })
            .map(|(id, _)| id)
            .collect();
        for root in swept {
            let mut queue = vec![root];
            while let Some(dir) = queue.pop() {
                if protection.directories.contains(&dir.0) || kept.contains(&dir.0) {
                    continue;
                }
                for name in self.old.children(dir) {
                    let edge = self.old.name(name);
                    if let Target::Inode(child) = edge.target()
                        && self.old.is_directory(child)
                    {
                        queue.push(child);
                    }
                    if !self.names.contains(name.0) {
                        self.budget.charge(1, 48)?;
                        self.removed.insert(name.0);
                    }
                }
            }
            if dropped.iter().any(|p| {
                self.old
                    .roots()
                    .any(|(id, path)| id == root && path == p.as_os_str().as_bytes())
            }) || !self.inodes.contains(root.0)
            {
                push(
                    &self.budget,
                    &mut self.changes.records,
                    Record::RootDelete { id: root.0 },
                )?;
                self.references.entry(root.0).or_default();
            }
        }
        Ok(())
    }

    /// Puts each placed directory's row where it changed, and clears stale
    /// markers.
    fn directory_records(
        &mut self,
        tree: &Tree<'_>,
        protection: &crate::coverage::Protection,
    ) -> Result<(), Error> {
        let dirs = &tree.dirs;
        let own_names: BTreeMap<_, _> = self
            .changes
            .records
            .iter()
            .filter_map(|r| match r {
                Record::NamePut { id, child, .. } => Some((*child, *id)),
                _ => None,
            })
            .collect();
        let mut marked = BTreeSet::new();
        for d in dirs {
            let Some(id) = d.id else {
                continue;
            };
            let stale = protection.markers.contains(&id);
            if stale {
                marked.insert(id);
            }
            if let Some(record) = self.directory_record(id, d, &own_names, stale) {
                push(&self.budget, &mut self.changes.records, record)?;
            }
        }
        for &id in protection.markers.difference(&marked) {
            if let Some(record) = self.cleared_marker(InoId(id)) {
                push(&self.budget, &mut self.changes.records, record)?;
            }
        }
        Ok(())
    }

    /// The row for placed directory `d`, or None when the old catalog already
    /// has it. A `stale` marker drops its entry count.
    fn directory_record(
        &self,
        id: u32,
        d: &Directory<'_>,
        own_names: &BTreeMap<u32, u32>,
        stale: bool,
    ) -> Option<Record> {
        let name = if d.observation.parent.is_some() {
            own_names.get(&id).copied().or_else(|| {
                self.old
                    .is_live_inode(InoId(id))
                    .then(|| self.old.dir_name(InoId(id)))
                    .flatten()
                    .map(|n| n.0)
            })
        } else {
            None
        };
        let retained_at = stale.then(|| self.retained_sequence(InoId(id))).flatten();
        let entries = if stale || d.opaque { None } else { d.entries };
        let flags = if retained_at.is_some() {
            8
        } else if stale || d.opaque {
            0
        } else {
            4
        } | if d.observation.traversed { 3 } else { 0 };
        let unchanged = self.old.is_live_inode(InoId(id))
            && self.old.dir_name(InoId(id)).map(|n| n.0) == name
            && self.old.entry_count(InoId(id)) == entries
            && self.old.is_traversed(InoId(id)) == d.observation.traversed
            && self.old.retained_at(InoId(id)) == retained_at;
        (!unchanged).then_some(Record::DirPut {
            id,
            name,
            entries,
            flags,
            retained_at,
        })
    }

    /// The row clearing the entry count of `dir`, a stale marker this run
    /// did not place, or None when the old catalog already has it.
    fn cleared_marker(&self, dir: InoId) -> Option<Record> {
        let retained = self.retained_sequence(dir);
        (self.old.entry_count(dir).is_some() || self.old.retained_at(dir) != retained).then(|| {
            Record::DirPut {
                id: dir.0,
                name: self.old.dir_name(dir).map(|n| n.0),
                entries: None,
                flags: if retained.is_some() { 8 } else { 0 }
                    | if self.old.is_traversed(dir) { 3 } else { 0 },
                retained_at: retained,
            }
        })
    }

    fn work_trees(&mut self, tree: &Tree<'_>, batches: &[Batch]) -> Result<(), Error> {
        let Tree { dirs, tokens } = tree;
        let work_trees: BTreeMap<_, _> = batches
            .iter()
            .flat_map(Batch::work_tree_observations)
            .filter_map(|(token, kind, path, identity)| {
                if dirs[tokens[&token]].blocked || dirs[tokens[&token]].opaque {
                    return None;
                }
                dirs[tokens[&token]]
                    .id
                    .map(|id| (id, (kind, path, identity)))
            })
            .collect();
        for d in dirs {
            let Some(id) = d.id else {
                continue;
            };
            let previous = self
                .old
                .is_live_inode(InoId(id))
                .then(|| self.old.work_tree(InoId(id)))
                .flatten();
            match (previous, work_trees.get(&id)) {
                (old, Some(&(kind, path, common_id)))
                    if old.is_none_or(|old| {
                        old.kind != kind || old.common_dir != path || old.common_id != common_id
                    }) =>
                {
                    push(
                        &self.budget,
                        &mut self.changes.records,
                        Record::WorkTreePut {
                            id,
                            kind,
                            common_id,
                            path: path.to_vec(),
                        },
                    )?;
                }
                (Some(_), None) => push(
                    &self.budget,
                    &mut self.changes.records,
                    Record::WorkTreeDelete { id },
                )?,
                _ => {}
            }
        }
        Ok(())
    }

    fn retained_sequence(&self, id: InoId) -> Option<u64> {
        self.old.retained_at(id).or_else(|| {
            self.old
                .entry_count(id)
                .map(|_| self.old.generation().sequence)
        })
    }
    fn edge(&mut self, parent: u32, child: u32, name: &[u8], revealed: bool) -> Result<(), Error> {
        if self.old.is_live_inode(InoId(parent))
            && let Some(id) = self.old.lookup(InoId(parent), name)
        {
            let before = self.old.name(id);
            if before.child.0 == child {
                return self.keep(id, revealed);
            }
        }
        self.budget
            .charge(1, std::mem::size_of::<Edge>() + name.len())?;
        self.pending.push(Edge {
            parent,
            child,
            name: name.to_vec(),
            revealed,
        });
        Ok(())
    }
    /// Keeps old name `id`. When only its revealed flag changed (D127), as
    /// when an ignore file's `!` line was added or removed, re-puts it under
    /// the same id, as a rename does: no reference changes.
    fn keep(&mut self, id: NameId, revealed: bool) -> Result<(), Error> {
        self.names.insert(id.0);
        if self.old.is_revealed(id) == revealed {
            return Ok(());
        }
        let edge = self.old.name(id);
        push(
            &self.budget,
            &mut self.changes.records,
            Record::NamePut {
                id: id.0,
                parent: edge.parent.0,
                child: edge.child.0,
                name: edge.bytes.to_vec(),
                revealed,
            },
        )
    }
    fn observe_inode(
        &mut self,
        id: u32,
        kind: Kind,
        stat: Stat,
        content: Content,
    ) -> Result<(), Error> {
        let live = self.old.is_live_inode(InoId(id));
        if live {
            self.inodes.insert(id);
        } else {
            self.references.entry(id).or_default();
        }
        let doc = if let Content::Hashed(hash) = content {
            let continuing = live
                .then(|| self.old.doc(InoId(id)))
                .flatten()
                .filter(|&doc| self.old.doc_hash(doc) == Some(hash))
                .map(|doc| doc.0);
            match continuing
                .or_else(|| self.session.document(hash).map(|id| id.0))
                .or_else(|| self.new_docs.get(&hash).copied())
            {
                Some(id) => Some(id),
                None => {
                    let id = allocate(&mut self.changes.counters[2], u32::MAX)?;
                    self.new_docs.insert(hash, id);
                    Some(id)
                }
            }
        } else {
            None
        };
        if !live || {
            let inode = self.old.inode(InoId(id));
            inode.stat != stat || inode.state != content.state() || inode.doc.map(|id| id.0) != doc
        } {
            self.budget.charge(1, std::mem::size_of::<Record>() + 48)?;
            self.puts.insert(
                id,
                Record::InodePut {
                    id,
                    kind,
                    state: content.state(),
                    doc,
                    stat,
                },
            );
        }
        Ok(())
    }
}

fn record_key(record: &Record) -> (u8, u32) {
    match record {
        Record::LifePut { id, .. } => (1, *id),
        Record::InodeDelete { id } => (2, *id),
        Record::NamePut { id, .. } => (3, *id),
        Record::NameDelete { id } => (4, *id),
        Record::DirPut { id, .. } => (5, *id),
        Record::RootPut { id, .. } => (6, *id),
        Record::RootDelete { id } => (7, *id),
        Record::PolicyPut { .. } => (8, 0),
        Record::InodePut { id, .. } => (9, *id),
        Record::LinkPut { id, .. } => (10, *id),
        Record::LinkDelete { id } => (11, *id),
        Record::WorkTreePut { id, .. } => (12, *id),
        Record::WorkTreeDelete { id } => (13, *id),
        Record::DocPut { id, .. } => (14, *id),
        Record::DocDelete { id } => (15, *id),
    }
}

fn push(
    budget: &ferret_catalog::InputBudget,
    records: &mut Vec<Record>,
    record: Record,
) -> Result<(), Error> {
    let bytes = match &record {
        Record::NamePut { name, .. } => name.len(),
        Record::RootPut { path, .. } | Record::WorkTreePut { path, .. } => path.len(),
        Record::LinkPut { target, .. } => target.len(),
        _ => 0,
    };
    budget.charge(1, std::mem::size_of::<Record>() + bytes)?;
    records.push(record);
    Ok(())
}
