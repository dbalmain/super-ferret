//! What a run reports: its counts and publication, the roots it
//! withdrew, and the errors that publish nothing.

use std::os::unix::ffi::OsStrExt;
use std::{fmt, io};

use ferret_catalog::{CommitError, ContentState, DirToken, KeepError, NameId};

use super::*;
use crate::observe::ContentFault;
use crate::walk::IoOp;

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
    /// No publication or cache mutation; retry a complete backstop later.
    DeferredBulk(ferret_catalog::bulk::Blocked),
    /// A command's full rebuild was refused by memory admission.
    DeferredMemory { required: u64, available: u64 },
    /// Lowering an index thread priority failed.
    Priority(io::Error),
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
    /// The new generation is visible to readers, but a later step of its
    /// publication failed, so it may not survive a crash. `error` is the
    /// [`IndexError::Commit`] or [`IndexError::Update`] that says so.
    /// `report` is the run's report up to publication: its D119
    /// `failed_roots` were withdrawn from what readers now see, while
    /// `published` and the content faults, read back from the published
    /// catalog, are absent.
    Undurable {
        error: Box<IndexError>,
        report: Box<Report>,
    },
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeferredBulk(reason) => reason.fmt(f),
            Self::DeferredMemory {
                required,
                available,
            } => write!(
                f,
                "insufficient memory for a full rebuild: need {required} bytes, have {available}"
            ),
            Self::Priority(error) => write!(f, "index thread priority: {error}"),
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
            Self::Undurable { error, .. } => error.fmt(f),
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
    pub(super) fn add(&mut self, other: &Counts) {
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
    /// Roots withdrawn from this run (D119), sorted: neither published nor
    /// configured. Nonempty means the run published only part of what was
    /// asked. `refreshed` excludes them; `counts` still include their walks.
    pub failed_roots: Vec<FailedRoot>,
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
    /// Guard high-water before publication; refused allocations are excluded.
    pub input_usage: ferret_catalog::InputUsage,
    /// The bounded attempt was discarded and every configured root rewalked.
    pub input_fallback: bool,
    /// The published generation's shape, when one was published.
    pub published: Option<Published>,
}

/// A root withdrawn from a run (D119): new to the catalog, disjoint from every
/// other configured root, and with a coverage fault that nothing old can
/// anchor. Its uncertain observations are discarded and it is not configured,
/// so `ferret index ROOT` adds it afresh once the fault is repaired.
#[derive(Debug)]
pub struct FailedRoot {
    /// The root, as the catalog would have stored it.
    pub root: PathBuf,
    /// Its faults, every one of them on this root's walk.
    pub faults: Vec<CoverageFault>,
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

/// Records withdrawn roots in a report that will publish.
pub(super) fn withdrawn(report: &mut Report, plan: &Plan, failed: Vec<FailedRoot>) {
    report.refreshed.clone_from(&plan.refresh);
    report.failed_roots.extend(failed);
    report.failed_roots.sort_by(|a, b| a.root.cmp(&b.root));
}

/// The atomic refusal when withdrawing roots cannot leave a publishable run:
/// every fault is reported, the withdrawn roots' included.
pub(super) fn unpublished(mut report: Report, failed: Vec<FailedRoot>) -> IndexError {
    let mut faults = std::mem::take(&mut report.coverage_faults);
    let earlier = std::mem::take(&mut report.failed_roots);
    for root in earlier.into_iter().chain(failed) {
        faults.extend(root.faults);
    }
    IndexError::Coverage {
        faults,
        report: Box::new(report),
    }
}

// Changes contain epoch-scoped ids. A checkpoint remaps them; reporting must
// scan the resulting catalog before looking up any numeric id from the diff.
pub(super) fn reporting_ids(
    session: &WriterSession,
    previous: ferret_catalog::Generation,
    changes: &ferret_catalog::log::ChangeSet,
) -> (Option<Vec<InoId>>, Option<Vec<NameId>>) {
    if session.view().generation().checkpoint != previous.checkpoint {
        return (None, None);
    }
    let faulted = faulted_inodes(changes);
    let aliases = faulted
        .iter()
        .flat_map(|&id| session.names_for(id))
        .collect();
    (Some(faulted), Some(aliases))
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

pub(super) fn finish_report(
    report: &mut Report,
    catalog: &Catalog,
    plan: &Plan,
    changed: bool,
    faulted: Option<&[InoId]>,
    aliases: Option<&[NameId]>,
) {
    let started = Instant::now();
    report
        .coverage_faults
        .retain(|f| !crate::coverage::directory_denied(f));
    let seen = std::mem::take(&mut report.content_faults)
        .into_iter()
        .filter(|(path, _)| published_content_fault(catalog, path))
        .collect();
    report.content_faults = content_faults_with(catalog, &plan.refresh, seen, faulted, aliases);
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
    content_faults_with(catalog, refresh, seen, None, None)
}

fn content_faults_with(
    catalog: &Catalog,
    refresh: &[PathBuf],
    seen: Vec<(PathBuf, ContentFault)>,
    faulted: Option<&[InoId]>,
    aliases: Option<&[NameId]>,
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
    let report_alias = |id| {
        let name = catalog.name(id);
        if matches!(name.target(), ferret_catalog::Target::Ignored(_))
            || !fault(name.child)
            || !refreshed.contains(&root_of(name.parent))
        {
            return;
        }
        buf.clear();
        catalog.path(id, &mut buf);
        let path = PathBuf::from(OsStr::from_bytes(&buf));
        listed.entry(path).or_insert(ContentFault::Alias);
    };
    if let Some(aliases) = aliases {
        aliases.iter().copied().for_each(report_alias);
    } else {
        catalog.names().map(|(id, _)| id).for_each(report_alias);
    }
    listed.into_iter().collect()
}

/// A publication error that left the new generation visible takes the run's
/// report with it ([`IndexError::Undurable`]), so the roots D119 withdrew
/// from that generation are still reported. Any other error is returned as
/// it is: nothing was published, and the report would describe nothing.
pub(super) fn undurable(error: IndexError, report: &mut Report) -> IndexError {
    let published = match &error {
        IndexError::Commit(e) => e.published(),
        IndexError::Update(e) => e.published(),
        _ => false,
    };
    if !published {
        return error;
    }
    IndexError::Undurable {
        error: Box::new(error),
        report: Box::new(std::mem::take(report)),
    }
}

pub(super) fn update_error(error: ferret_catalog::log::Error) -> IndexError {
    match error {
        ferret_catalog::log::Error::Deferred(reason) => IndexError::DeferredBulk(reason),
        error => IndexError::Update(error),
    }
}
