//! The overlay's namespace: names and child keys replayed from the log, and
//! the graph checks over them.

use super::*;
use crate::{Name, NameId, Target};

type ChildKey = Row<(u32, Vec<u8>), Option<u32>>;

impl Overlay {
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
        let mut replay = match &self.parent {
            Some(parent) => {
                parent.load_namespace()?;
                let ns = parent.namespace();
                NameReplay {
                    rows: ns.rows.clone(),
                    keys: ns.keys.clone(),
                    touched: ns.touched.clone(),
                    references: ns.references.clone(),
                    counter: parent.manifest.counters[1],
                    count: parent.manifest.counts[1],
                }
            }
            None => NameReplay {
                rows: Projection::default(),
                keys: Runs::default(),
                touched: BTreeSet::new(),
                references: BTreeMap::new(),
                counter: self.base.next_name().0,
                count: self.base.name_count(),
            },
        };
        for (seq, next, records) in self.transactions(Family::Namespace) {
            replay.apply(&self.base, seq, next, records)?;
        }
        if replay.count != self.manifest.counts[1] {
            return Err(bad("live name count"));
        }
        let NameReplay {
            rows,
            keys,
            touched,
            references,
            ..
        } = replay;
        let namespace = Namespace::new(&self.base, rows, keys, touched, references);
        let _ = self.namespace.set(Arc::new(namespace));
        Ok(())
    }
}

/// A namespace as the log's transactions replay over it.
struct NameReplay {
    rows: Projection,
    keys: ChildKeys,
    touched: BTreeSet<u32>,
    references: BTreeMap<u32, i64>,
    /// The next name id to allocate.
    counter: u32,
    /// Live names.
    count: u32,
}

impl NameReplay {
    /// Applies transaction `seq`, which allocates names up to `next`,
    /// checking its names, keys, graph and references.
    fn apply(
        &mut self,
        base: &Catalog,
        seq: u64,
        next: [u32; 3],
        records: Vec<Arc<Record>>,
    ) -> Result<(), OpenError> {
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
            if id >= self.counter {
                if !matches!(r.as_ref(), Record::NamePut { .. }) {
                    return Err(bad("delete unassigned name"));
                }
                births.insert(id);
                self.count += 1;
            } else {
                let old = name(base, &self.rows, id).ok_or_else(|| bad("retired name reused"))?;
                if let Target::Inode(child) = old.target() {
                    *self.references.entry(child.0).or_default() -= 1;
                    *ref_changes.entry(child.0).or_default() -= 1;
                    if kind(base, &self.rows, child.0) == Some(Kind::Dir) {
                        old_dirs.insert(child.0);
                    }
                }
                key_rows.push(Row {
                    key: (old.parent.0, old.bytes.to_vec()),
                    sequence: seq,
                    value: None,
                });
                if matches!(r.as_ref(), Record::NameDelete { .. }) {
                    self.count = self
                        .count
                        .checked_sub(1)
                        .ok_or_else(|| bad("name counts"))?;
                }
            }
        }
        check_births(&births, self.counter..next[1], "name allocation gaps")?;
        for r in final_rows.values() {
            if let Record::NamePut { child, .. } = r.as_ref()
                && Kind::from_ignored_child(*child).is_none()
            {
                *self.references.entry(*child).or_default() += 1;
                *ref_changes.entry(*child).or_default() += 1;
            }
        }
        let changed: Vec<_> = final_rows.into_values().collect();
        self.rows.apply(seq, changed.iter().cloned());
        let mut removed = self.keys.clone();
        removed.insert(key_rows.clone());
        // Coalesce one transaction into one run: removals and additions
        // have the same sequence, so separate runs cannot order them.
        key_rows.extend(added_keys(base, &self.rows, &removed, &changed, seq)?);
        self.touched.extend(key_rows.iter().map(|r| r.key.0));
        self.keys.insert(key_rows);
        validate_graph(base, &self.rows, &changed, old_dirs)?;
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
        check_references(base, &self.rows, &self.keys, &self.references, affected)?;
        self.references.retain(|_, delta| *delta != 0);
        self.counter = next[1];
        Ok(())
    }
}

impl Namespace {
    fn new(
        base: &Catalog,
        rows: Projection,
        keys: ChildKeys,
        touched: BTreeSet<u32>,
        references: BTreeMap<u32, i64>,
    ) -> Self {
        let suppressed: Vec<_> = rows
            .records(NAME)
            .filter_map(|r| match r {
                Record::NamePut { id, .. } | Record::NameDelete { id }
                    if *id < base.base_name_count() =>
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
        Self {
            rows,
            keys,
            touched,
            suppressed,
            names,
            heap,
            spans,
            references,
        }
    }
    pub(crate) fn name<'a>(&'a self, base: &'a Catalog, id: u32) -> Option<Name<'a>> {
        name(base, &self.rows, id)
    }
    /// A name's sparse row, `Some(None)` for a deletion; `None` leaves the
    /// answer to the checkpoint.
    pub(crate) fn sparse_name(&self, id: u32) -> Option<Option<Name<'_>>> {
        sparse_name(&self.rows, id)
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
    /// Whether `dir`'s children differ from its checkpoint range.
    pub(crate) fn touches(&self, dir: u32) -> bool {
        self.touched.contains(&dir)
    }
    pub(crate) fn suppresses(&self, id: u32) -> bool {
        self.suppressed.binary_search(&id).is_ok()
    }
}
fn name<'a>(base: &'a Catalog, rows: &'a Projection, id: u32) -> Option<Name<'a>> {
    sparse_name(rows, id)
        .unwrap_or_else(|| (id < base.base_name_count()).then(|| base.name(NameId(id))))
}
/// A name's sparse row: `Some(None)` where it was deleted, `None` where the
/// checkpoint answers.
fn sparse_name(rows: &Projection, id: u32) -> Option<Option<Name<'_>>> {
    match rows.get(NAME, id) {
        Some(Record::NamePut {
            parent,
            child,
            name,
            ..
        }) => Some(Some(Name {
            parent: InoId(*parent),
            child: InoId(*child),
            bytes: name,
        })),
        Some(Record::NameDelete { .. }) => Some(None),
        _ => None,
    }
}
fn lookup(
    base: &Catalog,
    rows: &Projection,
    keys: &ChildKeys,
    parent: u32,
    bytes: &[u8],
) -> Option<u32> {
    if let Some(value) = keys.get_by(|k| k.0.cmp(&parent).then_with(|| k.1.as_slice().cmp(bytes))) {
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
/// One transaction's new child keys, checked against the namespace as it
/// stands after the transaction, with `removed` holding its retired keys.
fn added_keys(
    base: &Catalog,
    rows: &Projection,
    removed: &ChildKeys,
    changed: &[Arc<Record>],
    seq: u64,
) -> Result<Vec<ChildKey>, OpenError> {
    let mut additions = Vec::new();
    for r in changed {
        if let Record::NamePut {
            id,
            parent,
            child,
            name: bytes,
            ..
        } = r.as_ref()
        {
            if !live(base, rows, *parent)
                || kind(base, rows, *parent) != Some(Kind::Dir)
                || (Kind::from_ignored_child(*child).is_none() && !live(base, rows, *child))
            {
                return Err(bad("name references dead/non-directory inode"));
            }
            // Changed names are checked against the final transaction,
            // including removals of overwritten destinations.
            if let Some(other) = lookup(base, rows, removed, *parent, bytes)
                && other != *id
            {
                return Err(bad("duplicate child key"));
            }
            if kind(base, rows, *child) == Some(Kind::Dir)
                && (own(base, rows, *child) != Some(*id) || root(base, rows, *child))
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
    Ok(additions)
}
/// Checks each affected inode's name count against its life row, and that
/// a deleted directory keeps no children.
fn check_references(
    base: &Catalog,
    rows: &Projection,
    keys: &ChildKeys,
    references: &BTreeMap<u32, i64>,
    affected: BTreeSet<u32>,
) -> Result<(), OpenError> {
    if affected.is_empty() {
        return Ok(());
    }
    let base_refs = if affected.iter().any(|&id| id < base.base_inode_count()) {
        base.name_references()
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
            || (!live(base, rows, id) && actual != 0)
            || (live(base, rows, id) && kind(base, rows, id) != Some(Kind::Dir) && actual == 0)
        {
            return Err(bad("namespace inode reference count"));
        }
        if !live(base, rows, id) {
            let base_children = if id < base.base_inode_count() {
                base.children(InoId(id))
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
    Ok(())
}
fn validate_graph(
    base: &Catalog,
    rows: &Projection,
    changed: &[Arc<Record>],
    old_dirs: BTreeSet<u32>,
) -> Result<(), OpenError> {
    check_roots(base, rows)?;
    touched_dirs(base, rows, changed, old_dirs)
        .into_iter()
        .try_for_each(|dir| reaches_root(base, rows, dir))
}
/// Every root is a live directory with no incoming edge, at a distinct path.
fn check_roots(base: &Catalog, rows: &Projection) -> Result<(), OpenError> {
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
    Ok(())
}
/// The live directories whose path to a root `changed` may have altered:
/// `old_dirs`, the parents and directory children of changed names, and
/// directories whose life, record or root changed.
fn touched_dirs(
    base: &Catalog,
    rows: &Projection,
    changed: &[Arc<Record>],
    old_dirs: BTreeSet<u32>,
) -> BTreeSet<u32> {
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
    dirs
}
/// `dir`'s incoming edges lead, through live directories and without a
/// cycle, to a root.
fn reaches_root(base: &Catalog, rows: &Projection, mut at: u32) -> Result<(), OpenError> {
    let mut seen = BTreeSet::new();
    loop {
        if !live(base, rows, at) || kind(base, rows, at) != Some(Kind::Dir) || !seen.insert(at) {
            return Err(bad("directory cycle/dead parent"));
        }
        let Some(edge) = own(base, rows, at) else {
            return if root(base, rows, at) {
                Ok(())
            } else {
                Err(bad("directory has no root"))
            };
        };
        let n = name(base, rows, edge).ok_or_else(|| bad("missing directory incoming edge"))?;
        if n.target() != Target::Inode(InoId(at)) || root(base, rows, at) {
            return Err(bad("directory incoming edge/root"));
        }
        at = n.parent.0;
    }
}
