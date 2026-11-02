//! Where a run's observations come from, and observing a plan's refreshed
//! roots through it.

use ferret_catalog::{Content, Stat};

use super::*;
use crate::index::hasher::Output;
use crate::observe::Cache;
use crate::walk::{WalkOptions, walk_parallel};

#[derive(Clone, Copy)]
pub(super) enum Source<'a> {
    Checkpoint(&'a Transaction),
    Bounded(
        &'a WriterSession,
        u32,
        &'a std::sync::Arc<ferret_catalog::InputBudget>,
    ),
    Rebuild(&'a WriterSession, u32, usize),
}
impl Source<'_> {
    pub(super) fn batch(self) -> ferret_catalog::Batch {
        match self {
            Self::Checkpoint(txn) => txn.batch(),
            Self::Bounded(session, _, budget) => {
                session.batch().with_input_budget((*budget).clone())
            }
            Self::Rebuild(session, _, hint) => session.checkpoint_batch(hint),
        }
    }
    pub(super) fn budget(self) -> Option<std::sync::Arc<ferret_catalog::InputBudget>> {
        match self {
            Self::Bounded(_, _, budget) => Some(budget.clone()),
            _ => None,
        }
    }
    pub(super) fn carry(self, stat: &Stat) -> Option<Content> {
        match self {
            Self::Checkpoint(txn) => txn.carry(stat),
            Self::Bounded(session, sniffer, _) | Self::Rebuild(session, sniffer, _) => {
                session.carry(stat, sniffer)
            }
        }
    }
}

/// One observation of a plan's refreshed roots: the batches, and the root each
/// batch came from.
pub(super) struct Observed {
    pub(super) batches: Vec<ferret_catalog::Batch>,
    pub(super) owners: Vec<PathBuf>,
}

pub(super) fn observe(
    source: Source<'_>,
    plan: &Plan,
    options: &IndexOptions,
) -> Result<(Observed, Report), IndexError> {
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
    let mut owners = Vec::new();
    for root in &plan.refresh {
        let bulk_read = matches!(source, Source::Rebuild(..))
            || plan.selections.get(root).is_none_or(|s| s.whole_root());
        if bulk_read {
            admit_bulk(source, options)?;
        }
        if !plan.selections.contains_key(root)
            && let Some(watch) = &options.watch
        {
            watch.begin_policy_root(root);
        }
        let mut visitors = walk_root(source, &cache, root, plan, options, bulk_read);
        if let Some(error) = visitors.iter_mut().find_map(|v| v.priority_error.take()) {
            return Err(IndexError::Priority(error));
        }
        within_budget(source)?;
        let outputs: Vec<Output> = visitors
            .into_iter()
            .map(|v| v.finish(&mut faults))
            .collect();
        let mut observation_rows = 0;
        let mut observation_bytes = 0;
        for mut output in outputs {
            output.resolve(&cache);
            output.batch.seal();
            within_budget(source)?;
            let (rows, bytes) = output.batch.observation_peak();
            observation_rows += rows;
            observation_bytes += bytes;
            report.counts.add(&output.counts);
            report.hash_time += output.read_time;
            report.content_faults.append(&mut output.content_faults);
            report.pattern_errors.append(&mut output.pattern_errors);
            batches.push(output.batch);
            owners.push(root.clone());
        }
        report.observation_rows_peak = report.observation_rows_peak.max(observation_rows);
        report.observation_bytes_peak = report.observation_bytes_peak.max(observation_bytes);
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
    report.coverage_faults = faults;
    Ok((Observed { batches, owners }, report))
}

/// Admits a bulk read of the next root from a bounded source with a bulk
/// control. Between roots is a safe phase boundary: a refusal drops all
/// unpublished batches, and no partial listing is published or cancelled.
fn admit_bulk(source: Source<'_>, options: &IndexOptions) -> Result<(), IndexError> {
    if let Source::Bounded(session, ..) = source
        && let Some(control) = &options.bulk
    {
        control
            .admit(ferret_catalog::bulk::Kind::FullRewalk, &session.view())
            .map_err(IndexError::DeferredBulk)?;
    }
    Ok(())
}

/// Fails with the input limit once a bounded source's budget is spent.
fn within_budget(source: Source<'_>) -> Result<(), IndexError> {
    if let Source::Bounded(_, _, budget) = source
        && budget.exceeded()
    {
        return Err(IndexError::Update(ferret_catalog::log::Error::InputLimit(
            budget.usage(),
        )));
    }
    Ok(())
}

/// Walks `root` in parallel, returning each worker's hasher. A bulk read
/// paces its reads by the bulk control's limiter.
fn walk_root<'a>(
    source: Source<'a>,
    cache: &'a Cache,
    root: &'a Path,
    plan: &'a Plan,
    options: &'a IndexOptions,
    bulk_read: bool,
) -> Vec<Hasher<'a>> {
    let walk_options = WalkOptions {
        workers: options.bulk.as_ref().map_or(options.workers, |control| {
            control.workers().min(options.workers.max(1))
        }),
        boundaries: plan.boundaries(root),
    };
    walk_parallel(
        root,
        options.global.as_deref(),
        options.config,
        &walk_options,
        || {
            let mut hasher = Hasher::with_source(source, cache, root);
            hasher.watch = options.watch.as_deref();
            // An unpaced bulk control is an explicit command. Its workers
            // keep the spawning thread's nice; background crawls still lower.
            hasher.lower_priority = options.bulk.as_ref().is_none_or(|control| control.paced());
            if bulk_read {
                hasher.reader.limiter = options
                    .bulk
                    .as_ref()
                    .filter(|control| control.paced())
                    .map(|control| control.limiter());
            }
            hasher.selection = plan.selections.get(root).map(std::convert::AsRef::as_ref);
            hasher
        },
    )
}
