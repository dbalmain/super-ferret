//! Bounded sibling donation over the ordinary DFS engine. Suspended parents
//! return to the queue rather than occupying a worker while descendants run.
//! Each task owns its expression control and pending batches; effects are
//! worker-local handles to the host's synchronized output and prompt sinks.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use super::{
    Control, Effects, EntrySource, EvaluationError, Expression, LiveWalk, Outcome, Plan,
    Unsupported, WalkError, evaluate, resolve_references,
};

struct Task {
    walk: LiveWalk,
    control: Control,
    descend: bool,
    completion: Option<Arc<AtomicUsize>>,
    errors: u64,
}

impl Task {
    fn new(walk: LiveWalk, completion: Option<Arc<AtomicUsize>>, quit: &Arc<AtomicBool>) -> Self {
        Self {
            walk,
            control: Control {
                cancelled: Some(quit.clone()),
                ..Control::default()
            },
            descend: true,
            completion,
            errors: 0,
        }
    }

    fn step(&mut self, plan: &Plan, expression: &Expression, effects: &mut impl Effects) -> bool {
        let Some(item) = self.walk.next_with(self.descend, &mut || {
            self.control.actions.flush(effects, true)
        }) else {
            return false;
        };
        self.descend = true;
        let entry = match item {
            Ok(entry) => entry,
            Err(error) => {
                effects.error(&error);
                self.errors += 1;
                return true;
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
        if let Err(error) = evaluate(expression, entry, effects, &mut self.control) {
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

    fn donate(&mut self, quit: &Arc<AtomicBool>) -> Option<Self> {
        if let Some(walk) = self.walk.split_start() {
            return Some(Self::new(walk, None, quit));
        }
        let (walk, completion) = self.walk.split()?;
        Some(Self::new(walk, Some(completion), quit))
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
            while !self.quit.load(Ordering::Acquire) && task.step(plan, expression, &mut effects) {
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
                        && let Some(donated) = task.donate(&self.quit)
                    {
                        // Publish earlier records before children can
                        // evaluate.
                        if let Err(error) = effects.flush() {
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
    /// after traversal discovers independent sibling or start-operand work.
    pub fn run_parallel<E: Effects + Clone + Send>(
        &self,
        mut source: LiveWalk,
        mut effects: E,
        workers: usize,
    ) -> Result<Outcome, Unsupported> {
        let workers = source.worker_limit(workers);
        if workers <= 1 || self.options.max_depth == Some(0) || self.is_information() {
            let mut outcome = self.run(&mut source, &mut effects)?;
            if let Err(error) = effects.flush() {
                effects.error(&WalkError {
                    path: ".".into(),
                    error,
                });
                outcome.errors += 1;
            }
            return Ok(outcome);
        }
        if let Some(feature) = &self.unsupported {
            return Err(Unsupported {
                feature: feature.clone(),
            });
        }
        for warning in &self.warnings {
            effects.warning(warning);
        }
        let mut expression = self.expression.clone();
        if let Err(error) = resolve_references(&mut expression, source.catalog()) {
            effects.error(&WalkError {
                path: ".".into(),
                error,
            });
            return Ok(Outcome { errors: 1 });
        }
        let quit = Arc::new(AtomicBool::new(false));
        let mut task = Task::new(source, None, &quit);
        while !quit.load(Ordering::Acquire) && task.step(self, &expression, &mut effects) {
            if task.control.quit {
                break;
            }
            if let Some(donated) = task.donate(&quit) {
                if let Err(error) = effects.flush() {
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
                return Ok(Outcome { errors });
            }
        }
        Ok(Outcome {
            errors: task.finish(&mut effects),
        })
    }
}

#[cfg(test)]
mod tests;
