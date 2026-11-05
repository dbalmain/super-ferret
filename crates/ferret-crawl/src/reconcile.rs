//! Whole-walk observations to a sparse final change set. Directory tokens are
//! resolved against the effective graph, never a renumbered candidate snapshot.
//! Only covered portions of refreshed roots are swept; kept roots remain in
//! the pinned view. Typed protection scopes stop sweeps at checked old
//! boundaries and discard every worker's new observations below them before
//! alias grouping.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use ferret_catalog::batch::DirectoryObservation;
use ferret_catalog::log::{ChangeSet, Error, Record};

mod directories;
mod files;
mod finish;

use ferret_catalog::{
    Batch, Catalog, Content, DecodeError, DirToken, Hash, InoId, Kind, NameId, Stat, Target,
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
                .or_else(|| self.session.returning_document(hash).map(|id| id.0))
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
        Record::DocDormant { id, .. } => (16, *id),
        Record::DocForget { id } => (17, *id),
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
