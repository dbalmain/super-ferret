//! Running a [`Query`] over one catalog generation: loading the sections the
//! strategy needs, finding candidates, testing them, and streaming one
//! [`Row`] per path (D15).

use std::fmt;
use std::ops::ControlFlow;

use ferret_catalog::{Catalog, InoId, Kind, Kinds, Name, NameId, OpenError, RUN, Section};

use crate::query::{Cmp, MetaTest, NameTest, Query, Strategy};

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
/// loads this set before its first `consider`; the inode scan loads each
/// metadata test's sections as that test's pass begins, and none after a
/// pass leaves nothing, and a name-driven strategy loads
/// [`Query::meta_sections`] when the first name passes its name tests.
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
            up: None,
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
    /// `dir`'s parent and the length of its path, with a trailing `/`, at
    /// the start of `path`. Directories are numbered breadth first, so the
    /// next directory is most often a sibling of the last, and its path is
    /// this plus its name rather than a walk to the root.
    up: Option<(InoId, usize)>,
    path: Vec<u8>,
}

impl<'c> Run<'_, 'c> {
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
        let (heap, names, mut kinds) =
            (catalog.name_heap(), catalog.name_reader(), catalog.kinds());
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
            let name = names.get(id);
            if self
                .consider(&mut kinds, id, name, skip, false, emit)?
                .is_break()
            {
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
        let pass = self.meta_pass()?;
        // Nothing passed: no name can, so the name sections stay on disk.
        if pass.iter().all(|&word| word == 0) {
            return Ok(());
        }
        catalog.load(&ROW_SECTIONS)?;
        let (names, mut kinds) = (catalog.name_reader(), catalog.kinds());
        for (id, child) in names.children().enumerate() {
            let child = child.0;
            if child < catalog.inode_count() && pass[child as usize / 64] >> (child % 64) & 1 == 1 {
                self.stats.candidates += 1;
                let id = NameId(id as u32);
                if self
                    .consider(&mut kinds, id, names.get(id), None, true, emit)?
                    .is_break()
                {
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
        let catalog = self.catalog;
        catalog.load(&ROW_SECTIONS)?;
        let (names, mut kinds) = (catalog.name_reader(), catalog.kinds());
        for (id, name) in names.runs_from(NameId(0)) {
            self.stats.candidates += 1;
            if self
                .consider(&mut kinds, id, name, None, false, emit)?
                .is_break()
            {
                break;
            }
        }
        Ok(())
    }

    /// Tests one name, already read, and emits it if everything passes.
    /// Cheapest first: name bytes, then the inode's fields, then the path.
    /// `skip` is a name test the candidate source already proved; `tested`
    /// says the source already tested the inode.
    fn consider(
        &mut self,
        kinds: &mut Kinds<'c>,
        id: NameId,
        name: Name<'_>,
        skip: Option<usize>,
        tested: bool,
        emit: &mut impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> RunResult {
        let catalog = self.catalog;
        // Validated children are either real inode ids or ignored type tags.
        if name.child.0 >= catalog.inode_count() {
            return Ok(ControlFlow::Continue(()));
        }
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
        // Search keeps its existing file/directory/symlink result domain;
        // the catalog's special entries are available to the find source.
        let kind = kinds.kind(name.child);
        if !matches!(kind, Kind::Dir | Kind::File | Kind::Symlink) {
            return Ok(ControlFlow::Continue(()));
        }
        if !tested && !self.query.meta.is_empty() {
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
            kind,
        }))
    }

    /// Whether an inode passes every metadata test, read one field at a time
    /// from the sections [`Query::meta_sections`] names: for a sparse hit.
    fn meta_passes(&self, id: InoId) -> bool {
        let catalog = self.catalog;
        self.query.meta.iter().all(|test| match *test {
            MetaTest::Size(cmp, n) => cmp.holds(catalog.size(id), n),
            MetaTest::Age(cmp, secs) => self.age_holds(cmp, secs, catalog.mtime(id)),
            MetaTest::Type(kind) => catalog.kind(id) == kind,
        })
    }

    /// [`Run::meta_passes`] of every inode, as a bitset by `InoId`. Each test
    /// is a pass over its column, loading its sections as the pass begins
    /// and decoding only the runs of 64 inodes that still have a bit set; a
    /// pass that clears every bit ends the conjunction, so a later test's
    /// sections stay on disk.
    fn meta_pass(&self) -> Result<Vec<u64>, OpenError> {
        let catalog = self.catalog;
        let n = catalog.inode_count() as usize;
        let mut pass = vec![!0; n.div_ceil(64)];
        if let Some(last) = pass.last_mut()
            && !n.is_multiple_of(64)
        {
            *last = (1 << (n % 64)) - 1;
        }
        let (mut sizes, mut times) = ([0; RUN], [0; RUN]);
        for test in &self.query.meta {
            if pass.iter().all(|&word| word == 0) {
                break;
            }
            catalog.load(test.sections())?;
            match *test {
                MetaTest::Size(cmp, v) => and_runs(&mut pass, |run, _| {
                    word(
                        catalog
                            .size_run(run, &mut sizes)
                            .iter()
                            .map(|&s| cmp.holds(s, v)),
                    )
                }),
                MetaTest::Age(cmp, secs) => and_runs(&mut pass, |run, _| {
                    let times = catalog.mtime_run(run, &mut times);
                    word(times.iter().map(|&t| self.age_holds(cmp, secs, t)))
                }),
                MetaTest::Type(kind) => {
                    let mut kinds = catalog.kinds();
                    and_runs(&mut pass, |run, live| {
                        let mut out = 0;
                        let mut rest = live;
                        while rest != 0 {
                            let bit = rest.trailing_zeros();
                            rest &= rest - 1;
                            let id = InoId((run * RUN) as u32 + bit);
                            out |= u64::from(kinds.kind(id) == kind) << bit;
                        }
                        out
                    });
                }
            }
        }
        Ok(pass)
    }

    /// Whether a file modified at `mtime` is `cmp` `secs` old. Widened: mtime
    /// is whatever the file holds, and a corrupt or far-future value must not
    /// overflow.
    fn age_holds(&self, cmp: Cmp, secs: i64, mtime: i64) -> bool {
        cmp.holds(
            i128::from(self.query.now) - i128::from(mtime),
            i128::from(secs),
        )
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
            _ => self.enter(parent),
        };
        self.path.truncate(prefix);
        self.path.extend_from_slice(name);
    }

    /// Sets `path` to `dir`'s path and a `/`, and returns its length.
    fn enter(&mut self, dir: InoId) -> usize {
        let catalog = self.catalog;
        let edge = catalog.dir_name(dir).map(|edge| catalog.name(edge));
        match (edge, self.up) {
            (Some(edge), Some((up, len))) if up == edge.parent => self.path.truncate(len),
            (Some(edge), _) => {
                self.path.clear();
                catalog.dir_path(edge.parent, &mut self.path);
                push_slash(&mut self.path);
                self.up = Some((edge.parent, self.path.len()));
            }
            (None, _) => {
                self.path.clear();
                self.up = None;
            }
        }
        match edge {
            Some(edge) => self.path.extend_from_slice(edge.bytes),
            None => catalog.dir_path(dir, &mut self.path),
        }
        push_slash(&mut self.path);
        self.dir = Some((dir, self.path.len()));
        self.path.len()
    }
}

fn push_slash(path: &mut Vec<u8>) {
    if path.last() != Some(&b'/') {
        path.push(b'/');
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

/// ANDs into each word of the bitset `pass` that still has a bit set the
/// word `test(run, word)` returns for its run of 64 inodes; a cleared word's
/// run is never decoded.
fn and_runs(pass: &mut [u64], mut test: impl FnMut(usize, u64) -> u64) {
    for (run, word) in pass.iter_mut().enumerate() {
        if *word != 0 {
            *word &= test(run, *word);
        }
    }
}

/// A word with bit `i` set when the `i`th of up to 64 `bits` is true.
fn word(bits: impl Iterator<Item = bool>) -> u64 {
    bits.enumerate()
        .fold(0, |word, (i, bit)| word | u64::from(bit) << i)
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
