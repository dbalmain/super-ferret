//! Running a query with content atoms (docs/S2.md § Composition with S1's
//! name and metadata predicates, § Planning order, § Coverage).
//!
//! ```text
//! refuse      uncovered > bound, unless the caller scans them
//! estimate    every content atom, from the dictionaries alone (no list)
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

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};

use ferret_catalog::{Catalog, NameId, OpenError, Section, Target};
use ferret_index::{Certainty, Cursor, Pinned};
use ferret_verify::{MatchStats, TextMatcher};

use crate::expr::{Node, Test, Truth};
use crate::query::{Cmp, MetaTest, Query};
use crate::{NameIndex, Row, RunError, Stats, TextAtom};

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
    /// A stable, bracketed read, replacing the output buffer.
    Bytes(&'a mut Vec<u8>),
}

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
    /// The most uncovered documents to verify; `None` verifies any number
    /// (`--scan-uncovered`).
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
    /// read/stat failed. Stable changed bytes are evaluated for this path
    /// alone; vanished or unreadable paths are dropped.
    pub changed: u64,
    /// The matcher's work over the documents read: atoms checked, rejected
    /// by byte search alone, tokenized whole, bytes tokenized.
    pub matching: MatchStats,
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
            "{:?} drives; atoms [{}]; uncovered {} of {}; documents {}; verified {} ({} changed)",
            self.driver,
            atoms.join(", "),
            self.uncovered,
            self.live,
            self.documents,
            self.verified,
            self.changed
        )
    }
}

/// Every live name of each document, by DocId: D30's lazy `DocId →
/// [InoId]` inverse, taken one step further to names, which is what rows
/// need. Built by one pass over the names; the host keeps it for the
/// catalog generation it was built from.
#[derive(Debug)]
pub struct DocNames {
    /// `names[starts[d]..starts[d + 1]]` are document `d`'s.
    starts: Vec<u32>,
    names: Vec<NameId>,
}

impl DocNames {
    pub fn new(catalog: &Catalog) -> Result<Self, OpenError> {
        catalog.load(&Section::INODE)?;
        catalog.load(&[Section::Names, Section::Doc])?;
        let bound = catalog.next_doc().0 as usize;
        let mut starts = vec![0u32; bound + 1];
        let each = |f: &mut dyn FnMut(usize, NameId)| {
            for (id, name) in catalog.name_reader().runs_from(NameId(0)) {
                if let Target::Inode(inode) = name.target()
                    && catalog.is_live_name(id)
                    && catalog.is_live_inode(inode)
                    && let Some(doc) = catalog.doc(inode)
                    && (doc.0 as usize) < bound
                {
                    f(doc.0 as usize, id);
                }
            }
        };
        each(&mut |doc, _| starts[doc + 1] += 1);
        for d in 0..bound {
            starts[d + 1] += starts[d];
        }
        let mut fill = starts.clone();
        let mut names = vec![NameId(0); starts[bound] as usize];
        each(&mut |doc, id| {
            names[fill[doc] as usize] = id;
            fill[doc] += 1;
        });
        Ok(Self { starts, names })
    }

    /// The document's live names, ascending; empty for an id it never saw.
    pub fn names(&self, doc: u32) -> &[NameId] {
        let doc = doc as usize;
        match (self.starts.get(doc), self.starts.get(doc + 1)) {
            (Some(&from), Some(&to)) => &self.names[from as usize..to as usize],
            _ => &[],
        }
    }

    /// Bytes held.
    pub fn bytes(&self) -> usize {
        (self.starts.len() + self.names.len()) * 4
    }
}

impl Query {
    /// Runs a query with content atoms; see the module doc. Rows come in
    /// name order. A query without content atoms runs as
    /// [`Query::run_indexed_until`] does.
    pub fn run_content(
        &self,
        catalog: &Catalog,
        index: &NameIndex,
        content: &Content<'_>,
        read: &mut Reader<'_>,
        cancelled: Option<&AtomicBool>,
        emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<Stats, RunError> {
        match self.content_until(catalog, index, content, read, cancelled, emit) {
            Err(RunError::Index(ferret_index::ReadError::Cancelled)) => Ok(Stats::default()),
            result => result,
        }
    }

    fn content_until(
        &self,
        catalog: &Catalog,
        index: &NameIndex,
        content: &Content<'_>,
        read: &mut Reader<'_>,
        cancelled: Option<&AtomicBool>,
        mut emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<Stats, RunError> {
        let stopped = || cancelled.is_some_and(|flag| flag.load(Ordering::Acquire));
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
            .texts
            .iter()
            .map(|text| {
                if stopped() {
                    return Err(ferret_index::ReadError::Cancelled);
                }
                TextAtom::estimate_until(text, pinned, &stopped)
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(RunError::Index)?;

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

        let read_atoms = self
            .texts
            .iter()
            .map(|text| TextAtom::read_until(text.clone(), pinned, &stopped))
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
                    let Some((doc, _)) = top.next_geq(target) else {
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
                table[j * width + i] = match cursor.next_geq(doc) {
                    Some((found, certainty)) if found == doc => Some(certainty),
                    _ => None,
                };
            }
        }

        let mut report = ContentReport {
            driver: side,
            atoms: atoms
                .iter()
                .map(|&(estimate, certainty)| AtomReport {
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
        };
        catalog.load(&self.residual_sections())?;
        let mut matcher = TextMatcher::new();
        let mut bytes = Vec::new();
        // Only content facts established by a bracketed catalog-version read.
        // Path failures never enter this map; every path has its own stat.
        let mut exact: HashMap<u32, Vec<bool>> = HashMap::new();
        let mut rows = 0;
        let mut stats = self.s1_ids(catalog, &ids, cancelled, |row| {
            let doc = catalog.doc(row.inode).map(|d| d.0);
            let at = doc.and_then(|d| docs.binary_search(&d).ok());
            let indexed = |i: usize| match at.and_then(|j| table[j * width + i]) {
                None => Truth::No,
                Some(Certainty::Maybe) => Truth::Maybe,
                Some(Certainty::Yes) => Truth::Yes,
            };
            let mut truth = self.eval_residual(&mut |test| match test {
                Test::Content(i) => indexed(*i),
                other => self.test_row(catalog, row, other),
            });
            if truth == Truth::Maybe
                && let Some(doc) = doc
            {
                let reusable = if exact.contains_key(&doc) {
                    match read(row.name, ReadRequest::Stat) {
                        Some(ReadVersion::Catalogued) => true,
                        Some(ReadVersion::Current) => false,
                        None => {
                            report.changed += 1;
                            return ControlFlow::Continue(());
                        }
                    }
                } else {
                    false
                };
                let current;
                let answers = if reusable {
                    &exact[&doc]
                } else {
                    report.verified += 1;
                    let Some(version) = read(row.name, ReadRequest::Bytes(&mut bytes)) else {
                        report.changed += 1;
                        return ControlFlow::Continue(());
                    };
                    let answers: Vec<bool> = self
                        .texts
                        .iter()
                        .map(|text| matcher.is_match(text, &bytes))
                        .collect();
                    match version {
                        ReadVersion::Catalogued => exact.entry(doc).or_insert(answers),
                        ReadVersion::Current => {
                            report.changed += 1;
                            current = answers;
                            &current
                        }
                    }
                };
                truth = self.eval_residual(&mut |test| match test {
                    Test::Content(i) => answers[*i].into(),
                    other => self.test_row(catalog, row, other),
                });
            }
            if truth != Truth::Yes {
                return ControlFlow::Continue(());
            }
            rows += 1;
            emit(row)
        })?;
        stats.rows = rows;
        report.matching = matcher.stats();
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
fn estimate(tree: &Node<Test>, atoms: &[(u64, Certainty)], live: u64) -> u64 {
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
    atoms: &'a [TextAtom],
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
            let mut cursor = if positive.is_empty() {
                Cursor::bits(pinned.live(), Certainty::Yes)
            } else {
                Cursor::and(
                    positive
                        .iter()
                        .map(|n| compile(n, atoms, pinned, cancelled))
                        .collect::<Result<_, _>>()?,
                )
            };
            for node in negated {
                if let Node::Not(inner) = node {
                    cursor = Cursor::and_not(cursor, compile(inner, atoms, pinned, cancelled)?);
                }
            }
            cursor
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
