//! Sparse, immutable replacements over a pinned checkpoint. Payload families
//! stay independent: namespace graph checks wait for a name/path consumer.
mod ids;
mod namespace;
pub(crate) mod runs;
mod validate;
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
        Record::DocPut { id, .. }
        | Record::DocDelete { id }
        | Record::DocDormant { id, .. }
        | Record::DocForget { id } => (DOC, *id),
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
    /// Retained permissions as of the last applied transaction; Docs only.
    pub(crate) dormant: Option<crate::dormant::State>,
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
        if family == Family::Docs && p.dormant.is_none() {
            self.base.load(&[Section::Dormant])?;
            p.dormant = Some(self.base.dormant_state().clone());
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
            if family == Family::Docs {
                self.advance_dormant(&mut p, &records, seq)?;
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
        self.base.load(&[Section::Docs, Section::Dormant])?;
        let mut final_rows = BTreeMap::new();
        for r in records {
            final_rows.insert(record_key(r), r.as_ref());
        }
        let mut births = BTreeSet::new();
        for r in final_rows.values() {
            let id = match r {
                Record::DocPut { id, .. }
                | Record::DocDelete { id }
                | Record::DocDormant { id, .. }
                | Record::DocForget { id } => *id,
                _ => continue,
            };
            let (live, dormant) = match p.get(DOC, id) {
                Some(Record::DocPut { hash, .. }) => (Some(*hash), None),
                Some(Record::DocDormant { hash, .. }) => (None, Some(*hash)),
                Some(_) => (None, None),
                None => (
                    self.base.doc_hash(crate::DocId(id)),
                    self.base.dormant_hash(crate::DocId(id)),
                ),
            };
            match r {
                Record::DocPut { hash, .. } => {
                    if id >= counters[2] {
                        births.insert(id);
                        counts[3] += 1;
                    } else if let Some(old) = live.or(dormant) {
                        if old != *hash {
                            return Err(bad("document hash changed"));
                        }
                        counts[3] += u32::from(live.is_none());
                    } else {
                        return Err(bad("unassigned/forgotten document reused"));
                    }
                }
                Record::DocDelete { .. } | Record::DocDormant { .. } => {
                    let Some(hash) = live else {
                        return Err(bad("delete absent document"));
                    };
                    if let Record::DocDormant { hash: retained, .. } = r
                        && hash != *retained
                    {
                        return Err(bad("dormant hash changed"));
                    }
                    counts[3] = counts[3]
                        .checked_sub(1)
                        .ok_or_else(|| bad("document counts"))?;
                }
                Record::DocForget { .. } if dormant.is_none() => {
                    return Err(bad("forget absent dormant document"));
                }
                _ => {}
            }
        }
        check_births(&births, counters[2]..next[2], "document allocation gaps")
    }
    /// Checks a transaction's permission changes against the retained state
    /// before it: dormant hashes stay unique and within the cap. Costs are in
    /// the transaction's final document rows, not the retained table.
    fn advance_dormant(
        &self,
        projection: &mut Projection,
        records: &[Arc<Record>],
        sequence: u64,
    ) -> Result<(), OpenError> {
        let mut final_rows = BTreeMap::new();
        for record in records {
            let id = match record.as_ref() {
                Record::DocPut { id, .. }
                | Record::DocDelete { id }
                | Record::DocDormant { id, .. }
                | Record::DocForget { id } => *id,
                _ => continue,
            };
            final_rows.insert(id, record.as_ref());
        }
        let mut removed = Vec::new();
        let mut added = Vec::new();
        for (id, record) in final_rows {
            let prior = match projection.get(DOC, id) {
                Some(Record::DocDormant { hash, .. }) => Some(*hash),
                Some(_) => None,
                None => self.base.dormant_hash(crate::DocId(id)),
            };
            if let Some(hash) = prior {
                removed.push((crate::DocId(id), hash));
            }
            if let Record::DocDormant { hash, .. } = record {
                added.push((crate::DocId(id), *hash));
            }
        }
        let Some(state) = projection.dormant.as_mut() else {
            unreachable!("Docs projection initializes retained metadata");
        };
        state
            .advance(sequence, &removed, &added)
            .map_err(OpenError::Decode)
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
            Section::Docs | Section::Dormant => {
                self.families[Family::Docs as usize].get().is_some()
                    && (self.families[Family::Inodes as usize].get().is_none()
                        || self.docs_checked.get().is_some())
            }
            Section::DocRefs => self.docs_checked.get().is_some(),
            Section::Strings | Section::Policy => true,
            _ => self.fields_checked.get().is_some(),
        }
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
    pub(crate) fn bytes_read(&self) -> u64 {
        self.log.as_ref().map_or(0, |l| l.bytes_read())
            + self.parent.as_ref().map_or(0, |p| p.bytes_read())
    }
}
