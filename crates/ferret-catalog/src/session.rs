//! Resident crawl writer: one lock and checked view, with sorted base-id
//! lookups and sparse lookup updates after publication. Reconciliation belongs
//! to ferret-crawl; this module knows only observations and catalog records.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::log::{ChangeSet, Error, Record, Writer};
use crate::{Batch, Catalog, Content, ContentState, DocId, Hash, InoId, Stat};

/// A resident writer for same-epoch recrawls. Opening warms and validates the
/// published view once. Failed durable publication poisons the underlying
/// writer; drop and reopen rather than retrying an uncertain tail.
pub struct WriterSession {
    writer: Writer,
    lookup_base: Catalog,
    identities: Vec<InoId>,
    documents: Vec<DocId>,
    directories: Vec<(InoId, InoId)>,
    directory_changes: BTreeMap<(u32, u64, u64), Vec<InoId>>,
    identity_changes: BTreeMap<(u64, u64), Option<InoId>>,
    document_changes: BTreeMap<Hash, Option<DocId>>,
    references: Vec<u32>,
    next_batch: AtomicU32,
}

impl WriterSession {
    /// Takes the writer lock. Initial indexing needs a checkpoint first.
    pub fn open(dir: &Path) -> Result<Self, Error> {
        std::fs::create_dir_all(dir).map_err(Error::Io)?;
        let writer = Writer::open(dir)?;
        let view = writer.view();
        let mut identities: Vec<_> = view
            .inode_ids()
            .filter(|&id| !view.is_directory(id))
            .collect();
        identities.sort_unstable_by_key(|&id| view.identity(id));
        let mut documents: Vec<_> = view.docs().map(|(id, _)| id).collect();
        documents.sort_unstable_by_key(|&id| view.doc_hash(id));
        let mut directories: Vec<_> = view.dir_ids().map(|id| (root_of(&view, id), id)).collect();
        directories.sort_unstable_by_key(|&(root, id)| (root, view.identity(id)));
        let mut references = vec![0; view.next_inode().0 as usize];
        for (id, _) in view.names() {
            if let crate::Target::Inode(child) = view.name(id).target() {
                references[child.0 as usize] += 1;
            }
        }
        Ok(Self {
            writer,
            lookup_base: view,
            identities,
            documents,
            identity_changes: BTreeMap::new(),
            document_changes: BTreeMap::new(),
            directories,
            directory_changes: BTreeMap::new(),
            references,
            next_batch: AtomicU32::new(0),
        })
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
            .binary_search_by_key(&Some(hash), |&id| self.lookup_base.doc_hash(id))
            .ok()?;
        Some(self.documents[at])
    }

    /// Exact indexed-name count, independent of filesystem `st_nlink`.
    pub fn name_references(&self, id: InoId) -> u32 {
        self.references.get(id.0 as usize).copied().unwrap_or(0)
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
            .commit_with_sniffer(previous.generation(), changes, sniffer)?;
        let view = self.writer.view();
        self.references.resize(view.next_inode().0 as usize, 0);
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
                    self.directory_changes
                        .entry((root.0, stat.dev, stat.ino))
                        .or_default()
                        .push(InoId(*id));
                }
                Record::InodeDelete { id } => {
                    if !previous.is_directory(InoId(*id)) {
                        self.identity_changes
                            .insert(previous.identity(InoId(*id)), None);
                    }
                    self.references[*id as usize] = 0;
                }
                Record::LifePut { id, names, .. } => self.references[*id as usize] = *names,
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
