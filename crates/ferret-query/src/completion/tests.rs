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
            while prepared.load(Ordering::Acquire) < 2 * threads + 1 && Instant::now() < deadline {
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
                        // A rendezvous: each send returns once the publisher
                        // has taken the chunk, so the owner that panics after
                        // its first chunk has delivered exactly one.
                        let (sender, receiver) = std::sync::mpsc::sync_channel(0);
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
    if completion_child("active_handoffs_publish_synchronously_on_serial_and_spawn_fallback_paths")
    {
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
