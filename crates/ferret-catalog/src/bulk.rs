//! Host-selected bulk admission and byte pacing. The catalog asks before any
//! checkpoint scratch allocation; admitted publication is never interrupted.
//! A thread-local scope covers only checkpoint writes, never query reads.

use std::cell::RefCell;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::Catalog;

/// Allocation class requested under the retained writer lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Checkpoint,
    FullRewalk,
}

/// A refusal leaves the selected generation and writer caches usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blocked {
    Memory,
    Disk,
    Battery,
    IoPressure,
    Unavailable,
}
impl Blocked {
    pub fn status(self) -> &'static str {
        match self {
            Self::Memory => "memory-blocked",
            Self::Disk => "disk-blocked",
            Self::Battery => "battery-paused",
            Self::IoPressure => "io-pressure",
            Self::Unavailable => "signal-unavailable",
        }
    }
}
impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.status())
    }
}

/// The host owns signals, reserves and a shared limiter; storage owns the
/// actual checkpoint seam. Admission is once per new bulk allocation phase.
pub trait Control: Send + Sync + std::fmt::Debug {
    fn admit(&self, kind: Kind, view: &Catalog) -> Result<(), Blocked>;
    fn limiter(&self) -> Arc<Limiter>;
}

/// Monotonic clock and waiting mechanism shared by production and tests.
pub trait Clock: Send + Sync + std::fmt::Debug {
    fn now(&self) -> Duration;
    fn sleep(&self, duration: Duration);
}
#[derive(Debug)]
pub struct Monotonic(Instant);
impl Default for Monotonic {
    fn default() -> Self { Self(Instant::now()) }
}
impl Clock for Monotonic {
    fn now(&self) -> Duration { self.0.elapsed() }
    fn sleep(&self, duration: Duration) { std::thread::sleep(duration); }
}

/// Shared leaky-bucket reservations. Waits occur before I/O; at most one
/// bounded seam chunk may lead its transfer allowance. Zero disables pacing.
#[derive(Debug)]
pub struct Limiter {
    rate: u64,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
}
#[derive(Debug, Default)]
struct State {
    next: Duration,
    bytes: u64,
    waits: u64,
    waiting: u64,
}
#[derive(Clone, Copy, Debug)]
pub struct RateStatus {
    pub bytes_per_second: u64,
    pub reserved_bytes: u64,
    pub waits: u64,
    pub waiting: u64,
}
impl Limiter {
    pub fn new(rate: u64, clock: Arc<dyn Clock>) -> Self {
        Self { rate, clock, state: Mutex::new(State::default()) }
    }
    /// Reserves a bounded chunk before reading or writing it. Callers split
    /// large transfers so the allowance cannot become an unbounded burst.
    pub fn acquire(&self, bytes: usize) {
        if bytes == 0 { return; }
        let delay = {
            let mut s = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.bytes = s.bytes.saturating_add(bytes as u64);
            if self.rate == 0 { return; }
            let now = self.clock.now();
            let start = s.next.max(now);
            s.next = start.saturating_add(Duration::from_secs_f64(bytes as f64 / self.rate as f64));
            let delay = start.saturating_sub(now);
            if !delay.is_zero() { s.waits += 1; s.waiting += 1; }
            delay
        };
        if !delay.is_zero() {
            self.clock.sleep(delay);
            self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).waiting -= 1;
        }
    }
    pub fn status(&self) -> RateStatus {
        let s = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        RateStatus { bytes_per_second: self.rate, reserved_bytes: s.bytes, waits: s.waits, waiting: s.waiting }
    }
}

thread_local! {
    static WRITES: RefCell<Option<Arc<Limiter>>> = const { RefCell::new(None) };
}
pub(crate) fn writes<T>(limiter: Option<Arc<Limiter>>, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<Arc<Limiter>>);
    impl Drop for Restore {
        fn drop(&mut self) { WRITES.with(|slot| *slot.borrow_mut() = self.0.take()); }
    }
    let _restore = Restore(WRITES.with(|slot| slot.replace(limiter)));
    run()
}
pub(crate) fn write_at(file: &File, bytes: &[u8], mut offset: u64) -> io::Result<()> {
    for chunk in bytes.chunks(64 << 10) {
        WRITES.with(|slot| { if let Some(limiter) = slot.borrow().as_ref() { limiter.acquire(chunk.len()); } });
        file.write_all_at(chunk, offset)?;
        offset += chunk.len() as u64;
    }
    Ok(())
}
