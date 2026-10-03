//! One run of `ferret index`: walk the roots that need it, hash what changed,
//! and publish a new catalog generation, or nothing (D26 A′, D31, D33, D34,
//! D37).
//!
//! ```text
//! begin          take the writer lock, read the old generation
//! plan           which roots to walk, keep and drop; widen for overlaps
//! walk + hash    per refreshed root, walk_parallel with a hashing visitor
//! resolve        aliases that met an inode in flight take its observation:
//!                drained during the walk, and the rest as each root ends
//! add, keep      hand the batches over, copy the kept roots forward
//! commit         unless a coverage fault was seen
//! ```
//!
//! A **coverage fault** is any [`Event::Io`] except an entry's `lstat`
//! NotFound (a deletion), and EACCES from opening or listing a directory
//! (a catalogued directory with unknown contents, D26 amendment). It means the
//! walk may have missed entries or applied the wrong ignore rules, so nothing
//! is published and the old generation stays. A **content fault** leaves the
//! namespace intact: the file is published with
//! [`ContentState::Fault`](ferret_catalog::ContentState) and no document, and
//! the next run reads it again.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use ferret_catalog::{
    BeginError, Catalog, CommitError, Content, ContentState, DirToken, InoId, KeepError, Stat,
    Transaction, WriterSession,
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
    /// Incremental reconciliation or publication failed. An incomplete resident
    /// recrawl is blocked until M5 can represent protected fault scopes.
    Update(ferret_catalog::log::Error),
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
            Self::Update(e) => write!(f, "{e}"),
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
    /// Visible FIFOs, sockets and devices catalogued without content.
    pub specials: u64,
    /// File names the policy sent to the index.
    pub indexed: u64,
    /// Of those, whose content came from the previous generation unread.
    pub carried: u64,
    /// Of those, that took another name's observation from the cache.
    pub aliased: u64,
    /// Of those aliases, that met their inode in flight on another worker
    /// and were recorded once its observation was finished.
    pub deferred: u64,
    /// The most deferred aliases held unrecorded at once, across all workers
    /// and roots: the backlog's real footprint, which `deferred` is not.
    pub deferred_peak: u64,
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
            specials,
            indexed,
            carried,
            aliased,
            deferred,
            deferred_peak,
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
        self.specials += specials;
        self.indexed += indexed;
        self.carried += carried;
        self.aliased += aliased;
        self.deferred += deferred;
        self.deferred_peak = self.deferred_peak.max(deferred_peak);
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
    /// Of the walk, time the workers spent sniffing and hashing file
    /// content, summed over workers. Hashing runs on the walk's workers, so
    /// with several it overlaps the walk and can exceed `walk_time`.
    pub hash_time: Duration,
    /// Keeping roots, building, encoding, writing and syncing.
    pub commit_time: Duration,
    /// After the commit, listing every name the published generation holds
    /// as Fault (`content_faults`): a check of every inode row, and a scan
    /// of the names when one is Fault.
    pub fault_time: Duration,
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
/// fault other than a directory listing/open EACCES publishes nothing
/// ([`IndexError::Coverage`]).
pub fn index(
    catalog_dir: &Path,
    roots: &[PathBuf],
    refresh: Refresh<'_>,
    options: &IndexOptions,
) -> Result<Report, IndexError> {
    run(catalog_dir, |_| Ok(roots.to_vec()), refresh, options)
}

/// A change to the configured roots, made to the roots the previous
/// generation holds.
#[derive(Clone, Copy, Debug, Default)]
pub struct RootChange<'a> {
    /// Roots to add; one already configured is left as it is.
    pub add: &'a [PathBuf],
    /// Roots to remove; each must be configured.
    pub remove: &'a [PathBuf],
}

/// [`index`], with the root set given as a change to the previous
/// generation's roots rather than in full. The change is applied after the
/// writer lock is taken, so two runs that each add a root cannot drop each
/// other's: a caller that read the roots and passed the full set to
/// [`index`] could. Removing a root that is not configured is
/// [`IndexError::NotConfigured`]; a root to add or remove that is not
/// absolute, or has `..`, is [`IndexError::BadRoot`].
pub fn index_change(
    catalog_dir: &Path,
    change: RootChange<'_>,
    refresh: Refresh<'_>,
    options: &IndexOptions,
) -> Result<Report, IndexError> {
    run(
        catalog_dir,
        |previous| {
            let mut roots: Vec<PathBuf> = previous
                .map(|p| {
                    p.roots()
                        .map(|(_, path)| PathBuf::from(OsStr::from_bytes(path)))
                        .collect()
                })
                .unwrap_or_default();
            for gone in change.remove {
                let gone = normalise(gone)?;
                let before = roots.len();
                roots.retain(|r| *r != gone);
                if roots.len() == before {
                    return Err(IndexError::NotConfigured(gone));
                }
            }
            for added in change.add {
                roots.push(normalise(added)?);
            }
            Ok(roots)
        },
        refresh,
        options,
    )
}

/// One run, with the configured roots decided by `roots` from the previous
/// generation once the lock is held.
fn run(
    catalog_dir: &Path,
    roots: impl FnOnce(Option<&Catalog>) -> Result<Vec<PathBuf>, IndexError>,
    refresh: Refresh<'_>,
    options: &IndexOptions,
) -> Result<Report, IndexError> {
    let session = WriterSession::open(catalog_dir);
    match session {
        Ok(mut session) => {
            let previous = session.view();
            let roots = roots(Some(&previous))?;
            let plan = Plan::new(
                Some(&previous),
                &roots,
                refresh,
                options.sniffer,
                fingerprint(options),
            )?;
            let (batches, mut report) =
                observe(Source::Session(&session, options.sniffer), &plan, options)?;
            let started = Instant::now();
            let changes = crate::reconcile::changes(
                &session,
                &batches,
                &plan.refresh,
                &plan.dropped,
                fingerprint(options),
                options.sniffer,
            )
            .map_err(IndexError::Update)?;
            let (catalog, changed) = match changes {
                Some(changes) => {
                    let changed = !changes.records.is_empty();
                    let catalog = session
                        .commit(&changes, options.sniffer)
                        .map_err(IndexError::Update)?;
                    (catalog, changed)
                }
                None => {
                    let mut txn = session.into_checkpoint(options.sniffer);
                    txn.set_policy(fingerprint(options));
                    for batch in batches {
                        txn.add(batch);
                    }
                    for root in &plan.keep {
                        txn.keep(root.as_os_str().as_bytes())
                            .map_err(IndexError::Keep)?;
                    }
                    (txn.commit().map_err(IndexError::Commit)?, true)
                }
            };
            report.commit_time = started.elapsed();
            finish_report(&mut report, &catalog, &plan, changed);
            Ok(report)
        }
        Err(ferret_catalog::log::Error::MissingCheckpoint)
        | Err(ferret_catalog::log::Error::Previous(ferret_catalog::OpenError::Decode(
            ferret_catalog::DecodeError::Version(_),
        ))) => {
            let mut txn =
                Transaction::begin(catalog_dir, options.sniffer).map_err(IndexError::Begin)?;
            let roots = roots(txn.previous())?;
            let plan = Plan::new(
                txn.previous(),
                &roots,
                refresh,
                options.sniffer,
                fingerprint(options),
            )?;
            let (batches, mut report) = observe(Source::Checkpoint(&txn), &plan, options)?;
            let started = Instant::now();
            txn.set_policy(fingerprint(options));
            for batch in batches {
                txn.add(batch);
            }
            for root in &plan.keep {
                txn.keep(root.as_os_str().as_bytes())
                    .map_err(IndexError::Keep)?;
            }
            let catalog = txn.commit().map_err(IndexError::Commit)?;
            report.commit_time = started.elapsed();
            finish_report(&mut report, &catalog, &plan, true);
            Ok(report)
        }
        Err(ferret_catalog::log::Error::Locked) => Err(IndexError::Begin(BeginError::Locked)),
        Err(error) => Err(IndexError::Update(error)),
    }
}

/// Recrawls using a resident session, retaining its lookups and writer lock.
/// The complete configured root set and D34 widening have the same contract as
/// `index`. Incomplete EACCES coverage blocks this resident API in M4; the
/// batch CLI can transfer its owned session to the amended A′ checkpoint
/// fallback.
pub fn recrawl(
    session: &mut WriterSession,
    roots: &[PathBuf],
    refresh: Refresh<'_>,
    options: &IndexOptions,
) -> Result<Report, IndexError> {
    let previous = session.view();
    let plan = Plan::new(
        Some(&previous),
        roots,
        refresh,
        options.sniffer,
        fingerprint(options),
    )?;
    let (batches, mut report) = observe(Source::Session(session, options.sniffer), &plan, options)?;
    let started = Instant::now();
    let changes = crate::reconcile::changes(
        session,
        &batches,
        &plan.refresh,
        &plan.dropped,
        fingerprint(options),
        options.sniffer,
    )
    .map_err(IndexError::Update)?
    .ok_or_else(|| {
        IndexError::Update(ferret_catalog::log::Error::Invalid(
            ferret_catalog::DecodeError::Corrupt("incomplete resident recrawl requires checkpoint"),
        ))
    })?;
    let changed = !changes.records.is_empty();
    let catalog = session
        .commit(&changes, options.sniffer)
        .map_err(IndexError::Update)?;
    report.commit_time = started.elapsed();
    finish_report(&mut report, &catalog, &plan, changed);
    Ok(report)
}

fn fingerprint(options: &IndexOptions) -> ferret_catalog::Hash {
    let mut hash = blake3::Hasher::new();
    hash.update(b"ferret-policy-v1\0");
    hash.update(&options.config.size_cap.to_le_bytes());
    hash.update(&[u8::from(options.global.is_some())]);
    hash.update(options.global.as_deref().unwrap_or_default().as_bytes());
    let mut fingerprint = [0; 16];
    fingerprint.copy_from_slice(&hash.finalize().as_bytes()[..16]);
    fingerprint
}

#[derive(Clone, Copy)]
enum Source<'a> {
    Checkpoint(&'a Transaction),
    Session(&'a WriterSession, u32),
}
impl Source<'_> {
    fn batch(self) -> ferret_catalog::Batch {
        match self {
            Self::Checkpoint(txn) => txn.batch(),
            Self::Session(session, _) => session.batch(),
        }
    }
    fn carry(self, stat: &Stat) -> Option<Content> {
        match self {
            Self::Checkpoint(txn) => txn.carry(stat),
            Self::Session(session, sniffer) => session.carry(stat, sniffer),
        }
    }
}

fn observe(
    source: Source<'_>,
    plan: &Plan,
    options: &IndexOptions,
) -> Result<(Vec<ferret_catalog::Batch>, Report), IndexError> {
    let mut report = Report {
        refreshed: plan.refresh.clone(),
        kept: plan.keep.clone(),
        dropped: plan.dropped.clone(),
        ..Report::default()
    };
    let started = Instant::now();
    let cache = Cache::new();
    let mut faults = Vec::new();
    let mut batches = Vec::new();
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
            || Hasher::with_source(source, &cache, root),
        );
        let outputs: Vec<Output> = visitors
            .into_iter()
            .map(|v| v.finish(&mut faults))
            .collect();
        for mut output in outputs {
            output.resolve(&cache);
            report.counts.add(&output.counts);
            report.hash_time += output.read_time;
            report.content_faults.append(&mut output.content_faults);
            report.pattern_errors.append(&mut output.pattern_errors);
            batches.push(output.batch);
        }
    }
    report.counts.cached_inodes = cache.len() as u64;
    report.counts.deferred_peak = cache.deferred_peak();
    report.content_faults.sort_by(|a, b| a.0.cmp(&b.0));
    report.counts.content_faults = report.content_faults.len() as u64;
    report.walk_time = started.elapsed();
    if !faults.is_empty() {
        return Err(IndexError::Coverage {
            faults,
            report: Box::new(report),
        });
    }
    Ok((batches, report))
}

fn finish_report(report: &mut Report, catalog: &Catalog, plan: &Plan, changed: bool) {
    let started = Instant::now();
    let seen = std::mem::take(&mut report.content_faults);
    report.content_faults = content_faults(catalog, &plan.refresh, seen);
    report.counts.content_faults = report.content_faults.len() as u64;
    report.fault_time = started.elapsed();
    report.published = changed.then(|| Published::of(catalog));
}

/// Every name under a refreshed root whose inode the catalog published as
/// Fault, merged with the faults the workers saw. The build is the
/// authority: it faults an inode when its names' observations disagree
/// (D31), which no single worker can see, so each such name is listed as
/// [`ContentFault::Alias`] unless a worker recorded its own error. Kept
/// roots are skipped: their faults are last run's, already reported.
///
/// Paths come from the catalog, so the walk holds none; the scan over the
/// names runs only when some inode is Fault.
pub(crate) fn content_faults(
    catalog: &Catalog,
    refresh: &[PathBuf],
    seen: Vec<(PathBuf, ContentFault)>,
) -> Vec<(PathBuf, ContentFault)> {
    let fault = |id: InoId| catalog.state(id) == ContentState::Fault;
    let mut listed: BTreeMap<PathBuf, ContentFault> = seen.into_iter().collect();
    if !catalog
        .inode_ids()
        .filter(|&id| !catalog.is_directory(id))
        .any(fault)
    {
        return listed.into_iter().collect();
    }
    let refreshed: Vec<InoId> = catalog
        .roots()
        .filter(|(_, path)| refresh.iter().any(|r| r.as_os_str().as_bytes() == *path))
        .map(|(id, _)| id)
        .collect();
    let root_of = |mut dir: InoId| {
        while let Some(name) = catalog.dir_name(dir) {
            dir = catalog.name(name).parent;
        }
        dir
    };
    let mut buf = Vec::new();
    for (id, _) in catalog.names() {
        let name = catalog.name(id);
        if matches!(name.target(), ferret_catalog::Target::Ignored(_))
            || !fault(name.child)
            || !refreshed.contains(&root_of(name.parent))
        {
            continue;
        }
        buf.clear();
        catalog.path(id, &mut buf);
        let path = PathBuf::from(OsStr::from_bytes(&buf));
        listed.entry(path).or_insert(ContentFault::Alias);
    }
    listed.into_iter().collect()
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
        policy: ferret_catalog::Hash,
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
            || previous.is_some_and(|p| p.sniffer_version() != sniffer || p.policy() != policy);
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
/// when this one reached it; recorded once that inode's observation is
/// finished ([`Output::defer`]).
pub(crate) struct Deferred {
    pub(crate) parent: DirToken,
    pub(crate) name: Vec<u8>,
    pub(crate) stat: Stat,
    pub(crate) path: PathBuf,
}

/// The per-worker visitor: fills one batch and reads the files it must.
pub(crate) struct Hasher<'a> {
    txn: Source<'a>,
    cache: &'a Cache,
    root: &'a Path,
    pub(crate) out: Output,
    reader: Reader,
}

/// What a worker leaves behind.
pub(crate) struct Output {
    pub(crate) batch: ferret_catalog::Batch,
    pub(crate) counts: Counts,
    faults: Vec<CoverageFault>,
    pub(crate) content_faults: Vec<(PathBuf, ContentFault)>,
    pattern_errors: Vec<String>,
    pub(crate) deferred: Vec<Deferred>,
    /// The backlog length at which [`Output::defer`] next drains it.
    drain_at: usize,
    read_time: Duration,
}

/// The smallest backlog [`Output::defer`] drains. Draining is a cache lookup
/// per entry, and the threshold doubles past what stays, so the cost is
/// amortised constant per alias.
pub(crate) const DRAIN_MIN: usize = 64;

impl<'a> Hasher<'a> {
    #[cfg(test)]
    pub(crate) fn new(txn: &'a Transaction, cache: &'a Cache, root: &'a Path) -> Self {
        Self::with_source(Source::Checkpoint(txn), cache, root)
    }
    fn with_source(txn: Source<'a>, cache: &'a Cache, root: &'a Path) -> Self {
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
                drain_at: DRAIN_MIN,
                read_time: Duration::ZERO,
            },
            reader: Reader::new(),
        }
    }

    fn finish(mut self, faults: &mut Vec<CoverageFault>) -> Output {
        self.out.counts.files_read = self.reader.files_read;
        self.out.counts.bytes_read = self.reader.bytes_read;
        self.out.read_time = self.reader.read_time;
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
                self.record(decided, stat, Err(fault));
                return;
            }
        };
        let key = (stat.dev, stat.ino);
        if links > 1 {
            match self.cache.claim(key) {
                Lookup::Claimed => {}
                Lookup::InFlight => {
                    let alias = Deferred {
                        parent: decided.parent,
                        name: decided.name.as_bytes().to_vec(),
                        stat,
                        path: self.root.join(decided.path),
                    };
                    self.out.defer(alias, self.cache);
                    #[cfg(test)]
                    hook(self.root, Probe::Deferred);
                    return;
                }
                Lookup::Done(stored) => {
                    self.out.counts.aliased += 1;
                    let (stat, content) = observe::consume(stored, &stat);
                    self.record(decided, stat, content);
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
        self.record(decided, stat, content);
    }

    /// Records a read or aliased file.
    fn record(
        &mut self,
        decided: &Decided<'_, DirToken>,
        stat: Stat,
        content: Result<Content, ContentFault>,
    ) {
        let content = self.out.outcome(|| self.root.join(decided.path), content);
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
    /// The content to publish for one name, listing a fault with its path.
    /// These are the faults a worker saw, with their errors; the build may
    /// fault more names, which `index` lists from the published catalog.
    fn outcome(
        &mut self,
        path: impl FnOnce() -> PathBuf,
        content: Result<Content, ContentFault>,
    ) -> Content {
        content.unwrap_or_else(|fault| {
            self.content_faults.push((path(), fault));
            Content::Fault
        })
    }

    /// Sets `alias` aside until its inode's observation is finished. When
    /// the backlog reaches `drain_at`, every alias whose inode has finished
    /// since is recorded, and the rest resolve when the root's walk ends.
    ///
    /// So a worker holds, within one root, the aliases of inodes still in
    /// flight plus at most `max(DRAIN_MIN, 2 × what stayed at its last
    /// drain)` finished ones; nothing survives into the next root. Aliases of
    /// one inode are bounded by its link count on one filesystem, but not
    /// across bind mounts, which show the same inode under several paths.
    pub(crate) fn defer(&mut self, alias: Deferred, cache: &Cache) {
        self.counts.deferred += 1;
        cache.deferred(1);
        self.deferred.push(alias);
        if self.deferred.len() < self.drain_at {
            return;
        }
        for alias in std::mem::take(&mut self.deferred) {
            match cache.finished((alias.stat.dev, alias.stat.ino)) {
                Some(stored) => self.record_alias(alias, Some(stored), cache),
                None => self.deferred.push(alias),
            }
        }
        self.drain_at = (2 * self.deferred.len()).max(DRAIN_MIN);
    }

    /// Records the deferred aliases left once the walk is over.
    pub(crate) fn resolve(&mut self, cache: &Cache) {
        for alias in std::mem::take(&mut self.deferred) {
            let stored = cache.finished((alias.stat.dev, alias.stat.ino));
            self.record_alias(alias, stored, cache);
        }
    }

    /// Records one deferred alias from its inode's observation; `None`, a
    /// claim never completed, is a fault.
    fn record_alias(&mut self, alias: Deferred, stored: Option<Observation>, cache: &Cache) {
        self.counts.aliased += 1;
        cache.deferred(-1);
        let (stat, content) = match stored {
            Some(stored) => observe::consume(stored, &alias.stat),
            None => (alias.stat, Err(ContentFault::Alias)),
        };
        let content = self.outcome(|| alias.path, content);
        self.batch.file(alias.parent, &alias.name, stat, content);
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
                if decided.decision == Decision::Skip {
                    self.out
                        .batch
                        .ignored(decided.parent, decided.name.as_bytes(), decided.kind);
                    return None;
                }
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
                    Decision::Catalog(Reason::Special) => {
                        self.out.counts.specials += 1;
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
            Event::Entered {
                dir,
                work_tree,
                entries,
            } => {
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
                if let Some(count) = entries {
                    self.out.batch.entry_count(dir, count);
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
                } else if matches!(op, IoOp::OpenDir | IoOp::List)
                    && error.raw_os_error() == Some(rustix::io::Errno::ACCESS.raw_os_error())
                {
                    // The directory row precedes its open/list. No Entered
                    // event sets a count, so it remains unknown (D26).
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
