//! Running a [`Query`] over one catalog generation: loading the sections the
//! strategy needs, finding candidates, testing them, and streaming one
//! [`Row`] per path (D15).

use std::fmt;
use std::ops::ControlFlow;

use ferret_catalog::{Catalog, InoId, Inode, Kind, NameId, OpenError, Section};

use crate::query::{MetaTest, NameTest, Query, Strategy};

/// One result: a path, and what the catalog knows about what it names.
#[derive(Clone, Copy, Debug)]
pub struct Row<'a> {
    /// The full path: the root's path, then each name below it.
    pub path: &'a [u8],
    /// The name edge that matched.
    pub name: NameId,
    /// The inode it names; valid in this generation only (D27).
    pub inode: InoId,
    /// Directory, file or symlink.
    pub kind: Kind,
    /// The inode row: `stat`, content state, and its document, if any.
    pub meta: Inode,
}

/// What a run did, for the query log and for tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Names tested: heap hits for a heap scan, names whose inode passed
    /// for an inode scan, every name otherwise.
    pub candidates: u64,
    /// Rows emitted.
    pub rows: u64,
    /// Inode rows read one at a time rather than with their section.
    pub single_inode_reads: u64,
}

/// A run that could not read the catalog.
#[derive(Debug)]
pub struct RunError(pub OpenError);

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for RunError {}

impl From<OpenError> for RunError {
    fn from(e: OpenError) -> Self {
        RunError(e)
    }
}

/// What a row needs besides the names: paths, whether a directory is
/// structural (D29), and link rows to tell a symlink from a file. All are
/// small beside the heap and the inode rows.
const ROW_SECTIONS: [Section; 6] = [
    Section::Names,
    Section::NameHeap,
    Section::DirNames,
    Section::Roots,
    Section::Traversed,
    Section::Links,
];

impl Query {
    /// Runs the query, calling `emit` with each row, in name order (parent
    /// directory, then name). `emit` returns `Break` to stop early; the path
    /// it is lent is valid only for the call.
    ///
    /// Loads only what the strategy needs: a name query never reads the
    /// document rows, and reads inode rows singly until that costs more than
    /// the section (see [`Catalog::read_inode`]).
    pub fn run(
        &self,
        catalog: &Catalog,
        mut emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<Stats, RunError> {
        catalog.load(&ROW_SECTIONS)?;
        let mut run = Run {
            query: self,
            catalog,
            inodes: Inodes::new(catalog),
            stats: Stats::default(),
            dir: None,
            path: Vec::new(),
        };
        match self.strategy() {
            Strategy::HeapScan => run.heap_scan(&mut emit)?,
            Strategy::InodeScan => run.inode_scan(&mut emit)?,
            Strategy::AllNames => run.all_names(&mut emit)?,
        }
        run.stats.single_inode_reads = run.inodes.singles;
        Ok(run.stats)
    }
}

struct Run<'q, 'c> {
    query: &'q Query,
    catalog: &'c Catalog,
    inodes: Inodes<'c>,
    stats: Stats,
    /// The directory whose path `path` starts with, and that prefix's
    /// length. Names arrive grouped by parent, so a directory's path is
    /// resolved once for all its hits.
    dir: Option<(InoId, usize)>,
    path: Vec<u8>,
}

impl Run<'_, '_> {
    fn heap_scan(
        &mut self,
        emit: &mut impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<(), RunError> {
        let query = self.query;
        let Some(driver) = &query.driver else {
            return Ok(());
        };
        let catalog = self.catalog;
        let heap = catalog.name_heap();
        let count = catalog.name_count();
        let skip = matches!(query.names.get(driver.from), Some(NameTest::Substring(_)))
            .then_some(driver.from);
        let (mut from, mut next) = (0, 0);
        while let Some(hit) = driver.finder.find_from(heap, from) {
            let id = locate(catalog, hit, next);
            next = id.0 + 1;
            // One test per name however often it matches.
            from = if next < count {
                catalog.name_start(NameId(next))
            } else {
                heap.len()
            };
            self.stats.candidates += 1;
            if self.consider(id, skip, None, emit)?.is_break() {
                break;
            }
        }
        Ok(())
    }

    fn inode_scan(
        &mut self,
        emit: &mut impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<(), RunError> {
        let catalog = self.catalog;
        catalog.load(&[Section::Inodes, Section::States])?;
        let mut pass = vec![0u64; (catalog.inode_count() as usize).div_ceil(64)];
        for id in 0..catalog.inode_count() {
            if self.meta_passes(InoId(id), &catalog.inode(InoId(id))) {
                pass[id as usize / 64] |= 1 << (id % 64);
            }
        }
        for id in (0..catalog.name_count()).map(NameId) {
            let child = catalog.child(id).0;
            if pass[child as usize / 64] >> (child % 64) & 1 == 1 {
                self.stats.candidates += 1;
                let meta = catalog.inode(InoId(child));
                if self.consider(id, None, Some(meta), emit)?.is_break() {
                    break;
                }
            }
        }
        Ok(())
    }

    fn all_names(
        &mut self,
        emit: &mut impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<(), RunError> {
        for id in (0..self.catalog.name_count()).map(NameId) {
            self.stats.candidates += 1;
            if self.consider(id, None, None, emit)?.is_break() {
                break;
            }
        }
        Ok(())
    }

    /// Tests one name and emits it if everything passes. Cheapest first:
    /// name bytes, then the inode row, then the path. `skip` is a name test
    /// the candidate source already proved; `meta` an inode row it already
    /// read and tested.
    fn consider(
        &mut self,
        id: NameId,
        skip: Option<usize>,
        meta: Option<Inode>,
        emit: &mut impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> RunResult {
        let catalog = self.catalog;
        let name = catalog.name(id);
        let structural = name.child.0 < catalog.dir_count() && catalog.is_traversed(name.child);
        let names_pass = self
            .query
            .names
            .iter()
            .enumerate()
            .all(|(i, test)| Some(i) == skip || test.matches(name.bytes));
        if structural || !names_pass {
            return Ok(ControlFlow::Continue(()));
        }
        let meta = match meta {
            Some(meta) => meta,
            None => {
                let meta = self.inodes.get(name.child)?;
                if !self.meta_passes(name.child, &meta) {
                    return Ok(ControlFlow::Continue(()));
                }
                meta
            }
        };
        self.resolve(name.parent, name.bytes);
        if !self.query.paths.iter().all(|t| t.matches(&self.path)) {
            return Ok(ControlFlow::Continue(()));
        }
        self.stats.rows += 1;
        Ok(emit(&Row {
            path: &self.path,
            name: id,
            inode: name.child,
            kind: catalog.kind(name.child),
            meta,
        }))
    }

    fn meta_passes(&self, id: InoId, meta: &Inode) -> bool {
        self.query.meta.iter().all(|test| match *test {
            MetaTest::Size(cmp, n) => cmp.holds(meta.stat.size, n),
            MetaTest::Age(cmp, secs) => cmp.holds(self.query.now - meta.stat.mtime_sec, secs),
            MetaTest::Type(kind) => self.catalog.kind(id) == kind,
        })
    }

    /// Sets `path` to `parent`'s path plus `name`.
    fn resolve(&mut self, parent: InoId, name: &[u8]) {
        let prefix = match self.dir {
            Some((dir, len)) if dir == parent => len,
            _ => {
                self.path.clear();
                self.catalog.dir_path(parent, &mut self.path);
                if self.path.last() != Some(&b'/') {
                    self.path.push(b'/');
                }
                self.dir = Some((parent, self.path.len()));
                self.path.len()
            }
        };
        self.path.truncate(prefix);
        self.path.extend_from_slice(name);
    }
}

type RunResult = Result<ControlFlow<()>, RunError>;

/// The name holding heap offset `hit`, searching forwards from `from`:
/// hits arrive in heap order, so a gallop from the last hit's successor
/// costs O(log gap) rather than a binary search over every name.
fn locate(catalog: &Catalog, hit: usize, from: u32) -> NameId {
    let count = catalog.name_count();
    let starts_by = |id: u32| catalog.name_start(NameId(id)) <= hit;
    // Invariant: `lo` starts at or before `hit`; `hi` is past it or `count`.
    let (mut lo, mut step) = (from, 1);
    let mut hi = from + 1;
    while hi < count && starts_by(hi) {
        lo = hi;
        hi = hi.saturating_add(step).min(count);
        step *= 2;
    }
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if starts_by(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    NameId(lo)
}

/// Inode rows for results: read one at a time while that is cheaper than
/// the whole section, then the section.
struct Inodes<'c> {
    catalog: &'c Catalog,
    singles: u64,
    /// Single reads before the section is loaded instead: a 64th of the
    /// rows. A single read costs a syscall per row where the section costs
    /// a copy per byte; at this count the two are about equal on the
    /// measured machine (see the bench's `name + metadata` rows).
    limit: u64,
}

impl<'c> Inodes<'c> {
    fn new(catalog: &'c Catalog) -> Self {
        Self {
            catalog,
            singles: 0,
            limit: (u64::from(catalog.inode_count()) / 64).max(256),
        }
    }

    fn get(&mut self, id: InoId) -> Result<Inode, OpenError> {
        if !self.catalog.is_loaded(Section::Inodes) {
            if self.singles < self.limit {
                self.singles += 1;
                return self.catalog.read_inode(id);
            }
            self.catalog.load(&[Section::Inodes, Section::States])?;
        }
        Ok(self.catalog.inode(id))
    }
}
