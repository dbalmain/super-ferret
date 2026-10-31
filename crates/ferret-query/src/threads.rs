//! Starting worker threads: named for profiles, refused on demand in debug
//! builds, and after the process's descriptor table has grown; and the
//! [`Shortfall`] a run reports when the system refuses some.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{Scope, ScopedJoinHandle};

/// Starts thread `i` on the scope, named so profiles can tell it apart. The
/// process's first spawn reserves the descriptor table its workers share.
pub(crate) fn spawn_named<'scope, R: Send + 'scope>(
    scope: &'scope Scope<'scope, '_>,
    i: usize,
    name: &str,
    work: impl FnOnce() -> R + Send + 'scope,
) -> std::io::Result<ScopedJoinHandle<'scope, R>> {
    #[cfg(test)]
    if crate::verify::seam::Seams::current().fail_spawn_at == Some(i) {
        return Err(std::io::Error::other("injected spawn failure"));
    }
    #[cfg(not(test))]
    let _ = i;
    #[cfg(debug_assertions)]
    if refused_for_tests(name) {
        return Err(rustix::io::Errno::AGAIN.into());
    }
    reserve_descriptor_table();
    std::thread::Builder::new()
        .name(name.into())
        .spawn_scoped(scope, work)
}

/// Whether `FERRET_TEST_REFUSE_THREADS=NAME=K` refuses this spawn, as
/// `pthread_create` does under `RLIMIT_NPROC`: the first `K` threads named
/// `NAME` start, and later ones are refused. Debug builds only, so the CLI's
/// tests can refuse one kind of thread at a time.
#[cfg(debug_assertions)]
fn refused_for_tests(name: &str) -> bool {
    static LIMIT: std::sync::OnceLock<Option<(String, usize)>> = std::sync::OnceLock::new();
    static STARTED: AtomicUsize = AtomicUsize::new(0);
    let limit = LIMIT.get_or_init(|| {
        let value = std::env::var("FERRET_TEST_REFUSE_THREADS").ok()?;
        let (name, count) = value.rsplit_once('=')?;
        Some((name.to_owned(), count.parse().ok()?))
    });
    limit.as_ref().is_some_and(|(refused, count)| {
        refused == name && STARTED.fetch_add(1, Ordering::Relaxed) >= *count
    })
}

/// Search threads the system refused to start: the run went on with
/// `running` of the `asked` threads.
#[derive(Debug)]
pub struct Shortfall {
    /// Threads searching, the calling thread included when it searches.
    pub running: usize,
    /// Threads the run asked for.
    pub asked: usize,
    /// The first refusal.
    pub cause: std::io::Error,
}

/// Grows the process's descriptor table to 1024 slots, once, before the first
/// worker shares it. Growing a table that several threads share waits out an
/// RCU grace period while every other opener waits on the resize, and a deep
/// tree's held directory chains (D21) reach a few hundred descriptors, so the
/// table used to grow several times mid-search: 46–49 ms of a 520 ms index
/// grep at 32 threads on a 300k-file tree (kdiag, 2026-10-20). Growing it is
/// cheap only while the process has one thread: a parallel `--files`, which
/// spawned its writer's thread first, waited 20 ms here. A host calls this
/// before its first thread; every worker spawn calls it too. Best effort:
/// under a low open-file limit, or with no descriptor 2 to duplicate, the
/// table grows as it did before.
pub fn reserve_descriptor_table() {
    static RESERVED: std::sync::Once = std::sync::Once::new();
    RESERVED.call_once(grow_descriptor_table);
}

fn grow_descriptor_table() {
    use std::os::fd::AsFd;
    let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile)
        .current
        .unwrap_or(u64::MAX);
    let Ok(top) = i32::try_from(limit.saturating_sub(1).min(1023)) else {
        return;
    };
    // Dropped at once: the table keeps its size after the slot is freed.
    let _ = rustix::io::fcntl_dupfd_cloexec(std::io::stderr().as_fd(), top);
}

/// A pool lock; a panicking worker holds none across user code, so what it
/// guards is consistent.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn wait<'a, T>(signal: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    signal.wait(guard).unwrap_or_else(PoisonError::into_inner)
}
