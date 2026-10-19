//! A parallel depth-first walk, generic over the work done per entry. Workers
//! lend part of a walk to each other: a start operand, or half of a level's
//! unvisited siblings. A walk that has lent waits for the borrowers before it
//! completes that level; suspended, it returns to the queue rather than
//! occupying a worker. Find evaluates its expression per entry; grep searches
//! each file.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

/// A walk that can lend part of its remaining work to another worker.
pub trait Split: Send + Sized {
    /// Splits off work: a start operand if `starts` allows, else half of the
    /// shallowest level's unvisited siblings. The counter, when given, is the
    /// lender's count of outstanding loans for that level; it falls by one
    /// when the borrowed walk completes.
    fn split(&mut self, starts: bool) -> Option<(Self, Option<Arc<AtomicUsize>>)>;
    /// Waiting for its loans to complete; not runnable yet.
    fn waiting(&self) -> bool;
    /// Stopped only to wait for its loans.
    fn suspended(&self) -> bool;
}

/// What one step of a task did.
pub enum Step {
    /// Visited an entry; the task goes on.
    Next,
    /// The task's walk ended or suspended.
    Done,
    /// Stop every worker.
    Quit,
}

/// A worker's per-entry work, over tasks whose state is `T` walking `S`.
pub trait Visitor<S, T> {
    /// Advances `walk` by one entry.
    fn step(&mut self, walk: &mut S, task: &mut T) -> Step;
    /// State for work lent from `parent`, after publishing whatever must
    /// precede the borrower's entries.
    fn lend(&mut self, parent: &mut T) -> T;
    /// The task is about to run on this worker.
    fn resume(&mut self, _task: &mut T) {}
    /// The task suspends, to be queued again.
    fn park(&mut self, _task: &mut T) {}
    /// The task has ended; the errors it counted.
    fn end(&mut self, task: &mut T) -> u64;
}

/// How [`run`] spreads a walk.
pub struct Schedule<'q> {
    /// Workers, the calling thread included.
    pub workers: usize,
    /// Start operands may be lent, so they may overlap.
    pub starts: bool,
    /// Set to stop every worker; [`Step::Quit`] and a panic set it too.
    pub quit: &'q AtomicBool,
    /// Called as a worker's panic unwinds, after `quit` is set: for a caller
    /// whose visitors also wait on something `quit` does not wake.
    pub on_panic: Option<&'q (dyn Fn() + Sync)>,
}

/// Lends work only while fewer than this many tasks per worker are queued or
/// running, and only every this many steps of a task.
const QUEUED_PER_WORKER: usize = 2;
const STEPS_PER_LOAN: usize = 64;

/// Walks `walk` from task state `task` on the calling thread with `caller`,
/// lending work once `first` allows a split and more than one worker is
/// scheduled; then `workers - 1` threads join, worker `i` visiting with
/// `visitor(i)`, made on the calling thread. Every task ends on some visitor;
/// those left queued by a quit end on `caller`. Returns the errors the tasks
/// counted.
pub fn run<S: Split, T: Send, V: Visitor<S, T> + Send>(
    schedule: &Schedule<'_>,
    mut walk: S,
    mut task: T,
    caller: &mut V,
    first: impl Fn(&S) -> bool,
    visitor: impl FnMut(usize) -> V,
) -> u64 {
    let quit = schedule.quit;
    loop {
        if quit.load(Ordering::Acquire) {
            break;
        }
        match caller.step(&mut walk, &mut task) {
            Step::Next => {}
            Step::Done => break,
            Step::Quit => {
                quit.store(true, Ordering::Release);
                break;
            }
        }
        if schedule.workers > 1
            && first(&walk)
            && let Some((lent, completion)) = walk.split(schedule.starts)
        {
            let borrowed = caller.lend(&mut task);
            let pool = Pool {
                queue: Mutex::new(Queue {
                    tasks: vec![
                        Lent {
                            walk,
                            task,
                            completion: None,
                        },
                        Lent {
                            walk: lent,
                            task: borrowed,
                            completion,
                        },
                    ],
                    active: 0,
                }),
                changed: Condvar::new(),
                schedule,
            };
            return pool.run(caller, visitor);
        }
    }
    caller.end(&mut task)
}

struct Lent<S, T> {
    walk: S,
    task: T,
    /// The lender's loan count, for a borrowed walk.
    completion: Option<Arc<AtomicUsize>>,
}

impl<S, T> Lent<S, T> {
    fn end<V: Visitor<S, T>>(mut self, visitor: &mut V) -> u64 {
        let errors = visitor.end(&mut self.task);
        if let Some(completion) = self.completion {
            completion.fetch_sub(1, Ordering::Release);
        }
        errors
    }
}

struct Queue<S, T> {
    tasks: Vec<Lent<S, T>>,
    active: usize,
}

struct Pool<'s, 'q, S, T> {
    queue: Mutex<Queue<S, T>>,
    changed: Condvar,
    schedule: &'s Schedule<'q>,
}

// A worker panic must wake siblings before the scoped join can propagate it.
// Taking the queue lock closes the check-then-condvar-wait lost-wakeup window.
// The caller's hook runs outside that lock.
struct PanicStop<'p, 's, 'q, S, T>(&'p Pool<'s, 'q, S, T>);
impl<S, T> Drop for PanicStop<'_, '_, '_, S, T> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            {
                let _queue = self.0.lock();
                self.0.schedule.quit.store(true, Ordering::Release);
                self.0.changed.notify_all();
            }
            if let Some(on_panic) = self.0.schedule.on_panic {
                on_panic();
            }
        }
    }
}

impl<S, T> Pool<'_, '_, S, T> {
    fn lock(&self) -> MutexGuard<'_, Queue<S, T>> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<S: Split, T: Send> Pool<'_, '_, S, T> {
    fn run<V: Visitor<S, T> + Send>(
        self,
        caller: &mut V,
        mut visitor: impl FnMut(usize) -> V,
    ) -> u64 {
        let errors = std::thread::scope(|scope| {
            let pool = &self;
            // From here the caller's panic, even in `visitor`, wakes the
            // workers already started.
            let _panic = PanicStop(pool);
            // A refused spawn leaves the walk to fewer workers: the caller is
            // one, and any worker takes any queued task.
            let handles: Vec<_> = (1..self.schedule.workers)
                .filter_map(|i| {
                    let mut visitor = visitor(i);
                    crate::verify::spawn_named(scope, i, "ferret-walk", move || {
                        pool.worker(&mut visitor)
                    })
                    .ok()
                })
                .collect();
            let mut errors = pool.worker(caller);
            for handle in handles {
                errors += handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            }
            errors
        });
        let queue = self
            .queue
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        errors
            + queue
                .tasks
                .into_iter()
                .map(|lent| lent.end(caller))
                .sum::<u64>()
    }

    fn worker<V: Visitor<S, T>>(&self, visitor: &mut V) -> u64 {
        let _panic = PanicStop(self);
        let quit = self.schedule.quit;
        let mut errors = 0;
        loop {
            let mut queue = self.lock();
            let mut lent = loop {
                if quit.load(Ordering::Acquire) {
                    return errors;
                }
                if let Some(index) = queue.tasks.iter().rposition(|lent| !lent.walk.waiting()) {
                    queue.active += 1;
                    break queue.tasks.swap_remove(index);
                }
                if queue.active == 0 {
                    return errors;
                }
                queue = self
                    .changed
                    .wait(queue)
                    .unwrap_or_else(PoisonError::into_inner);
            };
            drop(queue);
            visitor.resume(&mut lent.task);
            let mut steps = 0usize;
            while !quit.load(Ordering::Acquire) {
                match visitor.step(&mut lent.walk, &mut lent.task) {
                    Step::Next => {}
                    Step::Done => break,
                    Step::Quit => {
                        quit.store(true, Ordering::Release);
                        break;
                    }
                }
                if steps.is_multiple_of(STEPS_PER_LOAN) {
                    let mut queue = self.lock();
                    if queue.tasks.len() + queue.active < self.schedule.workers * QUEUED_PER_WORKER
                        && let Some((walk, completion)) = lent.walk.split(self.schedule.starts)
                    {
                        // Under the queue lock, as the borrower must not run
                        // before the lender has published.
                        let task = visitor.lend(&mut lent.task);
                        queue.tasks.push(Lent {
                            walk,
                            task,
                            completion,
                        });
                        self.changed.notify_all();
                    }
                }
                steps += 1;
            }
            // Completion can race with the None returned for suspension. Use
            // the recorded reason, not a second counter read, to
            // retain the task.
            let waiting = lent.walk.suspended() && !quit.load(Ordering::Acquire);
            let suspended = if waiting {
                visitor.park(&mut lent.task);
                Some(lent)
            } else {
                errors += lent.end(visitor);
                None
            };
            let mut queue = self.lock();
            queue.active -= 1;
            if let Some(lent) = suspended {
                queue.tasks.push(lent);
            }
            self.changed.notify_all();
        }
    }
}

/// Runs `tasks`, already queued, on one worker: `visitor`.
#[cfg(test)]
pub(crate) fn queued<S: Split, T: Send, V: Visitor<S, T>>(
    schedule: &Schedule<'_>,
    tasks: Vec<(S, T)>,
    visitor: &mut V,
) -> u64 {
    let pool = Pool {
        queue: Mutex::new(Queue {
            tasks: tasks
                .into_iter()
                .map(|(walk, task)| Lent {
                    walk,
                    task,
                    completion: None,
                })
                .collect(),
            active: 0,
        }),
        changed: Condvar::new(),
        schedule,
    };
    pool.worker(visitor)
}
