//! Sparse, immutable replacements over a pinned checkpoint. Payload families
//! stay independent: namespace graph checks wait for a name/path consumer.
mod runs;
use crate::generation::Manifest;
use crate::log::{Family, Log, Record};
use crate::{Catalog, DecodeError, InoId, Kind, Name, NameId, OpenError, Section, Target};
use runs::{Row, Runs};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

pub(crate) const LIFE: u64 = 0;
pub(crate) const NAME: u64 = 1;
pub(crate) const DIR: u64 = 2;
pub(crate) const ROOT: u64 = 3;
pub(crate) const POLICY: u64 = 4;
pub(crate) const INODE: u64 = 5;
pub(crate) const LINK: u64 = 6;
pub(crate) const WORKTREE: u64 = 7;
pub(crate) const DOC: u64 = 8;
fn key(category: u64, id: u32) -> u64 {
    category << 32 | u64::from(id)
}
fn record_key(r: &Record) -> u64 {
    let (category, id) = match r {
        Record::LifePut { id, .. } | Record::InodeDelete { id } => (LIFE, *id),
        Record::NamePut { id, .. } | Record::NameDelete { id } => (NAME, *id),
        Record::DirPut { id, .. } => (DIR, *id),
        Record::RootPut { id, .. } | Record::RootDelete { id } => (ROOT, *id),
        Record::PolicyPut { .. } => (POLICY, 0),
        Record::InodePut { id, .. } => (INODE, *id),
        Record::LinkPut { id, .. } | Record::LinkDelete { id } => (LINK, *id),
        Record::WorkTreePut { id, .. } | Record::WorkTreeDelete { id } => (WORKTREE, *id),
        Record::DocPut { id, .. } | Record::DocDelete { id } => (DOC, *id),
    };
    key(category, id)
}
fn bad(label: &'static str) -> OpenError {
    OpenError::Decode(DecodeError::Corrupt(label))
}
#[derive(Clone, Default)]
pub(crate) struct Projection {
    rows: Runs<u64, Arc<Record>>,
    categories: u16,
}
impl Projection {
    pub(crate) fn get(&self, category: u64, id: u32) -> Option<&Record> {
        if self.categories & (1 << category) == 0 {
            return None;
        }
        self.rows.get(&key(category, id)).map(Arc::as_ref)
    }
    pub(crate) fn records(&self, category: u64) -> impl Iterator<Item = &Record> {
        self.rows
            .latest_range(key(category, 0), key(category + 1, 0))
            .into_iter()
            .filter(move |r| r.key >> 32 == category)
            .map(|r| r.value.as_ref())
    }
    pub(crate) fn range(
        &self,
        category: u64,
        start: u32,
        end: u32,
    ) -> impl Iterator<Item = &Record> {
        self.rows
            .range(key(category, start), key(category, end))
            .map(|r| r.value.as_ref())
    }
    pub(crate) fn run_count(&self) -> usize {
        self.rows.run_count()
    }
    fn apply(&mut self, sequence: u64, records: impl Iterator<Item = Arc<Record>>) {
        self.rows.insert(
            records
                .map(|r| {
                    self.categories |= 1 << (record_key(&r) >> 32);
                    Row {
                        key: record_key(&r),
                        sequence,
                        value: r,
                    }
                })
                .collect(),
        );
    }
}
/// A view owns its prefix, or shares a predecessor and one checked transaction.
pub(crate) struct Overlay {
    pub(crate) manifest: Manifest,
    pub(crate) base: Catalog,
    log: Option<Arc<Log>>,
    parent: Option<Arc<Overlay>>,
    delta: Vec<Arc<Record>>,
    families: [OnceLock<Projection>; 4],
    namespace: OnceLock<Arc<Namespace>>,
    checked: OnceLock<()>,
    fields_checked: OnceLock<()>,
    aux_checked: OnceLock<()>,
    kinds_checked: OnceLock<()>,
    docs_checked: OnceLock<()>,
}
pub(crate) struct Namespace {
    pub(crate) rows: Projection,
    keys: Runs<(u32, Vec<u8>), Option<u32>>,
    pub(crate) suppressed: Vec<u32>,
    pub(crate) names: Vec<u32>,
    pub(crate) heap: Vec<u8>,
    pub(crate) spans: Vec<(usize, u32)>,
    pub(crate) references: BTreeMap<u32, i64>,
}
impl Overlay {
    pub(crate) fn empty(base: Catalog, manifest: Manifest) -> Self {
        Self {
            base,
            manifest,
            log: None,
            parent: None,
            delta: Vec::new(),
            families: Default::default(),
            namespace: OnceLock::new(),
            checked: OnceLock::new(),
            fields_checked: OnceLock::new(),
            aux_checked: OnceLock::new(),
            kinds_checked: OnceLock::new(),
            docs_checked: OnceLock::new(),
        }
    }
    pub(crate) fn new(base: Catalog, manifest: Manifest, log: Log) -> Self {
        Self {
            base,
            manifest,
            log: Some(Arc::new(log)),
            parent: None,
            delta: Vec::new(),
            families: Default::default(),
            namespace: OnceLock::new(),
            checked: OnceLock::new(),
            fields_checked: OnceLock::new(),
            aux_checked: OnceLock::new(),
            kinds_checked: OnceLock::new(),
            docs_checked: OnceLock::new(),
        }
    }
    pub(crate) fn advance(
        &self,
        parent: Arc<Self>,
        manifest: Manifest,
        records: &[Record],
    ) -> Self {
        Self {
            base: self.base.clone(),
            manifest,
            log: None,
            parent: Some(parent),
            delta: records.iter().cloned().map(Arc::new).collect(),
            families: Default::default(),
            namespace: OnceLock::new(),
            checked: OnceLock::new(),
            fields_checked: OnceLock::new(),
            aux_checked: OnceLock::new(),
            kinds_checked: OnceLock::new(),
            docs_checked: OnceLock::new(),
        }
    }
    fn transactions(&self, family: Family) -> Vec<(u64, [u32; 3], Vec<Arc<Record>>)> {
        if let Some(log) = &self.log {
            let mut groups: BTreeMap<u64, Vec<Arc<Record>>> = BTreeMap::new();
            for (seq, r) in log.records(family) {
                groups.entry(seq).or_default().push(Arc::new(r.clone()));
            }
            log.frames()
                .map(|(seq, counters)| (seq, counters, groups.remove(&seq).unwrap_or_default()))
                .collect()
        } else {
            vec![(
                self.manifest.generation.sequence,
                self.manifest.counters,
                self.delta
                    .iter()
                    .filter(|r| r.family() == family)
                    .cloned()
                    .collect(),
            )]
        }
    }
    pub(crate) fn load(&self, family: Family) -> Result<(), OpenError> {
        if self.families[family as usize].get().is_some() {
            return Ok(());
        }
        let mut p = if let Some(parent) = &self.parent {
            parent.load(family)?;
            parent.projection(family).clone()
        } else {
            Projection::default()
        };
        if let Some(log) = &self.log {
            log.load(family)?;
        }
        let mut counters = self
            .parent
            .as_ref()
            .map_or(self.base.manifest().counters, |p| p.manifest.counters);
        let mut counts = self
            .parent
            .as_ref()
            .map_or(self.base.manifest().counts, |p| p.manifest.counts);
        for (seq, next, records) in self.transactions(family) {
            if family == Family::Namespace {
                let mut final_rows: BTreeMap<u64, &Record> = BTreeMap::new();
                for r in &records {
                    final_rows.insert(record_key(r), r);
                }
                let mut births = BTreeSet::new();
                for r in final_rows.values() {
                    match r {
                        Record::LifePut { id, kind, .. } => {
                            if matches!(p.get(LIFE, *id), Some(Record::InodeDelete { .. })) {
                                return Err(bad("retired inode reused"));
                            }
                            if *id >= counters[0] {
                                births.insert(*id);
                                counts[0] += 1;
                                counts[2] += u32::from(*kind == Kind::Dir);
                            } else if let Some(Record::LifePut { kind: old, .. }) = p.get(LIFE, *id)
                            {
                                if old != kind {
                                    return Err(bad("inode lifetime type changed"));
                                }
                            } else if (*id < self.base.base_dir_count()) != (*kind == Kind::Dir) {
                                return Err(bad("base directory type changed"));
                            }
                        }
                        Record::InodeDelete { id } => {
                            if *id >= counters[0]
                                || matches!(p.get(LIFE, *id), Some(Record::InodeDelete { .. }))
                            {
                                return Err(bad("delete absent inode"));
                            }
                            let dir = match p.get(LIFE, *id) {
                                Some(Record::LifePut { kind, .. }) => *kind == Kind::Dir,
                                _ => *id < self.base.base_dir_count(),
                            };
                            counts[0] = counts[0]
                                .checked_sub(1)
                                .ok_or_else(|| bad("inode counts"))?;
                            counts[2] = counts[2]
                                .checked_sub(u32::from(dir))
                                .ok_or_else(|| bad("directory counts"))?;
                        }
                        _ => {}
                    }
                }
                if births.len() as u64 != u64::from(next[0] - counters[0])
                    || births.iter().copied().ne(counters[0]..next[0])
                {
                    return Err(bad("inode allocation gaps"));
                }
            }
            if family == Family::Docs {
                self.base.load(&[Section::Docs])?;
                let mut final_rows = BTreeMap::new();
                for r in &records {
                    final_rows.insert(record_key(r), r.as_ref());
                }
                let mut births = BTreeSet::new();
                for r in final_rows.values() {
                    match r {
                        Record::DocPut { id, hash, .. } => {
                            let previous = match p.get(DOC, *id) {
                                Some(Record::DocPut { hash, .. }) => Some(*hash),
                                Some(Record::DocDelete { .. }) => {
                                    return Err(bad("retired document reused"));
                                }
                                _ => self.base.doc_hash(crate::DocId(*id)),
                            };
                            if *id >= counters[2] {
                                births.insert(*id);
                                counts[3] += 1;
                            } else if previous.is_none() {
                                return Err(bad("unassigned/retired document reused"));
                            }
                            if previous.is_some_and(|old| old != *hash) {
                                return Err(bad("document hash changed"));
                            }
                        }
                        Record::DocDelete { id } => {
                            let existed = match p.get(DOC, *id) {
                                Some(Record::DocPut { .. }) => true,
                                Some(Record::DocDelete { .. }) => false,
                                _ => self.base.doc_hash(crate::DocId(*id)).is_some(),
                            };
                            if !existed {
                                return Err(bad("delete absent document"));
                            }
                            counts[3] = counts[3]
                                .checked_sub(1)
                                .ok_or_else(|| bad("document counts"))?;
                        }
                        _ => {}
                    }
                }
                if births.len() as u64 != u64::from(next[2] - counters[2])
                    || births.iter().copied().ne(counters[2]..next[2])
                {
                    return Err(bad("document allocation gaps"));
                }
            }
            p.apply(seq, records.into_iter());
            counters = next;
        }
        if family == Family::Namespace
            && (counts[0] != self.manifest.counts[0] || counts[2] != self.manifest.counts[2])
        {
            return Err(bad("live inode/directory counts"));
        }
        if family == Family::Docs && counts[3] != self.manifest.counts[3] {
            return Err(bad("live document count"));
        }
        let _ = self.families[family as usize].set(p);
        Ok(())
    }
    pub(crate) fn loaded(&self, section: Section) -> bool {
        if self.families[Family::Namespace as usize].get().is_none() {
            return false;
        }
        match section {
            Section::Names
            | Section::NameHeap
            | Section::DirNames
            | Section::Roots
            | Section::Entries
            | Section::Traversed
            | Section::RetainedAt => self.namespace.get().is_some(),
            Section::Links | Section::Specials | Section::WorkTrees => {
                self.families[Family::Aux as usize].get().is_some()
            }
            Section::Docs => {
                self.families[Family::Docs as usize].get().is_some()
                    && (self.families[Family::Inodes as usize].get().is_none()
                        || self.docs_checked.get().is_some())
            }
            Section::DocRefs => self.docs_checked.get().is_some(),
            Section::Strings | Section::Policy => true,
            _ => self.fields_checked.get().is_some(),
        }
    }
    pub(crate) fn check_aux(&self) -> Result<(), OpenError> {
        self.base.load(&[Section::Links])?;
        if self.aux_checked.get().is_none() {
            // A checked predecessor proves the inherited rows; validate only
            // current changes. Disk replay has no predecessor proof.
            let rows: Vec<_> = if self
                .parent
                .as_ref()
                .is_some_and(|p| p.checked.get().is_some())
            {
                self.delta.iter().map(Arc::as_ref).collect()
            } else {
                self.projection(Family::Namespace)
                    .records(LIFE)
                    .chain(self.projection(Family::Aux).records(LINK))
                    .chain(self.projection(Family::Aux).records(WORKTREE))
                    .collect()
            };
            for r in rows {
                match r {
                    Record::LifePut { id, kind, .. }
                        if *id < self.base.base_inode_count()
                            && self.base.kind(InoId(*id)) != *kind =>
                    {
                        return Err(bad("base inode lifetime type changed"));
                    }
                    Record::LinkPut { id, .. }
                        if self.live_inode(*id)
                            && self.kind(*id).unwrap_or_else(|| self.base.kind(InoId(*id)))
                                != Kind::Symlink =>
                    {
                        return Err(bad("link target on non-symlink"));
                    }
                    Record::WorkTreePut { id, .. }
                        if self.live_inode(*id)
                            && !self
                                .kind(*id)
                                .map_or(*id < self.base.base_dir_count(), |kind| {
                                    kind == Kind::Dir
                                }) =>
                    {
                        return Err(bad("work tree on non-directory inode"));
                    }
                    _ => {}
                }
            }
            let _ = self.aux_checked.set(());
        }
        if self.kinds_checked.get().is_none()
            && let Some(fields) = self.families[Family::Inodes as usize].get()
        {
            let rows: Vec<_> = if self
                .parent
                .as_ref()
                .is_some_and(|p| p.checked.get().is_some())
            {
                self.delta.iter().map(Arc::as_ref).collect()
            } else {
                fields.records(INODE).collect()
            };
            for r in rows {
                if let Record::InodePut { id, kind, .. } = r
                    && self.live_inode(*id)
                {
                    let actual = self.kind(*id).unwrap_or_else(|| self.base.kind(InoId(*id)));
                    if *kind != actual {
                        return Err(bad("inode/life kind mismatch"));
                    }
                }
            }
            let _ = self.kinds_checked.set(());
        }
        Ok(())
    }
    pub(crate) fn projection(&self, family: Family) -> &Projection {
        let Some(rows) = self.families[family as usize].get() else {
            panic!("overlay family {family:?} not loaded")
        };
        rows
    }
    pub(crate) fn namespace(&self) -> &Namespace {
        {
            let Some(ns) = self.namespace.get() else {
                panic!("namespace not loaded")
            };
            ns
        }
    }
    pub(crate) fn life(&self, id: u32) -> Option<&Record> {
        self.projection(Family::Namespace).get(LIFE, id)
    }
    pub(crate) fn live_inode(&self, id: u32) -> bool {
        id < self.manifest.counters[0]
            && match self.life(id) {
                Some(Record::InodeDelete { .. }) => false,
                Some(Record::LifePut { .. }) => true,
                _ => id < self.base.base_inode_count(),
            }
    }
    pub(crate) fn kind(&self, id: u32) -> Option<Kind> {
        match self.life(id) {
            Some(Record::LifePut { kind, .. }) => Some(*kind),
            Some(Record::InodeDelete { .. }) => panic!("dead inode"),
            _ => None,
        }
    }
    pub(crate) fn inode(&self, id: u32) -> Option<&Record> {
        self.projection(Family::Inodes).get(INODE, id)
    }
    pub(crate) fn detach(&mut self) {
        self.parent = None;
        self.delta.clear();
    }
    pub(crate) fn check_fields(&self) -> Result<(), OpenError> {
        if self.fields_checked.get().is_some() {
            return Ok(());
        }
        let rows = self.projection(Family::Inodes);
        for r in self.projection(Family::Namespace).records(LIFE) {
            if let Record::LifePut { id, .. } = r
                && *id >= self.base.base_inode_count()
                && rows.get(INODE, *id).is_none()
            {
                return Err(bad("new inode has no fields"));
            }
        }
        if self.families[Family::Aux as usize].get().is_some() {
            self.check_aux()?;
        }
        let _ = self.fields_checked.set(());
        Ok(())
    }
    pub(crate) fn check_documents(&self, view: &Catalog) -> Result<(), OpenError> {
        if self.docs_checked.get().is_some()
            || self.families[Family::Inodes as usize].get().is_none()
            || self.families[Family::Docs as usize].get().is_none()
        {
            return Ok(());
        }
        if let Some(parent) = &self.parent
            && parent.checked.get().is_some()
            && self.families[Family::Aux as usize].get().is_some()
        {
            self.validate_delta(view, parent)?;
            let _ = self.docs_checked.set(());
            return Ok(());
        }
        self.base.load(&[Section::DocRefs])?;
        let mut doc_delta = BTreeMap::<u32, i64>::new();
        let mut changed = BTreeSet::new();
        for r in self.projection(Family::Inodes).records(INODE) {
            if let Record::InodePut { id, .. } = r {
                changed.insert(*id);
            }
        }
        for r in self.projection(Family::Namespace).records(LIFE) {
            if let Record::InodeDelete { id } = r {
                changed.insert(*id);
            }
        }
        for id in changed {
            if id < self.base.base_inode_count()
                && let Some(doc) = self.base.doc(InoId(id))
            {
                *doc_delta.entry(doc.0).or_default() -= 1;
            }
            if self.live_inode(id)
                && let Some(doc) = view.doc(InoId(id))
            {
                if view.doc_hash(doc).is_none() {
                    return Err(bad("inode names absent document"));
                }
                *doc_delta.entry(doc.0).or_default() += 1;
            }
        }
        let mut affected: BTreeSet<_> = doc_delta.keys().copied().collect();
        let mut live_docs = i64::from(self.base.doc_count());
        for r in self.projection(Family::Docs).records(DOC) {
            match r {
                Record::DocPut { id, hash, .. } => {
                    if let Some(old) = self.base.doc_hash(crate::DocId(*id)) {
                        if old != *hash {
                            return Err(bad("document hash changed"));
                        }
                    } else {
                        if *id < self.base.next_doc().0 {
                            return Err(bad("retired document reused"));
                        }
                        live_docs += 1;
                    }
                    affected.insert(*id);
                }
                Record::DocDelete { id } => {
                    if self.base.doc_hash(crate::DocId(*id)).is_some() {
                        live_docs -= 1;
                    }
                    affected.insert(*id);
                }
                _ => {}
            }
        }
        if live_docs != i64::from(self.manifest.counts[3]) {
            return Err(bad("live document count"));
        }
        for id in affected {
            let actual = i64::from(self.base.doc_references(crate::DocId(id)).unwrap_or(0))
                + doc_delta.get(&id).copied().unwrap_or(0);
            let declared = i64::from(view.doc_references(crate::DocId(id)).unwrap_or(0));
            if actual != declared {
                return Err(bad("document reference count"));
            }
        }
        let _ = self.docs_checked.set(());
        Ok(())
    }
    pub(crate) fn validate_all(&self, view: &Catalog) -> Result<(), OpenError> {
        if self.checked.get().is_some() {
            return Ok(());
        }
        self.check_fields()?;
        if let Some(parent) = &self.parent
            && parent.checked.get().is_some()
        {
            self.validate_delta(view, parent)?;
            let _ = self.docs_checked.set(());
            let _ = self.checked.set(());
            return Ok(());
        }
        let ns = self.namespace();
        let needs_refs = ns.references.values().any(|&d| d != 0)
            || self
                .projection(Family::Namespace)
                .records(LIFE)
                .next()
                .is_some();
        let refs = if needs_refs {
            self.base.name_references()
        } else {
            &[]
        };
        for r in self.projection(Family::Namespace).records(LIFE) {
            let (id, expected) = match r {
                Record::LifePut { id, names, .. } => (*id, i64::from(*names)),
                Record::InodeDelete { id } => (*id, 0),
                _ => continue,
            };
            let actual = i64::from(refs.get(id as usize).copied().unwrap_or(0))
                + ns.references.get(&id).copied().unwrap_or(0);
            if actual != expected {
                return Err(bad("inode name reference count"));
            }
        }
        for (&id, &delta) in &ns.references {
            let actual = i64::from(refs.get(id as usize).copied().unwrap_or(0)) + delta;
            if actual < 0
                || (!self.live_inode(id) && actual != 0)
                || (self.live_inode(id) && !view.is_directory(InoId(id)) && actual == 0)
            {
                return Err(bad("dangling name/inode reference"));
            }
        }
        self.check_documents(view)?;
        for r in self.projection(Family::Aux).records(LINK) {
            if let Record::LinkPut { id, .. } = r
                && self.live_inode(*id)
                && view.kind(InoId(*id)) != Kind::Symlink
            {
                return Err(bad("link target on non-symlink"));
            }
        }
        for r in self.projection(Family::Aux).records(WORKTREE) {
            if let Record::WorkTreePut { id, .. } = r
                && self.live_inode(*id)
                && !view.is_directory(InoId(*id))
            {
                return Err(bad("work tree on absent/non-directory inode"));
            }
        }
        let _ = self.checked.set(());
        Ok(())
    }
    fn validate_delta(&self, view: &Catalog, parent: &Overlay) -> Result<(), OpenError> {
        let mut docs = BTreeMap::<u32, i64>::new();
        let mut changed = BTreeSet::new();
        for r in &self.delta {
            match r.as_ref() {
                Record::InodePut { id, .. } | Record::InodeDelete { id } => {
                    changed.insert(*id);
                }
                Record::DocPut { id, .. } | Record::DocDelete { id } => {
                    docs.entry(*id).or_default();
                }
                Record::LinkPut { id, .. }
                    if self.live_inode(*id) && view.kind(InoId(*id)) != Kind::Symlink =>
                {
                    return Err(bad("link target on non-symlink"));
                }
                Record::WorkTreePut { id, .. }
                    if self.live_inode(*id) && !view.is_directory(InoId(*id)) =>
                {
                    return Err(bad("work tree on absent/non-directory inode"));
                }
                _ => {}
            }
        }
        for id in changed {
            let old = parent
                .inode(id)
                .map(|r| match r {
                    Record::InodePut { doc, .. } => *doc,
                    _ => None,
                })
                .unwrap_or_else(|| {
                    (id < self.base.base_inode_count())
                        .then(|| self.base.doc(InoId(id)).map(|d| d.0))
                        .flatten()
                });
            if parent.live_inode(id)
                && let Some(doc) = old
            {
                *docs.entry(doc).or_default() -= 1;
            }
            if self.live_inode(id) {
                if let Some(Record::InodePut { kind, .. }) = self.inode(id)
                    && *kind != view.kind(InoId(id))
                {
                    return Err(bad("inode/life kind mismatch"));
                }
                if let Some(doc) = view.doc(InoId(id)) {
                    *docs.entry(doc.0).or_default() += 1;
                }
            }
        }
        let mut count = i64::from(parent.manifest.counts[3]);
        for (id, delta) in docs {
            let prior = match parent.projection(Family::Docs).get(DOC, id) {
                Some(Record::DocPut { references, .. }) => Some(*references),
                Some(Record::DocDelete { .. }) => None,
                _ => self.base.doc_references(crate::DocId(id)),
            };
            let now = view.doc_references(crate::DocId(id));
            if i64::from(prior.unwrap_or(0)) + delta != i64::from(now.unwrap_or(0)) {
                return Err(bad("document reference count"));
            }
            if prior.is_none() && now.is_some() {
                if id < parent.manifest.counters[2] {
                    return Err(bad("retired document reused"));
                }
                count += 1;
            }
            if prior.is_some() && now.is_none() {
                count -= 1;
            }
            let old_hash = match parent.projection(Family::Docs).get(DOC, id) {
                Some(Record::DocPut { hash, .. }) => Some(*hash),
                Some(Record::DocDelete { .. }) => None,
                _ => self.base.doc_hash(crate::DocId(id)),
            };
            if let (Some(a), Some(b)) = (old_hash, view.doc_hash(crate::DocId(id)))
                && a != b
            {
                return Err(bad("document hash changed"));
            }
        }
        if count != i64::from(self.manifest.counts[3]) {
            return Err(bad("live document count"));
        }
        Ok(())
    }
    pub(crate) fn bytes_read(&self) -> u64 {
        self.log.as_ref().map_or(0, |l| l.bytes_read())
            + self.parent.as_ref().map_or(0, |p| p.bytes_read())
    }
    pub(crate) fn load_namespace(&self) -> Result<(), OpenError> {
        if self.namespace.get().is_some() {
            return Ok(());
        }
        if let Some(parent) = &self.parent
            && !self.delta.iter().any(|r| r.family() == Family::Namespace)
        {
            parent.load_namespace()?;
            let Some(ns) = parent.namespace.get() else {
                unreachable!("loaded predecessor namespace")
            };
            let _ = self.namespace.set(ns.clone());
            return Ok(());
        }
        self.load(Family::Namespace)?;
        self.base
            .load(&[Section::Names, Section::DirNames, Section::Roots])?;
        let mut rows = if let Some(parent) = &self.parent {
            parent.load_namespace()?;
            parent.namespace().rows.clone()
        } else {
            Projection::default()
        };
        let mut keys = if let Some(parent) = &self.parent {
            parent.namespace().keys.clone()
        } else {
            Runs::default()
        };
        let mut references = self
            .parent
            .as_ref()
            .map_or_else(BTreeMap::new, |p| p.namespace().references.clone());
        let mut counter = self
            .parent
            .as_ref()
            .map_or(self.base.next_name().0, |p| p.manifest.counters[1]);
        let mut count = self
            .parent
            .as_ref()
            .map_or(self.base.name_count(), |p| p.manifest.counts[1]);
        for (seq, next, records) in self.transactions(Family::Namespace) {
            let mut final_rows: BTreeMap<u64, Arc<Record>> = BTreeMap::new();
            for r in records {
                final_rows.insert(record_key(&r), r);
            }
            let mut key_rows = Vec::new();
            let mut old_dirs = BTreeSet::new();
            let mut ref_changes = BTreeMap::<u32, i64>::new();
            let mut births = BTreeSet::new();
            for r in final_rows.values() {
                let id = match r.as_ref() {
                    Record::NamePut { id, .. } | Record::NameDelete { id } => *id,
                    _ => continue,
                };
                if id >= counter {
                    if !matches!(r.as_ref(), Record::NamePut { .. }) {
                        return Err(bad("delete unassigned name"));
                    }
                    births.insert(id);
                    count += 1;
                } else {
                    let old =
                        name(&self.base, &rows, id).ok_or_else(|| bad("retired name reused"))?;
                    if let Target::Inode(child) = old.target() {
                        *references.entry(child.0).or_default() -= 1;
                        *ref_changes.entry(child.0).or_default() -= 1;
                        if kind(&self.base, &rows, child.0) == Some(Kind::Dir) {
                            old_dirs.insert(child.0);
                        }
                    }
                    key_rows.push(Row {
                        key: (old.parent.0, old.bytes.to_vec()),
                        sequence: seq,
                        value: None,
                    });
                    if matches!(r.as_ref(), Record::NameDelete { .. }) {
                        count = count.checked_sub(1).ok_or_else(|| bad("name counts"))?;
                    }
                }
            }
            if births.len() as u64 != u64::from(next[1] - counter)
                || births.iter().copied().ne(counter..next[1])
            {
                return Err(bad("name allocation gaps"));
            }
            for r in final_rows.values() {
                if let Record::NamePut { child, .. } = r.as_ref()
                    && Kind::from_ignored_child(*child).is_none()
                {
                    *references.entry(*child).or_default() += 1;
                    *ref_changes.entry(*child).or_default() += 1;
                }
            }
            let changed: Vec<_> = final_rows.into_values().collect();
            rows.apply(seq, changed.iter().cloned());
            let mut removed = keys.clone();
            removed.insert(key_rows.clone());
            let mut additions = Vec::new();
            for r in &changed {
                if let Record::NamePut {
                    id,
                    parent,
                    child,
                    name: bytes,
                } = r.as_ref()
                {
                    if !live(&self.base, &rows, *parent)
                        || kind(&self.base, &rows, *parent) != Some(Kind::Dir)
                        || (Kind::from_ignored_child(*child).is_none()
                            && !live(&self.base, &rows, *child))
                    {
                        return Err(bad("name references dead/non-directory inode"));
                    }
                    // Changed names are checked against the final transaction,
                    // including removals of overwritten destinations.
                    if let Some(other) = lookup(&self.base, &rows, &removed, *parent, bytes)
                        && other != *id
                    {
                        return Err(bad("duplicate child key"));
                    }
                    if kind(&self.base, &rows, *child) == Some(Kind::Dir)
                        && (own(&self.base, &rows, *child) != Some(*id)
                            || root(&self.base, &rows, *child))
                    {
                        return Err(bad("directory has multiple incoming edges"));
                    }
                    additions.push(Row {
                        key: (*parent, bytes.clone()),
                        sequence: seq,
                        value: Some(*id),
                    });
                }
            }
            // Detect equal new keys before run coalescing could hide either.
            additions.sort_by(|a, b| a.key.cmp(&b.key));
            if additions.windows(2).any(|w| w[0].key == w[1].key) {
                return Err(bad("duplicate changed child key"));
            }
            // Coalesce one transaction into one run: removals and additions
            // have the same sequence, so separate runs cannot order them.
            key_rows.extend(additions);
            keys.insert(key_rows);
            validate_graph(&self.base, &rows, &changed, old_dirs)?;
            let mut affected: BTreeSet<_> = ref_changes
                .into_iter()
                .filter(|(_, delta)| *delta != 0)
                .map(|(id, _)| id)
                .collect();
            for r in &changed {
                if let Record::LifePut { id, .. } | Record::InodeDelete { id } = r.as_ref() {
                    affected.insert(*id);
                }
            }
            if !affected.is_empty() {
                let base_refs = if affected.iter().any(|&id| id < self.base.base_inode_count()) {
                    self.base.name_references()
                } else {
                    &[]
                };
                for id in affected {
                    let actual = i64::from(base_refs.get(id as usize).copied().unwrap_or(0))
                        + references.get(&id).copied().unwrap_or(0);
                    let declared = match rows.get(LIFE, id) {
                        Some(Record::LifePut { names, .. }) => Some(i64::from(*names)),
                        Some(Record::InodeDelete { .. }) => Some(0),
                        _ => None,
                    };
                    if actual < 0
                        || declared.is_some_and(|n| n != actual)
                        || (!live(&self.base, &rows, id) && actual != 0)
                        || (live(&self.base, &rows, id)
                            && kind(&self.base, &rows, id) != Some(Kind::Dir)
                            && actual == 0)
                    {
                        return Err(bad("namespace inode reference count"));
                    }
                    if !live(&self.base, &rows, id) {
                        let base_children = if id < self.base.base_inode_count() {
                            self.base
                                .children(InoId(id))
                                .filter(|n| rows.get(NAME, n.0).is_none())
                                .count()
                        } else {
                            0
                        };
                        let delta_children = keys
                            .latest_range((id, Vec::new()), (id + 1, Vec::new()))
                            .into_iter()
                            .filter(|r| r.value.is_some())
                            .count();
                        if base_children + delta_children != 0 {
                            return Err(bad("deleted directory retains children"));
                        }
                    }
                }
            }
            references.retain(|_, delta| *delta != 0);
            counter = next[1];
        }
        if count != self.manifest.counts[1] {
            return Err(bad("live name count"));
        }
        let suppressed: Vec<_> = rows
            .records(NAME)
            .filter_map(|r| match r {
                Record::NamePut { id, .. } | Record::NameDelete { id }
                    if *id < self.base.base_name_count() =>
                {
                    Some(*id)
                }
                _ => None,
            })
            .collect();
        let mut names = Vec::new();
        let mut heap = Vec::new();
        let mut spans = Vec::new();
        for r in rows.records(NAME) {
            if let Record::NamePut { id, name, .. } = r {
                names.push(*id);
                spans.push((heap.len(), *id));
                heap.extend_from_slice(name);
                heap.push(0);
            }
        }
        let _ = self.namespace.set(Arc::new(Namespace {
            rows,
            keys,
            suppressed,
            names,
            heap,
            spans,
            references,
        }));
        Ok(())
    }
}
impl Namespace {
    pub(crate) fn name<'a>(&'a self, base: &'a Catalog, id: u32) -> Option<Name<'a>> {
        name(base, &self.rows, id)
    }
    pub(crate) fn lookup(&self, base: &Catalog, parent: u32, bytes: &[u8]) -> Option<u32> {
        lookup(base, &self.rows, &self.keys, parent, bytes)
    }
    pub(crate) fn children(&self, parent: u32) -> impl Iterator<Item = u32> {
        self.keys
            .latest_range((parent, Vec::new()), (parent + 1, Vec::new()))
            .into_iter()
            .filter_map(|r| r.value)
    }
    pub(crate) fn suppresses(&self, id: u32) -> bool {
        self.suppressed.binary_search(&id).is_ok()
    }
}
fn name<'a>(base: &'a Catalog, rows: &'a Projection, id: u32) -> Option<Name<'a>> {
    match rows.get(NAME, id) {
        Some(Record::NamePut {
            parent,
            child,
            name,
            ..
        }) => Some(Name {
            parent: InoId(*parent),
            child: InoId(*child),
            bytes: name,
        }),
        Some(Record::NameDelete { .. }) => None,
        _ => (id < base.base_name_count()).then(|| base.name(NameId(id))),
    }
}
fn lookup(
    base: &Catalog,
    rows: &Projection,
    keys: &Runs<(u32, Vec<u8>), Option<u32>>,
    parent: u32,
    bytes: &[u8],
) -> Option<u32> {
    if let Some(value) = keys.get(&(parent, bytes.to_vec())) {
        return *value;
    }
    base.lookup(InoId(parent), bytes)
        .filter(|id| rows.get(NAME, id.0).is_none())
        .map(|id| id.0)
}
fn live(base: &Catalog, rows: &Projection, id: u32) -> bool {
    match rows.get(LIFE, id) {
        Some(Record::LifePut { .. }) => true,
        Some(Record::InodeDelete { .. }) => false,
        _ => id < base.base_inode_count(),
    }
}
fn kind(base: &Catalog, rows: &Projection, id: u32) -> Option<Kind> {
    match rows.get(LIFE, id) {
        Some(Record::LifePut { kind, .. }) => Some(*kind),
        Some(Record::InodeDelete { .. }) => None,
        _ => (id < base.base_dir_count()).then_some(Kind::Dir),
    }
}
fn own(base: &Catalog, rows: &Projection, id: u32) -> Option<u32> {
    match rows.get(DIR, id) {
        Some(Record::DirPut { name, .. }) => *name,
        _ => (id < base.base_dir_count())
            .then(|| base.dir_name(InoId(id)))
            .flatten()
            .map(|n| n.0),
    }
}
fn root(base: &Catalog, rows: &Projection, id: u32) -> bool {
    match rows.get(ROOT, id) {
        Some(Record::RootPut { .. }) => true,
        Some(Record::RootDelete { .. }) => false,
        _ => base.roots().any(|(i, _)| i.0 == id),
    }
}
fn validate_graph(
    base: &Catalog,
    rows: &Projection,
    changed: &[Arc<Record>],
    old_dirs: BTreeSet<u32>,
) -> Result<(), OpenError> {
    let mut paths = BTreeSet::new();
    let roots = base
        .roots()
        .filter(|(id, _)| rows.get(ROOT, id.0).is_none())
        .chain(rows.records(ROOT).filter_map(|r| match r {
            Record::RootPut { id, path } => Some((InoId(*id), path.as_slice())),
            _ => None,
        }));
    for (id, path) in roots {
        if !live(base, rows, id.0)
            || kind(base, rows, id.0) != Some(Kind::Dir)
            || own(base, rows, id.0).is_some()
            || !paths.insert(path)
        {
            return Err(bad("root liveness/incoming edge/duplicate path"));
        }
    }
    let mut dirs: BTreeSet<_> = old_dirs
        .into_iter()
        .filter(|&id| live(base, rows, id))
        .collect();
    for r in changed.iter().map(Arc::as_ref) {
        if let Record::NamePut { parent, child, .. } = r {
            dirs.insert(*parent);
            if kind(base, rows, *child) == Some(Kind::Dir) {
                dirs.insert(*child);
            }
        }
    }
    {
        for r in changed.iter().map(Arc::as_ref) {
            match r {
                Record::LifePut {
                    id,
                    kind: Kind::Dir,
                    ..
                }
                | Record::DirPut { id, .. }
                | Record::RootPut { id, .. }
                | Record::RootDelete { id }
                    if live(base, rows, *id) =>
                {
                    dirs.insert(*id);
                }
                _ => {}
            }
        }
    }
    for mut at in dirs {
        let mut seen = BTreeSet::new();
        loop {
            if !live(base, rows, at) || kind(base, rows, at) != Some(Kind::Dir) || !seen.insert(at)
            {
                return Err(bad("directory cycle/dead parent"));
            }
            if let Some(edge) = own(base, rows, at) {
                let n =
                    name(base, rows, edge).ok_or_else(|| bad("missing directory incoming edge"))?;
                if n.target() != Target::Inode(InoId(at)) || root(base, rows, at) {
                    return Err(bad("directory incoming edge/root"));
                }
                at = n.parent.0;
            } else {
                if !root(base, rows, at) {
                    return Err(bad("directory has no root"));
                }
                break;
            }
        }
    }
    Ok(())
}
