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
mod tests {
    use super::*;
    use crate::budget::IN_FLIGHT_BYTES;
    use crate::verify::seam;
    use std::sync::Barrier;

    fn completion_child(name: &str) -> bool {
        const MARKER: &str = "FERRET_TEST_COMPLETION_CHILD";
        if std::env::var_os(MARKER).is_some() {
            return false;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("completion::tests::{name}")])
            .env(MARKER, name)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                return true;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("completion worker left a waiter asleep");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // D106 B: the owner cannot step or lend while task zero is blocked.
    // Every other task in its run must still reach the real publisher.
    #[test]
    fn indexed_run_is_stolen_while_its_head_is_blocked() {
        let published = AtomicUsize::new(0);
        let mut tasks = (0..RUN_TASKS).map(|task| (1, task));
        completion_groups(
            GroupSchedule::Indexed(2),
            &AtomicBool::new(false),
            |_| tasks.next(),
            || (),
            |_, task, _| {
                if task == 0 {
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while published.load(Ordering::Acquire) < RUN_TASKS - 1 {
                        assert!(Instant::now() < deadline, "run stranded behind its head");
                        std::thread::yield_now();
                    }
                }
                task
            },
            |_| 0,
            |task| {
                if task != 0 {
                    published.fetch_add(1, Ordering::Release);
                }
                ControlFlow::Continue(())
            },
        );
        assert_eq!(published.load(Ordering::Acquire), RUN_TASKS - 1);
    }

    // A full queue must bound captures, and dropping its receiver on Break
    // must unblock senders before scope join. Isolate a regression's deadlock.
    // Groups exactly at the handoff threshold go alone, so the bound stays
    // one group per queue slot, per worker and on the caller.
    #[test]
    fn completion_queue_bounds_captures_and_releases_blocked_senders() {
        if completion_child("completion_queue_bounds_captures_and_releases_blocked_senders") {
            return;
        }
        let threads = 4;
        let prepared = AtomicUsize::new(0);
        let mut tasks = (0..256).map(|task| (1, task));
        let mut emitted = 0;
        completion_groups(
            GroupSchedule::Streaming(threads),
            &AtomicBool::new(false),
            |_| tasks.next(),
            || (),
            |_, task, _| {
                prepared.fetch_add(1, Ordering::Release);
                task
            },
            |_| HANDOFF_BYTES,
            |_| {
                emitted += 1;
                let deadline = Instant::now() + Duration::from_secs(2);
                while prepared.load(Ordering::Acquire) < 2 * threads + 1
                    && Instant::now() < deadline
                {
                    std::thread::yield_now();
                }
                // Once saturated, another prepare cannot finish until the
                // caller consumes a group, which it deliberately does not.
                std::thread::sleep(Duration::from_millis(10));
                assert_eq!(prepared.load(Ordering::Acquire), 2 * threads + 1);
                ControlFlow::Break(())
            },
        );
        assert_eq!(emitted, 1);
    }

    /// Runs `body`, catching its panic, and returns the message of every panic
    /// on any thread, in order. The hook is process-wide: call this only in a
    /// [`completion_child`], which runs one test per process.
    fn panics(body: impl FnOnce()) -> Vec<String> {
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new({
            let seen = seen.clone();
            move |info| {
                let payload = info.payload();
                let message = payload
                    .downcast_ref::<&str>()
                    .map(|message| (*message).to_owned())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                lock(&seen).push(message);
            }
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        std::panic::set_hook(previous);
        std::mem::take(&mut *lock(&seen))
    }

    /// Asserts the only panic was `intended`, re-raised by the scopes it
    /// unwound through.
    fn assert_only_panic(seen: &[String], intended: &str) {
        assert!(
            seen.first().is_some_and(|first| first == intended)
                && seen[1..]
                    .iter()
                    .all(|rest| rest == "a scoped thread panicked"),
            "expected only {intended:?}, saw {seen:?}"
        );
    }

    // A panicking worker must stop the run before the scope joins: a sibling
    // waiting for bytes that the panic's own release cannot satisfy, and a
    // reader polling the stop, would otherwise sleep forever. The poller
    // keeps all but one buffer of the budget, so only the stop wakes the
    // waiter.
    #[test]
    fn completion_worker_panic_wakes_input_budget_waiters() {
        if completion_child("completion_worker_panic_wakes_input_budget_waiters") {
            return;
        }
        #[derive(Clone, Copy, Debug)]
        enum Task {
            Poll,
            Panic,
            Wait,
        }
        const INJECTED: &str = "injected worker panic with a sibling waiting";
        let buffer = crate::scan::BUFFER as u64;
        let waiting = std::sync::Arc::new(AtomicBool::new(false));
        let _seams = seam::Seams {
            on_budget_wait: Some({
                let waiting = waiting.clone();
                std::sync::Arc::new(move || waiting.store(true, Ordering::SeqCst))
            }),
            ..Default::default()
        }
        .set();
        let polling = AtomicBool::new(false);
        let serial = AtomicBool::new(false);
        let mut tasks = [
            (IN_FLIGHT_BYTES - buffer, Task::Poll),
            (buffer, Task::Panic),
            (2 * buffer, Task::Wait),
        ]
        .into_iter();
        let until = |flag: &AtomicBool, stopped: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !flag.load(Ordering::SeqCst) && !stopped() && Instant::now() < deadline {
                std::thread::yield_now();
            }
            flag.load(Ordering::SeqCst)
        };
        let seen = panics(|| {
            completion_groups_with_handoff(
                GroupSchedule::Streaming(3),
                &AtomicBool::new(false),
                |stopped| {
                    // The poller reserves first, so the panicking reader's
                    // buffer fits beside it and the waiter's two do not.
                    let next = tasks.next()?;
                    if !matches!(next.1, Task::Poll) {
                        until(&polling, stopped);
                    }
                    Some(next)
                },
                || (),
                |_, task, stopped, _, concurrent| {
                    if !concurrent {
                        serial.store(true, Ordering::SeqCst);
                        polling.store(true, Ordering::SeqCst);
                        return;
                    }
                    match task {
                        Task::Poll => {
                            polling.store(true, Ordering::SeqCst);
                            while !stopped() {
                                std::thread::yield_now();
                            }
                        }
                        Task::Panic if until(&waiting, &|| false) => panic!("{INJECTED}"),
                        Task::Panic => panic!("setup: no sibling waited for bytes"),
                        Task::Wait => {}
                    }
                },
                |_| 0,
                |()| ControlFlow::Continue(()),
            );
        });
        assert!(
            !serial.load(Ordering::SeqCst),
            "no worker threads: {seen:?}"
        );
        assert!(
            polling.load(Ordering::SeqCst),
            "the poller never ran: {seen:?}"
        );
        assert!(
            waiting.load(Ordering::SeqCst),
            "no sibling waited: {seen:?}"
        );
        assert_only_panic(&seen, INJECTED);
    }

    // The first task is a quiet hit and the producer then blocks until that
    // hit cancels the run, as grep's filtering producer can. A worker that
    // fetched the hit and called the producer again before working it would
    // wait forever (grep-why P2); isolate a regression's deadlock. An
    // indexed producer holding the hit in its batch must push it from its
    // polls (Astra, 2026-10-31).
    #[test]
    fn a_first_task_hit_reaches_a_worker_while_the_producer_blocks() {
        if completion_child("a_first_task_hit_reaches_a_worker_while_the_producer_blocks") {
            return;
        }
        for schedule in [GroupSchedule::Streaming(2), GroupSchedule::Indexed(2)] {
            let cancelled = AtomicBool::new(false);
            let mut calls = 0;
            completion_groups(
                schedule,
                &cancelled,
                |stopped| {
                    calls += 1;
                    if calls == 1 {
                        return Some((1, ()));
                    }
                    while !stopped() {
                        std::thread::yield_now();
                    }
                    None
                },
                || (),
                |_, (), _| cancelled.store(true, Ordering::Release),
                |_| 0,
                |()| ControlFlow::Continue(()),
            );
            assert!(cancelled.load(Ordering::Acquire));
        }
    }

    // Small groups travel in batches, so more are prepared before the queue
    // stalls than one per slot, but never more than the batch rules allow:
    // with quarter-threshold groups, four per batch, each worker may hold a
    // full batch it is sending plus one group, the queue a full batch per
    // slot and the caller one batch.
    #[test]
    fn completion_queue_bounds_small_groups_by_batch() {
        if completion_child("completion_queue_bounds_small_groups_by_batch") {
            return;
        }
        let threads = 4;
        let prepared = AtomicUsize::new(0);
        let mut tasks = (0..1024).map(|task| (1, task));
        completion_groups(
            GroupSchedule::Streaming(threads),
            &AtomicBool::new(false),
            |_| tasks.next(),
            || (),
            |_, task, _| {
                prepared.fetch_add(1, Ordering::Release);
                task
            },
            |_| HANDOFF_BYTES / 4,
            |_| {
                // Wait for preparation to stall, then never consume more.
                let mut seen = prepared.load(Ordering::Acquire);
                loop {
                    std::thread::sleep(Duration::from_millis(50));
                    let now = prepared.load(Ordering::Acquire);
                    if now == seen {
                        break;
                    }
                    seen = now;
                }
                assert!(seen > 2 * threads + 1, "groups were not batched: {seen}");
                assert!(
                    seen <= 9 * threads + 4,
                    "batches exceeded the bound: {seen}"
                );
                ControlFlow::Break(())
            },
        );
    }

    // A group a worker completes while the caller is idle is published while
    // its worker's next task, which here is slow, is still running: the
    // caller's sweep collects it.
    #[test]
    fn group_completed_while_the_caller_is_idle_publishes_before_a_slow_task() {
        let published = AtomicBool::new(false);
        let waited_out = AtomicBool::new(false);
        let mut tasks = (0..3).map(|task| (1, task));
        let mut emitted = Vec::new();
        completion_groups(
            GroupSchedule::Streaming(2),
            &AtomicBool::new(false),
            |_| tasks.next(),
            || (),
            |_, task, _| {
                // Tasks 0 and 2 finish only once something is published;
                // task 1 is the fast one its worker holds.
                if task != 1 {
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while !published.load(Ordering::Acquire) {
                        if Instant::now() >= deadline {
                            waited_out.store(true, Ordering::Release);
                            break;
                        }
                        std::thread::yield_now();
                    }
                }
                task
            },
            |_| 0,
            |task| {
                emitted.push(task);
                published.store(true, Ordering::Release);
                ControlFlow::Continue(())
            },
        );
        assert!(!waited_out.load(Ordering::Acquire));
        assert_eq!(emitted[0], 1);
        emitted.sort_unstable();
        assert_eq!(emitted, [0, 1, 2]);
    }

    // A worker that completes a group while the caller is busy holds it, then
    // blocks on a slow task. When the caller runs out of queued groups it
    // must collect the held one rather than sleep: both other tasks finish
    // only once that group is published.
    #[test]
    fn group_held_while_the_caller_is_busy_publishes_before_a_slow_task() {
        let emitting = AtomicBool::new(false);
        let held_done = AtomicBool::new(false);
        let published = AtomicBool::new(false);
        let waited_out = AtomicBool::new(false);
        let wait_for = |flag: &AtomicBool| {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !flag.load(Ordering::Acquire) {
                if Instant::now() >= deadline {
                    waited_out.store(true, Ordering::Release);
                    return;
                }
                std::thread::yield_now();
            }
        };
        let mut tasks = (0..4).map(|task| (1, task));
        let mut emitted = Vec::new();
        completion_groups(
            GroupSchedule::Streaming(2),
            &AtomicBool::new(false),
            |_| tasks.next(),
            || (),
            |_, task, _| {
                match task {
                    // Task 0 keeps the caller busy, below.
                    0 => {}
                    // Task 1 completes while the caller publishes task 0.
                    1 => {
                        wait_for(&emitting);
                        held_done.store(true, Ordering::Release);
                    }
                    // Whichever worker takes 2 and 3 is stuck until task 1
                    // is published, so neither can hand it off.
                    _ => wait_for(&published),
                }
                task
            },
            |_| 0,
            |task| {
                if task == 0 {
                    emitting.store(true, Ordering::Release);
                    wait_for(&held_done);
                    // Let task 1's worker hold it and start its next task.
                    std::thread::sleep(Duration::from_millis(50));
                }
                if task == 1 {
                    published.store(true, Ordering::Release);
                }
                emitted.push(task);
                ControlFlow::Continue(())
            },
        );
        assert!(!waited_out.load(Ordering::Acquire));
        assert_eq!(emitted[..2], [0, 1]);
        emitted.sort_unstable();
        assert_eq!(emitted, [0, 1, 2, 3]);
    }

    // The batch rules, as the workers apply them: the threshold is inclusive,
    // a group that would cross it flushes the held batch first, a large group
    // follows the batch it overtakes, and a batch is capped in count.
    #[test]
    fn held_batches_follow_the_handoff_rules() {
        let mut held = Held::<u32>::default();
        // Just under the threshold is held; exactly at it goes alone.
        assert_eq!(held.add(0, HANDOFF_BYTES - 1), [None, None]);
        assert_eq!(held.add(1, HANDOFF_BYTES), [Some(vec![0]), Some(vec![1])]);
        assert_eq!(held.take(), None);
        // Just over goes alone too, with nothing held before it.
        assert_eq!(held.add(2, HANDOFF_BYTES + 1), [None, Some(vec![2])]);
        // Filling the batch to exactly the threshold holds it; one more byte
        // flushes it and starts the next batch.
        assert_eq!(held.add(3, HANDOFF_BYTES / 2), [None, None]);
        assert_eq!(held.add(4, HANDOFF_BYTES / 2), [None, None]);
        assert_eq!(held.add(5, 1), [Some(vec![3, 4]), None]);
        assert_eq!(held.bytes, 1);
        // A batch that would cross by its last group flushes before it.
        assert_eq!(held.add(6, HANDOFF_BYTES - 2), [None, None]);
        assert_eq!(held.add(7, 2), [Some(vec![5, 6]), None]);
        assert_eq!(held.take(), Some(vec![7]));
        // Count cap: the sixteenth small group hands the batch off.
        for group in 0..HANDOFF_GROUPS as u32 - 1 {
            assert_eq!(held.add(group, 0), [None, None]);
        }
        let full = held.add(99, 0);
        assert_eq!(full[0], None);
        assert_eq!(full[1].as_ref().map(Vec::len), Some(HANDOFF_GROUPS));
        assert_eq!(held.take(), None);
    }

    // Four workers each hold their first task at a barrier, so the groups
    // are prepared in parallel; each payload arrives whole, once, and small
    // ones travel through the held batches.
    #[test]
    fn groups_use_parallel_workers_and_preserve_each_payload() {
        if completion_child("groups_use_parallel_workers_and_preserve_each_payload") {
            return;
        }
        let barrier = Barrier::new(4);
        let mut emitted = Vec::new();
        let mut tasks = (0..80).map(|at| (1, at));
        completion_groups(
            GroupSchedule::Streaming(4),
            &AtomicBool::new(false),
            move |_| tasks.next(),
            || false,
            |started, at, _| {
                if !*started {
                    *started = true;
                    barrier.wait();
                }
                (at, vec![at as u8; 1000])
            },
            |(_, payload)| payload.len() as u64,
            |item| {
                emitted.push(item);
                ControlFlow::Continue(())
            },
        );
        emitted.sort();
        assert_eq!(
            emitted,
            (0..80)
                .map(|at| (at, vec![at as u8; 1000]))
                .collect::<Vec<_>>()
        );
    }

    // Every task but the first waits for the stop. The caller's Break must
    // reach those workers and the producer, blocked on a full feed, before
    // the scope joins; isolate a regression's deadlock.
    #[test]
    fn a_group_emission_break_cancels_in_progress_work() {
        if completion_child("a_group_emission_break_cancels_in_progress_work") {
            return;
        }
        let mut emitted = Vec::new();
        let mut tasks = (0..80).map(|at| (1, at));
        completion_groups(
            GroupSchedule::Streaming(4),
            &AtomicBool::new(false),
            move |_| tasks.next(),
            || (),
            |_, at, stopped| {
                if at != 0 {
                    while !stopped() {
                        std::thread::yield_now();
                    }
                }
                at
            },
            |_| 0,
            |at| {
                emitted.push(at);
                ControlFlow::Break(())
            },
        );
        assert_eq!(emitted, [0]);
    }

    // A refused spawn still emits every group: with no worker or no producer
    // the caller works alone, in source order; with some workers started,
    // they share the work in completion order.
    #[test]
    fn spawn_failure_still_emits_every_group() {
        if completion_child("spawn_failure_still_emits_every_group") {
            return;
        }
        let threads = 4;
        // (thread whose spawn fails, whether the caller works alone)
        for (fail, alone) in [(0, true), (1, false), (3, false), (threads, true)] {
            let _seams = seam::Seams {
                fail_spawn_at: Some(fail),
                ..Default::default()
            }
            .set();
            let caller = std::thread::current().id();
            let mut tasks = (0..32).map(|task| (1, task));
            let mut emitted = Vec::new();
            let on_caller = AtomicUsize::new(0);
            completion_groups(
                GroupSchedule::Streaming(threads),
                &AtomicBool::new(false),
                |_| tasks.next(),
                || (),
                |_, task, _| {
                    if std::thread::current().id() == caller {
                        on_caller.fetch_add(1, Ordering::Relaxed);
                    }
                    task
                },
                |_| 0,
                |task| {
                    emitted.push(task);
                    ControlFlow::Continue(())
                },
            );
            let on_caller = on_caller.into_inner();
            if alone {
                assert_eq!(emitted, (0..32).collect::<Vec<_>>(), "fail at {fail}");
                assert_eq!(on_caller, 32, "fail at {fail}");
            } else {
                emitted.sort_unstable();
                assert_eq!(emitted, (0..32).collect::<Vec<_>>(), "fail at {fail}");
                assert_eq!(on_caller, 0, "fail at {fail}");
            }
        }
    }

    // A refused walk-thread spawn must not pass for an empty search: the
    // caller either runs the search or hears why it could not.
    #[test]
    fn a_refused_walk_spawn_is_reported() {
        let threads = 2;
        let _seams = seam::Seams {
            fail_spawn_at: Some(threads),
            ..Default::default()
        }
        .set();
        let ran = AtomicBool::new(false);
        let result = lane_groups(
            threads,
            &AtomicBool::new(false),
            |_, _| ran.store(true, Ordering::Relaxed),
            |(): ()| ControlFlow::Continue(()),
        );
        assert!(!ran.load(Ordering::Relaxed));
        let error = result.expect_err("the search never ran, silently");
        assert_eq!(error.to_string(), "injected spawn failure");
    }

    // A reservation taken by a task that panics is released as it unwinds,
    // so a later reader is not left waiting on leaked bytes.
    #[test]
    fn a_panicking_reader_releases_its_reservation() {
        if completion_child("a_panicking_reader_releases_its_reservation") {
            return;
        }
        lane_groups(
            1,
            &AtomicBool::new(false),
            |lanes, stopped| {
                let lane = &lanes[0];
                let held = lane.reserve(u64::MAX, stopped);
                assert!(held.is_some(), "the reader never held the budget");
                let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _held = held;
                    panic!("reader fails mid-read");
                }));
                let payload = unwound.expect_err("the reader did not panic");
                assert_eq!(
                    payload.downcast_ref::<&str>(),
                    Some(&"reader fails mid-read")
                );
                assert!(lane.reserve(u64::MAX, stopped).is_some());
            },
            |(): ()| ControlFlow::Continue(()),
        )
        .unwrap();
    }

    /// A walk of [`Job`]s for the find scheduler. Its first step lends every
    /// job but the next, so each job after it runs as its own task.
    struct Jobs(Vec<Job>);

    #[derive(Clone, Copy)]
    enum Job {
        /// Does nothing, so the walk splits.
        Lend,
        /// Reserves the whole input budget and panics once a sibling waits
        /// for bytes.
        Hog,
        /// Waits for the hog's reservation, then reserves.
        Wait,
        /// Panics once a sibling polls.
        Panic,
        /// Holds a reservation until the run stops, as a slow read does.
        Poll,
    }

    impl crate::find::schedule::Split for Jobs {
        fn split(
            &mut self,
            _: bool,
            _: crate::find::schedule::Share,
        ) -> Option<(Self, Option<std::sync::Arc<AtomicUsize>>)> {
            (self.0.len() > 1).then(|| (Jobs(self.0.split_off(1)), None))
        }
        fn waiting(&self) -> bool {
            false
        }
        fn suspended(&self) -> bool {
            false
        }
    }

    /// Runs [`Job`]s against its lane, as grep's walk workers search files.
    struct Jobber<'l, 'g> {
        lane: &'l Lane<'g, ()>,
        stopped: &'l (dyn Fn() -> bool + Sync),
        /// Both jobs of a run are on their own workers before either acts.
        met: &'l Barrier,
        hogged: &'l AtomicBool,
        waiting: &'l AtomicBool,
        polling: &'l AtomicBool,
    }

    impl crate::find::schedule::Visitor<Jobs, ()> for Jobber<'_, '_> {
        fn step(&mut self, walk: &mut Jobs, (): &mut ()) -> crate::find::schedule::Step {
            use crate::find::schedule::Step;
            if walk.0.is_empty() {
                return Step::Done;
            }
            let job = walk.0.remove(0);
            if !matches!(job, Job::Lend) {
                self.met.wait();
            }
            let until = |flag: &AtomicBool| {
                while !flag.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            };
            match job {
                Job::Lend => {}
                Job::Hog => {
                    let _held = self.lane.reserve(u64::MAX, self.stopped);
                    self.hogged.store(true, Ordering::SeqCst);
                    until(self.waiting);
                    panic!("hog fails holding the budget");
                }
                Job::Wait => {
                    until(self.hogged);
                    let _held = self.lane.reserve(1, self.stopped);
                }
                Job::Panic => {
                    until(self.polling);
                    panic!("walker fails")
                }
                Job::Poll => {
                    let _held = self.lane.reserve(1, self.stopped);
                    self.polling.store(true, Ordering::SeqCst);
                    while !(self.stopped)() {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
            Step::Next
        }
        fn lend(&mut self, (): &mut ()) {}
        fn end(&mut self, (): &mut ()) -> u64 {
            0
        }
    }

    /// Walks `jobs` with two scheduler workers inside [`lane_groups`], as
    /// grep's live walk does, and returns its [`panics`].
    fn walk_jobs(jobs: Vec<Job>) -> Vec<String> {
        let waiting = std::sync::Arc::new(AtomicBool::new(false));
        let _seams = seam::Seams {
            on_budget_wait: Some({
                let waiting = waiting.clone();
                std::sync::Arc::new(move || waiting.store(true, Ordering::SeqCst))
            }),
            ..Default::default()
        }
        .set();
        let met = Barrier::new(2);
        let hogged = AtomicBool::new(false);
        let polling = AtomicBool::new(false);
        panics(|| {
            let _ = lane_groups(
                2,
                &AtomicBool::new(false),
                |lanes, stopped| {
                    let quit = AtomicBool::new(false);
                    let stop = || lanes[0].stop();
                    let schedule = crate::find::schedule::Schedule {
                        workers: 2,
                        starts: false,
                        quit: &quit,
                        on_panic: Some(&stop),
                        on_refused: None,
                    };
                    let jobber = |i: usize| Jobber {
                        lane: &lanes[i],
                        stopped,
                        met: &met,
                        hogged: &hogged,
                        waiting: &waiting,
                        polling: &polling,
                    };
                    crate::find::schedule::run(
                        &schedule,
                        Jobs(jobs),
                        (),
                        &mut jobber(0),
                        |_| true,
                        jobber,
                    );
                },
                |(): ()| ControlFlow::Continue(()),
            );
        })
    }

    // P2 of the D105 review: a walker panicking inside the scheduler stops
    // the lane pool, so a sibling waiting for bytes, or reading until the
    // run stops, wakes and the panic reaches the caller.
    #[test]
    fn a_walker_panic_stops_sibling_walkers() {
        if completion_child("a_walker_panic_stops_sibling_walkers") {
            return;
        }
        // Each panic happens only after its sibling is waiting or polling.
        let seen = walk_jobs(vec![Job::Lend, Job::Hog, Job::Wait]);
        assert_only_panic(&seen, "hog fails holding the budget");
        let seen = walk_jobs(vec![Job::Lend, Job::Panic, Job::Poll]);
        assert_only_panic(&seen, "walker fails");
    }

    // Streaming keeps the input reservation while the publisher consumes an
    // active task. A sibling waiting for that budget must not strand either
    // thread, including when publication stops or the stream owner panics.
    #[test]
    fn active_lane_streams_release_budget_waiters_and_channel_senders() {
        if completion_child("active_lane_streams_release_budget_waiters_and_channel_senders") {
            return;
        }
        enum Handoff {
            Held,
            Stream(std::sync::mpsc::Receiver<usize>),
        }
        for mode in 0..3 {
            let waiting = std::sync::Arc::new(AtomicBool::new(false));
            let _seams = seam::Seams {
                on_budget_wait: Some({
                    let waiting = waiting.clone();
                    std::sync::Arc::new(move || waiting.store(true, Ordering::SeqCst))
                }),
                ..Default::default()
            }
            .set();
            let mut held = false;
            let mut received = 0;
            let seen = panics(|| {
                lane_groups(
                    2,
                    &AtomicBool::new(false),
                    |lanes, stopped| {
                        std::thread::scope(|scope| {
                            let _reservation = lanes[0].reserve(u64::MAX, stopped).unwrap();
                            let sibling = &lanes[1];
                            scope.spawn(move || {
                                let _reservation = sibling.reserve(1, stopped);
                            });
                            while !waiting.load(Ordering::SeqCst) {
                                std::thread::yield_now();
                            }
                            assert!(lanes[0].hand(Handoff::Held, 0).is_continue());
                            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
                            assert!(lanes[0].handoff(Handoff::Stream(receiver), stopped));
                            for chunk in 0..32 {
                                if sender.send(chunk).is_err() {
                                    break;
                                }
                                assert!(mode != 2, "stream owner panics after its first chunk");
                            }
                        });
                    },
                    |handoff| match handoff {
                        Handoff::Held => {
                            held = true;
                            ControlFlow::Continue(())
                        }
                        Handoff::Stream(receiver) => {
                            assert!(held, "active handoff did not flush the held batch");
                            for chunk in receiver {
                                assert_eq!(chunk, received);
                                received += 1;
                                if mode == 1 {
                                    return ControlFlow::Break(());
                                }
                            }
                            ControlFlow::Continue(())
                        }
                    },
                )
                .unwrap();
            });
            if mode == 2 {
                assert_only_panic(&seen, "stream owner panics after its first chunk");
            } else {
                assert_eq!(seen, Vec::<String>::new());
            }
            assert_eq!(received, if mode == 0 { 32 } else { 1 });
        }
    }

    // An active task cannot wait for its own synchronous publisher. Both
    // worker-start and producer-start fallback must expose that same mode.
    #[test]
    fn active_handoffs_publish_synchronously_on_serial_and_spawn_fallback_paths() {
        if completion_child(
            "active_handoffs_publish_synchronously_on_serial_and_spawn_fallback_paths",
        ) {
            return;
        }
        for (threads, fail_spawn_at) in [(1, None), (2, Some(0)), (2, Some(2))] {
            let _seams = seam::Seams {
                fail_spawn_at,
                ..Default::default()
            }
            .set();
            let mut tasks = (0..4).map(|task| (1, task));
            let mut emitted = Vec::new();
            completion_groups_with_handoff(
                GroupSchedule::Streaming(threads),
                &AtomicBool::new(false),
                |_| tasks.next(),
                || (),
                |_, task, _, handoff, concurrent| {
                    assert!(!concurrent);
                    assert!(handoff(task * 2));
                    task * 2 + 1
                },
                |_| 0,
                |item| {
                    emitted.push(item);
                    ControlFlow::Continue(())
                },
            );
            assert_eq!(emitted, (0..8).collect::<Vec<_>>());
        }
    }
}
