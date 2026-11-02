//! Query admission and the shared pool of query-worker permits.

use std::sync::MutexGuard;

use super::*;

pub(super) struct QueryPermit<'a>(&'a Host);
impl Drop for QueryPermit<'_> {
    fn drop(&mut self) {
        lock(&self.0.lifecycle).queries -= 1;
        self.0.lifecycle_changed.notify_one();
    }
}

/// One pool of query-worker permits, `min(16, CPUs)` via
/// [`ferret_crawl::default_workers`]. Find, search and grep all draw from it.
/// Admission ([`QUERIES`]) is a separate limit.
pub(super) struct Permits {
    budget: usize,
    state: Mutex<PermitState>,
}
struct PermitState {
    free: usize,
    /// Parallelism of each in-flight grant, in the order it was taken.
    running: Vec<usize>,
}
/// One query's permits. Drop returns them after that query's workers join.
pub(super) struct Grant<'a> {
    permits: &'a Permits,
    /// Returned to the pool. Zero when the pool was empty and the query kept
    /// its connection thread without charging past the budget.
    charged: usize,
    workers: usize,
}
impl Permits {
    pub(super) fn new(budget: usize) -> Self {
        let budget = budget.max(1);
        Self {
            budget,
            state: Mutex::new(PermitState {
                free: budget,
                running: Vec::new(),
            }),
        }
    }

    /// Grants `min(asked, free)` workers, and never fewer than one. Does not
    /// wait. An empty pool still grants the connection thread; that worker is
    /// not charged, so the charged total stays inside the budget. Waiting
    /// could deadlock: an admitted query blocked on a permit another query
    /// holds, while that query is blocked on this connection.
    pub(super) fn take(&self, asked: usize) -> Grant<'_> {
        let asked = asked.max(1);
        let mut state = self.lock();
        let charged = asked.min(state.free);
        let workers = charged.max(1);
        state.free -= charged;
        state.running.push(workers);
        Grant {
            permits: self,
            charged,
            workers,
        }
    }

    /// The pool's size.
    pub(super) fn budget(&self) -> usize {
        self.budget
    }

    /// Charged permits, and the parallelism of each in-flight grant.
    pub(super) fn snapshot(&self) -> (usize, Vec<usize>) {
        let state = self.lock();
        (self.budget - state.free, state.running.clone())
    }

    fn lock(&self) -> MutexGuard<'_, PermitState> {
        lock(&self.state)
    }
}
impl Grant<'_> {
    pub(super) fn workers(&self) -> usize {
        self.workers
    }
}
impl Drop for Grant<'_> {
    fn drop(&mut self) {
        let mut state = self.permits.lock();
        state.free += self.charged;
        if let Some(index) = state
            .running
            .iter()
            .position(|&workers| workers == self.workers)
        {
            state.running.swap_remove(index);
        }
        self.charged = 0;
        debug_assert!(state.free <= self.permits.budget);
    }
}
pub(super) fn admit<'a>(host: &'a Host, cancelled: &AtomicBool) -> Option<QueryPermit<'a>> {
    loop {
        let mut state = lock(&host.lifecycle);
        if state.draining || cancelled.load(Ordering::Acquire) {
            return None;
        }
        if state.queries < host.query_limit {
            state.queries += 1;
            return Some(QueryPermit(host));
        }
        state = host
            .lifecycle_changed
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(state);
    }
}
