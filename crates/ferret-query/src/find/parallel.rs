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
        buffered: bool,
    ) -> bool {
        let Some(item) = self.walk.next_with(self.descend, &mut || {
            self.control.actions.flush(effects, true).inspect_err(|_| {
                self.control.quit = true;
            })
        }) else {
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
        let result = if buffered {
            let mut output = super::output::EntryEffects {
                host: effects,
                record: &mut self.record,
                gate: &self.gate,
                quit: &self.quit,
            };
            let result = evaluate(expression, entry, &mut output, &mut self.control);
            let committed = output.commit(false);
            committed
                .map_err(EvaluationError::Output)
                .and(result.map(|_| ()))
        } else {
            evaluate(expression, entry, effects, &mut self.control).map(|_| ())
        };
        if let Err(error) = result {
            let (error, stop) = match error {
                EvaluationError::Metadata(error) => (error, false),
                EvaluationError::Output(error) => (error, true),
            };
            effects.error(&WalkError {
                path: entry.path().to_owned(),
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
        if let Err(error) = self
            .control
            .actions
            .flush(effects, false)
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

    fn finish(mut self, effects: &mut impl Effects) -> u64 {
        self.flush(effects);
        if let Some(completion) = self.completion {
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
    let buffered = super::output::needs_record(&expression);
    let shared = task.control.actions.shared.clone();
    let gate = task.gate.clone();
    while !quit.load(Ordering::Acquire) && task.step(plan, &expression, effects, buffered) {
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

struct Queue {
    tasks: Vec<Task>,
    active: usize,
}

struct Pool {
    queue: Mutex<Queue>,
    changed: Condvar,
    quit: Arc<AtomicBool>,
    workers: usize,
}

impl Pool {
    fn worker(&self, plan: &Plan, expression: &Expression, mut effects: impl Effects) -> u64 {
        let mut errors = 0;
        let buffered = super::output::needs_record(expression);
        let sequential = super::sequential_starts(expression);
        loop {
            let mut queue = self
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut task = loop {
                if self.quit.load(Ordering::Acquire) {
                    return errors;
                }
                if let Some(index) = queue.tasks.iter().rposition(|task| !task.walk.waiting()) {
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
            let mut steps = 0usize;
            while !self.quit.load(Ordering::Acquire)
                && task.step(plan, expression, &mut effects, buffered)
            {
                if task.control.quit {
                    self.quit.store(true, Ordering::Release);
                    break;
                }
                if steps.is_multiple_of(64) {
                    let mut queue = self
                        .queue
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if queue.tasks.len() + queue.active < self.workers * 2
                        && let Some(donated) = task.donate(&self.quit, sequential)
                    {
                        // Publish earlier records before children can
                        // evaluate.
                        if let Err(error) = task
                            .control
                            .actions
                            .flush_files()
                            .and_then(|()| effects.flush())
                        {
                            effects.error(&WalkError {
                                path: ".".into(),
                                error,
                            });
                            task.errors += 1;
                            self.quit.store(true, Ordering::Release);
                        }
                        queue.tasks.push(donated);
                        self.changed.notify_all();
                    }
                }
                steps += 1;
            }
            if task.control.quit {
                self.quit.store(true, Ordering::Release);
            }
            // Completion can race with the None returned for suspension. Use
            // the recorded reason, not a second counter read, to
            // retain the task.
            let waiting = task.walk.suspended() && !self.quit.load(Ordering::Acquire);
            let suspended = if waiting {
                task.flush(&mut effects);
                Some(task)
            } else {
                errors += task.finish(&mut effects);
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
        let buffered = super::output::needs_record(&expression);
        let sequential = super::sequential_starts(&expression);
        let shared = task.control.actions.shared.clone();
        let gate = task.gate.clone();
        while !quit.load(Ordering::Acquire) && task.step(self, &expression, &mut effects, buffered)
        {
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
                if let Err(error) = task
                    .control
                    .actions
                    .flush_files()
                    .and_then(|()| effects.flush())
                {
                    effects.error(&WalkError {
                        path: ".".into(),
                        error,
                    });
                    task.errors += 1;
                    quit.store(true, Ordering::Release);
                }
                let pool = Pool {
                    queue: Mutex::new(Queue {
                        tasks: vec![task, donated],
                        active: 0,
                    }),
                    changed: Condvar::new(),
                    quit,
                    workers,
                };
                let errors = std::thread::scope(|scope| {
                    let handles: Vec<_> = (1..workers)
                        .map(|_| {
                            let pool = &pool;
                            let expression = &expression;
                            let effects = effects.clone();
                            scope.spawn(move || pool.worker(self, expression, effects))
                        })
                        .collect();
                    let mut errors = pool.worker(self, &expression, effects.clone());
                    for handle in handles {
                        errors += handle
                            .join()
                            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                    }
                    errors
                });
                let queue = pool
                    .queue
                    .into_inner()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let errors = errors
                    + queue
                        .tasks
                        .into_iter()
                        .map(|task| task.finish(&mut effects))
                        .sum::<u64>();
                let errors = errors
                    + super::action::flush_shared(&shared, &mut effects, &gate, &pool.quit)
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
