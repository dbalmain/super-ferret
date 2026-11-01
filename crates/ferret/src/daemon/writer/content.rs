//! Content follow at writer idle boundaries and an independently paced merge.
//! The writer owns the worker's lifetime: teardown cancels and joins it before
//! Engine releases the catalog lock. Watch refreshes never cancel a merge.

use std::io;
#[cfg(debug_assertions)]
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ferret_catalog::bulk::Control;

use super::{Host, Operation};
use crate::engine::Engine;
use crate::lock;

#[derive(Default)]
pub(super) struct State {
    merge: Option<JoinHandle<io::Result<()>>>,
    cancelled: Arc<AtomicBool>,
}
impl Drop for State {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(merge) = self.merge.take() {
            let _ = merge.join();
        }
    }
}

fn urgent(host: &Host, scheduler: &crate::scheduler::Scheduler) -> bool {
    host.stop.load(Ordering::Acquire)
        || host.writer_pending.load(Ordering::Acquire) != 0
        || scheduler.status().paused.is_some()
        || lock(&host.lifecycle).draining
}

pub(super) fn turn(
    host: &Arc<Host>,
    engine: &Arc<Engine>,
    scheduler: &Arc<crate::scheduler::Scheduler>,
    state: &mut State,
) -> io::Result<Duration> {
    if let Some(merge) = state.merge.take_if(|merge| merge.is_finished()) {
        merge
            .join()
            .map_err(|_| io::Error::other("content merge panicked"))??;
    }
    if let Some(wait) = follow(host, engine, scheduler)? {
        return Ok(wait);
    }
    if state.merge.is_some() {
        return Ok(Duration::from_millis(100));
    }
    start_merge(host, engine, scheduler, state)
}

/// Follows the catalog into the content index for one bounded slice. Returns
/// how long to wait before the next turn, or None once it is caught up and a
/// merge may start.
fn follow(
    host: &Arc<Host>,
    engine: &Arc<Engine>,
    scheduler: &Arc<crate::scheduler::Scheduler>,
) -> io::Result<Option<Duration>> {
    let started = Instant::now();
    let should_interrupt = || {
        urgent(host, scheduler)
            || lock(&host.writer_status)
                .watch
                .as_ref()
                .and_then(|w| w.next_due())
                .is_some_and(|due| due <= Instant::now())
    };
    #[cfg(debug_assertions)]
    let checks = std::sync::atomic::AtomicU64::new(0);
    let cancelled = || {
        #[cfg(debug_assertions)]
        {
            let checks = checks.fetch_add(1, Ordering::Relaxed) + 1;
            content_checkpoint(host, "follow", checks, &should_interrupt);
        }
        should_interrupt() || started.elapsed() >= Duration::from_millis(100)
    };
    if cancelled() {
        return Ok(Some(Duration::from_secs(1)));
    }
    let limiter = scheduler.limiter();
    let pace = |bytes: usize| {
        for chunk in (0..bytes).step_by(64 << 10) {
            limiter.transfer((bytes - chunk).min(64 << 10), || Ok(()))?;
        }
        Ok(())
    };
    let budget = ferret_index::Budget {
        bytes: 256 << 10,
        buffer: 1 << 20,
        segments: 1,
        pace: &pace,
        cancelled: &cancelled,
    };
    let operation = Operation::start(host, "follow");
    let followed = engine
        .try_follow_content(&budget, Some(limiter.clone()))
        .map_err(io::Error::other)?;
    drop(operation);
    let Some(followed) = followed else {
        return Ok(Some(Duration::from_millis(100)));
    };
    if cancelled() || followed.remaining > 0 {
        return Ok(Some(Duration::ZERO));
    }
    Ok(None)
}

/// Starts a background merge when the scheduler admits one, returning how long
/// to wait before the next turn.
fn start_merge(
    host: &Arc<Host>,
    engine: &Arc<Engine>,
    scheduler: &Arc<crate::scheduler::Scheduler>,
    state: &mut State,
) -> io::Result<Duration> {
    let pin = engine.pin();
    let Some((memory, disk)) = pin.content().and_then(|v| v.merge_resources(pin.live())) else {
        return Ok(Duration::from_secs(3600));
    };
    if let Err(reason) = scheduler.admit_merge(pin.catalog(), memory, disk) {
        lock(&host.writer_status).blocked = Some(reason);
        return Ok(Duration::from_secs(1));
    }
    lock(&host.writer_status).blocked = None;
    state.cancelled.store(false, Ordering::Release);
    let cancelled = state.cancelled.clone();
    let host = host.clone();
    let engine = engine.clone();
    let scheduler = scheduler.clone();
    let operation = MergeOperation::start(&host);
    state.merge = Some(
        std::thread::Builder::new()
            .name("ferret-merge".into())
            .spawn(move || {
                let _operation = operation;
                ferret_crawl::lower_index_priority()?;
                merge(&host, &engine, &pin, &scheduler, &cancelled)
            })?,
    );
    Ok(Duration::from_millis(100))
}

struct MergeOperation(Arc<Host>);
impl MergeOperation {
    fn start(host: &Arc<Host>) -> Self {
        lock(&host.writer_status).merge_running = true;
        Self(host.clone())
    }
}
impl Drop for MergeOperation {
    fn drop(&mut self) {
        lock(&self.0.writer_status).merge_running = false;
        self.0.writer_send.wake();
        super::super::wake_listener(&self.0);
    }
}

fn merge(
    host: &Host,
    engine: &Engine,
    pin: &crate::engine::QuerySession,
    scheduler: &crate::scheduler::Scheduler,
    cancelled: &AtomicBool,
) -> io::Result<()> {
    let should_interrupt = || cancelled.load(Ordering::Acquire) || urgent(host, scheduler);
    #[cfg(debug_assertions)]
    let checks = std::sync::atomic::AtomicU64::new(0);
    let interrupted = || {
        #[cfg(debug_assertions)]
        {
            let checks = checks.fetch_add(1, Ordering::Relaxed) + 1;
            content_checkpoint(host, "merge", checks, &should_interrupt);
        }
        should_interrupt()
    };
    let limiter = scheduler.limiter();
    let pace = |bytes: usize| {
        for chunk in (0..bytes).step_by(64 << 10) {
            if should_interrupt() {
                return Err(io::Error::other(ferret_index::Error::Cancelled));
            }
            limiter.transfer((bytes - chunk).min(64 << 10), || Ok(()))?;
        }
        Ok(())
    };
    let budget = ferret_index::Budget {
        bytes: u64::MAX,
        buffer: 1 << 20,
        segments: 1,
        pace: &pace,
        cancelled: &interrupted,
    };
    match engine.merge_content_at(pin, &budget) {
        Ok(_) | Err(crate::engine::Error::Content(ferret_index::Error::Cancelled)) => Ok(()),
        Err(error) => Err(io::Error::other(error)),
    }
}

/// Deterministic integration barrier on production cancellation checkpoints.
/// Commands, battery, drain and teardown release it through the real predicate.
#[cfg(debug_assertions)]
fn content_checkpoint(host: &Host, phase: &str, checks: u64, interrupted: &dyn Fn() -> bool) {
    let Some(gate) = std::env::var_os("FERRET_CONTENT_TEST_GATE").map(PathBuf::from) else {
        return;
    };
    let held = gate.join(phase);
    if !held.exists() || checks < 10 {
        return;
    }
    if phase == "merge" {
        let written = std::fs::read_dir(host.index.join(crate::engine::CONTENT_DIR))
            .ok()
            .is_some_and(|entries| {
                entries.filter_map(Result::ok).any(|entry| {
                    entry.file_name().to_string_lossy().starts_with("tmp-")
                        && entry.path().extension().is_some_and(|e| e == "seg")
                        && entry.metadata().is_ok_and(|m| m.len() > 0)
                })
            });
        if !written {
            return;
        }
    }
    let _ = std::fs::write(gate.join(format!("{phase}.reached")), b"checkpoint");
    while held.exists() && !interrupted() {
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Follows every uncovered document for an explicit command. No byte pacing.
/// Drain and shutdown stop the pass. If documents remain, that is a content
/// error: the catalog change is already published, and the reply tells the
/// user to run `ferret index` again. A finished follow (`remaining == 0`) is
/// success even when the same check also fired. A merge this makes due stays
/// on the background merge worker.
///
/// Debug builds share [`content_checkpoint`] on the `explicit` phase, which
/// is not the background `follow` gate.
pub(super) fn follow_explicit(host: &Host, engine: &Engine) -> Result<(), String> {
    loop {
        let stop = || host.stop.load(Ordering::Acquire) || lock(&host.lifecycle).draining;
        // A fresh counter per pass. The background follow's gate is a
        // different phase, and this command must not park in it.
        #[cfg(debug_assertions)]
        let checks = std::sync::atomic::AtomicU64::new(0);
        let cancelled = || {
            #[cfg(debug_assertions)]
            {
                let checks = checks.fetch_add(1, Ordering::Relaxed) + 1;
                content_checkpoint(host, "explicit", checks, &stop);
            }
            stop()
        };
        let pace = |_bytes: usize| Ok(());
        let budget = ferret_index::Budget {
            bytes: u64::MAX,
            buffer: ferret_index::BUFFER,
            segments: 1,
            pace: &pace,
            cancelled: &cancelled,
        };
        let followed = engine
            .follow_content(&budget, None)
            .map_err(|error| error.to_string())?;
        if followed.remaining == 0 {
            return Ok(());
        }
        if followed.stopped == ferret_index::Stopped::Asked {
            return Err("interrupted (daemon stopping); run ferret index again".to_owned());
        }
    }
}
