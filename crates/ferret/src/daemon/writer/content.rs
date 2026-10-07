//! Background content maintenance at the real writer's idle boundaries.
//! The parent loop owns queue priority and publication; this turn uses its
//! Operation guard and the shared controller for pacing, admission and pause.
//! Query pins and durable/cancellable index operations remain in Engine/store.

use std::io;
#[cfg(debug_assertions)]
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::{Host, Operation};
use crate::engine::Engine;

/// After two cancelled attempts on the same immutable inputs, watch traffic
/// queues behind one admitted merge. Commands, battery, drain and shutdown
/// still interrupt it. The protected work is bounded by the selected inputs.
const CANCELLATIONS: u8 = 2;

#[derive(Default)]
pub(super) struct State {
    selection: Option<([u8; 16], Vec<u64>)>,
    cancellations: u8,
}

pub(super) fn turn(
    host: &Host,
    engine: &Engine,
    scheduler: &crate::scheduler::Scheduler,
    state: &mut State,
) -> io::Result<Duration> {
    use ferret_catalog::bulk::Control;
    let started = Instant::now();
    let watch_due = || {
        host.writer_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .watch
            .as_ref()
            .and_then(|w| w.next_due())
            .is_some_and(|due| due <= Instant::now())
    };
    let urgent = || {
        host.stop.load(Ordering::Acquire)
            || host.writer_pending.load(Ordering::Acquire) != 0
            || scheduler.status().paused.is_some()
            || host
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .draining
    };
    let should_interrupt = || urgent() || watch_due();
    #[cfg(debug_assertions)]
    let checks = std::cell::Cell::new(0u64);
    let interrupted = || {
        #[cfg(debug_assertions)]
        {
            checks.set(checks.get() + 1);
            content_checkpoint(host, checks.get(), &should_interrupt);
        }
        should_interrupt()
    };
    let cancelled = || interrupted() || started.elapsed() >= Duration::from_millis(100);
    if cancelled() {
        return Ok(Duration::from_secs(1));
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
        pace: &pace,
        cancelled: &cancelled,
    };
    let operation = Operation::start(host, "follow");
    let followed = engine
        .follow_content(&budget, Some(limiter.clone()))
        .map_err(io::Error::other)?;
    drop(operation);
    if cancelled() || followed.remaining > 0 {
        return Ok(Duration::ZERO);
    }
    let pin = engine.pin();
    let Some((memory, disk)) = pin.content().and_then(|v| v.merge_resources(pin.live())) else {
        *state = State::default();
        return Ok(Duration::from_secs(3600));
    };
    let view = pin
        .content()
        .ok_or_else(|| io::Error::other("merge without content view"))?;
    let selection = view
        .merge_selection(pin.live())
        .ok_or_else(|| io::Error::other("merge without selection"))?;
    let key = (
        view.manifest().incarnation,
        selection.iter().map(|entry| entry.number).collect(),
    );
    if state.selection.as_ref() != Some(&key) {
        state.selection = Some(key);
        state.cancellations = 0;
    }
    let protected = state.cancellations >= CANCELLATIONS;
    if let Err(reason) = scheduler.admit_merge(pin.catalog(), memory, disk) {
        host.writer_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .blocked = Some(reason);
        return Ok(Duration::from_secs(1));
    }
    host.writer_status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .blocked = None;
    let _operation = Operation::start(host, "merge");
    let merge_should_interrupt = || urgent() || (!protected && watch_due());
    let merge_interrupted = || {
        #[cfg(debug_assertions)]
        {
            checks.set(checks.get() + 1);
            content_checkpoint(host, checks.get(), &merge_should_interrupt);
        }
        merge_should_interrupt()
    };
    // Merge input size is admitted separately; cooperative cancellation is
    // checked during streaming and before cutover, never during publication.
    let budget = ferret_index::Budget {
        bytes: u64::MAX,
        cancelled: &merge_interrupted,
        ..budget
    };
    match engine.merge_content(&budget) {
        Ok(Some(_)) => {
            *state = State::default();
            Ok(Duration::ZERO)
        }
        Ok(None) => {
            *state = State::default();
            Ok(Duration::from_secs(3600))
        }
        Err(crate::engine::Error::Content(ferret_index::Error::Cancelled)) => {
            state.cancellations = state.cancellations.saturating_add(1);
            Ok(Duration::ZERO)
        }
        Err(error) => Err(io::Error::other(error)),
    }
}
/// Deterministic integration barrier on production cancellation checkpoints.
/// A command, battery transition or drain releases it through the real
/// predicate.
#[cfg(debug_assertions)]
fn content_checkpoint(host: &Host, checks: u64, interrupted: &dyn Fn() -> bool) {
    let Some(gate) = std::env::var_os("FERRET_CONTENT_TEST_GATE").map(PathBuf::from) else {
        return;
    };
    let phase = host
        .writer_status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .operation;
    let Some(phase @ ("follow" | "merge")) = phase else {
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
