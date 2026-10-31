//! The input bytes worker threads may hold at once, shared by a query's
//! verification pool and grep's completion groups.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};

use crate::threads::lock;

/// Document bytes the workers may hold at once, by catalogued size.
pub const IN_FLIGHT_BYTES: u64 = 64 << 20;

/// Bytes reserved by workers reading now. Every task's reservation writes
/// `used`, so the budget keeps cache lines of its own: the pool's stop flag,
/// which every worker reads between lines, would otherwise miss on each.
#[derive(Default)]
#[repr(align(128))]
pub(crate) struct Budget {
    used: AtomicU64,
    /// Workers sleeping for bytes to be freed.
    waiting: AtomicUsize,
    lock: Mutex<()>,
    freed: Condvar,
}

impl Budget {
    /// Waits until `size` bytes (at most the whole budget) fit, and reserves
    /// them; one reader always proceeds alone. `None` once `halt` says the
    /// holder or the query stopped, waiting or not, so no read starts after.
    pub(crate) fn reserve(
        &self,
        size: u64,
        halt: &dyn Fn() -> bool,
        waiting: &dyn Fn(),
    ) -> Option<u64> {
        let size = size.min(IN_FLIGHT_BYTES);
        let budget = self;
        let fits = |used: u64| used == 0 || used + size <= IN_FLIGHT_BYTES;
        loop {
            // A [`Budget::wake`] wakes this wait. A cancelled query does not,
            // but a reader holding bytes stops at its next chunk
            // and frees them, which does.
            if halt() {
                return None;
            }
            let used = budget.used.load(Ordering::SeqCst);
            if fits(used) {
                if budget
                    .used
                    .compare_exchange(used, used + size, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    return Some(size);
                }
                continue;
            }
            let guard = lock(&budget.lock);
            budget.waiting.fetch_add(1, Ordering::SeqCst);
            if !halt() && !fits(budget.used.load(Ordering::SeqCst)) {
                waiting();
                drop(
                    budget
                        .freed
                        .wait(guard)
                        .unwrap_or_else(PoisonError::into_inner),
                );
            }
            budget.waiting.fetch_sub(1, Ordering::SeqCst);
        }
    }

    pub(crate) fn release(&self, size: u64) {
        let budget = self;
        budget.used.fetch_sub(size, Ordering::SeqCst);
        if budget.waiting.load(Ordering::SeqCst) > 0 {
            drop(lock(&budget.lock));
            budget.freed.notify_all();
        }
    }

    /// Wakes every waiter to check its stop condition again, which the
    /// caller set first. Taking the lock orders that store before any
    /// sleeper's next check.
    pub(crate) fn wake(&self) {
        drop(lock(&self.lock));
        self.freed.notify_all();
    }
}
