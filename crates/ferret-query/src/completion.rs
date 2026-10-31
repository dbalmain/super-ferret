//! Grep's completion groups: file tasks run on a bounded set of workers,
//! fed by a producer thread or found by the workers themselves, and the
//! calling thread emits each finished group in completion order, under the
//! same input-byte budget as document verification.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::budget::Budget;
use crate::threads::{Shortfall, lock, spawn_named, wait};

/// What file groups share of a worker pool: the input-byte budget and a
/// stop flag that wakes its waiters.
struct Gate {
    budget: Budget,
    stop: AtomicBool,
    #[cfg(test)]
    seams: crate::verify::seam::Seams,
}

impl Gate {
    fn new() -> Self {
        Self {
            budget: Budget::default(),
            stop: AtomicBool::new(false),
            #[cfg(test)]
            seams: crate::verify::seam::Seams::current(),
        }
    }

    fn halt(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.budget.wake();
    }

    fn halted(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn reserve(&self, size: u64, halt: &dyn Fn() -> bool) -> Option<u64> {
        self.budget.reserve(size, halt, &|| {
            #[cfg(test)]
            self.seams.waiting_for_bytes();
        })
    }

    fn release(&self, size: u64) {
        self.budget.release(size);
    }
}

/// Halts the gate when the run's scope ends, however it ends.
struct HaltOnDrop<'g>(&'g Gate);

impl Drop for HaltOnDrop<'_> {
    fn drop(&mut self) {
        self.0.halt();
    }
}

// A panicking file worker may still own a byte reservation. Stop and wake
// its peers before the scope joins, rather than waiting on those leaked bytes.
struct StopOnPanic<'p>(&'p Gate);

impl Drop for StopOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.halt();
        }
    }
}

/// Tasks the [`completion_groups`] producer may run ahead of the workers, per
/// worker. A few per worker keep the queue from emptying between the
/// producer's wakes; the backlog is paths, not captures (D93 bounds those).
const FEED_PER_THREAD: usize = 4;

/// At most this many consecutive indexed paths, and this many input bytes,
/// belong to a worker's run. A larger file travels alone.
const RUN_TASKS: usize = 16;
const RUN_BYTES: u64 = 64 << 10;
/// An indexed producer pushes up to [`RUN_TASKS`] paths under one feed
/// lock, but holds the first no longer than this: a sparse walk would
/// otherwise keep idle workers waiting for a run to fill.
const BATCH_WAIT: Duration = Duration::from_micros(100);

/// Tasks produced on a separate thread. Indexed paths are taken in runs for
/// checked-directory reuse; their pending tails remain accessible even while
/// the owner is blocked in a read. Streaming producers keep immediate wakes:
/// they may block after producing a single task whose execution releases them.
struct Feed<Task> {
    state: Mutex<FeedState<Task>>,
    runs: Vec<Mutex<VecDeque<(u64, Task)>>>,
    /// Tasks in `runs`, never fewer: raised under the feed lock as a run
    /// fills, lowered after an owner or thief removes one. While it reads
    /// zero under the feed lock there is nothing to steal, and a worker
    /// skips locking every run in turn, which on an empty feed held the
    /// feed lock against the producer: `grep -l fn` over `~/w` on 32
    /// workers fell from 152 to 127 ms (2026-10-31).
    stealable: AtomicUsize,
    ready: Condvar,
    room: Condvar,
    capacity: usize,
    indexed: bool,
}

struct FeedState<Task> {
    tasks: VecDeque<(u64, Task)>,
    /// No more tasks will arrive; pending runs can still be stolen.
    closed: bool,
    sleeping: usize,
    /// The producer is asleep on `room` and has not been woken.
    full: bool,
}

impl<Task> Feed<Task> {
    fn new(threads: usize, indexed: bool) -> Self {
        // D107's wake threshold must fit even with only two workers.
        let capacity = (threads * FEED_PER_THREAD).max(if indexed { RUN_TASKS } else { 1 });
        Self {
            state: Mutex::new(FeedState {
                tasks: VecDeque::with_capacity(capacity),
                closed: false,
                sleeping: 0,
                full: false,
            }),
            runs: (0..threads).map(|_| Mutex::default()).collect(),
            stealable: AtomicUsize::new(0),
            ready: Condvar::new(),
            room: Condvar::new(),
            capacity,
            indexed,
        }
    }

    /// Runs `next` without holding a feed or run lock. An indexed producer
    /// gathers a batch and pushes it under one lock: pushing each path
    /// alone contended with every returning worker, and `grep -l fn` over
    /// `~/w` on 16 workers fell from 114 to 83 ms. The batch is pushed when
    /// full, or once [`BATCH_WAIT`] has passed since its first path, which
    /// `next` observes through its cancellation polls, so a long scan of
    /// rejected entries cannot hold a gathered path back. A batch may carry
    /// the feed past its capacity by less than a run.
    fn produce(
        &self,
        next: &mut impl FnMut(&dyn Fn() -> bool) -> Option<(u64, Task)>,
        stopped: &dyn Fn() -> bool,
    ) {
        let batch_len = if self.indexed { RUN_TASKS } else { 1 };
        // A deque, so the push moves the batch with one bulk copy under the
        // lock rather than a copy per task.
        let batch = RefCell::new(VecDeque::with_capacity(batch_len));
        let first = Cell::new(None::<Instant>);
        let polls = Cell::new(0u32);
        let closed = Cell::new(false);
        let flushed = Cell::new(false);
        // Pushes the batch; at a batch's end, then waits under the same lock
        // for room for the next one.
        let push = |room: bool| {
            first.set(None);
            flushed.set(true);
            let mut state = lock(&self.state);
            if !state.closed {
                state.tasks.append(&mut batch.borrow_mut());
                let wake = if self.indexed {
                    state.sleeping > 0
                } else {
                    state.sleeping >= state.tasks.len()
                };
                if wake {
                    self.ready.notify_one();
                }
            }
            while room && state.tasks.len() >= self.capacity && !state.closed {
                state.full = true;
                state = wait(&self.room, state);
            }
            if room {
                state.full = false;
            }
            closed.set(state.closed);
        };
        // Reading the clock on every poll cost about 5 ms of a 60 ms walk.
        let due = || {
            polls.set(polls.get().wrapping_add(1));
            polls.get().is_multiple_of(64)
                && first
                    .get()
                    .is_some_and(|first| first.elapsed() >= BATCH_WAIT)
        };
        let poll = || {
            if due() {
                push(false);
            }
            closed.get() || stopped()
        };
        loop {
            // Gathers until the batch is full or due; a push from a poll ends
            // it too, so the capacity is checked again.
            flushed.set(false);
            loop {
                if poll() {
                    return;
                }
                let Some(task) = next(&poll) else {
                    push(false);
                    return;
                };
                let mut gathered = batch.borrow_mut();
                gathered.push_back(task);
                let (len, full) = (gathered.len(), gathered.len() >= batch_len);
                drop(gathered);
                if first.get().is_none() {
                    first.set(Some(Instant::now()));
                }
                // Every fourth path reads the clock, as polls do every 64th.
                if full
                    || flushed.get()
                    || len.is_multiple_of(4)
                        && first
                            .get()
                            .is_some_and(|first| first.elapsed() >= BATCH_WAIT)
                {
                    break;
                }
            }
            push(true);
            if closed.get() {
                return;
            }
        }
    }

    /// Takes the near end of this worker's run, a new run, or the far end of
    /// another worker's run (half its pending tail). The feed lock orders
    /// assignment and stealing against going to sleep, closing the
    /// lost-wakeup window. Owners take only their run lock, and release it
    /// before any read or publication.
    fn take(&self, worker: usize) -> Option<(u64, Task)> {
        if self.indexed
            && let Some(task) = lock(&self.runs[worker]).pop_front()
        {
            self.stealable.fetch_sub(1, Ordering::Relaxed);
            return Some(task);
        }
        let mut state = lock(&self.state);
        loop {
            if let Some(task) = state.tasks.pop_front() {
                let lend = self.indexed
                    && self.fill_run(worker, &mut state.tasks, task.0)
                    && state.sleeping > 0;
                let wake = state.full && state.tasks.len() <= self.capacity / 2;
                if wake {
                    state.full = false;
                }
                drop(state);
                if wake {
                    self.room.notify_one();
                }
                if lend {
                    self.ready.notify_one();
                }
                return Some(task);
            }
            if self.indexed && self.stealable.load(Ordering::Relaxed) > 0 {
                for offset in 1..self.runs.len() {
                    let mut run = lock(&self.runs[(worker + offset) % self.runs.len()]);
                    if !run.is_empty() {
                        // Keep locality in the stolen tail too; repeated
                        // single-file steals scatter the same directory
                        // across every idle worker.
                        let half = run.len() / 2;
                        let mut own = lock(&self.runs[worker]);
                        own.extend(run.drain(half..));
                        let task = own.pop_front();
                        self.stealable.fetch_sub(1, Ordering::Relaxed);
                        let lend = (!run.is_empty() || !own.is_empty()) && state.sleeping > 0;
                        drop(own);
                        drop(run);
                        drop(state);
                        if lend {
                            self.ready.notify_one();
                        }
                        return task;
                    }
                }
            }
            if state.closed {
                return None;
            }
            state.sleeping += 1;
            state = wait(&self.ready, state);
            state.sleeping -= 1;
        }
    }

    /// Leaves the current file on the caller and its consecutive tail in
    /// the shared run. The caller holds the feed lock; the run is empty.
    fn fill_run(&self, worker: usize, tasks: &mut VecDeque<(u64, Task)>, mut bytes: u64) -> bool {
        let mut run = lock(&self.runs[worker]);
        while bytes <= RUN_BYTES && run.len() + 1 < RUN_TASKS {
            let Some((size, _)) = tasks.front() else {
                break;
            };
            if *size > RUN_BYTES - bytes {
                break;
            }
            bytes += *size;
            if let Some(next) = tasks.pop_front() {
                run.push_back(next);
            }
        }
        self.stealable.fetch_add(run.len(), Ordering::Relaxed);
        !run.is_empty()
    }

    fn close(&self) {
        lock(&self.state).closed = true;
        self.ready.notify_all();
        self.room.notify_all();
    }
}

/// Closes the feed when its holder leaves, by any path: the producer's end
/// wakes the sleeping workers, and a worker's exit stops the producer.
struct CloseOnDrop<'f, Task>(&'f Feed<Task>);

impl<Task> Drop for CloseOnDrop<'_, Task> {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// A group whose capture holds this many bytes or more is handed to the caller
/// alone; smaller groups travel together up to this many bytes in all. Keeps
/// D93's capture bound to within one batch per worker.
const HANDOFF_BYTES: u64 = 64 << 10;
/// At most this many groups travel in one handoff.
const HANDOFF_GROUPS: usize = 16;
/// How long the caller sleeps, with nothing queued or held, before it looks
/// at every held batch again. Bounds how long a held group waits on an idle
/// caller; workers never wake it for a partial batch.
const SWEEP: std::time::Duration = std::time::Duration::from_micros(500);

/// A worker's completed groups not yet handed to the caller: at most
/// [`HANDOFF_BYTES`] and [`HANDOFF_GROUPS`] of them.
struct Held<G> {
    groups: Vec<G>,
    bytes: u64,
}

impl<G> Default for Held<G> {
    fn default() -> Self {
        Self {
            groups: Vec::new(),
            bytes: 0,
        }
    }
}

impl<G> Held<G> {
    /// Holds a completed group of `weight` bytes and returns the batches to
    /// hand off now, in completion order: first the held batch, when this
    /// group would push it past [`HANDOFF_BYTES`] or reaches that size alone;
    /// then the large group alone, or the batch this group filled to
    /// [`HANDOFF_GROUPS`].
    fn add(&mut self, group: G, weight: u64) -> [Option<Vec<G>>; 2] {
        if weight >= HANDOFF_BYTES {
            return [self.take(), Some(vec![group])];
        }
        let crossed = if self.bytes + weight > HANDOFF_BYTES {
            self.take()
        } else {
            None
        };
        self.bytes += weight;
        self.groups.push(group);
        let full = if self.groups.len() >= HANDOFF_GROUPS {
            self.take()
        } else {
            None
        };
        [crossed, full]
    }

    /// The held batch, if any, leaving the slot empty and sized for a full
    /// batch, so that holding the next one does not regrow it a push at a
    /// time.
    fn take(&mut self) -> Option<Vec<G>> {
        self.bytes = 0;
        (!self.groups.is_empty())
            .then(|| std::mem::replace(&mut self.groups, Vec::with_capacity(HANDOFF_GROUPS)))
    }
}

/// Input scheduling for completed file groups, with the worker count.
pub enum GroupSchedule {
    /// A producer that may block: wake a worker for each available task.
    Streaming(usize),
    /// Catalog enumeration: take bounded runs with stealing. Tasks are
    /// pushed and a worker woken a batch at a time: 16 tasks, or fewer once
    /// the first has waited 100 µs, or when enumeration ends.
    Indexed(usize),
}

/// Prepares checked file groups on workers and emits them on the caller in
/// completion order. One thread keeps source order. The producer runs on its
/// own thread, a few tasks per worker ahead of them, and no worker ever calls
/// it; if that thread cannot start, the caller works alone. Returns the
/// [`Shortfall`] if the system refused a worker or the producer.
///
/// Workers hand groups over in batches through a bounded queue of `threads`
/// handoffs: a group whose `weight` (its capture's allocation) reaches
/// [`HANDOFF_BYTES`] goes alone, smaller ones together up to that many bytes.
/// So the queue holds at most `threads` large groups; each worker owns one in
/// progress and a held batch of at most [`HANDOFF_BYTES`]; the caller owns one
/// handoff.
///
/// Workers send only full batches and large groups. The caller takes every
/// held batch whenever the queue is empty, and with nothing held sleeps at
/// most [`SWEEP`] before looking again. So a held group waits on other
/// output or at most [`SWEEP`], never on its worker's next file, nor on a
/// worker asleep waiting for the producer, and a worker never pays a wakeup
/// for a partial batch.
///
/// Each task carries the same input-byte reservation as document
/// verification; callers discard oversized input buffers before returning
/// bounded or spilled groups. A stop wakes the feed and the budget and reaches
/// active reads. The producer receives the same stop predicate and must poll
/// it during filtering or traversal before returning its next task.
pub fn completion_groups<Task: Send, State, Group: Send>(
    schedule: GroupSchedule,
    cancelled: &AtomicBool,
    next: impl FnMut(&dyn Fn() -> bool) -> Option<(u64, Task)> + Send,
    make: impl Fn() -> State + Sync,
    work: impl Fn(&mut State, Task, &dyn Fn() -> bool) -> Group + Sync,
    weight: impl Fn(&Group) -> u64 + Sync,
    emit: impl FnMut(Group) -> ControlFlow<()>,
) -> Option<Shortfall> {
    completion_groups_with_handoff(
        schedule,
        cancelled,
        next,
        make,
        |state, task, stopped, _, _| work(state, task, stopped),
        weight,
        emit,
    )
}

/// Like [`completion_groups`], with handoffs during an active task. A handoff
/// flushes that worker's held batch first and uses the same bounded queue.
/// The final boolean tells `work` whether publication runs concurrently. On
/// the single-thread and spawn-fallback paths handoffs publish synchronously;
/// callers must not wait for that same task to supply more data in that case.
/// A failed handoff means the publisher stopped and the task must unwind.
pub fn completion_groups_with_handoff<Task: Send, State, Group: Send>(
    schedule: GroupSchedule,
    cancelled: &AtomicBool,
    next: impl FnMut(&dyn Fn() -> bool) -> Option<(u64, Task)> + Send,
    make: impl Fn() -> State + Sync,
    work: impl Fn(&mut State, Task, &dyn Fn() -> bool, &dyn Fn(Group) -> bool, bool) -> Group + Sync,
    weight: impl Fn(&Group) -> u64 + Sync,
    mut emit: impl FnMut(Group) -> ControlFlow<()>,
) -> Option<Shortfall> {
    let (threads, indexed) = match schedule {
        GroupSchedule::Streaming(threads) => (threads, false),
        GroupSchedule::Indexed(threads) => (threads, true),
    };
    if threads <= 1 {
        groups_alone_with_handoff(next, cancelled, &make, &work, &mut emit);
        return None;
    }
    let gate = Gate::new();
    let source = Mutex::new(next);
    let feed = Feed::new(threads, indexed);
    let held: Vec<Mutex<Held<Group>>> = (0..threads).map(|_| Mutex::default()).collect();
    let (sender, receiver) = std::sync::mpsc::sync_channel::<Vec<Group>>(threads);
    std::thread::scope(|scope| {
        let _stop = HaltOnDrop(&gate);
        let _close = CloseOnDrop(&feed);
        let make = &make;
        let work = &work;
        let feed = &feed;
        let gate = &gate;
        let stopped = move || gate.halted() || cancelled.load(Ordering::Acquire);
        let weight = &weight;
        let held = &held;
        let mut refused = None;
        let started = (0..threads)
            .take_while(|&i| {
                let sender = sender.clone();
                let started = spawn_named(scope, i, "ferret-verify", move || {
                    let _panic = StopOnPanic(gate);
                    let _close = CloseOnDrop(feed);
                    let mut state = make();
                    let lane = Lane {
                        slot: &held[i],
                        sender,
                        gate,
                    };
                    while !stopped() {
                        let Some((size, task)) = feed.take(i) else {
                            break;
                        };
                        let Some(reservation) = lane.reserve(size, &stopped) else {
                            break;
                        };
                        let handoff = |group| lane.handoff(group, &stopped);
                        let group = work(&mut state, task, &stopped, &handoff, true);
                        drop(reservation);
                        if stopped() {
                            break;
                        }
                        let bytes = weight(&group);
                        if lane.hand(group, bytes).is_break() {
                            break;
                        }
                    }
                    lane.flush(stopped());
                });
                started.map_err(|error| refused = Some(error)).is_ok()
            })
            .count();
        drop(sender);
        let source = &source;
        let fed = started > 0
            && spawn_named(scope, threads, "ferret-producer", move || {
                let _panic = StopOnPanic(gate);
                let _close = CloseOnDrop(feed);
                feed.produce(&mut *lock(source), &stopped);
            })
            .map_err(|error| refused.get_or_insert(error))
            .is_ok();
        let shortfall = refused.map(|cause| Shortfall {
            running: if fed { started } else { 1 },
            asked: threads,
            cause,
        });
        if !fed {
            // Any started workers find the feed closed and empty, and leave.
            feed.close();
            groups_alone_with_handoff(&mut *lock(source), cancelled, make, work, &mut emit);
            return shortfall;
        }
        collect(&receiver, held, &stopped, &mut emit);
        // Dropping the receiver unblocks senders before scope join. The stop
        // guard also wakes input-budget waiters.
        drop(receiver);
        shortfall
    })
}

/// A worker's way to the caller: its slot of held groups, and the bounded
/// queue of handoffs. Shared by [`completion_groups`] and [`lane_groups`].
/// Weight bounds allocations, not publication effects: a zero-weight group
/// may print a count or diagnostic, and is held and swept like any other.
pub struct Lane<'g, Group> {
    slot: &'g Mutex<Held<Group>>,
    sender: std::sync::mpsc::SyncSender<Vec<Group>>,
    gate: &'g Gate,
}

impl<'g, Group> Lane<'g, Group> {
    /// Reserves the input bytes for a task of `size` (at least one read
    /// buffer) until the reservation drops; `None` once the run stops.
    pub fn reserve(&self, size: u64, stopped: &dyn Fn() -> bool) -> Option<Reservation<'g>> {
        let size = self
            .gate
            .reserve(size.max(crate::scan::BUFFER as u64), stopped)?;
        Some(Reservation {
            gate: self.gate,
            size,
        })
    }

    /// Stops the run and wakes every lane waiting for bytes. For a walker
    /// that panics: its siblings poll the stop, not the walker.
    pub fn stop(&self) {
        self.gate.halt();
    }

    /// Hands over an active task's group after flushing its held batch.
    /// Sends outside the slot lock: the publisher may consume this task until
    /// it finishes, so it must never wait on a lock held by the sender.
    /// False means the publisher stopped and the task must unwind.
    pub fn handoff(&self, group: Group, stopped: &dyn Fn() -> bool) -> bool {
        let batch = lock(self.slot).take();
        if let Some(batch) = batch
            && self.sender.send(batch).is_err()
        {
            return false;
        }
        !stopped() && self.sender.send(vec![group]).is_ok()
    }

    /// Holds a completed group of `bytes`, or hands batches over now (see
    /// [`completion_groups`]). Breaks once the caller has gone.
    pub fn hand(&self, group: Group, bytes: u64) -> ControlFlow<()> {
        let handoffs = lock(self.slot).add(group, bytes);
        // Send outside the slot lock: the caller takes that lock to collect,
        // and a full queue waits on the caller.
        for batch in handoffs.into_iter().flatten() {
            if self.sender.send(batch).is_err() {
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }

    /// Out of work, hands over what is held. A stopped run drops it, as the
    /// caller drops queued groups.
    pub fn flush(&self, stopped: bool) {
        // Take first: a guard in the condition would live through a send
        // that may wait on the collecting caller.
        let batch = lock(self.slot).take();
        if !stopped && let Some(batch) = batch {
            let _ = self.sender.send(batch);
        }
    }
}

/// Input bytes held by a [`Lane`]'s task, released when dropped, unwinding
/// included, so a panicking reader cannot strand its siblings.
pub struct Reservation<'g> {
    gate: &'g Gate,
    size: u64,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.gate.release(self.size);
    }
}

/// Collects handoffs on the caller until every lane has gone, the run stops
/// or `emit` breaks. Takes every held batch before sleeping, and sleeps at
/// most [`SWEEP`].
fn collect<Group>(
    receiver: &std::sync::mpsc::Receiver<Vec<Group>>,
    held: &[Mutex<Held<Group>>],
    stopped: &dyn Fn() -> bool,
    emit: &mut impl FnMut(Group) -> ControlFlow<()>,
) {
    // Slots seen empty in a row, and the next slot to look at. Sleeping
    // needs a full clean pass.
    let mut clean = 0;
    let mut at = 0;
    loop {
        let batch = match receiver.try_recv() {
            Ok(batch) => Some(batch),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            Err(std::sync::mpsc::TryRecvError::Empty) if clean < held.len() => {
                let batch = lock(&held[at]).take();
                at = (at + 1) % held.len();
                clean += 1;
                batch
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => match receiver.recv_timeout(SWEEP) {
                Ok(batch) => Some(batch),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    clean = 0;
                    None
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            },
        };
        if let Some(batch) = batch {
            clean = 0;
            if publish(batch, stopped, emit).is_break() {
                break;
            }
        }
    }
}

/// Like [`completion_groups`], for workers that find their own tasks: `run`
/// gets one [`Lane`] per worker and the stop predicate, starts its workers,
/// and returns once they have all finished. The caller emits groups in
/// completion order meanwhile, under the same handoff and input bounds.
///
/// `run` gets its own thread, as the caller is busy emitting; if that thread
/// cannot start, nothing runs and the spawn's error is returned. A worker
/// whose walk panics should [`Lane::stop`] the run before unwinding further.
pub fn lane_groups<'g, Group: Send + 'g>(
    threads: usize,
    cancelled: &AtomicBool,
    run: impl FnOnce(Vec<Lane<'_, Group>>, &(dyn Fn() -> bool + Sync)) + Send,
    mut emit: impl FnMut(Group) -> ControlFlow<()>,
) -> io::Result<()> {
    let threads = threads.max(1);
    let gate = Gate::new();
    let held: Vec<Mutex<Held<Group>>> = (0..threads).map(|_| Mutex::default()).collect();
    let (sender, receiver) = std::sync::mpsc::sync_channel::<Vec<Group>>(threads);
    std::thread::scope(|scope| {
        let _stop = HaltOnDrop(&gate);
        let gate = &gate;
        let held = &held;
        let stopped = move || gate.halted() || cancelled.load(Ordering::Acquire);
        let lanes = (0..threads)
            .map(|i| Lane {
                slot: &held[i],
                sender: sender.clone(),
                gate,
            })
            .collect();
        drop(sender);
        let started = spawn_named(scope, threads, "ferret-walk", move || {
            let _panic = StopOnPanic(gate);
            run(lanes, &stopped);
        })
        .map(drop);
        if started.is_ok() {
            collect(&receiver, held, &stopped, &mut emit);
        }
        drop(receiver);
        started
    })
}

/// Emits a handoff's groups in order until the run stops or `emit` breaks.
fn publish<Group>(
    batch: Vec<Group>,
    halted: &dyn Fn() -> bool,
    emit: &mut impl FnMut(Group) -> ControlFlow<()>,
) -> ControlFlow<()> {
    for group in batch {
        if halted() {
            return ControlFlow::Break(());
        }
        emit(group)?;
    }
    ControlFlow::Continue(())
}

fn groups_alone_with_handoff<Task, State, Group>(
    mut next: impl FnMut(&dyn Fn() -> bool) -> Option<(u64, Task)>,
    cancelled: &AtomicBool,
    make: &impl Fn() -> State,
    work: &impl Fn(&mut State, Task, &dyn Fn() -> bool, &dyn Fn(Group) -> bool, bool) -> Group,
    emit: &mut impl FnMut(Group) -> ControlFlow<()>,
) {
    let stopped = || cancelled.load(Ordering::Acquire);
    let emit = std::cell::RefCell::new(emit);
    let handoff = |group| {
        if stopped() || emit.borrow_mut()(group).is_break() {
            cancelled.store(true, Ordering::Release);
            false
        } else {
            true
        }
    };
    let mut state = make();
    while !stopped() {
        let Some((_, task)) = next(&stopped) else {
            break;
        };
        let item = work(&mut state, task, &stopped, &handoff, false);
        if stopped() || emit.borrow_mut()(item).is_break() {
            break;
        }
    }
}

#[cfg(test)]
mod tests;
