//! Settling a run's coverage faults: protection for a bounded observation,
//! the initial checkpoint's faults, and D119's withdrawal of a new disjoint
//! root that cannot be anchored.

use super::plan::strictly_inside;
use super::report::withdrawn;
use super::*;

/// Resolves a bounded observation's faults to protection. When some fault has
/// no anchor, first withdraws each new disjoint root whose own faults cannot be
/// anchored (D119), then resolves what remains; a run left with no refreshed
/// root, or still unresolvable, publishes nothing.
pub(super) fn protect(
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

/// Settles an initial checkpoint's faults. D26 directory EACCES is opaque even
/// for an initial root; every other initial fault lacks a trustworthy
/// retention anchor, so its root is withdrawn where D119 allows and the run
/// publishes nothing otherwise.
pub(super) fn initial(
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
