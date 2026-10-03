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
}
impl Projection {
    pub(crate) fn get(&self, category: u64, id: u32) -> Option<&Record> {
        self.rows.get(&key(category, id)).map(Arc::as_ref)
    }
    pub(crate) fn records(&self, category: u64) -> impl Iterator<Item = &Record> {
        self.rows
            .latest()
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
                .map(|r| Row {
                    key: record_key(&r),
                    sequence,
                    value: r,
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
    namespace: OnceLock<Namespace>,
}
pub(crate) struct Namespace {
    pub(crate) rows: Projection,
    keys: Runs<(u32, Vec<u8>), Option<u32>>,
    pub(crate) suppressed: Vec<u32>,
    pub(crate) names: Vec<u32>,
    pub(crate) heap: Vec<u8>,
    pub(crate) spans: Vec<(usize, u32)>,
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
            p.apply(seq, records.into_iter());
            counters = next;
        }
        if family == Family::Namespace
            && (counts[0] != self.manifest.counts[0] || counts[2] != self.manifest.counts[2])
        {
            return Err(bad("live inode/directory counts"));
        }
        let _ = self.families[family as usize].set(p);
        Ok(())
    }
    pub(crate) fn projection(&self, family: Family) -> &Projection {
        self.families[family as usize]
            .get()
            .expect("overlay family not loaded")
    }
    pub(crate) fn namespace(&self) -> &Namespace {
        self.namespace.get().expect("namespace not loaded")
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
    pub(crate) fn bytes_read(&self) -> u64 {
        self.log.as_ref().map_or(0, |l| l.bytes_read())
            + self.parent.as_ref().map_or(0, |p| p.bytes_read())
    }
    pub(crate) fn load_namespace(&self) -> Result<(), OpenError> {
        if self.namespace.get().is_some() {
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
            rows.apply(seq, final_rows.into_values());
            keys.insert(key_rows);
            let mut additions = Vec::new();
            for r in rows.records(NAME) {
                if let Record::NamePut {
                    id,
                    parent,
                    child,
                    name: bytes,
                } = r
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
                    if let Some(other) = lookup(&self.base, &rows, &keys, *parent, bytes)
                        && other != *id
                    {
                        return Err(bad("duplicate child key"));
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
            keys.insert(additions);
            validate_graph(&self.base, &rows)?;
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
        let _ = self.namespace.set(Namespace {
            rows,
            keys,
            suppressed,
            names,
            heap,
            spans,
        });
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
fn validate_graph(base: &Catalog, rows: &Projection) -> Result<(), OpenError> {
    let mut dirs = BTreeSet::new();
    for r in rows.records(NAME) {
        if let Record::NamePut { parent, child, .. } = r {
            dirs.insert(*parent);
            if kind(base, rows, *child) == Some(Kind::Dir) {
                dirs.insert(*child);
            }
        }
    }
    for category in [LIFE, DIR, ROOT] {
        for r in rows.records(category) {
            match r {
                Record::LifePut {
                    id,
                    kind: Kind::Dir,
                    ..
                }
                | Record::DirPut { id, .. }
                | Record::RootPut { id, .. } => {
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
