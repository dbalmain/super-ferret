//! Bounded sibling donation over the ordinary DFS engine. Suspended parents
//! return to the queue rather than occupying a worker while descendants run.
//! Tasks own expression control and directory-local batches; ordinary batches
//! are shared across the run. Effects are
//! worker-local handles to the host's synchronized output and prompt sinks.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use super::{
    Control, Effects, EntrySource, EvaluationError, Expression, LiveWalk, Outcome, Plan,
    Unsupported, WalkError, evaluate,
};

struct Task<W = LiveWalk> {
    walk: W,
    control: Control,
    descend: bool,
    completion: Option<Arc<AtomicUsize>>,
    errors: u64,
    record: super::output::Record,
    gate: Arc<Mutex<()>>,
    quit: Arc<AtomicBool>,
}

impl<W: EntrySource> Task<W> {
    fn new(walk: W, completion: Option<Arc<AtomicUsize>>, quit: &Arc<AtomicBool>) -> Self {
        let gate = Arc::new(Mutex::new(()));
        let mut control = Control {
            cancelled: Some(quit.clone()),
            ..Control::default()
        };
        control.actions.gate = gate.clone();
        control.actions.quit = quit.clone();
        Self {
            walk,
            control,
            descend: true,
            completion,
            errors: 0,
            record: super::output::Record::default(),
            gate,
            quit: quit.clone(),
        }
    }

    fn step(
        &mut self,
        plan: &Plan,
        expression: &Expression,
        effects: &mut impl Effects,
        policy: super::output::Policy,
    ) -> bool {
        if effects.cancelled() {
            self.quit.store(true, Ordering::Release);
            return false;
        }
        let effects = std::cell::RefCell::new(effects);
        let quit = self.quit.clone();
        let item = {
            let mut before_directory = || {
                self.control
                    .actions
                    .flush(&mut **effects.borrow_mut(), true)
                    .inspect_err(|_| {
                        self.control.quit = true;
                    })
            };
            let mut cancelled = || {
                if effects.borrow().cancelled() {
                    quit.store(true, Ordering::Release);
                }
                quit.load(Ordering::Acquire)
            };
            self.walk
                .next_with_cancel(self.descend, &mut before_directory, &mut cancelled)
        };
        let effects = effects.into_inner();
        let Some(item) = item else {
            return false;
        };
        self.descend = true;
        let entry = match item {
            Ok(entry) => entry,
            Err(error) => {
                effects.error(&error);
                self.errors += 1;
                return !self.control.quit;
            }
        };
        if entry.depth() < plan.options.min_depth {
            return true;
        }
        self.control.prune = false;
        if let Err(error) = self.control.actions.change_directory(entry.path(), effects) {
            effects.error(&WalkError {
                path: entry.path().to_owned(),
                error,
            });
            self.errors += 1;
            self.control.quit = true;
            return false;
        }
        let checkpoint = policy.checkpoint.then(|| self.record.checkpoint());
        let result = {
            let mut output = super::output::EntryEffects {
                host: effects,
                record: &mut self.record,
                gate: &self.gate,
                quit: &self.quit,
            };
            match evaluate(expression, entry, &mut output, &mut self.control) {
                Ok(_) if policy.immediate || output.record.full() => {
                    output.commit(false).map_err(EvaluationError::Output)
                }
                Ok(_) => Ok(()),
                Err(error) => checkpoint
                    .map_or(Ok(()), |checkpoint| output.record.rollback(checkpoint))
                    .and_then(|()| output.commit(false))
                    .map_err(EvaluationError::Output)
                    .and(Err(error)),
            }
        };
        if let Err(error) = result {
            let (error, stop) = match error {
                EvaluationError::Metadata(error) => (error, false),
                EvaluationError::Output(error) => (error, true),
            };
            effects.error(&WalkError {
                path: super::output::spill_path(&error)
                    .unwrap_or(entry.path())
                    .to_owned(),
                error,
            });
            self.errors += 1;
            self.descend = false;
            self.control.quit |= stop;
        } else {
            self.descend = !self.control.prune;
        }
        true
    }

    fn flush(&mut self, effects: &mut impl Effects) {
        let committed = super::output::EntryEffects {
            host: effects,
            record: &mut self.record,
            gate: &self.gate,
            quit: &self.quit,
        }
        .commit(false);
        if let Err(error) = committed
            .and_then(|()| self.control.actions.flush(effects, false))
            .and_then(|()| effects.flush())
        {
            effects.error(&WalkError {
                path: ".".into(),
                error,
            });
            self.errors += 1;
            self.quit.store(true, Ordering::Release);
        }
    }

    fn finish(&mut self, effects: &mut impl Effects) -> u64 {
        self.flush(effects);
        if let Some(completion) = self.completion.take() {
            completion.fetch_sub(1, Ordering::Release);
        }
        self.errors + self.control.actions.errors
    }
}

impl Task {
    fn donate(&mut self, quit: &Arc<AtomicBool>, sequential: bool) -> Option<Self> {
        let (walk, completion) = if !sequential && let Some(walk) = self.walk.split_start() {
            (walk, None)
        } else {
            let (walk, completion) = self.walk.split()?;
            (walk, Some(completion))
        };
        let mut task = Self::new(walk, completion, quit);
        task.gate = self.gate.clone();
        task.control.actions.gate = self.gate.clone();
        task.control.actions.shared = self.control.actions.shared.clone();
        Some(task)
    }
}

pub(super) fn run(
    plan: &Plan,
    source: &mut impl EntrySource,
    effects: &mut impl Effects,
) -> Result<Outcome, Unsupported> {
    let mut outcome = Outcome::default();
    let Some(expression) = plan.prepare(source, effects, &mut outcome)? else {
        return Ok(outcome);
    };
    let quit = Arc::new(AtomicBool::new(false));
    let mut task = Task::new(source, None, &quit);
    task.control.cancelled = None;
    let policy = super::output::policy(&expression);
    let shared = task.control.actions.shared.clone();
    let gate = task.gate.clone();
    while !quit.load(Ordering::Acquire) && task.step(plan, &expression, effects, policy) {
        if task.control.quit {
            break;
        }
    }
    outcome.errors += task.finish(effects)
        + super::action::flush_shared(&shared, effects, &gate, &quit).unwrap_or_else(|error| {
            effects.error(&WalkError {
                path: ".".into(),
                error,
            });
            1
        });
    Ok(outcome)
}

/// Walk work that [`Pool`] runs and donates between workers: find's
/// expression tasks, or grep's search tasks. `L` is a worker thread's own
/// state, lent to each task while that worker runs it.
pub trait Donor<L>: Send + Sized {
    /// Suspended until donated descendants complete; not runnable yet.
    fn waiting(&self) -> bool;
    /// Takes the worker's state before a run of steps.
    fn resume(&mut self, local: &mut L);
    /// Advances one entry; false once the task has ended or suspended.
    fn advance(&mut self, local: &mut L) -> bool;
    /// The task asks the whole pool to stop.
    fn quits(&self) -> bool;
    /// Splits sibling work off for an idle worker, having first published
    /// whatever must precede it. Called with the pool's queue locked.
    fn give(&mut self, local: &mut L, quit: &Arc<AtomicBool>) -> Option<Self>;
    /// The walk stopped only to wait for donated descendants.
    fn suspended(&self) -> bool;
    /// Hands the worker's state back before the task is queued again.
    fn park(&mut self, local: &mut L);
    /// Ends the task, handing the worker's state back; its error count.
    fn end(&mut self, local: &mut L) -> u64;
}

struct Queue<T> {
    tasks: Vec<T>,
    active: usize,
}

/// Bounded sibling donation among `workers` threads, each running
/// [`Pool::worker`]. Suspended parents return to the queue rather than
/// occupying a worker while descendants run.
pub struct Pool<T> {
    queue: Mutex<Queue<T>>,
    changed: Condvar,
    quit: Arc<AtomicBool>,
    workers: usize,
}

// A worker panic must wake siblings before the scoped join can propagate it.
// Taking the queue lock closes the check-then-condvar-wait lost-wakeup window.
struct PanicStop<'a, T>(&'a Pool<T>);
impl<T> Drop for PanicStop<'_, T> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let _queue = self
                .0
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.0.quit.store(true, Ordering::Release);
            self.0.changed.notify_all();
        }
    }
}

impl<T> Pool<T> {
    pub fn new(tasks: Vec<T>, quit: Arc<AtomicBool>, workers: usize) -> Self {
        Self {
            queue: Mutex::new(Queue { tasks, active: 0 }),
            changed: Condvar::new(),
            quit,
            workers,
        }
    }

    /// Tasks left queued when the workers returned: suspended ones, after a
    /// quit.
    pub fn into_tasks(self) -> Vec<T> {
        self.queue
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tasks
    }

    /// Runs queued tasks until none is runnable and none is active, or the
    /// pool quits; the errors the finished tasks counted.
    pub fn worker<L>(&self, mut local: L) -> u64
    where
        T: Donor<L>,
    {
        let _panic = PanicStop(self);
        let mut errors = 0;
        loop {
            let mut queue = self
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut task = loop {
                if self.quit.load(Ordering::Acquire) {
                    return errors;
                }
                if let Some(index) = queue.tasks.iter().rposition(|task| !task.waiting()) {
                    queue.active += 1;
                    break queue.tasks.swap_remove(index);
                }
                if queue.active == 0 {
                    return errors;
                }
                queue = self
                    .changed
                    .wait(queue)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            };
            drop(queue);
            task.resume(&mut local);
            let mut steps = 0usize;
            while !self.quit.load(Ordering::Acquire) && task.advance(&mut local) {
                if task.quits() {
                    self.quit.store(true, Ordering::Release);
                    break;
                }
                if steps.is_multiple_of(64) {
                    let mut queue = self
                        .queue
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if queue.tasks.len() + queue.active < self.workers * 2
                        && let Some(donated) = task.give(&mut local, &self.quit)
                    {
                        queue.tasks.push(donated);
                        self.changed.notify_all();
                    }
                }
                steps += 1;
            }
            if task.quits() {
                self.quit.store(true, Ordering::Release);
            }
            // Completion can race with the None returned for suspension. Use
            // the recorded reason, not a second counter read, to
            // retain the task.
            let waiting = task.suspended() && !self.quit.load(Ordering::Acquire);
            let suspended = if waiting {
                task.park(&mut local);
                Some(task)
            } else {
                errors += task.end(&mut local);
                None
            };
            let mut queue = self
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queue.active -= 1;
            if let Some(task) = suspended {
                queue.tasks.push(task);
            }
            self.changed.notify_all();
        }
    }
}

/// A find worker's state: its effects handle, and a record whose allocation
/// stays with the worker when a task completes or suspends. Queued tasks have
/// already flushed their transactions.
struct FindLocal<'p, E> {
    plan: &'p Plan,
    expression: &'p Expression,
    effects: E,
    record: super::output::Record,
    policy: super::output::Policy,
    sequential: bool,
}

impl<'p, E: Effects> FindLocal<'p, E> {
    fn new(plan: &'p Plan, expression: &'p Expression, effects: E) -> Self {
        Self {
            plan,
            expression,
            effects,
            record: super::output::Record::default(),
            policy: super::output::policy(expression),
            sequential: super::sequential_starts(expression),
        }
    }
}

impl<E: Effects> Donor<FindLocal<'_, E>> for Task {
    fn waiting(&self) -> bool {
        self.walk.waiting()
    }
    fn resume(&mut self, local: &mut FindLocal<'_, E>) {
        std::mem::swap(&mut local.record, &mut self.record);
    }
    fn advance(&mut self, local: &mut FindLocal<'_, E>) -> bool {
        self.step(local.plan, local.expression, &mut local.effects, local.policy)
    }
    fn quits(&self) -> bool {
        self.control.quit
    }
    fn give(&mut self, local: &mut FindLocal<'_, E>, quit: &Arc<AtomicBool>) -> Option<Self> {
        let donated = self.donate(quit, local.sequential)?;
        // Publish earlier records before children can evaluate.
        if let Err(error) = (super::output::EntryEffects {
            host: &mut local.effects,
            record: &mut self.record,
            gate: &self.gate,
            quit: &self.quit,
        })
        .commit(false)
        .and_then(|()| self.control.actions.flush_files())
        .and_then(|()| local.effects.flush())
        {
            local.effects.error(&WalkError {
                path: ".".into(),
                error,
            });
            self.errors += 1;
            quit.store(true, Ordering::Release);
        }
        Some(donated)
    }
    fn suspended(&self) -> bool {
        self.walk.suspended()
    }
    fn park(&mut self, local: &mut FindLocal<'_, E>) {
        self.flush(&mut local.effects);
        std::mem::swap(&mut local.record, &mut self.record);
    }
    fn end(&mut self, local: &mut FindLocal<'_, E>) -> u64 {
        let errors = self.finish(&mut local.effects);
        std::mem::swap(&mut local.record, &mut self.record);
        errors
    }
}

impl Plan {
    /// Executes a live or catalog DFS on at most `workers` threads. Cloned
    /// effects must synchronize records and prompts, and capture child stdout
    /// without holding the output lock while the child runs. Threads start only
    /// after traversal discovers independent sibling work; start operands
    /// may overlap unless the expression has effects on the tree or files.
    pub fn run_parallel<E: Effects + Clone + Send>(
        &self,
        source: LiveWalk,
        mut effects: E,
        workers: usize,
    ) -> Result<Outcome, Unsupported> {
        let workers = if self.options.max_depth == Some(0) || self.is_information() {
            1
        } else {
            source.worker_limit(workers)
        };
        let mut outcome = Outcome::default();
        let Some(expression) = self.prepare(&source, &mut effects, &mut outcome)? else {
            if let Err(error) = effects.flush() {
                effects.error(&WalkError {
                    path: ".".into(),
                    error,
                });
                outcome.errors += 1;
            }
            return Ok(outcome);
        };
        let shallow_catalog =
            source.catalog().is_some() && self.options.max_depth.is_some_and(|depth| depth <= 2);
        let quit = Arc::new(AtomicBool::new(false));
        let mut task = Task::new(source, None, &quit);
        task.control.cancelled = None;
        let policy = super::output::policy(&expression);
        let sequential = super::sequential_starts(&expression);
        let shared = task.control.actions.shared.clone();
        let gate = task.gate.clone();
        while !quit.load(Ordering::Acquire) && task.step(self, &expression, &mut effects, policy) {
            if task.control.quit {
                break;
            }
            if workers > 1
                && (!shallow_catalog
                    || task.walk.in_live_directory()
                    || !sequential && task.walk.has_starts())
                && let Some(donated) = task.donate(&quit, sequential)
            {
                task.control.cancelled = Some(quit.clone());
                if let Err(error) = (super::output::EntryEffects {
                    host: &mut effects,
                    record: &mut task.record,
                    gate: &task.gate,
                    quit: &task.quit,
                })
                .commit(false)
                .and_then(|()| task.control.actions.flush_files())
                .and_then(|()| effects.flush())
                {
                    effects.error(&WalkError {
                        path: ".".into(),
                        error,
                    });
                    task.errors += 1;
                    quit.store(true, Ordering::Release);
                }
                let pool = Pool::new(vec![task, donated], quit, workers);
                let errors = std::thread::scope(|scope| {
                    let handles: Vec<_> = (1..workers)
                        .map(|_| {
                            let pool = &pool;
                            let expression = &expression;
                            let effects = effects.clone();
                            scope.spawn(move || {
                                pool.worker(FindLocal::new(self, expression, effects))
                            })
                        })
                        .collect();
                    let mut errors = pool.worker(FindLocal::new(self, &expression, effects.clone()));
                    for handle in handles {
                        errors += handle
                            .join()
                            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                    }
                    errors
                });
                let quit = pool.quit.clone();
                let errors = errors
                    + pool
                        .into_tasks()
                        .into_iter()
                        .map(|mut task| task.finish(&mut effects))
                        .sum::<u64>();
                let errors = errors
                    + super::action::flush_shared(&shared, &mut effects, &gate, &quit)
                        .unwrap_or_else(|error| {
                            effects.error(&WalkError {
                                path: ".".into(),
                                error,
                            });
                            1
                        });
                return Ok(Outcome { errors });
            }
        }
        let errors = task.finish(&mut effects)
            + super::action::flush_shared(&shared, &mut effects, &gate, &quit).unwrap_or_else(
                |error| {
                    effects.error(&WalkError {
                        path: ".".into(),
                        error,
                    });
                    1
                },
            );
        Ok(Outcome { errors })
    }
}

#[cfg(test)]
mod tests;
