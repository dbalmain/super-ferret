//! Pool-wide capture allocation and exclusive streaming ownership. Completed
//! groups keep their charge through publication; the stream owner never waits
//! for capture space. Descriptor admissions remain held by the caller.

use super::{Arc, GROUP_BYTES, Mutex, lock, publisher_stopped};
use std::io;
use std::sync::{Condvar, PoisonError};
use std::time::Duration;

pub(super) struct Budget {
    limit: usize,
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    bytes: usize,
    streaming: bool,
    #[cfg(test)]
    peak: usize,
    #[cfg(test)]
    waits: usize,
}

impl Budget {
    pub(super) fn new(threads: usize) -> Arc<Self> {
        Arc::new(Self {
            limit: (2 * threads + 1) * GROUP_BYTES,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        })
    }

    pub(super) fn hold(self: &Arc<Self>) -> Held {
        Held {
            budget: self.clone(),
            bytes: 0,
            streaming: false,
        }
    }

    #[cfg(test)]
    pub(super) fn usage(&self) -> (usize, usize, usize) {
        let state = lock(&self.state);
        (state.bytes, state.peak, state.waits)
    }
}

pub(super) struct Held {
    budget: Arc<Budget>,
    bytes: usize,
    streaming: bool,
}

impl Held {
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
            if needed > GROUP_BYTES && !state.streaming {
                state.streaming = true;
                self.streaming = true;
                return Ok(None);
            }
            let available = self.budget.limit - state.bytes;
            if needed <= self.bytes + available {
                let capacity = needed
                    .max(self.bytes.saturating_mul(2))
                    .min(self.bytes + available);
                state.bytes += capacity - self.bytes;
                self.bytes = capacity;
                #[cfg(test)]
                {
                    state.peak = state.peak.max(state.bytes);
                }
                return Ok(Some(capacity));
            }
            // External/pool cancellation has no access to this condvar.
            // Poll it as descriptor and input-budget waits do.
            #[cfg(test)]
            {
                state.waits += 1;
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
            state.streaming = false;
        }
        self.budget.changed.notify_all();
    }
}
