//! The pool's shared state: workers claim batches of positions in order and
//! post each finished position to its batch's slot of a bounded ring, which
//! the calling thread takes in order.

use super::*;

pub(super) trait Placed {
    fn place(&mut self, path: Range<usize>);
}

/// Consecutive positions, finished.
pub(super) struct Batch<T = Item> {
    /// The first position.
    pub(super) start: usize,
    pub(super) items: Vec<T>,
    /// The rows' paths.
    pub(super) paths: Vec<u8>,
}

impl<T> Default for Batch<T> {
    fn default() -> Self {
        Self {
            start: 0,
            items: Vec::new(),
            paths: Vec::new(),
        }
    }
}

impl<T: Placed> Batch<T> {
    pub(super) fn clear(&mut self) {
        self.items.clear();
        self.paths.clear();
    }

    /// Appends `item`, whose path is all of `paths`.
    fn push(&mut self, mut item: T, paths: &[u8]) {
        let start = self.paths.len();
        item.place(start..start + paths.len());
        self.paths.extend_from_slice(paths);
        self.items.push(item);
    }
}

/// A slot of the ring: one batch's finished positions not yet taken.
struct Slot<T = Item> {
    /// The batch, plus one (0: none).
    index: usize,
    part: Batch<T>,
    /// Every position of the batch has been posted.
    complete: bool,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self {
            index: 0,
            part: Batch::default(),
            complete: false,
        }
    }
}

/// The batch counter, the ring of finished batches, and the byte budget
/// the workers share.
///
/// Every wait here pairs a sequentially consistent store of the condition
/// with a sequentially consistent load of a "someone is waiting" word on
/// the other side, and the notifier takes the waiter's lock before it
/// signals: so either the waiter sees the condition before it sleeps, or
/// the notifier sees the waiter and wakes it. Nobody signals a thread that
/// is not waiting, which keeps wakes, and their syscalls, per batch.
pub(super) struct Pool<T: Placed = Item> {
    /// Positions in the run.
    pub(super) positions: usize,
    /// Positions per batch, and the batch count.
    pub(super) batch: usize,
    pub(super) batches: usize,
    /// The next batch to claim.
    pub(super) next: AtomicUsize,
    /// Batches the calling thread has taken.
    head: AtomicUsize,
    /// Each batch's finished positions not yet taken, at
    /// `index % ring.len()`.
    ring: Vec<Mutex<Slot<T>>>,
    /// Positions posted to each slot, ever: the calling thread sleeps until
    /// its slot's count moves.
    posted: Vec<AtomicUsize>,
    /// The batch the calling thread sleeps for, plus one (0: none).
    awaited: AtomicUsize,
    ready: Mutex<()>,
    ready_signal: Condvar,
    /// Workers sleeping for room in the ring.
    roomless: AtomicUsize,
    room: Mutex<()>,
    room_signal: Condvar,
    budget: Budget,
    stop: AtomicBool,
    /// A worker panicked; its batch will never finish.
    lost: AtomicBool,
    /// Positions finished so far, for the test seams.
    #[cfg(test)]
    finished: AtomicU64,
    #[cfg(test)]
    pub(super) seams: seam::Seams,
}

impl<T: Placed> Pool<T> {
    pub(super) fn schedule(positions: usize, threads: usize) -> Self {
        let batch = (positions / (threads * BATCHES_PER_THREAD)).clamp(1, MAX_BATCH);
        let slots = threads * RING_PER_THREAD;
        Self {
            positions,
            batch,
            batches: positions.div_ceil(batch),
            next: AtomicUsize::new(0),
            head: AtomicUsize::new(0),
            ring: (0..slots).map(|_| Mutex::default()).collect(),
            posted: (0..slots).map(|_| AtomicUsize::new(0)).collect(),
            awaited: AtomicUsize::new(0),
            ready: Mutex::new(()),
            ready_signal: Condvar::new(),
            roomless: AtomicUsize::new(0),
            room: Mutex::new(()),
            room_signal: Condvar::new(),
            budget: Budget::default(),
            stop: AtomicBool::new(false),
            lost: AtomicBool::new(false),
            #[cfg(test)]
            finished: AtomicU64::new(0),
            #[cfg(test)]
            seams: seam::Seams::current(),
        }
    }

    #[cfg(test)]
    pub(super) fn finished(&self) -> u64 {
        self.finished.load(Ordering::Acquire)
    }

    pub(super) fn halt(&self) {
        self.stop.store(true, Ordering::SeqCst);
        // Taking each lock orders the store before any sleeper's next check.
        drop(lock(&self.room));
        self.room_signal.notify_all();
        self.budget.wake();
    }

    pub(super) fn halted(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// The budget's [`Budget::reserve`], for a worker of this pool.
    pub(super) fn reserve(&self, size: u64, halt: &dyn Fn() -> bool) -> Option<u64> {
        self.budget.reserve(size, halt, &|| {
            #[cfg(test)]
            self.seams.waiting_for_bytes();
        })
    }

    pub(super) fn release(&self, size: u64) {
        self.budget.release(size);
    }

    /// Waits until batch `index` has a slot in the ring: the calling thread
    /// has taken the batch that held it. False once the pool stops.
    pub(super) fn room_for(&self, index: usize) -> bool {
        let fits = || index < self.head.load(Ordering::SeqCst) + self.ring.len();
        if fits() {
            return !self.halted();
        }
        self.roomless.fetch_add(1, Ordering::SeqCst);
        let mut guard = lock(&self.room);
        while !fits() && !self.halted() {
            guard = self
                .room_signal
                .wait(guard)
                .unwrap_or_else(PoisonError::into_inner);
        }
        drop(guard);
        self.roomless.fetch_sub(1, Ordering::SeqCst);
        !self.halted()
    }

    /// Posts position `at` of batch `index`, whose row's path is `paths`;
    /// `last` if it is the batch's last.
    pub(super) fn post(&self, index: usize, at: usize, item: T, paths: &[u8], last: bool) {
        let slot = index % self.ring.len();
        {
            let mut held = lock(&self.ring[slot]);
            if held.index != index + 1 {
                held.index = index + 1;
                held.part.clear();
            }
            if held.part.items.is_empty() {
                held.part.start = at;
            }
            held.part.push(item, paths);
            held.complete = last;
        }
        #[cfg(test)]
        self.finished.fetch_add(1, Ordering::AcqRel);
        self.posted[slot].fetch_add(1, Ordering::SeqCst);
        if self.awaited.load(Ordering::SeqCst) == index + 1 {
            drop(lock(&self.ready));
            self.ready_signal.notify_one();
        }
        #[cfg(test)]
        self.seams.published(&|| self.finished());
    }

    /// Batch `index`'s next finished positions, into `out` (empty): waits
    /// until there is at least one, or the batch is complete. True once the
    /// batch is complete, which frees its slot; `None` if a worker was lost.
    pub(super) fn take(&self, index: usize, out: &mut Batch<T>) -> Option<bool> {
        let slot = index % self.ring.len();
        let posted = &self.posted[slot];
        loop {
            let seen = posted.load(Ordering::SeqCst);
            {
                let mut held = lock(&self.ring[slot]);
                if held.index == index + 1 && (!held.part.items.is_empty() || held.complete) {
                    std::mem::swap(&mut held.part, out);
                    let complete = held.complete;
                    if complete {
                        held.index = 0;
                        held.complete = false;
                    }
                    drop(held);
                    if complete {
                        self.head.store(index + 1, Ordering::SeqCst);
                        if self.roomless.load(Ordering::SeqCst) > 0 {
                            drop(lock(&self.room));
                            self.room_signal.notify_all();
                        }
                    }
                    return Some(complete);
                }
            }
            let mut guard = lock(&self.ready);
            self.awaited.store(index + 1, Ordering::SeqCst);
            let moved = || posted.load(Ordering::SeqCst) != seen;
            while !moved() && !self.lost.load(Ordering::SeqCst) {
                guard = self
                    .ready_signal
                    .wait(guard)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            self.awaited.store(0, Ordering::SeqCst);
            drop(guard);
            if !moved() {
                return None;
            }
        }
    }
}

/// Stops the pool when the calling thread leaves the scope, by any path.
pub(super) struct StopOnDrop<'p, T: Placed = Item>(pub(super) &'p Pool<T>);

impl<T: Placed> Drop for StopOnDrop<'_, T> {
    fn drop(&mut self) {
        self.0.halt();
    }
}

/// Tells the calling thread a worker panicked, so it stops waiting.
pub(super) struct LostOnPanic<'p, T: Placed = Item>(pub(super) &'p Pool<T>);

impl<T: Placed> Drop for LostOnPanic<'_, T> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.lost.store(true, Ordering::SeqCst);
            drop(lock(&self.0.ready));
            self.0.ready_signal.notify_all();
        }
    }
}
