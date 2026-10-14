//! Running a catalog's setup work on a few threads, with the calling thread
//! taking part, and without depending on any thread starting.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

/// Runs `work` on each of `items` and returns the results in `items`'
/// order, on the calling thread and up to `threads - 1` helpers.
///
/// Each item is claimed once, in order, by whichever thread gets to it. A
/// helper that cannot be started leaves its share to the threads that did,
/// the calling thread at least, so the results never depend on how many
/// threads ran, and a refused thread is not an error. With one thread, or
/// one item, nothing is spawned.
pub(crate) fn fan_out<I: Send, T: Send>(
    items: Vec<I>,
    threads: usize,
    work: impl Fn(I) -> T + Sync,
) -> Vec<T> {
    let helpers = threads.min(items.len()).saturating_sub(1);
    if helpers == 0 {
        return items.into_iter().map(work).collect();
    }
    let slots: Vec<Mutex<(Option<I>, Option<T>)>> = items
        .into_iter()
        .map(|item| Mutex::new((Some(item), None)))
        .collect();
    let next = AtomicUsize::new(0);
    let run = || {
        while let Some(slot) = slots.get(next.fetch_add(1, Ordering::Relaxed)) {
            let item = lock(slot).0.take();
            if let Some(item) = item {
                let done = work(item);
                lock(slot).1 = Some(done);
            }
        }
    };
    #[cfg(test)]
    let seams = seam::current();
    std::thread::scope(|scope| {
        for helper in 0..helpers {
            #[cfg(test)]
            let run = || {
                let _seams = seams.clone().set();
                run();
            };
            if spawn(scope, helper, run).is_err() {
                break;
            }
        }
        run();
    });
    // The scope has joined every helper, and re-raised any panic, so every
    // item has run.
    slots
        .into_iter()
        .map(|slot| {
            let (_, done) = slot.into_inner().unwrap_or_else(PoisonError::into_inner);
            done.unwrap_or_else(|| unreachable!("every item runs"))
        })
        .collect()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn spawn<'scope>(
    scope: &'scope std::thread::Scope<'scope, '_>,
    helper: usize,
    work: impl FnOnce() + Send + 'scope,
) -> std::io::Result<()> {
    #[cfg(test)]
    if !seam::spawn(helper) {
        return Err(std::io::Error::other("injected spawn failure"));
    }
    #[cfg(not(test))]
    let _ = helper;
    std::thread::Builder::new()
        .name("ferret-catalog".into())
        .spawn_scoped(scope, work)
        .map(drop)
}

/// Test-only control of [`fan_out`]'s helpers, set per thread and carried
/// into the helpers it starts: a helper index from which spawning fails,
/// and a count of the spawns asked for.
#[cfg(test)]
pub(crate) mod seam {
    use std::cell::RefCell;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Default)]
    pub(crate) struct Seams {
        /// Helpers from this index on fail to start.
        pub fail_from: Option<usize>,
        /// Spawns asked for, failed or not.
        pub asked: Arc<AtomicUsize>,
    }

    thread_local! {
        static SEAMS: RefCell<Seams> = RefCell::default();
    }

    pub(crate) fn current() -> Seams {
        SEAMS.with(|seams| seams.borrow().clone())
    }

    /// Whether helper `index` may start; counts the request.
    pub(super) fn spawn(index: usize) -> bool {
        SEAMS.with(|seams| {
            let seams = seams.borrow();
            seams.asked.fetch_add(1, Ordering::Relaxed);
            seams.fail_from.is_none_or(|from| index < from)
        })
    }

    impl Seams {
        /// Sets this thread's seams until the guard drops.
        pub(crate) fn set(self) -> impl Drop {
            struct Reset;
            impl Drop for Reset {
                fn drop(&mut self) {
                    SEAMS.with(|seams| *seams.borrow_mut() = Seams::default());
                }
            }
            SEAMS.with(|seams| *seams.borrow_mut() = self);
            Reset
        }
    }
}
