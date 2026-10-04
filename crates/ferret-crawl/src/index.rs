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
//! reconcile      resolve directory tokens; diff and sweep refreshed roots
//!                promote a kept alias root if its shared inode needs a stat
//! commit         append one final set; equal rows publish nothing
//! retain         typed faults protect checked old boundaries across workers
//! checkpoint     initial indexing or an explicit format migration
//! ```
//!
//! A coverage fault protects a checked old edge or directory subtree. New
//! observations in that scope are discarded after all workers finish; other
//! trustworthy scopes can publish. Missing anchors, changed global versions
//! and uncertain root boundaries block the transaction. Content faults publish
//! valid namespace/stat observations as Fault/no-document and retry next run.

use std::collections::{BTreeMap, BTreeSet};
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

/// Which configured roots to walk this run. The rest stay in the effective
/// view where keeping them is valid.
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

/// Owned walker context retained until all workers have finished.
#[derive(Clone, Debug)]
pub enum CoverageContext {
    /// Opening/statting/listing the configured root, possibly before a token.
    Root,
    /// A directory occurrence already observed by the walker.
    Directory(DirToken),
    /// A named entry of a checked directory occurrence.
    Child { parent: DirToken, name: Vec<u8> },
}

/// A typed coverage fault retained in the report or blocking publication.
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
    /// Checked directory/edge coordinates for retention, independent of paths.
    pub context: CoverageContext,
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
    /// A request names an inode that is not a live directory in its generation.
    BadScope(InoId),
    /// Entry scopes require a single nonempty basename, without NUL.
    BadEntry(Vec<u8>),
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
    /// Incremental reconciliation or publication failed.
    Update(ferret_catalog::log::Error),
    /// An incomplete observation has no typed protection result. An owned
    /// caller can transfer its session to an explicit checkpoint fallback.
    NeedsCheckpoint { report: Box<Report> },
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadRoot(p) => write!(f, "root {} must be absolute, without `..`", p.display()),
            Self::NotConfigured(p) => write!(f, "{} is not a configured root", p.display()),
            Self::BadEntry(name) => {
                write!(f, "invalid entry basename {:?}", OsStr::from_bytes(name))
            }
            Self::BadScope(id) => write!(f, "inode {} is not a live directory scope", id.0),
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
            Self::NeedsCheckpoint { .. } => write!(
                f,
                "incomplete directory coverage requires checkpoint fallback"
            ),
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
    /// Typed faults retained by scoped reconciliation; stale counts are
    /// unknown.
    pub coverage_faults: Vec<CoverageFault>,
    /// Outermost retained edges/subtrees and new opaque directory scopes.
    pub protected_scopes: usize,
    /// Walking and hashing, over every refreshed root.
    pub walk_time: Duration,
    /// Of the walk, time the workers spent sniffing and hashing file
    /// content, summed over workers. Hashing runs on the walk's workers, so
    /// with several it overlaps the walk and can exceed `walk_time`.
    pub hash_time: Duration,
    /// Reconciliation, final-set encoding, writing and syncing. A checkpoint
    /// fallback also includes copying kept roots and building packed columns.
    pub commit_time: Duration,
    /// After commit, fault reporting. Log recrawls inspect changed fault inode
    /// ids; a checkpoint fallback checks every inode. Live names are scanned
    /// only when a fault needs its aliases reported.
    pub fault_time: Duration,
    /// Sum of worker-local peak temporary file rows; changed final rows and
    /// epoch-sized seen bits are reported separately from this bounded buffer.
    pub observation_rows_peak: usize,
    /// Conservative temporary row/name/target peak across worker buffers.
    pub observation_bytes_peak: usize,
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
/// The writer lock is held through publication. Typed coverage faults retain
/// checked old scopes while trustworthy observations elsewhere publish. An
/// unresolvable scope or an incompatible version/root transition publishes
/// nothing ([`IndexError::Coverage`]).
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
            let mut plan = Plan::new(
                Some(&previous),
                &roots,
                refresh,
                options.sniffer,
                fingerprint(options),
            )?;
            let Reconciled {
                batches,
                mut report,
                changes,
            } = observe_reconcile(&session, &mut plan, options)?;
            let started = Instant::now();
            let (catalog, changed, faulted) = match changes {
                Some(changes) => {
                    let changed = !changes.records.is_empty();
                    let faulted = faulted_inodes(&changes);
                    let catalog = session
                        .commit(&changes, options.sniffer)
                        .map_err(IndexError::Update)?;
                    (catalog, changed, Some(faulted))
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
                    (txn.commit().map_err(IndexError::Commit)?, true, None)
                }
            };
            report.commit_time += started.elapsed();
            finish_report(&mut report, &catalog, &plan, changed, faulted.as_deref());
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
            report.commit_time += started.elapsed();
            finish_report(&mut report, &catalog, &plan, true, None);
            Ok(report)
        }
        Err(ferret_catalog::log::Error::Locked) => Err(IndexError::Begin(BeginError::Locked)),
        Err(error) => Err(IndexError::Update(error)),
    }
}

/// Recrawls using a resident session, retaining its lookups and writer lock.
/// The complete configured root set and D34 widening have the same contract as
/// `index`. Typed faults retain checked old scopes in the same log transaction
/// as trustworthy updates, without rebuilding the checkpoint.
pub fn recrawl(
    session: &mut WriterSession,
    roots: &[PathBuf],
    refresh: Refresh<'_>,
    options: &IndexOptions,
) -> Result<Report, IndexError> {
    recrawl_with_changes(session, roots, refresh, options).map(|(report, _)| report)
}

pub(crate) fn recrawl_with_changes(
    session: &mut WriterSession,
    roots: &[PathBuf],
    refresh: Refresh<'_>,
    options: &IndexOptions,
) -> Result<(Report, ferret_catalog::log::ChangeSet), IndexError> {
    recrawl_scoped(session, roots, refresh, options, BTreeMap::new())
}

pub(crate) fn recrawl_scoped(
    session: &mut WriterSession,
    roots: &[PathBuf],
    refresh: Refresh<'_>,
    options: &IndexOptions,
    selections: BTreeMap<PathBuf, std::sync::Arc<crate::refresh::Selection>>,
) -> Result<(Report, ferret_catalog::log::ChangeSet), IndexError> {
    let previous = session.view();
    let mut plan = Plan::new(
        Some(&previous),
        roots,
        refresh,
        options.sniffer,
        fingerprint(options),
    )?;
    if previous.policy() == fingerprint(options) && previous.sniffer_version() == options.sniffer {
        plan.selections = selections;
    }
    let Reconciled {
        mut report,
        changes,
        ..
    } = observe_reconcile(session, &mut plan, options)?;
    let started = Instant::now();
    let Some(changes) = changes else {
        return Err(IndexError::NeedsCheckpoint {
            report: Box::new(report),
        });
    };
    let faulted = faulted_inodes(&changes);
    let changed = !changes.records.is_empty();
    let catalog = session
        .commit(&changes, options.sniffer)
        .map_err(IndexError::Update)?;
    report.commit_time += started.elapsed();
    finish_report(&mut report, &catalog, &plan, changed, Some(&faulted));
    Ok((report, changes))
}

struct Reconciled {
    batches: Vec<ferret_catalog::Batch>,
    report: Report,
    changes: Option<ferret_catalog::log::ChangeSet>,
}

fn observe_reconcile(
    session: &WriterSession,
    plan: &mut Plan,
    options: &IndexOptions,
) -> Result<Reconciled, IndexError> {
    let source = Source::Session(session, options.sniffer);
    let (mut batches, mut report) = observe(source, plan, options)?;
    loop {
        let started = Instant::now();
        let Some(protection) =
            crate::coverage::resolve(session, &batches, &report.coverage_faults, &plan.roots)
        else {
            return Err(IndexError::Coverage {
                faults: std::mem::take(&mut report.coverage_faults),
                report: Box::new(report),
            });
        };
        if !protection.is_empty()
            && (session.view().policy() != fingerprint(options)
                || session.view().sniffer_version() != options.sniffer)
        {
            return Err(IndexError::Coverage {
                faults: std::mem::take(&mut report.coverage_faults),
                report: Box::new(report),
            });
        }
        report.protected_scopes =
            protection.directories.len() + protection.edges.len() + protection.opaque.len();
        let changes = crate::reconcile::with_protection(
            session,
            &batches,
            &plan.refresh,
            &plan.dropped,
            fingerprint(options),
            options.sniffer,
            &protection,
        )
        .map_err(IndexError::Update)?;
        report.commit_time += started.elapsed();
        let Some(ref final_changes) = changes else {
            return Ok(Reconciled {
                batches,
                report,
                changes: None,
            });
        };
        let previous = session.view();
        // A deleted last refreshed alias supplies no stat. Rewalk its kept
        // root so nlink/ctime and content are observed, rather than publishing
        // a stale shared row. M6 can narrow this to handle-relative alias
        // scopes.
        let observed: BTreeSet<_> = final_changes
            .records
            .iter()
            .filter_map(|r| match r {
                ferret_catalog::log::Record::InodePut { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        let missing: BTreeSet<_> = final_changes
            .records
            .iter()
            .filter_map(|r| match r {
                ferret_catalog::log::Record::LifePut {
                    id, names, kind, ..
                } if *kind != ferret_catalog::Kind::Dir
                    && previous.is_live_inode(InoId(*id))
                    && *names > 0
                    && *names < session.name_references(InoId(*id))
                    && !observed.contains(id) =>
                {
                    Some(*id)
                }
                _ => None,
            })
            .collect();
        if missing.is_empty() {
            return Ok(Reconciled {
                batches,
                report,
                changes,
            });
        }
        let mut extra = BTreeSet::new();
        let removed: BTreeSet<_> = final_changes
            .records
            .iter()
            .filter_map(|r| match r {
                ferret_catalog::log::Record::NameDelete { id } => Some(*id),
                _ => None,
            })
            .collect();
        let mut expanded = false;
        for id in &missing {
            for name in session
                .names_for(InoId(*id))
                .filter(|n| !removed.contains(&n.0))
            {
                let edge = previous.name(name);
                let mut root = edge.parent;
                while let Some(name) = previous.dir_name(root) {
                    root = previous.name(name).parent;
                }
                let Some(path) = previous
                    .roots()
                    .find(|(id, _)| *id == root)
                    .map(|(_, p)| PathBuf::from(OsStr::from_bytes(p)))
                else {
                    continue;
                };
                if let Some(selection) = plan.selections.get(&path) {
                    let mut alias = Vec::new();
                    previous.path(name, &mut alias);
                    if let Ok(relative) = Path::new(OsStr::from_bytes(&alias)).strip_prefix(&path)
                        && !selection.includes(relative)
                    {
                        selection.promote(relative);
                        expanded = true;
                    }
                } else if plan.keep.contains(&path) {
                    extra.insert(path);
                }
            }
        }
        if expanded {
            // Re-observe the expanded final scopes together: duplicate root
            // token graphs from a second partial pass cannot be reconciled.
            let (fresh, fresh_report) = observe(source, plan, options)?;
            batches = fresh;
            report = fresh_report;
            continue;
        }
        if extra.is_empty() {
            return Ok(Reconciled {
                batches,
                report,
                changes,
            });
        }
        let extra: Vec<_> = extra.into_iter().collect();
        plan.keep.retain(|p| !extra.contains(p));
        plan.refresh.extend(extra.iter().cloned());
        plan.refresh.sort();
        let added = Plan {
            roots: plan.roots.clone(),
            refresh: extra,
            keep: Vec::new(),
            dropped: Vec::new(),
            selections: BTreeMap::new(),
        };
        let (more, mut extra_report) = observe(source, &added, options)?;
        batches.extend(more);
        report.counts.add(&extra_report.counts);
        report.walk_time += extra_report.walk_time;
        report.hash_time += extra_report.hash_time;
        report
            .content_faults
            .append(&mut extra_report.content_faults);
        report
            .pattern_errors
            .append(&mut extra_report.pattern_errors);
        report
            .coverage_faults
            .append(&mut extra_report.coverage_faults);
        report.refreshed.clone_from(&plan.refresh);
        report.kept.clone_from(&plan.keep);
    }
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
            || {
                let mut hasher = Hasher::with_source(source, &cache, root);
                hasher.selection = plan.selections.get(root).map(std::convert::AsRef::as_ref);
                hasher
            },
        );
        let outputs: Vec<Output> = visitors
            .into_iter()
            .map(|v| v.finish(&mut faults))
            .collect();
        for mut output in outputs {
            output.resolve(&cache);
            output.batch.finish_observations();
            let (rows, bytes) = output.batch.observation_peak();
            report.observation_rows_peak += rows;
            report.observation_bytes_peak += bytes;
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
    if crate::coverage::discard_denied_prefix_faults(&batches, &mut faults).is_none() {
        return Err(IndexError::Coverage {
            faults,
            report: Box::new(report),
        });
    }
    if !faults.is_empty()
        && let Source::Checkpoint(txn) = source
    {
        // D26 directory EACCES is opaque even for an initial root. Every
        // other initial fault still lacks a trustworthy retention anchor.
        if faults.iter().all(crate::coverage::directory_denied)
            && let Some(opaque) = crate::coverage::opaque_checkpoint(txn, &batches, &faults)
        {
            batches = opaque;
            faults.clear();
        } else {
            return Err(IndexError::Coverage {
                faults,
                report: Box::new(report),
            });
        }
    }
    report.coverage_faults = faults;
    Ok((batches, report))
}

fn faulted_inodes(changes: &ferret_catalog::log::ChangeSet) -> Vec<InoId> {
    changes
        .records
        .iter()
        .filter_map(|r| match r {
            ferret_catalog::log::Record::InodePut {
                id,
                state: ContentState::Fault,
                ..
            } => Some(InoId(*id)),
            _ => None,
        })
        .collect()
}

fn finish_report(
    report: &mut Report,
    catalog: &Catalog,
    plan: &Plan,
    changed: bool,
    faulted: Option<&[InoId]>,
) {
    let started = Instant::now();
    report
        .coverage_faults
        .retain(|f| !crate::coverage::directory_denied(f));
    let seen = std::mem::take(&mut report.content_faults)
        .into_iter()
        .filter(|(path, _)| published_content_fault(catalog, path))
        .collect();
    report.content_faults = content_faults_with(catalog, &plan.refresh, seen, faulted);
    report.counts.content_faults = report.content_faults.len() as u64;
    report.fault_time = started.elapsed();
    report.published = changed.then(|| Published::of(catalog));
}

// Fault reports describe the effective indexed row, including retained data.
// Resolve's live-fallback boundary is deliberately crossed through checked
// child keys here; this is reporting, not permission to use a stale listing.
fn published_content_fault(catalog: &Catalog, path: &Path) -> bool {
    let Some(resolved) = catalog.resolve(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut target = resolved.target;
    for component in resolved
        .remainder
        .split(|&b| b == b'/')
        .filter(|part| !part.is_empty())
    {
        let ferret_catalog::Target::Inode(parent) = target else {
            return false;
        };
        if !catalog.is_directory(parent) {
            return false;
        }
        let Some(name) = catalog.lookup(parent, component) else {
            return false;
        };
        target = catalog.name(name).target();
    }
    matches!(target, ferret_catalog::Target::Inode(id) if catalog.state(id) == ContentState::Fault)
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
#[cfg(test)]
pub(crate) fn content_faults(
    catalog: &Catalog,
    refresh: &[PathBuf],
    seen: Vec<(PathBuf, ContentFault)>,
) -> Vec<(PathBuf, ContentFault)> {
    content_faults_with(catalog, refresh, seen, None)
}

fn content_faults_with(
    catalog: &Catalog,
    refresh: &[PathBuf],
    seen: Vec<(PathBuf, ContentFault)>,
    faulted: Option<&[InoId]>,
) -> Vec<(PathBuf, ContentFault)> {
    let fault = |id: InoId| catalog.state(id) == ContentState::Fault;
    let mut listed: BTreeMap<PathBuf, ContentFault> = seen.into_iter().collect();
    let any = faulted.map_or_else(
        || {
            catalog
                .inode_ids()
                .filter(|&id| !catalog.is_directory(id))
                .any(fault)
        },
        |ids| ids.iter().copied().any(fault),
    );
    if !any {
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
    selections: BTreeMap<PathBuf, std::sync::Arc<crate::refresh::Selection>>,
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
            selections: BTreeMap::new(),
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
    selection: Option<&'a crate::refresh::Selection>,
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
            selection: None,
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
                #[cfg(test)]
                hook(self.root, Probe::Carried(decided.path));
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

    fn fault(
        &mut self,
        path: &Path,
        op: IoOp,
        context: FaultContext<'_, DirToken>,
        error: io::Error,
    ) {
        self.out.faults.push(CoverageFault {
            root: self.root.to_owned(),
            path: path.to_owned(),
            op,
            on_root: matches!(context, FaultContext::Root),
            context: match context {
                FaultContext::Root => CoverageContext::Root,
                FaultContext::Dir(token) => CoverageContext::Directory(token),
                FaultContext::Child { parent, name } => CoverageContext::Child {
                    parent,
                    name: name.as_bytes().to_vec(),
                },
            },
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
        let token = self
            .out
            .batch
            .root(self.root.as_os_str().as_bytes(), observe::from_walk(&stat));
        if let Some(selection) = self.selection
            && token.previous_directory().is_none()
        {
            selection.promote(Path::new(""));
        }
        token
    }

    fn consider(&mut self, parent: DirToken, name: &OsStr, path: &Path) -> bool {
        let Some(selection) = self.selection else {
            return true;
        };
        if let Some(old) = parent.previous_directory()
            && let Source::Session(session, _) = self.txn
            && session.view().entry_count(old).is_none()
        {
            selection.promote(path.parent().unwrap_or_else(|| Path::new("")));
        }
        if selection.includes(path) {
            true
        } else {
            self.out.batch.preserve(parent, name.as_bytes());
            false
        }
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
                        let token = self.out.batch.dir(decided.parent, name, stat);
                        if let Some(selection) = self.selection
                            && token.previous_directory().is_none()
                        {
                            selection.promote(decided.path);
                        }
                        Some(token)
                    }
                    Decision::Traverse => {
                        self.out.counts.traversed += 1;
                        let token = self.out.batch.traversed_dir(decided.parent, name, stat);
                        if let Some(selection) = self.selection
                            && token.previous_directory().is_none()
                        {
                            selection.promote(decided.path);
                        }
                        Some(token)
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
                } else {
                    self.fault(path, op, context, error);
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
    /// Carried content and recorded its observation, before the next entry.
    Carried(&'a Path),
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
