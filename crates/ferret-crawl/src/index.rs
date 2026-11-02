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
//! trustworthy scopes can publish. A missing anchor withdraws a new root that
//! is disjoint from the others (D119); otherwise missing anchors, changed
//! global versions and uncertain root boundaries block the transaction. Content
//! faults publish valid namespace/stat observations as Fault/no-document and
//! retry next run.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ferret_catalog::{BeginError, Catalog, InoId, Transaction, WriterSession};
use ferret_policy::Config;

mod hasher;
mod plan;
mod report;
mod source;

pub(crate) use hasher::Hasher;
use hasher::Output;
#[cfg(test)]
pub(crate) use hasher::{DRAIN_MIN, Deferred, PROBES, Probe};
use plan::{Plan, configured_roots, normalise, strictly_inside};
#[cfg(test)]
pub(crate) use report::content_faults;
pub use report::{
    Counts, CoverageContext, CoverageFault, FailedRoot, IndexError, Published, Report,
};
use report::{finish_report, reporting_ids, undurable, unpublished, update_error, withdrawn};
use source::{Observed, Source, observe};

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
    /// Optional daemon intake, armed before directory listing.
    pub watch: Option<std::sync::Arc<crate::watch::Watch>>,
    /// Host bulk admission and pacing; absent for direct foreground indexing.
    pub bulk: Option<std::sync::Arc<dyn ferret_catalog::bulk::Control>>,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            global: None,
            config: Config::default(),
            workers: crate::default_workers(),
            sniffer: ferret_policy::SNIFFER_VERSION,
            watch: None,
            bulk: None,
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
/// checked old scopes while trustworthy observations elsewhere publish. A new
/// disjoint root whose faults have no anchor is withdrawn and listed in
/// [`Report::failed_roots`] (D119). Any other unresolvable scope, a run that
/// withdrew every refreshed root, or an incompatible version/root transition
/// publishes nothing ([`IndexError::Coverage`]).
pub fn index(
    catalog_dir: &Path,
    roots: &[PathBuf],
    refresh: Refresh<'_>,
    options: &IndexOptions,
) -> Result<Report, IndexError> {
    run(
        catalog_dir,
        |_| Ok(roots.to_vec()),
        refresh,
        options,
        |_| (),
    )
    .map(|(report, ())| report)
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

/// Opens the writer session for `dir`, first publishing an empty catalog
/// under the same lock when nothing is published there. A daemon started
/// before any `ferret index` serves commands that add roots to it.
pub fn open_or_create(
    dir: &Path,
    options: &IndexOptions,
) -> Result<WriterSession, ferret_catalog::log::Error> {
    WriterSession::open_or_create(dir, options.sniffer, fingerprint(options))
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
        |previous| changed_roots(previous, change),
        refresh,
        options,
        |_| (),
    )
    .map(|(report, ())| report)
}

/// Publishes a root change, then calls `after` under the same writer lock.
/// The callback observes the committed catalog; its result is separate because
/// it cannot undo that publication. Used by the one-shot host for content.
pub fn index_change_then<T>(
    catalog_dir: &Path,
    change: RootChange<'_>,
    refresh: Refresh<'_>,
    options: &IndexOptions,
    after: impl FnOnce(&Catalog) -> T,
) -> Result<(Report, T), IndexError> {
    run(
        catalog_dir,
        |previous| changed_roots(previous, change),
        refresh,
        options,
        after,
    )
}

/// Applies root edits under an already retained session lock, using the same
/// D34 widening and publication producer as the foreground index command.
pub fn session_change(
    session: &mut WriterSession,
    change: RootChange<'_>,
    refresh: Refresh<'_>,
    options: &IndexOptions,
) -> Result<Report, IndexError> {
    let roots = changed_roots(Some(&session.view()), change)?;
    recrawl(session, &roots, refresh, options)
}

fn changed_roots(
    previous: Option<&Catalog>,
    change: RootChange<'_>,
) -> Result<Vec<PathBuf>, IndexError> {
    let mut roots = configured_roots(previous);
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
}

/// One run, with the configured roots decided by `roots` from the previous
/// generation once the lock is held.
fn run<T>(
    catalog_dir: &Path,
    roots: impl FnOnce(Option<&Catalog>) -> Result<Vec<PathBuf>, IndexError>,
    refresh: Refresh<'_>,
    options: &IndexOptions,
    after: impl FnOnce(&Catalog) -> T,
) -> Result<(Report, T), IndexError> {
    let after_time = std::cell::Cell::new(Duration::ZERO);
    let after = |catalog: &Catalog| {
        let started = Instant::now();
        let result = after(catalog);
        after_time.set(started.elapsed());
        result
    };
    match WriterSession::open(catalog_dir) {
        Ok(session) => run_session(session, roots, refresh, options, after, &after_time),
        Err(ferret_catalog::log::Error::MissingCheckpoint)
        | Err(ferret_catalog::log::Error::Previous(ferret_catalog::OpenError::Decode(
            ferret_catalog::DecodeError::Version(_),
        ))) => run_initial(catalog_dir, roots, refresh, options, after, &after_time),
        Err(ferret_catalog::log::Error::Locked) => Err(IndexError::Begin(BeginError::Locked)),
        Err(error) => Err(IndexError::Update(error)),
    }
}

/// [`run`] over an open writer session: a log transaction when reconciling
/// produced changes, else a new checkpoint. `after_time` holds how long
/// `after` took, which the commit time excludes.
fn run_session<T>(
    mut session: WriterSession,
    roots: impl FnOnce(Option<&Catalog>) -> Result<Vec<PathBuf>, IndexError>,
    refresh: Refresh<'_>,
    options: &IndexOptions,
    after: impl FnOnce(&Catalog) -> T,
    after_time: &std::cell::Cell<Duration>,
) -> Result<(Report, T), IndexError> {
    session.set_bulk_control(options.bulk.clone());
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
    } = match observe_reconcile(&session, &mut plan, options) {
        Err(IndexError::Update(ferret_catalog::log::Error::InputLimit(usage))) => {
            // `roots`, not `plan.roots`: D119 may have withdrawn a root from
            // the plan, and the rewalk must find and report it again.
            let report = full_rewalk(&mut session, &roots, options, usage)?;
            return Ok((report, after(&session.view())));
        }
        result => result?,
    };
    let started = Instant::now();
    let (catalog, changed, faulted, aliases, result) = match changes {
        Some(changes) => {
            let changed = !changes.records.is_empty();
            let catalog = session
                .commit(&changes, options.sniffer)
                .map_err(|e| undurable(update_error(e), &mut report))?;
            let (faulted, aliases) = reporting_ids(&session, previous.generation(), &changes);
            let result = after(&catalog);
            (catalog, changed, faulted, aliases, result)
        }
        None => {
            let txn = session.into_checkpoint(options.sniffer);
            let (catalog, result) = checkpoint(txn, batches, &plan, options, after, &mut report)?;
            (catalog, true, None, None, result)
        }
    };
    report.commit_time += started.elapsed().saturating_sub(after_time.get());
    finish_report(
        &mut report,
        &catalog,
        &plan,
        changed,
        faulted.as_deref(),
        aliases.as_deref(),
    );
    Ok((report, result))
}

/// [`run`] where no log can be resumed: a full walk into a new checkpoint.
fn run_initial<T>(
    catalog_dir: &Path,
    roots: impl FnOnce(Option<&Catalog>) -> Result<Vec<PathBuf>, IndexError>,
    refresh: Refresh<'_>,
    options: &IndexOptions,
    after: impl FnOnce(&Catalog) -> T,
    after_time: &std::cell::Cell<Duration>,
) -> Result<(Report, T), IndexError> {
    let txn = Transaction::begin(catalog_dir, options.sniffer).map_err(IndexError::Begin)?;
    let roots = roots(txn.previous())?;
    let mut plan = Plan::new(
        txn.previous(),
        &roots,
        refresh,
        options.sniffer,
        fingerprint(options),
    )?;
    let (observed, mut report) = observe(Source::Checkpoint(&txn), &plan, options)?;
    let batches = initial(&txn, &mut plan, observed, &mut report)?;
    let started = Instant::now();
    let (catalog, result) = checkpoint(txn, batches, &plan, options, after, &mut report)?;
    report.commit_time += started.elapsed().saturating_sub(after_time.get());
    finish_report(&mut report, &catalog, &plan, true, None, None);
    Ok((report, result))
}

/// Commits `batches` and the plan's kept roots as a new checkpoint, then
/// runs `after` on it.
fn checkpoint<T>(
    mut txn: Transaction,
    batches: Vec<ferret_catalog::Batch>,
    plan: &Plan,
    options: &IndexOptions,
    after: impl FnOnce(&Catalog) -> T,
    report: &mut Report,
) -> Result<(Catalog, T), IndexError> {
    txn.set_policy(fingerprint(options));
    for batch in batches {
        txn.add(batch);
    }
    for root in &plan.keep {
        txn.keep(root.as_os_str().as_bytes())
            .map_err(IndexError::Keep)?;
    }
    txn.commit_then(after)
        .map_err(|e| undurable(IndexError::Commit(e), report))
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
    session.set_bulk_control(options.bulk.clone());
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
    } = match observe_reconcile(session, &mut plan, options) {
        Err(IndexError::Update(ferret_catalog::log::Error::InputLimit(usage))) => {
            // As in `run`: the requested roots, from before any withdrawal.
            let report = full_rewalk(session, roots, options, usage)?;
            let view = session.view();
            return Ok((
                report,
                ferret_catalog::log::ChangeSet {
                    records: Vec::new(),
                    counters: [view.next_inode().0, view.next_name().0, view.next_doc().0],
                    counts: [
                        view.inode_count(),
                        view.name_count(),
                        view.dir_count(),
                        view.doc_count(),
                    ],
                },
            ));
        }
        result => result?,
    };
    let started = Instant::now();
    let Some(changes) = changes else {
        return Err(IndexError::NeedsCheckpoint {
            report: Box::new(report),
        });
    };
    let changed = !changes.records.is_empty();
    let catalog = session
        .commit(&changes, options.sniffer)
        .map_err(|e| undurable(update_error(e), &mut report))?;
    let (faulted, aliases) = reporting_ids(session, previous.generation(), &changes);
    report.commit_time += started.elapsed();
    finish_report(
        &mut report,
        &catalog,
        &plan,
        changed,
        faulted.as_deref(),
        aliases.as_deref(),
    );
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
    let budget = std::sync::Arc::new(ferret_catalog::InputBudget::new(session.input_limits()));
    let source = Source::Bounded(session, options.sniffer, &budget);
    let (mut walked, mut report) = observe(source, plan, options)?;
    loop {
        let started = Instant::now();
        let protection = protect(session, plan, &mut walked, &mut report)?;
        if !protection.is_empty()
            && (session.view().policy() != fingerprint(options)
                || session.view().sniffer_version() != options.sniffer)
        {
            return Err(unpublished(report, Vec::new()));
        }
        report.protected_scopes =
            protection.directories.len() + protection.edges.len() + protection.opaque.len();
        let changes = crate::reconcile::with_budget(
            session,
            &walked.batches,
            &plan.refresh,
            &plan.dropped,
            fingerprint(options),
            options.sniffer,
            (&protection, budget.clone()),
        )
        .map_err(update_error)?;
        report.commit_time += started.elapsed();
        report.input_usage = budget.usage();
        let Some(ref final_changes) = changes else {
            return Ok(Reconciled {
                batches: walked.batches,
                report,
                changes: None,
            });
        };
        match rewalk(session, plan, final_changes) {
            Rewalk::Nothing => {
                return Ok(Reconciled {
                    batches: walked.batches,
                    report,
                    changes,
                });
            }
            Rewalk::Expanded => {
                // Re-observe the expanded final scopes together: duplicate root
                // token graphs from a second partial pass cannot be reconciled.
                let (fresh, fresh_report) = observe(source, plan, options)?;
                walked = fresh;
                let failed = std::mem::take(&mut report.failed_roots);
                report = fresh_report;
                report.failed_roots = failed;
            }
            Rewalk::Roots(extra) => {
                observe_kept(source, plan, options, extra, &mut walked, &mut report)?;
            }
        }
    }
}

/// What a reconciliation's stale aliases still need observed.
enum Rewalk {
    /// Every refreshed alias supplied its stat.
    Nothing,
    /// Selections were widened to the aliases; observe every scope again.
    Expanded,
    /// Kept roots holding aliases, to refresh as well.
    Roots(Vec<PathBuf>),
}

/// A deleted last refreshed alias supplies no stat. Rewalk its kept root so
/// nlink/ctime and content are observed, rather than publishing a stale shared
/// row. M6 can narrow this to handle-relative alias scopes.
fn rewalk(
    session: &WriterSession,
    plan: &Plan,
    changes: &ferret_catalog::log::ChangeSet,
) -> Rewalk {
    let previous = session.view();
    let missing = stale_aliases(session, &previous, changes);
    if missing.is_empty() {
        return Rewalk::Nothing;
    }
    let mut extra = BTreeSet::new();
    let removed: BTreeSet<_> = changes
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
        Rewalk::Expanded
    } else if extra.is_empty() {
        Rewalk::Nothing
    } else {
        Rewalk::Roots(extra.into_iter().collect())
    }
}

/// Live non-directory inodes that lost some but not all names and were not
/// re-observed, so they have no fresh stat to publish.
fn stale_aliases(
    session: &WriterSession,
    previous: &Catalog,
    changes: &ferret_catalog::log::ChangeSet,
) -> BTreeSet<u32> {
    let observed: BTreeSet<_> = changes
        .records
        .iter()
        .filter_map(|r| match r {
            ferret_catalog::log::Record::InodePut { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    changes
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
        .collect()
}

/// Refreshes kept roots `extra` and folds their walk into this pass's.
fn observe_kept(
    source: Source<'_>,
    plan: &mut Plan,
    options: &IndexOptions,
    extra: Vec<PathBuf>,
    walked: &mut Observed,
    report: &mut Report,
) -> Result<(), IndexError> {
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
    walked.batches.extend(more.batches);
    walked.owners.extend(more.owners);
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
    Ok(())
}

/// Resolves a bounded observation's faults to protection. When some fault has
/// no anchor, first withdraws each new disjoint root whose own faults cannot be
/// anchored (D119), then resolves what remains; a run left with no refreshed
/// root, or still unresolvable, publishes nothing.
fn protect(
    session: &WriterSession,
    plan: &mut Plan,
    observed: &mut Observed,
    report: &mut Report,
) -> Result<crate::coverage::Protection, IndexError> {
    let resolve = |plan: &Plan, observed: &Observed, faults: &[CoverageFault]| {
        crate::coverage::resolve(session, &observed.batches, faults, &plan.roots)
    };
    if let Some(protection) = resolve(plan, observed, &report.coverage_faults) {
        return Ok(protection);
    }
    let previous = configured_roots(Some(&session.view()));
    let configured = plan.roots.clone();
    let failed = withdraw(
        plan,
        &previous,
        observed,
        &mut report.coverage_faults,
        |batches, faults| crate::coverage::resolve(session, batches, faults, &configured).is_none(),
    );
    let protection = if failed.is_empty() || plan.refresh.is_empty() {
        None
    } else {
        resolve(plan, observed, &report.coverage_faults)
    };
    match protection {
        Some(protection) => {
            withdrawn(report, plan, failed);
            Ok(protection)
        }
        None => Err(unpublished(std::mem::take(report), failed)),
    }
}

fn fingerprint(options: &IndexOptions) -> ferret_catalog::Hash {
    let mut hash = blake3::Hasher::new();
    // v2: `.ignore` and `.rgignore` became policy inputs (D126).
    hash.update(b"ferret-policy-v2\0");
    hash.update(&options.config.size_cap.to_le_bytes());
    hash.update(&[u8::from(options.global.is_some())]);
    hash.update(options.global.as_deref().unwrap_or_default().as_bytes());
    let mut fingerprint = [0; 16];
    fingerprint.copy_from_slice(&hash.finalize().as_bytes()[..16]);
    fingerprint
}

/// Settles an initial checkpoint's faults. D26 directory EACCES is opaque even
/// for an initial root; every other initial fault lacks a trustworthy
/// retention anchor, so its root is withdrawn where D119 allows and the run
/// publishes nothing otherwise.
fn initial(
    txn: &Transaction,
    plan: &mut Plan,
    mut observed: Observed,
    report: &mut Report,
) -> Result<Vec<ferret_catalog::Batch>, IndexError> {
    if report.coverage_faults.is_empty() {
        return Ok(observed.batches);
    }
    let previous = configured_roots(txn.previous());
    let failed = withdraw(
        plan,
        &previous,
        &mut observed,
        &mut report.coverage_faults,
        |_, faults| !faults.iter().all(crate::coverage::directory_denied),
    );
    let faults = &mut report.coverage_faults;
    let settled = if faults.is_empty() {
        Some(observed.batches)
    } else if faults.iter().all(crate::coverage::directory_denied) {
        crate::coverage::opaque_checkpoint(txn, &observed.batches, faults)
    } else {
        None
    };
    match settled {
        Some(batches) if failed.is_empty() || !plan.refresh.is_empty() => {
            faults.clear();
            withdrawn(report, plan, failed);
            Ok(batches)
        }
        _ => Err(unpublished(std::mem::take(report), failed)),
    }
}

/// D119: withdraws from this run each refreshed root that the previous
/// generation lacks, that is disjoint from every other configured root, and
/// whose own batches and faults `unusable` rejects. Its observations and
/// faults leave the run and it leaves the plan's roots, so no generation
/// records it. Overlapping roots stay atomic: a nested root changes which
/// rules govern its outer root's subtree (D34), so its failure is not its own.
fn withdraw(
    plan: &mut Plan,
    previous: &[PathBuf],
    observed: &mut Observed,
    faults: &mut Vec<CoverageFault>,
    mut unusable: impl FnMut(&[ferret_catalog::Batch], &[CoverageFault]) -> bool,
) -> Vec<FailedRoot> {
    let candidates: Vec<PathBuf> = plan
        .refresh
        .iter()
        .filter(|root| !previous.contains(root))
        .filter(|root| {
            plan.roots
                .iter()
                .all(|other| !strictly_inside(other, root) && !strictly_inside(root, other))
        })
        .filter(|root| faults.iter().any(|f| f.root == **root))
        .cloned()
        .collect();
    let mut failed = Vec::new();
    for root in candidates {
        // Selected by owner, not by position, so another root's batches can
        // never leave with this one's. A root that stays gets its batches back
        // in their places.
        let mine: Vec<bool> = observed.owners.iter().map(|o| *o == root).collect();
        let (mut own_batches, mut rest_batches) = (Vec::new(), Vec::new());
        for (batch, &own) in std::mem::take(&mut observed.batches).into_iter().zip(&mine) {
            if own {
                own_batches.push(batch);
            } else {
                rest_batches.push(batch);
            }
        }
        let (own, rest): (Vec<_>, Vec<_>) = std::mem::take(faults)
            .into_iter()
            .partition(|f| f.root == root);
        *faults = rest;
        if !unusable(&own_batches, &own) {
            let (mut own_batches, mut rest_batches) =
                (own_batches.into_iter(), rest_batches.into_iter());
            observed.batches = mine
                .iter()
                .filter_map(|&own| {
                    if own {
                        own_batches.next()
                    } else {
                        rest_batches.next()
                    }
                })
                .collect();
            faults.extend(own);
            continue;
        }
        observed.batches = rest_batches;
        observed.owners.retain(|o| *o != root);
        plan.roots.retain(|r| *r != root);
        plan.refresh.retain(|r| *r != root);
        plan.selections.remove(&root);
        failed.push(FailedRoot { root, faults: own });
    }
    failed
}

// All configured roots are observed again: scoped batches cannot describe a
// checkpoint. The abandoned attempt has dropped before this function starts.
fn full_rewalk(
    session: &mut WriterSession,
    roots: &[PathBuf],
    options: &IndexOptions,
    usage: ferret_catalog::InputUsage,
) -> Result<Report, IndexError> {
    session.admit_full_rewalk().map_err(update_error)?;
    let old = session.view();
    let mut plan = Plan::new(
        Some(&old),
        roots,
        Refresh::All,
        options.sniffer,
        fingerprint(options),
    )?;
    let (mut observed, mut report) = observe(
        Source::Rebuild(
            session,
            options.sniffer,
            (old.name_count().saturating_sub(old.dir_count()) as usize).div_ceil(
                options
                    .workers
                    .max(1)
                    .saturating_mul(plan.roots.len().max(1)),
            ),
        ),
        &plan,
        options,
    )?;
    if !report.coverage_faults.is_empty() {
        // Resolved twice only on a faulted rebuild, to find D119 roots.
        protect(session, &mut plan, &mut observed, &mut report)?;
    }
    let (rebuilt, scopes) = crate::reconcile::checkpoint_batches(
        session,
        observed.batches,
        &report.coverage_faults,
        &plan.roots,
        options.sniffer,
        fingerprint(options),
    );
    report.input_usage = usage;
    report.input_fallback = true;
    report.protected_scopes = scopes;
    let Some(batches) = rebuilt else {
        return Err(unpublished(report, Vec::new()));
    };
    drop(old);
    let started = Instant::now();
    let catalog = session
        .rebuild_checkpoint(batches, options.sniffer, fingerprint(options))
        .map_err(|e| undurable(update_error(e), &mut report))?;
    report.commit_time += started.elapsed();
    report.input_usage = usage;
    report.input_fallback = true;
    finish_report(&mut report, &catalog, &plan, true, None, None);
    Ok(report)
}
