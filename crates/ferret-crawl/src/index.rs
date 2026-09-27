//! One run of `ferret index`: walk the roots that need it, hash what changed,
//! and publish a new catalog generation, or nothing (D26 A′, D31, D33, D34,
//! D37).
//!
//! ```text
//! begin          take the writer lock, read the old generation
//! plan           which roots to walk, keep and drop; widen for overlaps
//! walk + hash    per refreshed root, walk_parallel with a hashing visitor
//! resolve        aliases that met an inode in flight take its observation
//! add, keep      hand the batches over, copy the kept roots forward
//! commit         unless a coverage fault was seen
//! ```
//!
//! A **coverage fault** is any [`Event::Io`] except an entry's `lstat`
//! NotFound, which is a deletion. It means the walk may have missed entries
//! or applied the wrong ignore rules, so nothing is published and the old
//! generation stays. A **content fault** leaves the namespace intact: the file
//! is published with [`ContentState::Fault`](ferret_catalog::ContentState) and
//! no document, and the next run reads it again.

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use ferret_catalog::{
    BeginError, Catalog, CommitError, Content, DirToken, KeepError, Stat, Transaction,
};
use ferret_policy::{Config, Decision, Reason};

use crate::observe::{self, Cache, ContentFault, Lookup, Observation, Opened, Reader};
use crate::walk::{
    Boundary, Decided, Event, EventVisitor, FaultContext, IoOp, WalkOptions, WorkTreeKind,
    walk_parallel,
};

/// Which configured roots to walk this run. The rest are copied forward from
/// the previous generation where that is still valid.
#[derive(Clone, Copy, Debug)]
pub enum Refresh<'a> {
    /// Walk every root.
    All,
    /// Walk these roots, each of which must be configured, plus any root the
    /// previous generation lacks and any root that overlaps a changed one.
    Only(&'a [PathBuf]),
}

/// How [`index`] runs.
#[derive(Clone, Debug)]
pub struct IndexOptions {
    /// The global ignore file's text, if there is one.
    pub global: Option<String>,
    /// Policy settings.
    pub config: Config,
    /// Walk workers per root; zero means one.
    pub workers: usize,
    /// The sniffer's version. A change refreshes every root (D37).
    pub sniffer: u32,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            global: None,
            config: Config::default(),
            workers: crate::default_workers(),
            sniffer: ferret_policy::SNIFFER_VERSION,
        }
    }
}

/// A fault that stopped publication.
#[derive(Debug)]
pub struct CoverageFault {
    /// The root being walked.
    pub root: PathBuf,
    /// Root-relative path of the entry or directory.
    pub path: PathBuf,
    /// What failed.
    pub op: IoOp,
    /// Whether it was on the root itself.
    pub on_root: bool,
    /// The OS error.
    pub error: io::Error,
}

impl fmt::Display for CoverageFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {:?}: {}",
            self.root.join(&self.path).display(),
            self.op,
            self.error
        )
    }
}

/// Why [`index`] published nothing, or published with a caveat.
#[derive(Debug)]
pub enum IndexError {
    /// A root that is not absolute, or has a `..` component.
    BadRoot(PathBuf),
    /// A root to refresh that is not among the configured roots.
    NotConfigured(PathBuf),
    /// The catalog could not be opened for writing: another run holds it
    /// ([`BeginError::Locked`]) or the previous generation is unreadable.
    Begin(BeginError),
    /// A kept root was refused (a bug in the plan, since the plan only keeps
    /// roots the previous generation has, with an unchanged sniffer).
    Keep(KeepError),
    /// The walk may have missed entries. Nothing was published; the report
    /// says what was walked.
    Coverage {
        faults: Vec<CoverageFault>,
        report: Box<Report>,
    },
    /// Commit failed. [`CommitError::published`] says whether the new
    /// generation is visible anyway.
    Commit(CommitError),
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadRoot(p) => write!(f, "root {} must be absolute, without `..`", p.display()),
            Self::NotConfigured(p) => write!(f, "{} is not a configured root", p.display()),
            Self::Begin(e) => write!(f, "{e}"),
            Self::Keep(e) => write!(f, "{e}"),
            Self::Coverage { faults, .. } => {
                write!(
                    f,
                    "catalog not written: {} walk fault(s) may have hidden entries",
                    faults.len()
                )?;
                for fault in faults.iter().take(5) {
                    write!(f, "\n  {fault}")?;
                }
                Ok(())
            }
            Self::Commit(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for IndexError {}

/// Counts from the walk and the hashing visitor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Directories catalogued, roots included.
    pub dirs: u64,
    /// Directories walked through without cataloguing (D29).
    pub traversed: u64,
    /// Regular files catalogued, every name counted.
    pub files: u64,
    /// Symlinks catalogued.
    pub symlinks: u64,
    /// File names the policy sent to the index.
    pub indexed: u64,
    /// Of those, whose content came from the previous generation unread.
    pub carried: u64,
    /// Of those, that took another name's observation from the cache.
    pub aliased: u64,
    /// Of those aliases, that met their inode in flight on another worker
    /// and were recorded after the walk.
    pub deferred: u64,
    /// Of those, published with a content fault.
    pub content_faults: u64,
    /// Files opened and read (sniffed, and hashed unless binary).
    pub files_read: u64,
    /// Bytes read from them.
    pub bytes_read: u64,
    /// Entries gone between listing and `lstat`: deletions, not faults.
    pub vanished: u64,
    /// Inner roots the walk stopped at.
    pub boundaries: u64,
    /// Ignore-file lines dropped as invalid.
    pub pattern_errors: u64,
    /// Inodes the D31 cache held at the end.
    pub cached_inodes: u64,
}

impl Counts {
    fn add(&mut self, other: &Counts) {
        let Counts {
            dirs,
            traversed,
            files,
            symlinks,
            indexed,
            carried,
            aliased,
            deferred,
            content_faults,
            files_read,
            bytes_read,
            vanished,
            boundaries,
            pattern_errors,
            cached_inodes,
        } = *other;
        self.dirs += dirs;
        self.traversed += traversed;
        self.files += files;
        self.symlinks += symlinks;
        self.indexed += indexed;
        self.carried += carried;
        self.aliased += aliased;
        self.deferred += deferred;
        self.content_faults += content_faults;
        self.files_read += files_read;
        self.bytes_read += bytes_read;
        self.vanished += vanished;
        self.boundaries += boundaries;
        self.pattern_errors += pattern_errors;
        self.cached_inodes += cached_inodes;
    }
}

/// What a run did.
#[derive(Debug, Default)]
pub struct Report {
    /// Roots walked this run, sorted.
    pub refreshed: Vec<PathBuf>,
    /// Roots copied forward unchanged, sorted.
    pub kept: Vec<PathBuf>,
    /// Roots the previous generation had and this one does not, sorted.
    pub dropped: Vec<PathBuf>,
    /// Totals over every refreshed root.
    pub counts: Counts,
    /// Each file published unhashed, by absolute path, and why.
    pub content_faults: Vec<(PathBuf, ContentFault)>,
    /// Ignore-file problems, as text.
    pub pattern_errors: Vec<String>,
    /// Walking and hashing, over every refreshed root.
    pub walk_time: Duration,
    /// Keeping roots, building, encoding, writing and syncing.
    pub commit_time: Duration,
    /// The published generation's shape, when one was published.
    pub published: Option<Published>,
}

/// The shape of a published generation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Published {
    pub dirs: u32,
    pub inodes: u32,
    pub names: u32,
    pub docs: u32,
}

impl Published {
    fn of(catalog: &Catalog) -> Self {
        Self {
            dirs: catalog.dir_count(),
            inodes: catalog.inode_count(),
            names: catalog.name_count(),
            docs: catalog.doc_count(),
        }
    }
}

/// Indexes into the catalog in `catalog_dir`.
///
/// `roots` is the complete configured root set after this run: a root the
/// previous generation has and `roots` lacks is dropped. Every root must be
/// absolute and is compared by its lexically normalised path. `refresh` says
/// which to walk; the rest are kept, except where that would be wrong:
///
/// - a root the previous generation lacks is walked;
/// - every root is walked when the sniffer version changed (D37);
/// - a root with another root added or removed strictly inside it is walked,
///   because its kept copy stopped at the old inner roots (D34).
///
/// The writer lock is taken first and held until the commit. A coverage
/// fault anywhere publishes nothing ([`IndexError::Coverage`]).
pub fn index(
    catalog_dir: &Path,
    roots: &[PathBuf],
    refresh: Refresh<'_>,
    options: &IndexOptions,
) -> Result<Report, IndexError> {
    let mut txn = Transaction::begin(catalog_dir, options.sniffer).map_err(IndexError::Begin)?;
    let plan = Plan::new(txn.previous(), roots, refresh, options.sniffer)?;
    let mut report = Report {
        refreshed: plan.refresh.clone(),
        kept: plan.keep.clone(),
        dropped: plan.dropped.clone(),
        ..Report::default()
    };

    let started = Instant::now();
    let cache = Cache::new();
    let mut faults = Vec::new();
    let mut outputs = Vec::new();
    for root in &plan.refresh {
        let walk_options = WalkOptions {
            workers: options.workers,
            boundaries: plan.boundaries(root),
        };
        let visitors = walk_parallel(
            root,
            options.global.as_deref(),
            options.config,
            &walk_options,
            || Hasher::new(&txn, &cache, root),
        );
        for visitor in visitors {
            outputs.push(visitor.finish(&mut faults));
        }
    }
    report.counts.cached_inodes = cache.len() as u64;
    for output in &mut outputs {
        output.resolve(&cache);
    }
    // A clean name of an inode that another name faulted publishes unhashed
    // too, since the build keeps one row per inode (D31): list it.
    let faulted: HashSet<(u64, u64)> = outputs
        .iter()
        .flat_map(|o| o.fault_keys.iter().copied())
        .collect();
    for output in &mut outputs {
        for (key, path) in std::mem::take(&mut output.linked) {
            if faulted.contains(&key) {
                output.counts.content_faults += 1;
                output.content_faults.push((path, ContentFault::Alias));
            }
        }
    }
    for mut output in outputs {
        report.counts.add(&output.counts);
        report.content_faults.append(&mut output.content_faults);
        report.pattern_errors.append(&mut output.pattern_errors);
        txn.add(output.batch);
    }
    drop(cache);
    report.content_faults.sort_by(|a, b| a.0.cmp(&b.0));
    report.walk_time = started.elapsed();

    if !faults.is_empty() {
        return Err(IndexError::Coverage {
            faults,
            report: Box::new(report),
        });
    }

    let started = Instant::now();
    for root in &plan.keep {
        txn.keep(root.as_os_str().as_bytes())
            .map_err(IndexError::Keep)?;
    }
    let catalog = txn.commit().map_err(IndexError::Commit)?;
    report.commit_time = started.elapsed();
    report.published = Some(Published::of(&catalog));
    Ok(report)
}

/// Which roots a run walks, keeps and drops.
struct Plan {
    /// Every configured root, normalised and sorted.
    roots: Vec<PathBuf>,
    refresh: Vec<PathBuf>,
    keep: Vec<PathBuf>,
    dropped: Vec<PathBuf>,
}

impl Plan {
    fn new(
        previous: Option<&Catalog>,
        roots: &[PathBuf],
        refresh: Refresh<'_>,
        sniffer: u32,
    ) -> Result<Plan, IndexError> {
        let mut configured = roots
            .iter()
            .map(|r| normalise(r))
            .collect::<Result<Vec<_>, _>>()?;
        configured.sort();
        configured.dedup();
        let old: Vec<PathBuf> = previous
            .map(|p| {
                p.roots()
                    .map(|(_, path)| PathBuf::from(OsStr::from_bytes(path)))
                    .collect()
            })
            .unwrap_or_default();
        let everything = matches!(refresh, Refresh::All)
            || previous.is_some_and(|p| p.sniffer_version() != sniffer);
        let named = match refresh {
            Refresh::All => Vec::new(),
            Refresh::Only(paths) => {
                let mut named = Vec::with_capacity(paths.len());
                for path in paths {
                    let path = normalise(path)?;
                    if configured.binary_search(&path).is_err() {
                        return Err(IndexError::NotConfigured(path));
                    }
                    named.push(path);
                }
                named
            }
        };
        // Roots added or removed this run: a kept root with one of these
        // strictly inside it would copy a subtree whose boundaries changed.
        let changed: Vec<&PathBuf> = configured
            .iter()
            .filter(|r| !old.contains(r))
            .chain(old.iter().filter(|r| configured.binary_search(r).is_err()))
            .collect();
        let (mut refresh, mut keep) = (Vec::new(), Vec::new());
        for root in &configured {
            let walk = everything
                || named.contains(root)
                || !old.contains(root)
                || changed.iter().any(|c| strictly_inside(c, root));
            if walk {
                refresh.push(root.clone());
            } else {
                keep.push(root.clone());
            }
        }
        let dropped = old
            .into_iter()
            .filter(|r| configured.binary_search(r).is_err())
            .collect();
        Ok(Plan {
            roots: configured,
            refresh,
            keep,
            dropped,
        })
    }

    /// The configured roots strictly inside `root`, relative to it: where its
    /// walk stops, because the innermost root owns its subtree (D34).
    fn boundaries(&self, root: &Path) -> Vec<Boundary> {
        self.roots
            .iter()
            .filter(|inner| strictly_inside(inner, root))
            .filter_map(|inner| inner.strip_prefix(root).ok())
            .map(|rel| Boundary {
                path: rel.to_owned(),
                id: None,
            })
            .collect()
    }
}

/// Whether `inner` lies strictly below `outer`, by whole components.
fn strictly_inside(inner: &Path, outer: &Path) -> bool {
    inner != outer && inner.starts_with(outer)
}

/// `root` as the catalog names it: absolute, with `.` components and
/// trailing slashes gone. A `..` is refused rather than resolved, since
/// resolving it lexically can name a different directory than the kernel
/// would.
fn normalise(root: &Path) -> Result<PathBuf, IndexError> {
    if !root.is_absolute() || root.components().any(|c| c == Component::ParentDir) {
        return Err(IndexError::BadRoot(root.to_owned()));
    }
    Ok(root.components().collect())
}

/// A name of a multiply-linked inode that was in flight on another worker
/// when this one reached it; recorded once the walk is over.
struct Deferred {
    parent: DirToken,
    name: Vec<u8>,
    stat: Stat,
    path: PathBuf,
}

/// The per-worker visitor: fills one batch and reads the files it must.
struct Hasher<'a> {
    txn: &'a Transaction,
    cache: &'a Cache,
    root: &'a Path,
    out: Output,
    reader: Reader,
}

/// What a worker leaves behind.
struct Output {
    batch: ferret_catalog::Batch,
    counts: Counts,
    faults: Vec<CoverageFault>,
    content_faults: Vec<(PathBuf, ContentFault)>,
    pattern_errors: Vec<String>,
    deferred: Vec<Deferred>,
    /// `(dev, ino)` of every name recorded as a content fault.
    fault_keys: Vec<(u64, u64)>,
    /// Names of multiply-linked inodes recorded clean. If another name of
    /// the same inode faulted, the build faults the inode and this name
    /// publishes unhashed too, so it is reported after the walk.
    linked: Vec<((u64, u64), PathBuf)>,
}

impl<'a> Hasher<'a> {
    fn new(txn: &'a Transaction, cache: &'a Cache, root: &'a Path) -> Self {
        Self {
            txn,
            cache,
            root,
            out: Output {
                batch: txn.batch(),
                counts: Counts::default(),
                faults: Vec::new(),
                content_faults: Vec::new(),
                pattern_errors: Vec::new(),
                deferred: Vec::new(),
                fault_keys: Vec::new(),
                linked: Vec::new(),
            },
            reader: Reader::new(),
        }
    }

    fn finish(mut self, faults: &mut Vec<CoverageFault>) -> Output {
        self.out.counts.files_read = self.reader.files_read;
        self.out.counts.bytes_read = self.reader.bytes_read;
        faults.append(&mut self.out.faults);
        self.out
    }

    /// A file the policy sends to the index: carried, taken from another
    /// name's observation, read, or deferred.
    fn index_file(&mut self, decided: &Decided<'_, DirToken>, stat: Stat) {
        let out = &mut self.out;
        out.counts.indexed += 1;
        // An old `Unindexed` means the policy did not send it to the index
        // then; it does now, so it must be read (D37).
        match self.txn.carry(&stat) {
            Some(Content::Unindexed) | None => {}
            Some(content) => {
                out.counts.carried += 1;
                out.batch
                    .file(decided.parent, decided.name.as_bytes(), stat, content);
                return;
            }
        }
        let (file, links) = match Reader::open(decided.parent_fd, decided.name, &stat) {
            Opened::Ready { file, links } => (file, links),
            Opened::Fault(fault) => {
                self.record(decided, stat, Err(fault), 0);
                return;
            }
        };
        let key = (stat.dev, stat.ino);
        if links > 1 {
            match self.cache.claim(key) {
                Lookup::Claimed => {}
                Lookup::InFlight => {
                    self.out.counts.deferred += 1;
                    self.out.deferred.push(Deferred {
                        parent: decided.parent,
                        name: decided.name.as_bytes().to_vec(),
                        stat,
                        path: self.root.join(decided.path),
                    });
                    #[cfg(test)]
                    hook(self.root, Probe::Deferred);
                    return;
                }
                Lookup::Done(stored) => {
                    self.out.counts.aliased += 1;
                    let (stat, content) = observe::consume(stored, &stat);
                    self.record(decided, stat, content, links);
                    return;
                }
            }
            #[cfg(test)]
            hook(self.root, Probe::Claimed);
        }
        let mut file = file;
        let content = self.reader.read(&mut file);
        #[cfg(test)]
        hook(self.root, Probe::Hashed(decided.path));
        let content = content.and_then(|c| observe::bracket(&file, &stat, c));
        if links > 1 {
            let observation = Observation {
                stat,
                content: content.as_ref().ok().copied(),
            };
            self.cache.complete(key, observation);
        }
        #[cfg(test)]
        hook(self.root, Probe::Read(decided.path));
        self.record(decided, stat, content, links);
    }

    /// Records a read or aliased file. `links` is its `st_nlink`, or 0 when
    /// the open failed before it was known.
    fn record(
        &mut self,
        decided: &Decided<'_, DirToken>,
        stat: Stat,
        content: Result<Content, ContentFault>,
        links: u64,
    ) {
        let path = self.root.join(decided.path);
        let content = self.out.outcome(path, stat, content, links);
        self.out
            .batch
            .file(decided.parent, decided.name.as_bytes(), stat, content);
    }

    fn fault(&mut self, path: &Path, op: IoOp, on_root: bool, error: io::Error) {
        self.out.faults.push(CoverageFault {
            root: self.root.to_owned(),
            path: path.to_owned(),
            op,
            on_root,
            error,
        });
    }
}

impl Output {
    /// Records the deferred aliases from their inodes' finished observations.
    /// The content to publish for one name, noting what the report needs:
    /// a fault is listed and its inode noted, and a clean name of a
    /// multiply-linked inode is kept in case another name faults it.
    fn outcome(
        &mut self,
        path: PathBuf,
        stat: Stat,
        content: Result<Content, ContentFault>,
        links: u64,
    ) -> Content {
        let key = (stat.dev, stat.ino);
        match content {
            Ok(content) => {
                if links > 1 {
                    self.linked.push((key, path));
                }
                content
            }
            Err(fault) => {
                self.counts.content_faults += 1;
                self.content_faults.push((path, fault));
                self.fault_keys.push(key);
                Content::Fault
            }
        }
    }

    fn resolve(&mut self, cache: &Cache) {
        for alias in std::mem::take(&mut self.deferred) {
            self.counts.aliased += 1;
            let key = (alias.stat.dev, alias.stat.ino);
            let (stat, content) = match cache.finished(key) {
                Some(stored) => observe::consume(stored, &alias.stat),
                None => (alias.stat, Err(ContentFault::Alias)),
            };
            // A deferred name met its inode in the cache, so it has more
            // than one link.
            let content = self.outcome(alias.path, stat, content, 2);
            self.batch.file(alias.parent, &alias.name, stat, content);
        }
    }
}

impl EventVisitor for Hasher<'_> {
    type Dir = DirToken;

    fn root(&mut self, stat: crate::Stat<'_>) -> DirToken {
        self.out.counts.dirs += 1;
        self.out
            .batch
            .root(self.root.as_os_str().as_bytes(), observe::from_walk(&stat))
    }

    fn visit(&mut self, event: Event<'_, DirToken>) -> Option<DirToken> {
        match event {
            Event::Decided(decided) => {
                let stat = decided.stat.as_ref().map(observe::from_walk)?;
                let name = decided.name.as_bytes();
                match decided.decision {
                    Decision::Skip => None,
                    Decision::Descend => {
                        self.out.counts.dirs += 1;
                        Some(self.out.batch.dir(decided.parent, name, stat))
                    }
                    Decision::Traverse => {
                        self.out.counts.traversed += 1;
                        Some(self.out.batch.traversed_dir(decided.parent, name, stat))
                    }
                    Decision::Catalog(Reason::Symlink) => {
                        let target = decided.stat.and_then(|s| s.link_target)?;
                        self.out.counts.symlinks += 1;
                        self.out
                            .batch
                            .symlink(decided.parent, name, stat, target.as_bytes());
                        None
                    }
                    Decision::Catalog(Reason::TooLarge) => {
                        self.out.counts.files += 1;
                        self.out
                            .batch
                            .file(decided.parent, name, stat, Content::Unindexed);
                        None
                    }
                    Decision::Index => {
                        self.out.counts.files += 1;
                        self.index_file(&decided, stat);
                        None
                    }
                }
            }
            Event::Entered { dir, work_tree } => {
                #[cfg(test)]
                hook(self.root, Probe::Entered);
                if let Some(wt) = work_tree {
                    let kind = match wt.kind {
                        WorkTreeKind::Main => ferret_catalog::WorkTreeKind::Main,
                        WorkTreeKind::Linked => ferret_catalog::WorkTreeKind::Linked,
                        WorkTreeKind::Submodule => ferret_catalog::WorkTreeKind::Submodule,
                    };
                    self.out.batch.work_tree(
                        dir,
                        kind,
                        wt.common_dir.as_os_str().as_bytes(),
                        wt.common_id,
                    );
                }
                None
            }
            Event::Boundary { .. } => {
                self.out.counts.boundaries += 1;
                None
            }
            Event::Io {
                path,
                op,
                context,
                error,
            } => {
                let on_root = matches!(context, FaultContext::Root);
                if op == IoOp::Lstat && !on_root && error.kind() == io::ErrorKind::NotFound {
                    self.out.counts.vanished += 1;
                } else {
                    self.fault(path, op, on_root, error);
                }
                None
            }
            Event::Pattern(error) => {
                self.out.counts.pattern_errors += 1;
                self.out.pattern_errors.push(error.to_string());
                None
            }
        }
    }
}

/// Test seam: where a worker is in [`index`], for tests that must act
/// between two steps. Hooks are registered per root path, so parallel tests
/// see only their own walks, on whichever worker the event happens.
#[cfg(test)]
pub(crate) enum Probe<'a> {
    /// A directory was listed; its children have not been statted.
    Entered,
    /// Claimed an inode in the cache, before reading it.
    Claimed,
    /// Met an inode in flight and deferred this name.
    Deferred,
    /// Hashed a file, before the second `fstat`.
    Hashed(&'a Path),
    /// Read a file (after the cache was completed).
    Read(&'a Path),
}

#[cfg(test)]
pub(crate) type ProbeHook = std::sync::Arc<dyn Fn(Probe<'_>) + Send + Sync>;

#[cfg(test)]
pub(crate) static PROBES: std::sync::Mutex<Vec<(PathBuf, ProbeHook)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn hook(root: &Path, probe: Probe<'_>) {
    let found = PROBES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|(r, _)| r == root)
        .map(|(_, hook)| std::sync::Arc::clone(hook));
    if let Some(hook) = found {
        hook(probe);
    }
}
