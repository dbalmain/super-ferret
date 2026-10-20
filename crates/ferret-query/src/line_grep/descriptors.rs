//! Admission for grep's descriptor-using operations. The host charges both
//! listing/policy steps and complete reads, including their context spools.

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Shared admission for grep reads and listings. One admission reserves the
/// operation's entire descriptor footprint; it must never acquire another
/// admission while reading, rendering, or publishing that file.
pub struct IoBudget {
    available: Option<Mutex<usize>>,
    freed: Condvar,
}

impl IoBudget {
    /// Leaves both retained-directory caches their quarter of RLIMIT_NOFILE
    /// (each capped at 1,024), plus sixteen descriptors for the host. Each
    /// admitted operation gets six: a parent/input pair, two simultaneous
    /// context spools during compaction, and two spare for directory walks
    /// and policy loading. Always admits at least one operation, preserving
    /// the single-worker floor at very small limits.
    ///
    /// When every worker plus the feed producer fits, admissions are free of
    /// locking. At limit 1,024, sixteen workers take this path.
    pub fn new(workers: usize) -> Self {
        let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile).current;
        let slots = capacity(limit);
        Self {
            available: (slots <= workers).then(|| Mutex::new(slots)),
            freed: Condvar::new(),
        }
    }

    /// Waits before opening any transient descriptors. Cancellation is
    /// polled while asleep; dropping the admission wakes another operation.
    ///
    /// Hold this through the closing version check. Waiting for capture
    /// bytes or the publisher is safe only because the active streaming
    /// reader already owns its full descriptor allowance: neither it nor
    /// the publisher may wait for another admission. In particular, future
    /// capture-budget waits must not release and reacquire this admission
    /// with an input or context spool still open.
    pub fn acquire(&self, stopped: &dyn Fn() -> bool) -> Option<IoPermit<'_>> {
        if let Some(available) = &self.available {
            let mut left = lock(available);
            while *left == 0 {
                if stopped() {
                    return None;
                }
                (left, _) = self
                    .freed
                    .wait_timeout(left, Duration::from_millis(20))
                    .unwrap_or_else(PoisonError::into_inner);
            }
            if stopped() {
                return None;
            }
            *left -= 1;
        }
        Some(IoPermit(self))
    }
}

/// A complete operation's allowance, returned after its descriptors close.
pub struct IoPermit<'a>(&'a IoBudget);

impl Drop for IoPermit<'_> {
    fn drop(&mut self) {
        if let Some(available) = &self.0.available {
            *lock(available) += 1;
            self.0.freed.notify_one();
        }
    }
}

fn capacity(limit: Option<u64>) -> usize {
    limit.map_or(usize::MAX, |limit| {
        let retained = (limit / 4).min(1024) * 2;
        (limit.saturating_sub(retained + 16) / 6).max(1) as usize
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headroom_preserves_the_single_worker_floor_and_default_parallelism() {
        assert_eq!(capacity(Some(8)), 1);
        assert_eq!(capacity(Some(64)), 2);
        assert!(capacity(Some(1024)) > 16);
        assert_eq!(capacity(None), usize::MAX);
    }

    #[test]
    fn waiting_admission_is_returned_and_cancellable() {
        let budget = IoBudget {
            available: Some(Mutex::new(1)),
            freed: Condvar::new(),
        };
        let permit = budget.acquire(&|| false).unwrap();
        assert!(budget.acquire(&|| true).is_none());
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| budget.acquire(&|| false).unwrap());
            drop(permit);
            drop(waiting.join().unwrap());
        });
        assert_eq!(*lock(budget.available.as_ref().unwrap()), 1);
    }
}
