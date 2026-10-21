//! Running a query with content atoms (docs/S2.md § Composition with S1's
//! name and metadata predicates, § Planning order, § Coverage).
//!
//! ```text
//! refuse      uncovered > bound, unless the caller scans them
//! estimate    every content atom, from the dictionaries alone (no list)
//! refuse      M(root) > live / 4, unless the caller opts into scanning
//! drive       content: a top-level content-only conjunct false on rows
//!                      with no document, smallest estimate, when it is
//!                      below the name side's × 1.2 inodes per document;
//!                      its cursor tree, live-filtered, gives the documents,
//!                      and DocNames their names
//!             names:   S1's plan gives the rows, and so the documents
//! read        every atom's lists; probe each atom per document, ascending
//! emit        S1's tests per name, then the residual conjuncts in Kleene
//!             logic; a Maybe row reads its document once, through its own
//!             name, checked against the catalog, and evaluates every
//!             content atom against current bytes. Content facts from the
//!             catalogued version may be reused, with a fresh stat per copy.
//! ```
//!
//! The host supplies the pinned index, the live set, the DocId → names
//! inverse ([`DocNames`], built once per catalog generation) and the
//! checked reader; this crate opens no file.

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ferret_catalog::{Catalog, InoId, NameId, OpenError, Section, Target};
use ferret_index::{Advance, Certainty, Cursor, DocSet, Pinned};
use ferret_verify::MatchStats;

use crate::content::ReadAtom;
use crate::expr::{Node, Test, Truth};
use crate::query::{Cmp, MetaTest, Query};
use crate::verify::{Probed, Readers, Workers};
use crate::{NameIndex, Row, RunError, Stats};

/// The default bound on uncovered documents a query verifies rather than
/// refusing (docs/S2.md § Coverage): a judgement, not a measurement.
pub const UNCOVERED_BOUND: u32 = 10_000;

/// Inodes per document in the 10M fixture (9.96M / 8.13M), which scales a
/// content estimate to rows for comparison with the name side's.
const INODES_PER_DOC: (u64, u64) = (6, 5);

/// Work the host performs through this path's checked directory walk.
pub enum ReadRequest<'a> {
    /// A cheap stat against the catalog key; never reads content bytes.
    Stat,
    /// A stable, bracketed read, replacing the output buffer. The read
    /// should stop between chunks, failing, once the predicate returns
    /// true: the query was cancelled, or verification no longer needs it.
    Bytes(&'a mut Vec<u8>, &'a dyn Fn() -> bool),
    /// The same read, handing the bytes to the consumer as a reader with the
    /// document's size, instead of collecting them: the consumer reads what
    /// it needs, and stops between reads, failing, once the predicate
    /// returns true.
    Stream(Consumer<'a>, &'a dyn Fn() -> bool),
}

/// The consumer of a [`ReadRequest::Stream`].
pub type Consumer<'a> = &'a mut dyn FnMut(&mut dyn std::io::Read, u64) -> std::io::Result<()>;

/// The version established by a successful stat or bracketed read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadVersion {
    Catalogued,
    Current,
}

/// None means a path-specific read/stat failure, never a document fact.
/// Only bytes verified as Catalogued may be cached against the DocId.
pub type Reader<'a> = dyn FnMut(NameId, ReadRequest<'_>) -> Option<ReadVersion> + 'a;

/// What a content query needs from its host, besides the reader.
pub struct Content<'a> {
    /// The pinned index (or none) and the live set of the catalog view.
    pub pinned: &'a Pinned<'a>,
    /// The catalog view's DocId → names inverse.
    pub docs: &'a DocNames,
    /// The most uncovered documents to verify. `None` permits any number
    /// and overrides the quarter-of-live Maybe refusal (`--scan`).
    pub bound: Option<u32>,
}

/// Which side gave a content query its candidates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Content,
    Names,
}

/// One content atom's estimate, for `explain` and the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AtomReport {
    /// Documents its cursor may yield, uncovered ones included.
    pub estimate: u64,
    /// Yes when the index answers it without verification.
    pub certainty: Certainty,
}

/// What the content side of a run did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentReport {
    pub driver: Side,
    pub bounds: Bounds,
    pub plan: ContentPlan,
    /// Documents left Maybe by the snapshot before verification.
    pub snapshot_maybe: u64,
    pub snapshot_possible: u64,
    /// Reader plus engine time for bracketed Maybe verification, summed
    /// over the reads on every thread: work done, not wall time.
    pub verification_time: Duration,
    /// Time in regex engines, excluding reading and text matching, summed
    /// the same way.
    pub regex_time: Duration,
    /// In the order the atoms appear in the query.
    pub atoms: Vec<AtomReport>,
    pub uncovered: u32,
    pub live: u32,
    /// Documents whose atoms were probed: the content driver's, or those of
    /// the rows the name side gave.
    pub documents: u64,
    /// Bracketed byte reads attempted to settle Maybe rows.
    pub verified: u64,
    /// Paths whose current version differs from the catalog, or whose
    /// read/stat failed (including size/allocation refusal). Stable changed
    /// bytes are evaluated for this path alone; vanished or unreadable
    /// paths are dropped. A content read that failed or was abandoned once
    /// the run was stopped is not counted: it shows the stop, not a change.
    /// The stat that checks a second name of an already-read document
    /// against its cached answers is counted whenever it fails, stop or
    /// not.
    pub changed: u64,
    /// The matcher's work over the documents read: atoms checked, rejected
    /// by byte search alone, tokenized whole, bytes tokenized.
    pub matching: MatchStats,
    /// Threads that read and matched Maybe documents.
    pub threads: usize,
}

impl ContentReport {
    /// One line for the query log's plan.
    pub fn describe(&self) -> String {
        let atoms: Vec<String> = self
            .atoms
            .iter()
            .map(|a| format!("{} {:?}", a.estimate, a.certainty))
            .collect();
        format!(
            "{:?} drives; atoms [{}]; uncovered {} of {}; documents {}; verified {} ({} changed); {}",
            self.driver,
            atoms.join(", "),
            self.uncovered,
            self.live,
            self.documents,
            self.verified,
            self.changed,
            self.plan.describe()
        )
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
    /// and name rows and those documents' names, not by the catalog; the
    /// default compaction limits keep an overlay near 1% of its checkpoint.
    ///
    /// `Ok(None)` when an inode that had no document in `previous_catalog`
    /// gained one while keeping a name the overlay never rewrote: no
    /// previous row holds that name, and finding it means a pass over every
    /// name, which is the full build's job.
    pub fn adopt(
        catalog: &Catalog,
        previous_catalog: &Catalog,
        previous: &Self,
    ) -> Result<Option<Self>, OpenError> {
        catalog.load(&Section::INODE)?;
        catalog.load(&[Section::Names, Section::Doc])?;
        // A name counts toward its inode's document as in the full build.
        let owner = |view: &Catalog, name: NameId| {
            if !view.is_live_name(name) {
                return None;
            }
            match view.name(name).target() {
                Target::Inode(inode) if view.is_live_inode(inode) => {
                    view.doc(inode).map(|doc| doc.0)
                }
                _ => None,
            }
        };
        // A row changes only when one of its names or their inodes does, so
        // the documents of changed names and inodes, before and after, are
        // every row to rebuild.
        let mut affected = HashSet::new();
        let mut rewritten = Vec::new();
        for (name, live) in catalog.changed_names() {
            affected.extend(owner(previous_catalog, name));
            if live {
                affected.extend(owner(catalog, name));
                rewritten.push(name);
            }
        }
        let mut gained = Vec::new();
        for inode in catalog.changed_inodes() {
            let before = previous_catalog
                .is_live_inode(inode)
                .then(|| previous_catalog.doc(inode))
                .flatten();
            let after = catalog
                .is_live_inode(inode)
                .then(|| catalog.doc(inode))
                .flatten();
            if before != after {
                affected.extend(before.into_iter().chain(after).map(|doc| doc.0));
                if before.is_none() && previous_catalog.is_live_inode(inode) {
                    gained.push(inode);
                }
            }
        }
        if !gained.is_empty() {
            let mut found: HashMap<InoId, u32> = HashMap::new();
            for &name in &rewritten {
                if let Target::Inode(inode) = catalog.name(name).target() {
                    *found.entry(inode).or_default() += 1;
                }
            }
            if gained.into_iter().any(|inode| {
                found.get(&inode).copied().unwrap_or(0) != catalog.indexed_name_count(inode)
            }) {
                return Ok(None);
            }
        }

        let mut rows: HashMap<u32, Vec<NameId>> =
            affected.iter().map(|&doc| (doc, Vec::new())).collect();
        let candidates = affected
            .iter()
            .flat_map(|&doc| previous.names(doc))
            .copied()
            .chain(rewritten);
        for name in candidates {
            if let Some(row) = owner(catalog, name).and_then(|doc| rows.get_mut(&doc)) {
                row.push(name);
            }
        }
        let mut overrides = (*previous.overrides).clone();
        for (doc, mut names) in rows {
            names.sort_unstable();
            names.dedup();
            overrides.insert(doc, Arc::from(names));
        }
        Ok(Some(Self {
            base: previous.base.clone(),
            overrides: Arc::new(overrides),
        }))
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

impl Query {
    /// Runs a query with content atoms; see the module doc. Rows come in
    /// name order. A query without content atoms runs as
    /// [`Query::run_indexed_until`] does. Maybe documents are read on the
    /// calling thread, through `read`.
    pub fn run_content(
        &self,
        catalog: &Catalog,
        index: &NameIndex,
        content: &Content<'_>,
        read: &mut Reader<'_>,
        cancelled: Option<&AtomicBool>,
        emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<Stats, RunError> {
        self.content_or_cancelled(catalog, index, content, Readers::One(read), cancelled, emit)
    }

    /// [`Query::run_content`], reading and matching Maybe documents on
    /// `workers`. For files that do not change during the run, and without
    /// cancellation, rows, their order, where a `Break` stops, and every
    /// count in the report are as [`Query::run_content`] gives them; only
    /// the report's times and thread count differ. A document may be read
    /// before the callbacks for earlier rows run, so a callback whose
    /// effects later reads must see needs one thread (see `verify.rs`).
    /// One thread verifies on the calling thread.
    pub fn run_content_parallel(
        &self,
        catalog: &Catalog,
        index: &NameIndex,
        content: &Content<'_>,
        workers: Workers<'_, '_>,
        cancelled: Option<&AtomicBool>,
        emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<Stats, RunError> {
        self.content_or_cancelled(
            catalog,
            index,
            content,
            Readers::Pool(workers),
            cancelled,
            emit,
        )
    }

    fn content_or_cancelled(
        &self,
        catalog: &Catalog,
        index: &NameIndex,
        content: &Content<'_>,
        readers: Readers<'_, '_>,
        cancelled: Option<&AtomicBool>,
        emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<Stats, RunError> {
        match self.content_until(catalog, index, content, readers, cancelled, emit) {
            Err(RunError::Index(ferret_index::ReadError::Cancelled)) => Ok(Stats::default()),
            result => result,
        }
    }

    fn content_until(
        &self,
        catalog: &Catalog,
        index: &NameIndex,
        content: &Content<'_>,
        readers: Readers<'_, '_>,
        cancelled: Option<&AtomicBool>,
        emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<Stats, RunError> {
        let stopped = || cancelled.is_some_and(|flag| flag.load(Ordering::Acquire));
        let mut advance = Advance::new(&stopped);
        let check = || {
            if stopped() {
                Err(RunError::Index(ferret_index::ReadError::Cancelled))
            } else {
                Ok(())
            }
        };
        check()?;
        if !self.has_content() {
            return self.run_indexed_until(catalog, index, None, cancelled, emit);
        }
        let pinned = content.pinned;
        let live = pinned.live().len();
        let uncovered = pinned.uncovered().len();
        if content.bound.is_some_and(|bound| uncovered > bound) {
            return Err(RunError::IndexIncomplete { uncovered, live });
        }
        let atoms = self
            .atoms
            .iter()
            .map(|text| {
                if stopped() {
                    return Err(ferret_index::ReadError::Cancelled);
                }
                text.estimate_until(pinned, &stopped)
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(RunError::Index)?;

        let plan = self.plan_from_estimates(catalog, index, pinned, &atoms, &stopped)?;
        let bounds = plan.root;
        if content.bound.is_some() && bounds.maybe > u64::from(live) / 4 {
            return Err(RunError::ScanRequired {
                maybe: bounds.maybe,
                live,
            });
        }

        // The cheapest content conjunct that no document-less row holds.
        let driver = self
            .residual
            .iter()
            .filter(|tree| tree.is_content() && !tree.holds_without_document())
            .map(|tree| (tree, estimate(tree, &atoms, u64::from(live))))
            .min_by_key(|&(_, estimate)| estimate);
        let side = match driver {
            Some((_, estimate)) => {
                let rows = estimate.saturating_mul(INODES_PER_DOC.0) / INODES_PER_DOC.1;
                if rows <= self.name_estimate(catalog, index)? {
                    Side::Content
                } else {
                    Side::Names
                }
            }
            None => Side::Names,
        };

        let mut grep_plans = plan.greps.iter();
        let read_atoms = self
            .atoms
            .iter()
            .map(|atom| {
                let grep_plan = if matches!(atom, crate::ContentAtom::Grep(_)) {
                    grep_plans.next()
                } else {
                    None
                };
                atom.read_until(pinned, &stopped, grep_plan)
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(RunError::Index)?;
        catalog.load(&[Section::Doc])?;
        let (mut ids, mut docs) = (Vec::new(), Vec::new());
        match (side, driver) {
            (Side::Content, Some((tree, _))) => {
                let mut top = pinned
                    .top(compile(tree, &read_atoms, pinned, &stopped).map_err(RunError::Index)?);
                let mut target = 0;
                loop {
                    check()?;
                    let next = if cancelled.is_some() {
                        top.next_geq_until(target, &mut advance)
                            .map_err(RunError::Index)?
                    } else {
                        top.next_geq(target)
                    };
                    let Some((doc, _)) = next else {
                        break;
                    };
                    docs.push(doc);
                    for chunk in content.docs.names(doc).chunks(1024) {
                        check()?;
                        ids.extend_from_slice(chunk);
                    }
                    let Some(next) = doc.checked_add(1) else {
                        break;
                    };
                    target = next;
                }
                check()?;
                ids.sort_unstable();
                check()?;
            }
            _ => {
                self.s1_indexed(catalog, index, None, None, cancelled, |row| {
                    ids.push(row.name);
                    if let Some(doc) = catalog.doc(row.inode) {
                        docs.push(doc.0);
                    }
                    ControlFlow::Continue(())
                })?;
                check()?;
                docs.sort_unstable();
                check()?;
                docs.dedup();
                check()?;
            }
        }

        // Each atom's certainty on each document, probed in DocId order.
        let width = read_atoms.len();
        check()?;
        let cells = docs.len() * width;
        let mut table = Vec::new();
        while table.len() < cells {
            check()?;
            table.resize((table.len() + 4096).min(cells), None);
        }
        for (i, atom) in read_atoms.iter().enumerate() {
            check()?;
            let mut cursor = atom
                .cursor_until(pinned, &stopped)
                .map_err(RunError::Index)?;
            for (j, &doc) in docs.iter().enumerate() {
                check()?;
                let next = if cancelled.is_some() {
                    cursor
                        .next_geq_until(doc, &mut advance)
                        .map_err(RunError::Index)?
                } else {
                    cursor.next_geq(doc)
                };
                table[j * width + i] = match next {
                    Some((found, certainty)) if found == doc => Some(certainty),
                    _ => None,
                };
            }
        }

        let mut report = ContentReport {
            driver: side,
            bounds,
            plan,
            snapshot_maybe: 0,
            snapshot_possible: 0,
            verification_time: Duration::ZERO,
            regex_time: Duration::ZERO,
            atoms: atoms
                .iter()
                .map(|&(estimate, certainty, _)| AtomReport {
                    estimate,
                    certainty,
                })
                .collect(),
            uncovered,
            live,
            documents: docs.len() as u64,
            verified: 0,
            changed: 0,
            matching: MatchStats::default(),
            threads: 1,
        };
        catalog.load(&self.residual_sections())?;
        let probed = Probed {
            ids: &ids,
            names: content.docs,
            docs: &docs,
            table: &table,
            width,
        };
        let (mut stats, tally) = self.verify_rows(catalog, &probed, readers, cancelled, emit)?;
        report.verified = tally.verified;
        report.changed = tally.changed;
        report.verification_time = tally.time;
        report.regex_time = tally.regex;
        report.matching = tally.matching;
        report.snapshot_maybe = tally.maybe_docs;
        report.snapshot_possible = tally.possible_docs;
        report.threads = tally.threads;
        stats.content = Some(report);
        Ok(stats)
    }

    /// The name side's row estimate: D54's count for a name test, else
    /// every name.
    fn name_estimate(&self, catalog: &Catalog, index: &NameIndex) -> Result<u64, RunError> {
        if self.names.is_empty() && self.driver.is_none() {
            return Ok(u64::from(catalog.name_count()));
        }
        let selection = self
            .name_selection(catalog, index, None)
            .map_err(RunError::Stale)?;
        Ok(selection.estimate.hits)
    }

    /// Every residual conjunct, ANDed.
    pub(crate) fn eval_residual(&self, leaf: &mut impl FnMut(&Test) -> Truth) -> Truth {
        let mut result = Truth::Yes;
        for tree in &self.residual {
            match tree.eval(leaf) {
                Truth::No => return Truth::No,
                Truth::Maybe => result = Truth::Maybe,
                Truth::Yes => {}
            }
        }
        result
    }

    /// A name or metadata test on one row. A content atom is the caller's.
    pub(crate) fn test_row(&self, catalog: &Catalog, row: &Row<'_>, test: &Test) -> Truth {
        match test {
            Test::Name(t) => t.matches(catalog.name(row.name).bytes).into(),
            Test::Path(t) => t.matches(row.path).into(),
            Test::Meta(t) => self.meta_holds(catalog, row.inode, t).into(),
            Test::Content(_) => Truth::Maybe,
        }
    }

    /// Whether inode `id` passes one metadata test; its sections must be
    /// loaded.
    pub(crate) fn meta_holds(
        &self,
        catalog: &Catalog,
        id: ferret_catalog::InoId,
        test: &MetaTest,
    ) -> bool {
        match *test {
            MetaTest::Size(cmp, n) => cmp.holds(catalog.size(id), n),
            MetaTest::Age(cmp, secs) => self.age_holds(cmp, secs, catalog.mtime(id)),
            MetaTest::Type(kind) => catalog.kind(id) == kind,
        }
    }

    /// Whether a file modified at `mtime` is `cmp` `secs` old. Widened: mtime
    /// is whatever the file holds, and a corrupt or far-future value must not
    /// overflow.
    pub(crate) fn age_holds(&self, cmp: Cmp, secs: i64, mtime: i64) -> bool {
        cmp.holds(i128::from(self.now) - i128::from(mtime), i128::from(secs))
    }

    /// The sections the residual's metadata tests read.
    pub(crate) fn residual_sections(&self) -> Vec<Section> {
        fn walk(node: &Node<Test>, out: &mut Vec<Section>) {
            match node {
                Node::Leaf(Test::Meta(test)) => {
                    for &section in test.sections() {
                        if !out.contains(&section) {
                            out.push(section);
                        }
                    }
                }
                Node::Leaf(_) => {}
                Node::And(nodes) | Node::Or(nodes) => nodes.iter().for_each(|n| walk(n, out)),
                Node::Not(inner) => walk(inner, out),
            }
        }
        let mut sections = Vec::new();
        self.residual
            .iter()
            .for_each(|tree| walk(tree, &mut sections));
        sections
    }
}

/// An upper bound on the documents a content-only tree can yield, from the
/// atoms' estimates: an AND by its smallest positive child, an OR by its
/// sum, a NOT by every live document.
fn estimate(tree: &Node<Test>, atoms: &[(u64, Certainty, u64)], live: u64) -> u64 {
    match tree {
        Node::Leaf(Test::Content(i)) => atoms[*i].0,
        Node::Leaf(_) | Node::Not(_) => live,
        Node::And(all) => all
            .iter()
            .filter(|n| !matches!(n, Node::Not(_)))
            .map(|n| estimate(n, atoms, live))
            .min()
            .unwrap_or(live),
        Node::Or(any) => any
            .iter()
            .map(|n| estimate(n, atoms, live))
            .fold(0, u64::saturating_add),
    }
}

/// A content-only tree's cursor: AND's positive children leapfrog, its
/// negated ones subtract (`AndNot`), and a NOT with nothing positive beside
/// it subtracts from the live set.
fn compile<'a>(
    tree: &Node<Test>,
    atoms: &'a [ReadAtom],
    pinned: &'a Pinned<'_>,
    cancelled: &dyn Fn() -> bool,
) -> Result<Cursor<'a>, ferret_index::ReadError> {
    if cancelled() {
        return Err(ferret_index::ReadError::Cancelled);
    }
    Ok(match tree {
        Node::Leaf(Test::Content(i)) => atoms[*i].cursor_until(pinned, cancelled)?,
        // A driving tree is content-only; a name test never gets here.
        Node::Leaf(_) => Cursor::bits(pinned.live(), Certainty::Maybe),
        Node::And(all) => {
            let (negated, positive): (Vec<_>, Vec<_>) =
                all.iter().partition(|n| matches!(n, Node::Not(_)));
            let cursor = if positive.is_empty() {
                Cursor::bits(pinned.live(), Certainty::Yes)
            } else {
                Cursor::and(
                    positive
                        .iter()
                        .map(|n| compile(n, atoms, pinned, cancelled))
                        .collect::<Result<_, _>>()?,
                )
            };
            // Sibling exclusions add two layers (union and subtraction),
            // irrespective of width. Traversal, cost and Drop therefore stay
            // bounded by the parser's AST depth, not its number of siblings.
            let exclusions = negated
                .into_iter()
                .filter_map(|node| match node {
                    Node::Not(inner) => Some(compile(inner, atoms, pinned, cancelled)),
                    _ => None,
                })
                .collect::<Result<Vec<_>, _>>()?;
            Cursor::and_not(
                cursor,
                Cursor::or_until(exclusions, pinned.live().bound(), cancelled)?,
            )
        }
        Node::Or(any) => Cursor::or_until(
            any.iter()
                .map(|n| compile(n, atoms, pinned, cancelled))
                .collect::<Result<_, _>>()?,
            pinned.live().bound(),
            cancelled,
        )?,
        Node::Not(inner) => pinned.not(compile(inner, atoms, pinned, cancelled)?),
    })
}

/// Dictionary-only upper bounds in the document domain. Neither bound may
/// be subtracted: dead documents and aliases can inflate them independently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub possible: u64,
    pub maybe: u64,
}

impl Bounds {
    fn and(self, other: Self) -> Self {
        let possible = self.possible.min(other.possible);
        Self {
            possible,
            maybe: possible.min(self.maybe.saturating_add(other.maybe)),
        }
    }
}

impl Query {
    /// Explains the dictionary-only plan, including refused expressions.
    /// No postings are read. Execution records candidate counts separately.
    pub fn explain_content(
        &self,
        catalog: &Catalog,
        index: &NameIndex,
        pinned: &Pinned<'_>,
    ) -> Result<ContentPlan, RunError> {
        let atoms = self
            .atoms
            .iter()
            .map(|atom| atom.estimate_until(pinned, &|| false))
            .collect::<Result<Vec<_>, _>>()
            .map_err(RunError::Index)?;
        self.plan_from_estimates(catalog, index, pinned, &atoms, &|| false)
    }

    fn plan_from_estimates(
        &self,
        catalog: &Catalog,
        index: &NameIndex,
        pinned: &Pinned<'_>,
        atoms: &[(u64, Certainty, u64)],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ContentPlan, RunError> {
        fn node(
            tree: &Node<Test>,
            catalog: &Catalog,
            index: &NameIndex,
            atoms: &[(u64, Certainty, u64)],
            live: u64,
            reports: &mut Vec<NodeReport>,
        ) -> Result<Bounds, RunError> {
            let bounds = match tree {
                Node::Leaf(Test::Content(i)) => {
                    let possible = atoms[*i].0.min(live);
                    Bounds {
                        possible,
                        maybe: atoms[*i].2.min(possible),
                    }
                }
                Node::Leaf(Test::Name(test)) => Bounds {
                    possible: index
                        .select(catalog, None, &[], |name| test.matches(name))
                        .map_err(RunError::Stale)?
                        .estimate
                        .hits
                        .min(live),
                    maybe: 0,
                },
                Node::Leaf(_) => Bounds {
                    possible: live,
                    maybe: 0,
                },
                Node::Not(inner) => Bounds {
                    possible: live,
                    maybe: node(inner, catalog, index, atoms, live, reports)?.maybe,
                },
                Node::And(all) => {
                    let mut bounds = Bounds {
                        possible: live,
                        maybe: 0,
                    };
                    for child in all {
                        bounds = bounds.and(node(child, catalog, index, atoms, live, reports)?);
                    }
                    bounds
                }
                Node::Or(any) => {
                    let mut bounds = Bounds {
                        possible: 0,
                        maybe: 0,
                    };
                    for child in any {
                        let b = node(child, catalog, index, atoms, live, reports)?;
                        bounds.possible = bounds.possible.saturating_add(b.possible).min(live);
                        bounds.maybe = bounds.maybe.saturating_add(b.maybe).min(live);
                    }
                    bounds
                }
            };
            reports.push(NodeReport {
                bounds,
                expression: tree.describe(&|test| match test {
                    Test::Content(i) => format!("content[{i}]"),
                    Test::Name(_) => "name".into(),
                    Test::Path(_) => "path".into(),
                    Test::Meta(_) => "metadata".into(),
                }),
            });
            Ok(bounds)
        }
        let live = u64::from(pinned.live().len());
        let mut reports = Vec::new();
        for test in &self.names {
            reports.push(NodeReport {
                expression: "name".into(),
                bounds: Bounds {
                    possible: index
                        .select(catalog, None, &[], |name| test.matches(name))
                        .map_err(RunError::Stale)?
                        .estimate
                        .hits
                        .min(live),
                    maybe: 0,
                },
            });
        }
        for _ in &self.paths {
            reports.push(NodeReport {
                expression: "path".into(),
                bounds: Bounds {
                    possible: live,
                    maybe: 0,
                },
            });
        }
        for _ in &self.meta {
            reports.push(NodeReport {
                expression: "metadata".into(),
                bounds: Bounds {
                    possible: live,
                    maybe: 0,
                },
            });
        }
        let mut bounds = Bounds {
            possible: self.name_estimate(catalog, index)?.min(live),
            maybe: 0,
        };
        for tree in &self.residual {
            bounds = bounds.and(node(tree, catalog, index, atoms, live, &mut reports)?);
        }
        let mut greps = Vec::new();
        for atom in &self.atoms {
            if let crate::ContentAtom::Grep(grep) = atom {
                greps.push(crate::grep::explain(grep, pinned, cancelled).map_err(RunError::Index)?);
            }
        }
        Ok(ContentPlan {
            root: bounds,
            nodes: reports,
            greps,
        })
    }
}

/// One expression node's P/M bounds, in postorder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeReport {
    pub expression: String,
    pub bounds: Bounds,
}

/// The pre-read content plan, available even when execution refuses a scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentPlan {
    pub root: Bounds,
    pub nodes: Vec<NodeReport>,
    pub greps: Vec<crate::grep::GrepReport>,
}

impl ContentPlan {
    /// Includes every node, derived tree, selected keys and dictionary df.
    pub fn describe(&self) -> String {
        format!(
            "P={} M={}; nodes {:?}; regex plans {:?}",
            self.root.possible, self.root.maybe, self.nodes, self.greps
        )
    }
}
