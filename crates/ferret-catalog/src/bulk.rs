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
    /// Current concurrency ceiling when crawl starts a new worker pool.
    fn workers(&self) -> usize;
}

/// Monotonic clock and waiting mechanism shared by production and tests.
pub trait Clock: Send + Sync + std::fmt::Debug {
    fn now(&self) -> Duration;
    fn sleep(&self, duration: Duration);
}
#[derive(Debug)]
pub struct Monotonic(Instant);
impl Default for Monotonic {
    fn default() -> Self {
        Self(Instant::now())
    }
}
impl Clock for Monotonic {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Shared leaky-bucket reservations. Waits occur before I/O; at most one
/// bounded seam chunk may lead its transfer allowance. Zero disables pacing.
#[derive(Debug)]
pub struct Limiter {
    rate: u64,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
    transfer: Mutex<()>,
}
#[derive(Debug, Default)]
struct State {
    next: Duration,
    bytes: u64,
    waits: u64,
    waiting: u64,
    advice_failures: u64,
}
#[derive(Clone, Copy, Debug)]
pub struct RateStatus {
    pub bytes_per_second: u64,
    pub reserved_bytes: u64,
    pub waits: u64,
    pub waiting: u64,
    pub advice_failures: u64,
}
impl Limiter {
    pub fn new(rate: u64, clock: Arc<dyn Clock>) -> Self {
        Self {
            rate,
            clock,
            state: Mutex::new(State::default()),
            transfer: Mutex::new(()),
        }
    }
    /// Paces one bounded read/write seam. Serializing the seam prevents many
    /// blocked reads from completing together and exceeding a single burst.
    /// The next allowance starts at completion; waiting is before the next
    /// transfer, never after this one or after durable publication.
    pub fn transfer<T>(&self, bytes: usize, run: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        if self.rate == 0 {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.bytes = state.bytes.saturating_add(bytes as u64);
            drop(state);
            return run();
        }
        let _transfer = self
            .transfer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if bytes == 0 {
            return run();
        }
        let delay = {
            let mut s = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.bytes = s.bytes.saturating_add(bytes as u64);
            let delay = s.next.saturating_sub(self.clock.now());
            if !delay.is_zero() {
                s.waits += 1;
                s.waiting += 1;
            }
            delay
        };
        if !delay.is_zero() {
            self.clock.sleep(delay);
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .waiting -= 1;
        }
        let result = run();
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .next = self
            .clock
            .now()
            .saturating_add(Duration::from_secs_f64(bytes as f64 / self.rate as f64));
        result
    }
    /// Advice failure is diagnostic and never turns readable content into a
    /// fault.
    pub fn advice_failed(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .advice_failures += 1;
    }
    pub fn status(&self) -> RateStatus {
        let s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        RateStatus {
            bytes_per_second: self.rate,
            reserved_bytes: s.bytes,
            waits: s.waits,
            waiting: s.waiting,
            advice_failures: s.advice_failures,
        }
    }
}

thread_local! {
    static WRITES: RefCell<Option<Arc<Limiter>>> = const { RefCell::new(None) };
}
pub(crate) fn writes<T>(limiter: Option<Arc<Limiter>>, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<Arc<Limiter>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            WRITES.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(WRITES.with(|slot| slot.replace(limiter)));
    run()
}
pub(crate) fn write_at(file: &File, bytes: &[u8], mut offset: u64) -> io::Result<()> {
    for chunk in bytes.chunks(64 << 10) {
        let limiter = WRITES.with(|slot| slot.borrow().clone());
        if let Some(limiter) = limiter {
            limiter.transfer(chunk.len(), || file.write_all_at(chunk, offset))?;
        } else {
            file.write_all_at(chunk, offset)?;
        }
        offset += chunk.len() as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Debug, Default)]
    struct Time(Mutex<Duration>);
    impl Clock for Time {
        fn now(&self) -> Duration {
            *self.0.lock().unwrap_or_else(|e| panic!("fixture: {e:?}"))
        }
        fn sleep(&self, duration: Duration) {
            *self.0.lock().unwrap_or_else(|e| panic!("fixture: {e:?}")) += duration;
        }
    }
    #[test]
    fn checkpoint_write_seam_obeys_allowance_and_restores_scope() {
        let clock = Arc::new(Time::default());
        let limiter = Arc::new(Limiter::new(64 << 10, clock.clone()));
        let path = std::env::temp_dir().join(format!("ferret-bulk-write-{}", std::process::id()));
        let file = File::create(&path).unwrap_or_else(|e| panic!("fixture: {e:?}"));
        writes(Some(limiter.clone()), || {
            write_at(&file, &vec![42; 4 * (64 << 10)], 0)
        })
        .unwrap_or_else(|e| panic!("fixture: {e:?}"));
        assert_eq!(
            file.metadata()
                .unwrap_or_else(|e| panic!("fixture: {e:?}"))
                .len(),
            4 * (64 << 10)
        );
        let state = limiter.status();
        assert_eq!(state.reserved_bytes, 4 * (64 << 10));
        assert_eq!(clock.now(), Duration::from_secs(3));
        assert!(
            state.reserved_bytes <= state.bytes_per_second * clock.now().as_secs() + (64 << 10)
        );
        // Ordinary writes outside an admitted checkpoint have no bulk pacing.
        write_at(&file, &[1; 100], 0).unwrap_or_else(|e| panic!("fixture: {e:?}"));
        assert_eq!(limiter.status().reserved_bytes, state.reserved_bytes);
        drop(file);
        std::fs::remove_file(&path).unwrap_or_else(|e| panic!("fixture: {e:?}"));
    }
    #[test]
    fn reservations_cover_every_window_and_calm_gaps_do_not_accumulate_credit() {
        let clock = Arc::new(Time::default());
        let limiter = Limiter::new(1024, clock.clone());
        let mut transfers = Vec::new();
        for n in 0..30 {
            if n == 15 {
                *clock.0.lock().unwrap_or_else(|e| panic!("fixture: {e:?}")) +=
                    Duration::from_secs(50);
            }
            limiter
                .transfer(256, || Ok(()))
                .unwrap_or_else(|e| panic!("fixture: {e:?}"));
            transfers.push((clock.now(), 256));
        }
        for &(start, _) in &transfers {
            for window in [
                Duration::ZERO,
                Duration::from_millis(100),
                Duration::from_secs(1),
                Duration::from_secs(10),
            ] {
                let bytes: u64 = transfers
                    .iter()
                    .filter(|(time, _)| *time >= start && *time <= start + window)
                    .map(|(_, bytes)| bytes)
                    .sum();
                assert!(bytes as f64 <= 1024.0 * window.as_secs_f64() + 256.0);
            }
        }
    }
    #[test]
    fn a_stalled_transfer_cannot_bunch_completed_bytes_with_the_next_burst() {
        let clock = Arc::new(Time::default());
        let limiter = Limiter::new(1024, clock.clone());
        limiter
            .transfer(256, || {
                *clock.0.lock().unwrap_or_else(|e| panic!("clock: {e}")) += Duration::from_secs(10);
                Ok(())
            })
            .unwrap_or_else(|e| panic!("IO: {e}"));
        let first = clock.now();
        limiter
            .transfer(256, || Ok(()))
            .unwrap_or_else(|e| panic!("IO: {e}"));
        assert!(clock.now() - first >= Duration::from_millis(250));
    }
    #[test]
    fn checkpoint_scope_is_restored_after_unwind() {
        let clock = Arc::new(Time::default());
        let outer = Arc::new(Limiter::new(0, clock.clone()));
        let inner = Arc::new(Limiter::new(0, clock));
        writes(Some(outer.clone()), || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                writes(Some(inner), || panic!("unpublished write"))
            }));
            assert!(result.is_err());
            WRITES.with(|slot| {
                assert!(Arc::ptr_eq(
                    slot.borrow()
                        .as_ref()
                        .unwrap_or_else(|| panic!("missing pacing scope")),
                    &outer
                ))
            });
        });
        WRITES.with(|slot| assert!(slot.borrow().is_none()));
    }
}
