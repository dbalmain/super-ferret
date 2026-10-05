//! Whole-walk observations to a sparse final change set. Directory tokens are
//! resolved against the effective graph, never a renumbered candidate snapshot.
//! Only covered portions of refreshed roots are swept; kept roots remain in
//! the pinned view. Typed protection scopes stop sweeps at checked old
//! boundaries and discard every worker's new observations below them before
//! alias grouping.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use ferret_catalog::batch::{DirectoryObservation, FileObservation};
use ferret_catalog::log::{ChangeSet, Error, Record};
use ferret_catalog::{
    Batch, Catalog, Content, DecodeError, DirToken, DocId, Hash, InoId, Kind, NameId, Stat, Target,
    WriterSession,
};

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
    if !protection.is_empty() && (old.policy() != policy || old.sniffer_version() != sniffer) {
        return Err(invalid("version transition under protection"));
    }
    // Retained old scopes still prove their old re-inclusion ancestry (D29).
    for d in &mut dirs {
        if d.observation
            .token
            .previous_directory()
            .is_some_and(|id| protection.directories.contains(&id.0))
        {
            d.included = true;
        }
    }
    // D29: only ancestors of an included row survive a Traverse observation.
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
    let mut out = Final {
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
    };
    // Retain only checked boundary IDs. Descendants stay untouched because
    // sweeping stops before iterating that boundary's old child range.
    for &id in &protection.directories {
        out.inodes.insert(id);
        if let Some(name) = out.old.dir_name(InoId(id)) {
            out.names.insert(name.0);
        }
    }
    for &id in &protection.edges {
        out.names.insert(id);
        if let Target::Inode(child) = out.old.name(NameId(id)).target() {
            out.inodes.insert(child.0);
        }
    }
    let mut roots: Vec<_> = dirs
        .iter()
        .enumerate()
        .filter(|(_, d)| d.observation.parent.is_none())
        .map(|(i, _)| i)
        .collect();
    roots.sort_unstable_by_key(|&i| dirs[i].observation.name);
    let observed_roots: Vec<_> = roots.iter().map(|&i| dirs[i].observation.name).collect();
    if observed_roots
        != refreshed
            .iter()
            .filter(|p| {
                !out.old.roots().any(|(id, path)| {
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
                out.edge(parent, ignored(Kind::Dir), d.name)?;
            }
            continue;
        }
        let root = parent.map_or_else(
            || {
                out.old
                    .roots()
                    .find(|(_, path)| *path == d.name)
                    .map(|(id, _)| id.0)
            },
            |p| Some(dirs[p].root),
        );
        let hinted = d
            .token
            .previous_directory()
            .filter(|&id| !out.inodes.contains(id.0));
        let continuing = hinted
            .or_else(|| {
                if let Some(parent) = parent_id {
                    out.old
                        .is_live_inode(InoId(parent))
                        .then(|| out.old.lookup(InoId(parent), d.name))
                        .flatten()
                        .map(|id| out.old.name(id).child)
                        .filter(|&id| {
                            out.old.is_live_inode(id)
                                && out.old.is_directory(id)
                                && out.old.identity(id) == (d.stat.dev, d.stat.ino)
                        })
                        .or_else(|| {
                            root.and_then(|r| {
                                session.directory_identity(InoId(r), (d.stat.dev, d.stat.ino))
                            })
                        })
                } else {
                    root.map(InoId)
                        .filter(|&id| out.old.identity(id) == (d.stat.dev, d.stat.ino))
                }
            })
            .filter(|id| !out.inodes.contains(id.0));
        let id = match continuing {
            Some(id) => id.0,
            None => allocate(&mut out.changes.counters[0], u32::MAX - 16)?,
        };
        dirs[i].id = Some(id);
        dirs[i].root = parent.map_or(id, |p| dirs[p].root);
        out.observe_inode(id, Kind::Dir, *d.stat, Content::Unindexed)?;
        if let Some(parent) = parent_id {
            let same_edge = hinted
                .and_then(|_| out.old.dir_name(InoId(id)))
                .filter(|&name| {
                    let edge = out.old.name(name);
                    edge.parent.0 == parent && edge.bytes == d.name
                });
            if let Some(name) = same_edge {
                out.names.insert(name.0);
            } else {
                out.edge(parent, id, d.name)?;
            }
        } else if continuing.is_none() {
            push(
                &out.budget,
                &mut out.changes.records,
                Record::RootPut {
                    id,
                    path: d.name.to_vec(),
                },
            )?;
        }
    }
    // A disconnected/cyclic token graph cannot reach publication.
    if visited != dirs.len() {
        return Err(invalid("unreachable directory observation"));
    }
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
            out.names.insert(name.0);
            if let Target::Inode(child) = out.old.name(name).target() {
                out.inodes.insert(child.0);
                if out.old.is_directory(child) {
                    kept.insert(child.0);
                }
            }
        }
    }
    let mut files: Vec<File> = batches
        .iter()
        .enumerate()
        .flat_map(|(b, batch)| (0..batch.file_count()).map(move |f| File::Full(b as u32, f as u32)))
        .filter(|&loc| {
            let f = loc.observation(batches);
            let dir = &dirs[tokens[&f.parent]];
            if dir.blocked || dir.opaque {
                return false;
            }
            if !protection.edges.is_empty()
                && let Some(id) = dir.id
                && out.old.is_live_inode(InoId(id))
                && let Some(name) = out.old.lookup(InoId(id), f.name)
                && protection.edges.contains(&name.0)
            {
                return false;
            }
            true
        })
        .collect();
    let file = |loc: File| loc.observation(batches);
    // An unmatched observation may be another alias of a compacted row, even
    // with st_nlink == 1 (bind mounts). Expand that residue's equal aliases so
    // conflicts and canonical first-observation selection remain unchanged.
    let mut aliases = Seen::new(out.old.next_inode().0);
    for &loc in &files {
        let st = file(loc).stat;
        if let Some(id) = session.identity((st.dev, st.ino)) {
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
            out.names.extend(names);
            out.inodes.extend(inodes);
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
            let child = out.old.name(name).child;
            if continuing && !aliases.contains(child.0) {
                out.names.insert(name.0);
                out.inodes.insert(child.0);
            } else {
                files.push(File::Reused(b as u32, name.0));
            }
        }
    }
    files.sort_unstable_by(|&a, &b| {
        let a = file(a);
        let b = file(b);
        (a.stat.dev, a.stat.ino)
            .cmp(&(b.stat.dev, b.stat.ino))
            .then_with(|| {
                (dirs[tokens[&a.parent]].id, a.name).cmp(&(dirs[tokens[&b.parent]].id, b.name))
            })
    });
    let mut at = 0;
    while at < files.len() {
        let first = file(files[at]);
        // Same-identity runs are almost always length 1, so scan forward: a
        // binary search over the remaining slice per run is O(n log n).
        let mut end = at + 1;
        while end < files.len() {
            let st = file(files[end]).stat;
            if (st.dev, st.ino) != (first.stat.dev, first.stat.ino) {
                break;
            }
            end += 1;
        }
        let kind = Kind::from_mode(first.stat.mode);
        let id = match session
            .identity((first.stat.dev, first.stat.ino))
            .filter(|&id| out.old.kind(id) == kind)
        {
            Some(id) => id.0,
            None => allocate(&mut out.changes.counters[0], u32::MAX - 16)?,
        };
        let conflict = files[at..end].iter().any(|&loc| {
            let other = file(loc);
            other.stat != first.stat
                || other.content != first.content
                || other.target != first.target
        });
        out.observe_inode(
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
            && (!out.old.is_live_inode(InoId(id)) || out.old.link_target(InoId(id)) != first.target)
        {
            push(
                &out.budget,
                &mut out.changes.records,
                Record::LinkPut {
                    id,
                    target: first.target.unwrap_or_default().to_vec(),
                },
            )?;
        }
        for &loc in &files[at..end] {
            let f = file(loc);
            let parent = dirs[tokens[&f.parent]]
                .id
                .ok_or_else(|| invalid("collapsed file parent"))?;
            out.edge(parent, id, f.name)?;
        }
        at = end;
    }
    for batch in batches {
        for (parent, name, kind) in batch.ignored_entries() {
            let d = &dirs[tokens[&parent]];
            if d.blocked || d.opaque {
                continue;
            }
            if !protection.edges.is_empty()
                && let Some(id) = d.id
                && out.old.is_live_inode(InoId(id))
                && let Some(edge) = out.old.lookup(InoId(id), name)
                && protection.edges.contains(&edge.0)
            {
                continue;
            }
            if let Some(parent) = dirs[tokens[&parent]].id {
                out.edge(parent, ignored(kind), name)?;
            }
        }
    }
    let swept: Vec<_> = out
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
            for name in out.old.children(dir) {
                let edge = out.old.name(name);
                if let Target::Inode(child) = edge.target()
                    && out.old.is_directory(child)
                {
                    queue.push(child);
                }
                if !out.names.contains(name.0) {
                    out.budget.charge(1, 48)?;
                    out.removed.insert(name.0);
                }
            }
        }
        if dropped.iter().any(|p| {
            out.old
                .roots()
                .any(|(id, path)| id == root && path == p.as_os_str().as_bytes())
        }) || !out.inodes.contains(root.0)
        {
            push(
                &out.budget,
                &mut out.changes.records,
                Record::RootDelete { id: root.0 },
            )?;
            out.references.entry(root.0).or_default();
        }
    }
    out.finish_edges()?;
    let own_names: BTreeMap<_, _> = out
        .changes
        .records
        .iter()
        .filter_map(|r| match r {
            Record::NamePut { id, child, .. } => Some((*child, *id)),
            _ => None,
        })
        .collect();
    let mut marked = BTreeSet::new();
    for d in &dirs {
        let Some(id) = d.id else {
            continue;
        };
        let name = if d.observation.parent.is_some() {
            own_names.get(&id).copied().or_else(|| {
                out.old
                    .is_live_inode(InoId(id))
                    .then(|| out.old.dir_name(InoId(id)))
                    .flatten()
                    .map(|n| n.0)
            })
        } else {
            None
        };
        let stale = protection.markers.contains(&id);
        let retained_at = stale.then(|| out.retained_sequence(InoId(id))).flatten();
        let entries = if stale || d.opaque { None } else { d.entries };
        let flags = if retained_at.is_some() {
            8
        } else if stale || d.opaque {
            0
        } else {
            4
        } | if d.observation.traversed { 3 } else { 0 };
        if stale {
            marked.insert(id);
        }
        if !out.old.is_live_inode(InoId(id))
            || out.old.dir_name(InoId(id)).map(|n| n.0) != name
            || out.old.entry_count(InoId(id)) != entries
            || out.old.is_traversed(InoId(id)) != d.observation.traversed
            || out.old.retained_at(InoId(id)) != retained_at
        {
            push(
                &out.budget,
                &mut out.changes.records,
                Record::DirPut {
                    id,
                    name,
                    entries,
                    flags,
                    retained_at,
                },
            )?;
        }
    }
    for &id in protection.markers.difference(&marked) {
        let dir = InoId(id);
        let retained = out.retained_sequence(dir);
        if out.old.entry_count(dir).is_some() || out.old.retained_at(dir) != retained {
            push(
                &out.budget,
                &mut out.changes.records,
                Record::DirPut {
                    id,
                    name: out.old.dir_name(dir).map(|n| n.0),
                    entries: None,
                    flags: if retained.is_some() { 8 } else { 0 }
                        | if out.old.is_traversed(dir) { 3 } else { 0 },
                    retained_at: retained,
                },
            )?;
        }
    }
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
    for d in &dirs {
        let Some(id) = d.id else {
            continue;
        };
        let previous = out
            .old
            .is_live_inode(InoId(id))
            .then(|| out.old.work_tree(InoId(id)))
            .flatten();
        match (previous, work_trees.get(&id)) {
            (old, Some(&(kind, path, common_id)))
                if old.is_none_or(|old| {
                    old.kind != kind || old.common_dir != path || old.common_id != common_id
                }) =>
            {
                push(
                    &out.budget,
                    &mut out.changes.records,
                    Record::WorkTreePut {
                        id,
                        kind,
                        common_id,
                        path: path.to_vec(),
                    },
                )?;
            }
            (Some(_), None) => push(
                &out.budget,
                &mut out.changes.records,
                Record::WorkTreeDelete { id },
            )?,
            _ => {}
        }
    }
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

impl Final<'_> {
    fn retained_sequence(&self, id: InoId) -> Option<u64> {
        self.old.retained_at(id).or_else(|| {
            self.old
                .entry_count(id)
                .map(|_| self.old.generation().sequence)
        })
    }
    fn edge(&mut self, parent: u32, child: u32, name: &[u8]) -> Result<(), Error> {
        if self.old.is_live_inode(InoId(parent))
            && let Some(id) = self.old.lookup(InoId(parent), name)
        {
            let before = self.old.name(id);
            if before.child.0 == child {
                self.names.insert(id.0);
                return Ok(());
            }
        }
        self.budget
            .charge(1, std::mem::size_of::<Edge>() + name.len())?;
        self.pending.push(Edge {
            parent,
            child,
            name: name.to_vec(),
        });
        Ok(())
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
    fn finish_edges(&mut self) -> Result<(), Error> {
        self.pending
            .sort_unstable_by(|a, b| (a.parent, &a.name).cmp(&(b.parent, &b.name)));
        if self
            .pending
            .windows(2)
            .any(|e| (e[0].parent, &e[0].name) == (e[1].parent, &e[1].name))
        {
            return Err(invalid("duplicate recrawl name"));
        }
        let mut deleted_by_inode: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        let mut added_by_inode: BTreeMap<u32, usize> = BTreeMap::new();
        for &name in &self.removed {
            if let Target::Inode(child) = self.old.name(NameId(name)).target() {
                deleted_by_inode.entry(child.0).or_default().push(name);
            }
        }
        for edge in &self.pending {
            *added_by_inode.entry(edge.child).or_default() += 1;
        }
        let pending = std::mem::take(&mut self.pending);
        for edge in pending {
            let rename = deleted_by_inode
                .get(&edge.child)
                .filter(|ids| {
                    ids.len() == 1
                        && added_by_inode[&edge.child] == 1
                        && self.inodes.contains(edge.child)
                })
                .map(|ids| ids[0]);
            let id = match rename {
                Some(id) => {
                    self.removed.remove(&id);
                    id
                }
                None => {
                    self.changes.counts[1] += 1;
                    allocate(&mut self.changes.counters[1], u32::MAX)?
                }
            };
            if rename.is_none() && edge.child < u32::MAX - 16 {
                *self.references.entry(edge.child).or_default() += 1;
            }
            push(
                &self.budget,
                &mut self.changes.records,
                Record::NamePut {
                    id,
                    parent: edge.parent,
                    child: edge.child,
                    name: edge.name,
                },
            )?;
        }
        for &id in &self.removed {
            if let Target::Inode(child) = self.old.name(NameId(id)).target() {
                *self.references.entry(child.0).or_default() -= 1;
            }
            self.changes.counts[1] -= 1;
            push(
                &self.budget,
                &mut self.changes.records,
                Record::NameDelete { id },
            )?;
        }
        Ok(())
    }
    fn finish_inodes(&mut self) -> Result<(), Error> {
        let mut roots: BTreeSet<_> = self.old.roots().map(|(id, _)| id.0).collect();
        for record in &self.changes.records {
            match record {
                Record::RootPut { id, .. } => {
                    roots.insert(*id);
                }
                Record::RootDelete { id } => {
                    roots.remove(id);
                }
                _ => {}
            }
        }
        for (&id, &delta) in &self.references {
            let live = self.old.is_live_inode(InoId(id));
            let names = i64::from(self.session.name_references(InoId(id))) + delta;
            let names = u32::try_from(names).map_err(|_| invalid("recrawl name references"))?;
            let root = roots.contains(&id);
            if names == 0 && !root {
                if !live {
                    return Err(invalid("unreachable inode birth"));
                }
                self.puts.remove(&id);
                push(
                    &self.budget,
                    &mut self.changes.records,
                    Record::InodeDelete { id },
                )?;
                self.changes.counts[0] -= 1;
                if self.old.is_directory(InoId(id)) {
                    self.changes.counts[2] -= 1;
                    if self.old.work_tree(InoId(id)).is_some() {
                        push(
                            &self.budget,
                            &mut self.changes.records,
                            Record::WorkTreeDelete { id },
                        )?;
                    }
                } else if self.old.kind(InoId(id)) == Kind::Symlink {
                    push(
                        &self.budget,
                        &mut self.changes.records,
                        Record::LinkDelete { id },
                    )?;
                }
                if let Some(doc) = self.old.doc(InoId(id)) {
                    *self.document_refs.entry(doc.0).or_default() -= 1;
                }
            } else if !live || delta != 0 {
                let kind = self
                    .puts
                    .get(&id)
                    .and_then(|r| {
                        if let Record::InodePut { kind, .. } = r {
                            Some(*kind)
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| self.old.kind(InoId(id)));
                if !live {
                    self.changes.counts[0] += 1;
                    if kind == Kind::Dir {
                        self.changes.counts[2] += 1;
                    }
                }
                push(
                    &self.budget,
                    &mut self.changes.records,
                    Record::LifePut {
                        id,
                        kind,
                        flags: 0,
                        names,
                    },
                )?;
            }
        }
        for (&id, record) in &self.puts {
            let Record::InodePut { doc, .. } = record else {
                continue;
            };
            let before = self
                .old
                .is_live_inode(InoId(id))
                .then(|| self.old.doc(InoId(id)))
                .flatten()
                .map(|id| id.0);
            if before != *doc {
                if let Some(id) = before {
                    *self.document_refs.entry(id).or_default() -= 1;
                }
                if let Some(id) = doc {
                    *self.document_refs.entry(*id).or_default() += 1;
                }
            }
        }
        for record in std::mem::take(&mut self.puts).into_values() {
            push(&self.budget, &mut self.changes.records, record)?;
        }
        let hashes: BTreeMap<_, _> = self
            .new_docs
            .iter()
            .map(|(&hash, &id)| (id, hash))
            .collect();
        for (&id, &delta) in &self.document_refs {
            if delta == 0 {
                continue;
            }
            let old = self.old.doc_references(DocId(id));
            let refs = i64::from(old.unwrap_or(0)) + delta;
            let refs = u32::try_from(refs).map_err(|_| invalid("recrawl document references"))?;
            if refs == 0 {
                self.changes.counts[3] -= 1;
                push(
                    &self.budget,
                    &mut self.changes.records,
                    Record::DocDelete { id },
                )?;
            } else {
                let hash = self
                    .old
                    .doc_hash(DocId(id))
                    .or_else(|| hashes.get(&id).copied())
                    .ok_or_else(|| invalid("recrawl document hash"))?;
                if old.is_none() {
                    self.changes.counts[3] += 1;
                }
                push(
                    &self.budget,
                    &mut self.changes.records,
                    Record::DocPut {
                        id,
                        references: refs,
                        hash,
                    },
                )?;
            }
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
