//! Resident crawl writer: one lock and checked view, with sorted base-id
//! lookups and sparse lookup updates after publication. Reconciliation belongs
//! to ferret-crawl; this module knows only observations and catalog records.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::log::{ChangeSet, Error, Record, Writer};
use crate::{Batch, Catalog, Content, ContentState, DocId, Hash, InoId, NameId, Stat, Target};

/// A resident writer for same-epoch recrawls. Opening warms and validates the
/// published view once. Failed durable publication poisons the underlying
/// writer; drop and reopen rather than retrying an uncertain tail.
pub struct WriterSession {
    writer: Writer,
    lookup_base: Catalog,
    identities: Vec<InoId>,
    documents: Vec<u32>,
    directories: Vec<(InoId, InoId)>,
    directory_changes: BTreeMap<(u32, u64, u64), Vec<InoId>>,
    identity_changes: BTreeMap<(u64, u64), Option<InoId>>,
    document_changes: BTreeMap<Hash, Option<DocId>>,
    first_names: Vec<u32>,
    alias_names: BTreeMap<InoId, Vec<NameId>>,
    name_changes: BTreeMap<InoId, BTreeSet<NameId>>,
    next_batch: AtomicU32,
    limits: crate::CompactionLimits,
}

impl WriterSession {
    /// Takes the writer lock. Initial indexing needs a checkpoint first.
    pub fn open(dir: &Path) -> Result<Self, Error> {
        std::fs::create_dir_all(dir).map_err(Error::Io)?;
        let writer = Writer::open(dir)?;
        let mut session = Self {
            lookup_base: writer.view(),
            writer,
            identities: Vec::new(),
            documents: Vec::new(),
            directories: Vec::new(),
            directory_changes: BTreeMap::new(),
            identity_changes: BTreeMap::new(),
            document_changes: BTreeMap::new(),
            first_names: Vec::new(),
            alias_names: BTreeMap::new(),
            name_changes: BTreeMap::new(),
            next_batch: AtomicU32::new(0),
            limits: crate::CompactionLimits::default(),
        };
        session.rebuild();
        Ok(session)
    }

    fn rebuild(&mut self) {
        let view = self.writer.view();
        // Publication has selected a new epoch. Retire old lookup storage
        // before cached-key sorts allocate their transient keys.
        self.lookup_base = view.clone();
        self.identities = Vec::new();
        self.documents = Vec::new();
        self.directories = Vec::new();
        self.first_names = Vec::new();
        self.alias_names.clear();
        self.name_changes.clear();
        self.identity_changes.clear();
        self.document_changes.clear();
        self.directory_changes.clear();
        let mut identities: Vec<_> = view
            .inode_ids()
            .filter(|&id| !view.is_directory(id))
            .collect();
        // Each key decodes columns; cache it rather than recompute it per
        // comparison.
        identities.sort_by_cached_key(|&id| view.identity(id));
        let mut documents: Vec<_> = (0..view.base_doc_count()).collect();
        documents.sort_by_cached_key(|&row| view.base_hash(row));
        let mut document_changes = BTreeMap::new();
        for record in view.changed_docs() {
            match record {
                Record::DocPut { id, hash, .. } => {
                    document_changes.insert(*hash, Some(DocId(*id)));
                }
                Record::DocDelete { id } => {
                    if let Some(hash) = view.base_doc_hash(DocId(*id)) {
                        document_changes.insert(hash, None);
                    }
                }
                _ => {}
            }
        }
        let mut directories: Vec<_> = view.dir_ids().map(|id| (root_of(&view, id), id)).collect();
        directories.sort_by_cached_key(|&(root, id)| (root, view.identity(id)));
        // One dense first-name id per inode, with sparse extra aliases. Scope
        // promotion must not scan ten million names for one changed hard link.
        let mut first_names = vec![u32::MAX; view.next_inode().0 as usize];
        let mut alias_names: BTreeMap<InoId, Vec<NameId>> = BTreeMap::new();
        let names = view.name_reader();
        for (name, edge) in names.runs_from(NameId(0)) {
            if let Target::Inode(child) = edge.target() {
                let first = &mut first_names[child.0 as usize];
                if *first == u32::MAX {
                    *first = name.0;
                } else {
                    alias_names.entry(child).or_default().push(name);
                }
            }
        }
        self.lookup_base = view;
        self.identities = identities;
        self.documents = documents;
        self.identity_changes.clear();
        self.document_changes = document_changes;
        self.directories = directories;
        self.directory_changes.clear();
        self.first_names = first_names;
        self.alias_names = alias_names;
        self.name_changes.clear();
        self.next_batch.store(0, Ordering::Relaxed);
    }

    /// Changes host-selected checkpoint budgets. The defaults are S1+'s
    /// published limits; larger limits are useful for measuring replay costs.
    pub fn set_compaction_limits(&mut self, limits: crate::CompactionLimits) {
        self.limits = limits;
    }

    pub fn budget_usage(&self) -> crate::BudgetUsage {
        self.writer.budget_usage()
    }

    /// Compacts the published view without advancing its sequence. Every
    /// epoch cache is rebuilt before returning; queued handles must retry.
    pub fn compact(&mut self) -> Result<Catalog, Error> {
        self.writer.checkpoint_view(self.view())?;
        self.rebuild();
        Ok(self.view())
    }

    /// Transfers the held lock and view to the explicit checkpoint fallback.
    pub fn into_checkpoint(self, sniffer: u32) -> crate::Transaction {
        self.writer.into_checkpoint(sniffer)
    }

    /// Shares the current checked view, retaining old generations for readers.
    pub fn view(&self) -> Catalog {
        self.writer.view()
    }

    /// Mints an observation batch; workers can call this concurrently.
    pub fn batch(&self) -> Batch {
        Batch::new(self.next_batch.fetch_add(1, Ordering::Relaxed), false)
            .with_previous(self.view())
    }

    /// Finds a continuing non-directory inode by its kernel identity.
    pub fn identity(&self, key: (u64, u64)) -> Option<InoId> {
        if let Some(id) = self.identity_changes.get(&key) {
            return *id;
        }
        let at = self
            .identities
            .binary_search_by_key(&key, |&id| self.lookup_base.identity(id))
            .ok()?;
        Some(self.identities[at])
    }

    /// Finds an unambiguous directory occurrence within one rooted tree.
    pub fn directory_identity(&self, root: InoId, key: (u64, u64)) -> Option<InoId> {
        let view = self.writer.view();
        let lookup = (root, key);
        let start = self
            .directories
            .partition_point(|&(r, id)| (r, self.lookup_base.identity(id)) < lookup);
        let mut matches = self.directories[start..]
            .iter()
            .take_while(|&&(r, id)| (r, self.lookup_base.identity(id)) == lookup)
            .map(|&(_, id)| id)
            .chain(
                self.directory_changes
                    .get(&(root.0, key.0, key.1))
                    .into_iter()
                    .flatten()
                    .copied(),
            )
            .filter(|&id| view.is_live_inode(id) && root_of(&view, id) == root);
        let first = matches.next()?;
        matches.all(|id| id == first).then_some(first)
    }

    /// Finds a live document without duplicating all base hashes.
    pub fn document(&self, hash: Hash) -> Option<DocId> {
        if let Some(id) = self.document_changes.get(&hash) {
            return *id;
        }
        let at = self
            .documents
            .binary_search_by_key(&hash, |&row| self.lookup_base.base_hash(row))
            .ok()?;
        Some(self.lookup_base.base_doc(self.documents[at]))
    }

    /// Exact indexed-name count, independent of filesystem `st_nlink`.
    pub fn name_references(&self, id: InoId) -> u32 {
        self.writer.view().indexed_name_count(id)
    }

    /// Current indexed aliases, without a whole-namespace search per burst.
    /// These ids belong to `view()`'s epoch and change only after publication.
    pub fn names_for(&self, id: InoId) -> impl Iterator<Item = NameId> + '_ {
        let changed = self.name_changes.get(&id);
        changed
            .into_iter()
            .flat_map(|ids| ids.iter().copied())
            .chain(
                self.first_names
                    .get(id.0 as usize)
                    .copied()
                    .filter(|&first| first != u32::MAX && changed.is_none())
                    .map(NameId)
                    .into_iter()
                    .chain(
                        self.alias_names
                            .get(&id)
                            .into_iter()
                            .flatten()
                            .copied()
                            .filter(move |_| changed.is_none()),
                    ),
            )
    }

    /// Carries only a matching version under the same sniffer.
    pub fn carry(&self, stat: &Stat, sniffer: u32) -> Option<Content> {
        let view = self.writer.view();
        if view.sniffer_version() != sniffer {
            return None;
        }
        let inode = view.inode(self.identity((stat.dev, stat.ino))?);
        if !inode.stat.same_version(stat) {
            return None;
        }
        match inode.state {
            ContentState::Unindexed => Some(Content::Unindexed),
            ContentState::Binary => Some(Content::Binary),
            ContentState::Hashed => inode
                .doc
                .and_then(|id| view.doc_hash(id))
                .map(Content::Hashed),
            ContentState::Fault => None,
        }
    }

    /// Publishes a final set, updating cached lookups only after success.
    pub fn commit(&mut self, changes: &ChangeSet, sniffer: u32) -> Result<Catalog, Error> {
        let previous = self.writer.view();
        self.writer
            .commit_budgeted(previous.generation(), changes, sniffer, self.limits)?;
        let view = self.writer.view();
        if view.generation().checkpoint != previous.generation().checkpoint {
            drop(previous);
            self.rebuild();
            return Ok(view);
        }
        let mut names: BTreeMap<InoId, BTreeSet<NameId>> = BTreeMap::new();
        for record in &changes.records {
            let name = match record {
                Record::NamePut { id, .. } | Record::NameDelete { id } => NameId(*id),
                _ => continue,
            };
            if previous.is_live_name(name)
                && let Target::Inode(child) = previous.name(name).target()
            {
                names
                    .entry(child)
                    .or_insert_with(|| self.names_for(child).collect())
                    .remove(&name);
            }
            if let Record::NamePut { child, .. } = record
                && *child < u32::MAX - 16
            {
                let child = InoId(*child);
                names
                    .entry(child)
                    .or_insert_with(|| self.names_for(child).collect())
                    .insert(name);
            }
        }
        self.name_changes.extend(names);
        for record in &changes.records {
            match record {
                Record::InodePut { id, kind, stat, .. } if *kind != crate::Kind::Dir => {
                    self.identity_changes
                        .insert((stat.dev, stat.ino), Some(InoId(*id)));
                }
                Record::InodePut {
                    id,
                    kind: crate::Kind::Dir,
                    stat,
                    ..
                } => {
                    let root = root_of(&view, InoId(*id));
                    let ids = self
                        .directory_changes
                        .entry((root.0, stat.dev, stat.ino))
                        .or_default();
                    if !ids.contains(&InoId(*id)) {
                        ids.push(InoId(*id));
                    }
                }
                Record::InodeDelete { id } if !previous.is_directory(InoId(*id)) => {
                    self.identity_changes
                        .insert(previous.identity(InoId(*id)), None);
                }
                Record::DocPut { id, hash, .. } => {
                    self.document_changes.insert(*hash, Some(DocId(*id)));
                }
                Record::DocDelete { id } => {
                    if let Some(hash) = previous.doc_hash(DocId(*id)) {
                        self.document_changes.insert(hash, None);
                    }
                }
                _ => {}
            }
        }
        Ok(view)
    }
}

fn root_of(view: &Catalog, mut dir: InoId) -> InoId {
    while let Some(name) = view.dir_name(dir) {
        dir = view.name(name).parent;
    }
    dir
}
