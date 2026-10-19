//! Find's expression over the [`schedule`](super::schedule) walk. Tasks own
//! expression control and directory-local batches; ordinary batches are shared
//! across the run. Effects are worker-local handles to the host's synchronized
//! output and prompt sinks.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::schedule::{self, Schedule, Split, Step, Visitor};

use super::{
    Control, Effects, EntrySource, EvaluationError, Expression, LiveWalk, Outcome, Plan,
    Unsupported, WalkError, evaluate,
};

struct Task {
    control: Control,
    descend: bool,
    errors: u64,
    record: super::output::Record,
    gate: Arc<Mutex<()>>,
    quit: Arc<AtomicBool>,
}

impl Task {
    fn new(quit: &Arc<AtomicBool>) -> Self {
        let gate = Arc::new(Mutex::new(()));
        let mut control = Control {
            cancelled: Some(quit.clone()),
            ..Control::default()
        };
        control.actions.gate = gate.clone();
        control.actions.quit = quit.clone();
        Self {
            control,
            descend: true,
            errors: 0,
            record: super::output::Record::default(),
            gate,
            quit: quit.clone(),
        }
    }

    fn step(
        &mut self,
        walk: &mut impl EntrySource,
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
            walk.next_with_cancel(self.descend, &mut before_directory, &mut cancelled)
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
        self.errors + self.control.actions.errors
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
    let mut task = Task::new(&quit);
    task.control.cancelled = None;
    let policy = super::output::policy(&expression);
    let shared = task.control.actions.shared.clone();
    let gate = task.gate.clone();
    while !quit.load(Ordering::Acquire) && task.step(source, plan, &expression, effects, policy) {
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

impl Split for LiveWalk {
    // Read-only starts may overlap. Effectful starts stay in the lender and
    // advance only after all lent descendants have completed.
    fn split(&mut self, starts: bool) -> Option<(Self, Option<Arc<std::sync::atomic::AtomicUsize>>)> {
        if starts && let Some(walk) = self.split_start() {
            return Some((walk, None));
        }
        let (walk, completion) = LiveWalk::split(self)?;
        Some((walk, Some(completion)))
    }
    fn waiting(&self) -> bool {
        LiveWalk::waiting(self)
    }
    fn suspended(&self) -> bool {
        LiveWalk::suspended(self)
    }
}

/// A find worker: its effects handle, and a record whose allocation stays
/// with the worker when a task completes or suspends. Queued tasks have
/// already flushed their transactions.
struct Worker<'p, E> {
    plan: &'p Plan,
    expression: &'p Expression,
    effects: E,
    record: super::output::Record,
    policy: super::output::Policy,
    quit: &'p Arc<AtomicBool>,
}

impl<'p, E: Effects> Worker<'p, E> {
    fn new(plan: &'p Plan, expression: &'p Expression, effects: E, quit: &'p Arc<AtomicBool>) -> Self {
        Self {
            plan,
            expression,
            effects,
            record: super::output::Record::default(),
            policy: super::output::policy(expression),
            quit,
        }
    }
}

impl<E: Effects> Visitor<LiveWalk, Task> for Worker<'_, E> {
    fn step(&mut self, walk: &mut LiveWalk, task: &mut Task) -> Step {
        let more = task.step(walk, self.plan, self.expression, &mut self.effects, self.policy);
        if task.control.quit {
            Step::Quit
        } else if more {
            Step::Next
        } else {
            Step::Done
        }
    }
    fn lend(&mut self, parent: &mut Task) -> Task {
        parent.control.cancelled = Some(self.quit.clone());
        // Publish earlier records before children can evaluate.
        if let Err(error) = (super::output::EntryEffects {
            host: &mut self.effects,
            record: &mut parent.record,
            gate: &parent.gate,
            quit: &parent.quit,
        })
        .commit(false)
        .and_then(|()| parent.control.actions.flush_files())
        .and_then(|()| self.effects.flush())
        {
            self.effects.error(&WalkError {
                path: ".".into(),
                error,
            });
            parent.errors += 1;
            self.quit.store(true, Ordering::Release);
        }
        let mut task = Task::new(self.quit);
        task.gate = parent.gate.clone();
        task.control.actions.gate = parent.gate.clone();
        task.control.actions.shared = parent.control.actions.shared.clone();
        task
    }
    fn resume(&mut self, task: &mut Task) {
        std::mem::swap(&mut self.record, &mut task.record);
    }
    fn park(&mut self, task: &mut Task) {
        task.flush(&mut self.effects);
        std::mem::swap(&mut self.record, &mut task.record);
    }
    fn end(&mut self, task: &mut Task) -> u64 {
        let errors = task.finish(&mut self.effects);
        std::mem::swap(&mut self.record, &mut task.record);
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
        let mut task = Task::new(&quit);
        task.control.cancelled = None;
        let sequential = super::sequential_starts(&expression);
        let shared = task.control.actions.shared.clone();
        let gate = task.gate.clone();
        let schedule = Schedule {
            workers,
            starts: !sequential,
            quit: &quit,
        };
        let errors = schedule::run(
            &schedule,
            source,
            task,
            &mut Worker::new(self, &expression, effects.clone(), &quit),
            |walk| {
                !shallow_catalog || walk.in_live_directory() || !sequential && walk.has_starts()
            },
            |_| Worker::new(self, &expression, effects.clone(), &quit),
        );
        let errors = errors
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
