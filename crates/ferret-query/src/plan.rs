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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ferret_catalog::{Catalog, NameId, OpenError, Section, Target};
use ferret_index::{Advance, Certainty, Cursor, DocSet, Pinned};
use ferret_verify::{MatchStats, TextMatcher};

use crate::content::ReadAtom;
use crate::expr::{Node, Test, Truth};
use crate::query::{Cmp, MetaTest, Query};
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
    /// Reader plus engine time for bracketed Maybe verification.
    pub verification_time: Duration,
    /// Time in regex engines, excluding reading and text matching.
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
    /// paths are dropped.
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
        match Self::new_until(catalog, || false)? {
            Some(docs) => Ok(docs),
            None => unreachable!("a build that is never cancelled completes"),
        }
    }

    /// As [`DocNames::new`], `Ok(None)` once `cancelled` answers true. It is
    /// asked at entry, after loading, after every [`DocSet::CHECK_EVERY`]
    /// names each name pass reads, every [`DocSet::CHECK_EVERY`] ids of the
    /// prefix pass, and on each side of copying the offsets. A host that
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
        // False when cancelled before every name was seen.
        let each = |f: &mut dyn FnMut(usize, NameId)| {
            let names = catalog.name_reader().runs_from(NameId(0));
            for (seen, (id, name)) in names.enumerate() {
                if (seen + 1).is_multiple_of(DocSet::CHECK_EVERY) && cancelled() {
                    return false;
                }
                if let Target::Inode(inode) = name.target()
                    && catalog.is_live_name(id)
                    && catalog.is_live_inode(inode)
                    && let Some(doc) = catalog.doc(inode)
                    && (doc.0 as usize) < bound
                {
                    f(doc.0 as usize, id);
                }
            }
            true
        };
        if !each(&mut |doc, _| starts[doc + 1] += 1) {
            return Ok(None);
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
        let filled = each(&mut |doc, id| {
            names[fill[doc] as usize] = id;
            fill[doc] += 1;
        });
        Ok(filled.then_some(Self { starts, names }))
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
        };
        catalog.load(&self.residual_sections())?;
        let mut matcher = TextMatcher::new();
        let mut bytes = Vec::new();
        // Only content facts established by a bracketed catalog-version read.
        // Path failures never enter this map; every path has its own stat.
        let mut exact: HashMap<u32, Vec<Option<usize>>> = HashMap::new();
        let mut maybe_docs = HashSet::new();
        let mut possible_docs = HashSet::new();
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
            if let Some(doc) = doc {
                if truth != Truth::No {
                    possible_docs.insert(doc);
                }
                if truth == Truth::Maybe {
                    maybe_docs.insert(doc);
                }
            }
            let mut offset = None;
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
                    let verification_start = Instant::now();
                    report.verified += 1;
                    let Some(version) = read(row.name, ReadRequest::Bytes(&mut bytes)) else {
                        report.verification_time += verification_start.elapsed();
                        report.changed += 1;
                        return ControlFlow::Continue(());
                    };
                    let answers: Option<Vec<Option<usize>>> = self
                        .atoms
                        .iter()
                        .map(|text| {
                            if stopped() {
                                return None;
                            }
                            let engine_start = Instant::now();
                            let answer = text.find(&mut matcher, &bytes);
                            if matches!(text, crate::ContentAtom::Grep(_)) {
                                report.regex_time += engine_start.elapsed();
                                report.matching.regex_documents += 1;
                                report.matching.regex_bytes += bytes.len() as u64;
                                report.matching.regex_rejected += u64::from(answer.is_none());
                            }
                            (!stopped()).then_some(answer)
                        })
                        .collect();
                    let Some(answers) = answers else {
                        report.verification_time += verification_start.elapsed();
                        return ControlFlow::Break(());
                    };
                    report.verification_time += verification_start.elapsed();
                    match version {
                        ReadVersion::Catalogued => exact.entry(doc).or_insert(answers),
                        ReadVersion::Current => {
                            report.changed += 1;
                            current = answers;
                            &current
                        }
                    }
                };
                offset = answers
                    .iter()
                    .zip(&self.atoms)
                    .filter_map(|(answer, atom)| {
                        if matches!(atom, crate::ContentAtom::Grep(_)) {
                            *answer
                        } else {
                            None
                        }
                    })
                    .min();
                truth = self.eval_residual(&mut |test| match test {
                    Test::Content(i) => answers[*i].is_some().into(),
                    other => self.test_row(catalog, row, other),
                });
            }
            if truth != Truth::Yes {
                return ControlFlow::Continue(());
            }
            rows += 1;
            let row = Row { offset, ..*row };
            emit(&row)
        })?;
        stats.rows = rows;
        report.snapshot_maybe = maybe_docs.len() as u64;
        report.snapshot_possible = possible_docs.len() as u64;
        let text_stats = matcher.stats();
        report.matching.documents = text_stats.documents;
        report.matching.rejected = text_stats.rejected;
        report.matching.whole = text_stats.whole;
        report.matching.tokenized = text_stats.tokenized;
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
