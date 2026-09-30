//! Running a [`Query`] over one catalog generation: loading the sections the
//! strategy needs, finding candidates, testing them, and streaming one
//! [`Row`] per path (D15).

use std::fmt;
use std::ops::ControlFlow;

use ferret_catalog::{Catalog, InoId, Kind, NameId, OpenError, Section};

use crate::query::{MetaTest, NameTest, Query, Strategy};

/// One result: a path, and the ids to read anything else about it from the
/// catalog.
///
/// A row carries no metadata: a caller that prints some loads those fields'
/// sections before the run and reads them by [`Row::inode`]. Decoding every
/// field for every row would cost a plain path listing all the inode columns
/// (283 MB at 10M names) for fields it never prints.
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
}

/// What a run did, for the query log and for tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Names tested: heap hits for a heap scan, names whose inode passed
    /// for an inode scan, every name otherwise.
    pub candidates: u64,
    /// Rows emitted.
    pub rows: u64,
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

/// What testing and emitting a name needs, by the accessor that reads it:
/// `name`, `name_start` and `child` read Names and NameHeap; `dir_path`
/// reads DirNames, Names and Roots (Roots loads Strings); `is_traversed`
/// reads Traversed; `kind`, for a row and for `type:`, reads Links (which
/// loads Strings). All of these are small beside the heap. Every strategy
/// loads this set before its first `consider`; the inode scan loads its own
/// set, [`Query::meta_sections`], before its first metadata test, and a
/// name-driven strategy loads it when the first name passes its name tests.
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
    /// document rows or an inode section; a metadata test reads the one
    /// field it needs, and a metadata-only query reads the name sections only
    /// once an inode has passed.
    pub fn run(
        &self,
        catalog: &Catalog,
        mut emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<Stats, RunError> {
        let mut run = Run {
            query: self,
            catalog,
            meta_loaded: false,
            stats: Stats::default(),
            dir: None,
            path: Vec::new(),
        };
        match self.strategy() {
            Strategy::HeapScan => run.heap_scan(&mut emit)?,
            Strategy::InodeScan => run.inode_scan(&mut emit)?,
            Strategy::AllNames => run.all_names(&mut emit)?,
        }
        Ok(run.stats)
    }
}

struct Run<'q, 'c> {
    query: &'q Query,
    catalog: &'c Catalog,
    /// Whether [`Run::load_meta`] has loaded the metadata tests' sections.
    meta_loaded: bool,
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
        catalog.load(&ROW_SECTIONS)?;
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
            if self.consider(id, skip, false, emit)?.is_break() {
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
        catalog.load(&self.query.meta_sections())?;
        let mut pass = vec![0u64; (catalog.inode_count() as usize).div_ceil(64)];
        let mut any = false;
        for id in 0..catalog.inode_count() {
            if self.meta_passes(InoId(id)) {
                pass[id as usize / 64] |= 1 << (id % 64);
                any = true;
            }
        }
        // Nothing passed: no name can, so the name sections stay on disk.
        if !any {
            return Ok(());
        }
        catalog.load(&ROW_SECTIONS)?;
        for id in (0..catalog.name_count()).map(NameId) {
            let child = catalog.child(id).0;
            if pass[child as usize / 64] >> (child % 64) & 1 == 1 {
                self.stats.candidates += 1;
                if self.consider(id, None, true, emit)?.is_break() {
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
        self.catalog.load(&ROW_SECTIONS)?;
        for id in (0..self.catalog.name_count()).map(NameId) {
            self.stats.candidates += 1;
            if self.consider(id, None, false, emit)?.is_break() {
                break;
            }
        }
        Ok(())
    }

    /// Tests one name and emits it if everything passes. Cheapest first:
    /// name bytes, then the inode's fields, then the path. `skip` is a name
    /// test the candidate source already proved; `tested` says the source
    /// already tested the inode.
    fn consider(
        &mut self,
        id: NameId,
        skip: Option<usize>,
        tested: bool,
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
        if !tested {
            self.load_meta()?;
            if !self.meta_passes(name.child) {
                return Ok(ControlFlow::Continue(()));
            }
        }
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
        }))
    }

    /// Whether an inode passes every metadata test, read one field at a time
    /// from the sections [`Query::meta_sections`] names.
    fn meta_passes(&self, id: InoId) -> bool {
        let catalog = self.catalog;
        self.query.meta.iter().all(|test| match *test {
            MetaTest::Size(cmp, n) => cmp.holds(catalog.size(id), n),
            // Widened: mtime is whatever the file holds, and a corrupt or
            // far-future value must not overflow.
            MetaTest::Age(cmp, secs) => cmp.holds(
                i128::from(self.query.now) - i128::from(catalog.mtime(id)),
                i128::from(secs),
            ),
            MetaTest::Type(kind) => catalog.kind(id) == kind,
        })
    }

    /// Loads the sections the metadata tests read, once, before the first test.
    fn load_meta(&mut self) -> Result<(), OpenError> {
        if !self.meta_loaded {
            self.catalog.load(&self.query.meta_sections())?;
            self.meta_loaded = true;
        }
        Ok(())
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

impl Query {
    /// What testing inodes against the metadata atoms reads: whatever each
    /// test declares.
    fn meta_sections(&self) -> Vec<Section> {
        let mut sections = Vec::new();
        for test in &self.meta {
            for &section in test.sections() {
                if !sections.contains(&section) {
                    sections.push(section);
                }
            }
        }
        sections
    }
}

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
