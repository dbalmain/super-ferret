//! Pool-wide capture allocation and exclusive streaming ownership. Completed
//! groups keep their charge through publication; the stream owner never waits
//! for capture space. Descriptor admissions remain held by the caller.

use super::{Arc, CHUNK, GROUP_BYTES, Mutex, lock, publisher_stopped};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, PoisonError};
use std::time::Duration;

pub(in super::super) struct Budget {
    limit: usize,
    state: Mutex<State>,
    changed: Condvar,
    streaming: AtomicBool,
}

#[derive(Default)]
struct State {
    bytes: usize,
    #[cfg(test)]
    peak: usize,
    #[cfg(test)]
    waits: usize,
}

impl Budget {
    pub(super) fn new(threads: usize) -> Arc<Self> {
        let budget = Arc::new(Self {
            limit: (2 * threads + 1) * GROUP_BYTES,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            streaming: AtomicBool::new(false),
        });
        #[cfg(test)]
        CREATED.with_borrow_mut(|hook| {
            if let Some(hook) = hook {
                hook(&budget);
            }
        });
        budget
    }

    pub(super) fn hold(self: &Arc<Self>) -> Held {
        Held {
            budget: self.clone(),
            bytes: 0,
            streaming: false,
        }
    }

    #[cfg(test)]
    pub(super) fn wait_for_full(&self) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut state = lock(&self.state);
        while state.bytes != self.limit || state.waits == 0 {
            let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return false;
            };
            (state, _) = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner);
        }
        true
    }

    #[cfg(test)]
    pub(super) fn usage(&self) -> (usize, usize, usize) {
        let state = lock(&self.state);
        (state.bytes, state.peak, state.waits)
    }
}

pub(in super::super) struct Held {
    budget: Arc<Budget>,
    bytes: usize,
    streaming: bool,
}

impl Held {
    pub(super) fn capacity(&self, needed: usize) -> Option<usize> {
        (needed <= self.bytes
            && (needed <= GROUP_BYTES || self.budget.streaming.load(Ordering::Acquire)))
        .then_some(self.bytes)
    }

    /// Returns an allocation ceiling, or None to stream. Ownership is taken
    /// before checking space: even a completely full budget must let the
    /// publisher's next owner drain its prefix and finish its checked read.
    pub(super) fn grow(
        &mut self,
        needed: usize,
        stopped: &dyn Fn() -> bool,
    ) -> io::Result<Option<usize>> {
        let mut state = lock(&self.budget.state);
        loop {
            if stopped() {
                return Err(publisher_stopped());
            }
            if needed > GROUP_BYTES
                && self
                    .budget
                    .streaming
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                self.streaming = true;
                return Ok(None);
            }
            if needed <= self.bytes {
                return Ok(Some(self.bytes));
            }
            let available = self.budget.limit - state.bytes;
            let desired = needed.max(self.bytes.saturating_mul(2));
            // OutputBuffer can cap growth only at 64 KiB or above. Below
            // that floor charge its actual geometric allocation, so tiny
            // completed groups do not each consume a whole chunk.
            let required = if desired < CHUNK {
                desired
            } else {
                needed.next_multiple_of(CHUNK)
            };
            if required <= self.bytes + available {
                let capacity = if desired < CHUNK {
                    desired
                } else {
                    desired.next_multiple_of(CHUNK).min(self.bytes + available)
                };
                state.bytes += capacity - self.bytes;
                self.bytes = capacity;
                #[cfg(test)]
                {
                    state.peak = state.peak.max(state.bytes);
                    self.budget.changed.notify_all();
                }
                return Ok(Some(capacity));
            }
            // External/pool cancellation has no access to this condvar.
            // Poll it as descriptor and input-budget waits do.
            #[cfg(test)]
            {
                state.waits += 1;
                self.budget.changed.notify_all();
            }
            (state, _) = self
                .budget
                .changed
                .wait_timeout(state, Duration::from_millis(20))
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// The caller has freed its prefix allocation before returning its charge.
    pub(super) fn clear(&mut self) {
        let mut state = lock(&self.budget.state);
        state.bytes -= self.bytes;
        self.bytes = 0;
        self.budget.changed.notify_all();
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut state = lock(&self.budget.state);
        state.bytes -= self.bytes;
        if self.streaming {
            self.budget.streaming.store(false, Ordering::Release);
        }
        self.budget.changed.notify_all();
    }
}

#[cfg(test)]
type OnCreated = dyn FnMut(&Arc<Budget>);

#[cfg(test)]
thread_local! {
    pub(super) static CREATED: std::cell::RefCell<Option<Box<OnCreated>>> = const { std::cell::RefCell::new(None) };
}
