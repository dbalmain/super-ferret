//! [`DocNames`]: the DocId → names inverse a content query emits rows
//! through, built once per catalog generation and adopted across an
//! overlay's small changes rather than rebuilt.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ferret_catalog::{InoId, OpenError, Target};
use ferret_index::DocSet;

use super::*;

/// Names [`DocNames::adopt`] may place for each overlay inode or name row
/// before it leaves the set to the full build. Rows of hard links and small
/// groups of copies fit; a document shared by many copies does not.
const ADOPT_FANOUT: usize = 4;

/// Names [`DocNames::adopt`] may place regardless of the overlay's size, so
/// a small overlay touching a few multi-name rows still adopts.
const ADOPT_FLOOR: usize = 64;

/// The document a live name counts toward, as in the full build.
fn owner(view: &Catalog, name: NameId) -> Option<u32> {
    if !view.is_live_name(name) {
        return None;
    }
    match view.name(name).target() {
        Target::Inode(inode) if view.is_live_inode(inode) => view.doc(inode).map(|doc| doc.0),
        _ => None,
    }
}

/// What an overlay changed, for [`DocNames::adopt`].
struct Changes {
    /// A row changes only when one of its names or their inodes does, so the
    /// documents of changed names and inodes, before and after, are every
    /// row to rebuild.
    affected: HashSet<u32>,
    /// The changed names still live.
    rewritten: Vec<NameId>,
    /// Inodes live before that had no document then and have one now.
    gained: Vec<InoId>,
    /// Changed names and inodes.
    overlay_rows: usize,
}

impl Changes {
    fn between(previous: &Catalog, catalog: &Catalog) -> Self {
        let mut affected = HashSet::new();
        let mut rewritten = Vec::new();
        let mut overlay_rows = 0usize;
        for (name, live) in catalog.changed_names() {
            overlay_rows += 1;
            affected.extend(owner(previous, name));
            if live {
                affected.extend(owner(catalog, name));
                rewritten.push(name);
            }
        }
        let mut gained = Vec::new();
        for inode in catalog.changed_inodes() {
            overlay_rows += 1;
            let before = previous
                .is_live_inode(inode)
                .then(|| previous.doc(inode))
                .flatten();
            let after = catalog
                .is_live_inode(inode)
                .then(|| catalog.doc(inode))
                .flatten();
            if before != after {
                affected.extend(before.into_iter().chain(after).map(|doc| doc.0));
                if before.is_none() && previous.is_live_inode(inode) {
                    gained.push(inode);
                }
            }
        }
        Self {
            affected,
            rewritten,
            gained,
            overlay_rows,
        }
    }

    /// Whether every name of each inode that gained a document was
    /// rewritten, so that no previous row's name is missing from its row.
    fn gained_names_rewritten(&self, catalog: &Catalog) -> bool {
        if self.gained.is_empty() {
            return true;
        }
        let mut found: HashMap<InoId, u32> = HashMap::new();
        for &name in &self.rewritten {
            if let Target::Inode(inode) = catalog.name(name).target() {
                *found.entry(inode).or_default() += 1;
            }
        }
        self.gained.iter().all(|inode| {
            found.get(inode).copied().unwrap_or(0) == catalog.indexed_name_count(*inode)
        })
    }
}

/// The rows one full build placed, shared by the views that adopt it.
#[derive(Debug)]
struct DocNameBase {
    /// `names[starts[d]..starts[d + 1]]` are document `d`'s.
    starts: Vec<u32>,
    names: Vec<NameId>,
}

/// Every live name of each document, by DocId: D30's lazy `DocId →
/// [InoId]` inverse, taken one step further to names, which is what rows
/// need. Built by one pass over the names; the host keeps it for the
/// catalog generation it was built from, and a later view of the same
/// epoch adopts it ([`DocNames::adopt`]), replacing only the rows of
/// documents its overlay touched.
#[derive(Debug)]
pub struct DocNames {
    base: Arc<DocNameBase>,
    overrides: Arc<HashMap<u32, Arc<[NameId]>>>,
}

impl DocNames {
    pub fn new(catalog: &Catalog) -> Result<Self, OpenError> {
        match Self::new_until(catalog, || false)? {
            Some(docs) => Ok(docs),
            None => unreachable!("a build that is never cancelled completes"),
        }
    }

    /// As [`DocNames::new`], `Ok(None)` once `cancelled` answers true. It is
    /// asked at entry, after loading, after every [`DocSet::CHECK_EVERY`]
    /// names the name pass reads, every [`DocSet::CHECK_EVERY`] ids of the
    /// prefix pass, on each side of copying the offsets, and every
    /// [`DocSet::CHECK_EVERY`] names the fill places. A host that
    /// caches the result publishes it only when this returns one.
    pub fn new_until(
        catalog: &Catalog,
        cancelled: impl Fn() -> bool,
    ) -> Result<Option<Self>, OpenError> {
        if cancelled() {
            return Ok(None);
        }
        catalog.load(&Section::INODE)?;
        catalog.load(&[Section::Names, Section::Doc])?;
        if cancelled() {
            return Ok(None);
        }
        let bound = catalog.next_doc().0 as usize;
        let mut starts = vec![0u32; bound + 1];
        // One pass over the child column (a name's bytes are not needed),
        // keeping each document-bearing name with its document; the fill
        // reads these rather than walking the names again. A second walk
        // cost as much as the first, 2 ms at 112k names; these cost 8 bytes
        // a document's name until the build returns.
        let mut found = Vec::new();
        for (seen, (id, child)) in catalog.name_reader().child_ids().enumerate() {
            if (seen + 1).is_multiple_of(DocSet::CHECK_EVERY) && cancelled() {
                return Ok(None);
            }
            if let Target::Inode(inode) = Target::from_child(child)
                && catalog.is_live_name(id)
                && catalog.is_live_inode(inode)
                && let Some(doc) = catalog.doc(inode)
                && (doc.0 as usize) < bound
            {
                starts[doc.0 as usize + 1] += 1;
                found.push((doc.0, id));
            }
        }
        for d in 0..bound {
            if d.is_multiple_of(DocSet::CHECK_EVERY) && cancelled() {
                return Ok(None);
            }
            starts[d + 1] += starts[d];
        }
        if cancelled() {
            return Ok(None);
        }
        let mut fill = starts.clone();
        if cancelled() {
            return Ok(None);
        }
        let mut names = vec![NameId(0); starts[bound] as usize];
        for (seen, &(doc, id)) in found.iter().enumerate() {
            if (seen + 1).is_multiple_of(DocSet::CHECK_EVERY) && cancelled() {
                return Ok(None);
            }
            names[fill[doc as usize] as usize] = id;
            fill[doc as usize] += 1;
        }
        // The checkpoint's names come in id order, then the overlay's, which
        // may reuse a lower id: then a document's names are sorted here.
        if cancelled() {
            return Ok(None);
        }
        if !found.is_sorted_by_key(|&(_, id)| id)
            && order(&starts, &mut names, &cancelled).is_none()
        {
            return Ok(None);
        }
        if cancelled() {
            return Ok(None);
        }
        Ok(Some(Self {
            base: Arc::new(DocNameBase { starts, names }),
            overrides: Arc::new(HashMap::new()),
        }))
    }

    /// `previous`, built for `previous_catalog`, carried forward to
    /// `catalog`, a later view of the same checkpoint epoch: only the rows
    /// of documents the overlay touched are rebuilt, from their previous
    /// names and the overlay's. The work is bounded by the overlay's inode
    /// and name rows: at most `ADOPT_FANOUT` names placed for each, plus
    /// `ADOPT_FLOOR`; the default compaction limits keep an overlay near
    /// 1% of its checkpoint.
    ///
    /// `Ok(None)`, leaving the set for the full build:
    ///
    /// - when an inode that had no document in `previous_catalog` gained one
    ///   while keeping a name the overlay never rewrote: no previous row holds
    ///   that name, and finding it means a pass over every name;
    /// - when the touched rows exceed that bound. Distinct inodes with
    ///   identical content share a document, so one renamed copy touches a row
    ///   as long as all its copies, and the cumulative overlay would touch it
    ///   again on every later refresh of the epoch. A count per document would
    ///   not do: rows emit every name.
    pub fn adopt(
        catalog: &Catalog,
        previous_catalog: &Catalog,
        previous: &Self,
    ) -> Result<Option<Self>, OpenError> {
        catalog.load(&Section::INODE)?;
        catalog.load(&[Section::Names, Section::Doc])?;
        let changes = Changes::between(previous_catalog, catalog);
        if !changes.gained_names_rewritten(catalog) {
            return Ok(None);
        }
        // Placing the candidates and carrying the replaced rows is the rest
        // of the work; refuse it before doing it.
        let work = changes
            .affected
            .iter()
            .map(|&doc| previous.names(doc).len())
            .sum::<usize>()
            + changes.rewritten.len()
            + previous.overrides.len();
        if work > changes.overlay_rows.saturating_mul(ADOPT_FANOUT) + ADOPT_FLOOR {
            return Ok(None);
        }
        Ok(Some(previous.rebuild(catalog, changes)))
    }

    /// `self` with the rows of `changes`' affected documents rebuilt from
    /// their previous names and the rewritten ones.
    fn rebuild(&self, catalog: &Catalog, changes: Changes) -> Self {
        let Changes {
            affected,
            rewritten,
            ..
        } = changes;
        let mut rows: HashMap<u32, Vec<NameId>> =
            affected.iter().map(|&doc| (doc, Vec::new())).collect();
        let candidates = affected
            .iter()
            .flat_map(|&doc| self.names(doc))
            .copied()
            .chain(rewritten);
        for name in candidates {
            if let Some(row) = owner(catalog, name).and_then(|doc| rows.get_mut(&doc)) {
                row.push(name);
            }
        }
        let mut overrides = (*self.overrides).clone();
        for (doc, mut names) in rows {
            names.sort_unstable();
            names.dedup();
            overrides.insert(doc, Arc::from(names));
        }
        Self {
            base: self.base.clone(),
            overrides: Arc::new(overrides),
        }
    }

    /// The document's live names, ascending; empty for an id it never saw.
    pub fn names(&self, doc: u32) -> &[NameId] {
        if let Some(names) = self.overrides.get(&doc) {
            return names;
        }
        let doc = doc as usize;
        match (self.base.starts.get(doc), self.base.starts.get(doc + 1)) {
            (Some(&from), Some(&to)) => &self.base.names[from as usize..to as usize],
            _ => &[],
        }
    }

    /// Bytes held.
    pub fn bytes(&self) -> usize {
        (self.base.starts.len() + self.base.names.len()) * 4
            + self
                .overrides
                .values()
                .map(|names| names.len() * std::mem::size_of::<NameId>())
                .sum::<usize>()
    }
}

/// Sorts each document's slice of `names`, the documents delimited by
/// `starts`; `None` once `cancelled` says so. The pass runs over every
/// document id, holes included, so `cancelled` is asked every
/// [`DocSet::CHECK_EVERY`] ids and before any slice that long.
pub(crate) fn order(
    starts: &[u32],
    names: &mut [NameId],
    cancelled: &impl Fn() -> bool,
) -> Option<()> {
    for (d, doc) in starts.windows(2).enumerate() {
        let names = &mut names[doc[0] as usize..doc[1] as usize];
        if (d.is_multiple_of(DocSet::CHECK_EVERY) || names.len() >= DocSet::CHECK_EVERY)
            && cancelled()
        {
            return None;
        }
        names.sort_unstable();
    }
    Some(())
}
