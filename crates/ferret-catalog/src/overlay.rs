//! Sparse, immutable replacements over a pinned checkpoint. Payload families
//! stay independent: namespace graph checks wait for a name/path consumer.
mod ids;
mod namespace;
mod runs;
use crate::generation::Manifest;
use crate::log::{Family, Log, Record};
use crate::{Catalog, DecodeError, InoId, Kind, OpenError, Section};
use ids::Ids;
use runs::{Row, Runs};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::{Arc, OnceLock};

/// Child keys `(parent, basename)` mapped to a name id, `None` for a removal.
type ChildKeys = Runs<(u32, Vec<u8>), Option<u32>>;

pub(crate) const LIFE: u64 = 0;
pub(crate) const NAME: u64 = 1;
pub(crate) const DIR: u64 = 2;
pub(crate) const ROOT: u64 = 3;
pub(crate) const POLICY: u64 = 4;
pub(crate) const INODE: u64 = 5;
pub(crate) const LINK: u64 = 6;
pub(crate) const WORKTREE: u64 = 7;
pub(crate) const DOC: u64 = 8;
const CATEGORIES: usize = 9;
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
/// Fails unless a transaction's `births` are exactly the ids it allocated.
fn check_births(
    births: &BTreeSet<u32>,
    allocated: Range<u32>,
    label: &'static str,
) -> Result<(), OpenError> {
    if births.len() as u64 != u64::from(allocated.end - allocated.start)
        || births.iter().copied().ne(allocated)
    {
        return Err(bad(label));
    }
    Ok(())
}
fn bad(label: &'static str) -> OpenError {
    OpenError::Decode(DecodeError::Corrupt(label))
}
#[derive(Clone, Default)]
pub(crate) struct Projection {
    rows: Runs<u64, Arc<Record>>,
    /// The ids each category holds a row for. Most lookups miss, and a miss
    /// answers from one bit rather than probing every run: on a 57k-name
    /// tree, a 13k-record overlay otherwise made a daemon grep twice as
    /// slow (2026-10-30).
    present: [Ids; CATEGORIES],
}
impl Projection {
    pub(crate) fn get(&self, category: u64, id: u32) -> Option<&Record> {
        if !self.present[category as usize].contains(id) {
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
    /// As `records`, `None` once `stop` answers true; it is asked before
    /// each row read, superseded ones included.
    pub(crate) fn records_until(
        &self,
        category: u64,
        stop: &mut impl FnMut() -> bool,
    ) -> Option<Vec<&Record>> {
        let rows = self
            .rows
            .latest_range_until(key(category, 0), key(category + 1, 0), stop)?;
        Some(
            rows.into_iter()
                .filter(|r| r.key >> 32 == category)
                .map(|r| r.value.as_ref())
                .collect(),
        )
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
                    let key = record_key(&r);
                    self.present[(key >> 32) as usize].insert(key as u32);
                    Row {
                        key,
                        sequence,
                        value: r,
                    }
                })
                .collect(),
        );
    }
}
/// A view owns its prefix, or shares a predecessor and one checked transaction.
#[derive(Clone)]
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
    rows: Projection,
    keys: ChildKeys,
    /// Directories with any child key row: every other directory's children
    /// are its checkpoint range, unchanged and still in basename order.
    touched: BTreeSet<u32>,
    pub(crate) suppressed: Vec<u32>,
    pub(crate) names: Vec<u32>,
    pub(crate) heap: Vec<u8>,
    pub(crate) spans: Vec<(usize, u32)>,
    references: BTreeMap<u32, i64>,
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
            log: Some(Arc::new(log)),
            ..Self::empty(base, manifest)
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
            match family {
                Family::Namespace => {
                    self.check_inodes(&p, &records, counters, next, &mut counts)?
                }
                Family::Docs => self.check_docs(&p, &records, counters, next, &mut counts)?,
                _ => {}
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
    /// Checks a transaction's inode lifetimes against the projection
    /// before it, and counts its births and deaths into `counts`.
    fn check_inodes(
        &self,
        p: &Projection,
        records: &[Arc<Record>],
        counters: [u32; 3],
        next: [u32; 3],
        counts: &mut [u32; 4],
    ) -> Result<(), OpenError> {
        // Only LifePut/InodeDelete affect liveness and counts below;
        // most touched Namespace records are NamePut/DirPut and
        // never match. Key the last-wins map on id alone instead of
        // building a generic map over every touched record.
        let mut final_rows: BTreeMap<u32, &Record> = BTreeMap::new();
        for r in records {
            if let Record::LifePut { id, .. } | Record::InodeDelete { id } = r.as_ref() {
                final_rows.insert(*id, r.as_ref());
            }
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
                    } else if let Some(Record::LifePut { kind: old, .. }) = p.get(LIFE, *id) {
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
        check_births(&births, counters[0]..next[0], "inode allocation gaps")
    }
    /// Checks a transaction's documents against the projection before it,
    /// and counts its births and deaths into `counts`.
    fn check_docs(
        &self,
        p: &Projection,
        records: &[Arc<Record>],
        counters: [u32; 3],
        next: [u32; 3],
        counts: &mut [u32; 4],
    ) -> Result<(), OpenError> {
        self.base.load(&[Section::Docs])?;
        let mut final_rows = BTreeMap::new();
        for r in records {
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
        check_births(&births, counters[2]..next[2], "document allocation gaps")
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
            | Section::RetainedAt
            | Section::Revealed => self.namespace.get().is_some(),
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
            let rows = self.unproven(|| {
                self.projection(Family::Namespace)
                    .records(LIFE)
                    .chain(self.projection(Family::Aux).records(LINK))
                    .chain(self.projection(Family::Aux).records(WORKTREE))
            });
            rows.into_iter()
                .try_for_each(|r| self.check_aux_record(r))?;
            let _ = self.aux_checked.set(());
        }
        if self.kinds_checked.get().is_none()
            && let Some(fields) = self.families[Family::Inodes as usize].get()
        {
            let rows = self.unproven(|| fields.records(INODE));
            rows.into_iter()
                .try_for_each(|r| self.check_inode_kind(r))?;
            let _ = self.kinds_checked.set(());
        }
        Ok(())
    }
    /// The rows still to validate. A checked predecessor proves the inherited
    /// rows, so only current changes; disk replay has no predecessor proof,
    /// so every row `all` yields.
    fn unproven<'a, I: Iterator<Item = &'a Record>>(
        &'a self,
        all: impl FnOnce() -> I,
    ) -> Vec<&'a Record> {
        if self
            .parent
            .as_ref()
            .is_some_and(|p| p.checked.get().is_some())
        {
            self.delta.iter().map(Arc::as_ref).collect()
        } else {
            all().collect()
        }
    }
    fn check_aux_record(&self, r: &Record) -> Result<(), OpenError> {
        match r {
            Record::LifePut { id, kind, .. }
                if *id < self.base.base_inode_count() && self.base.kind(InoId(*id)) != *kind =>
            {
                Err(bad("base inode lifetime type changed"))
            }
            Record::LinkPut { id, .. }
                if self.live_inode(*id)
                    && self.kind(*id).unwrap_or_else(|| self.base.kind(InoId(*id)))
                        != Kind::Symlink =>
            {
                Err(bad("link target on non-symlink"))
            }
            Record::WorkTreePut { id, .. }
                if self.live_inode(*id)
                    && !self
                        .kind(*id)
                        .map_or(*id < self.base.base_dir_count(), |kind| kind == Kind::Dir) =>
            {
                Err(bad("work tree on non-directory inode"))
            }
            _ => Ok(()),
        }
    }
    fn check_inode_kind(&self, r: &Record) -> Result<(), OpenError> {
        if let Record::InodePut { id, kind, .. } = r
            && self.live_inode(*id)
        {
            let actual = self.kind(*id).unwrap_or_else(|| self.base.kind(InoId(*id)));
            if *kind != actual {
                return Err(bad("inode/life kind mismatch"));
            }
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
        let doc_delta = self.base_reference_deltas(view)?;
        let mut affected: BTreeSet<_> = doc_delta.keys().copied().collect();
        self.check_doc_records(&mut affected)?;
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
    /// The reference each changed inode moved from its base document to its
    /// document in `view`, per document; rejects an inode naming an absent
    /// document.
    fn base_reference_deltas(&self, view: &Catalog) -> Result<BTreeMap<u32, i64>, OpenError> {
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
        Ok(doc_delta)
    }
    /// Checks the document records against the base: no hash changes, no
    /// retired id reused, and the live document count. Adds each document a
    /// record names to `affected`.
    fn check_doc_records(&self, affected: &mut BTreeSet<u32>) -> Result<(), OpenError> {
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
        let (mut docs, changed) = self.delta_touches(view)?;
        self.count_references(view, parent, changed, &mut docs)?;
        self.check_documents_moved(view, parent, docs)
    }
    /// The documents this delta's records name, each with a zero reference
    /// delta, and the inodes it changes; rejects a link or work tree on an
    /// inode of the wrong kind.
    fn delta_touches(
        &self,
        view: &Catalog,
    ) -> Result<(BTreeMap<u32, i64>, BTreeSet<u32>), OpenError> {
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
        Ok((docs, changed))
    }
    /// Adds to `docs` the reference each `changed` inode moved from its
    /// parent document to its document in `view`.
    fn count_references(
        &self,
        view: &Catalog,
        parent: &Overlay,
        changed: BTreeSet<u32>,
        docs: &mut BTreeMap<u32, i64>,
    ) -> Result<(), OpenError> {
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
        Ok(())
    }
    /// Checks each document's references in `view` against its parent's plus
    /// `docs`, that no retired document returns or changes hash, and the live
    /// document count.
    fn check_documents_moved(
        &self,
        view: &Catalog,
        parent: &Overlay,
        docs: BTreeMap<u32, i64>,
    ) -> Result<(), OpenError> {
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
}
