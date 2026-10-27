//! A names-only listing of one live-walked directory on several workers
//! (D130). Workers send what they select, in chunks, to the calling thread,
//! which alone writes; names therefore arrive in no fixed order, as rg's do,
//! but each diagnostic and policy warning still follows the names its worker
//! listed before it.

use super::*;
use ferret_query::find::LiveWalk;
use ferret_query::find::schedule::{self, Schedule, Step, Visitor};
use std::sync::mpsc::{self, SyncSender};

/// Bytes of names a worker gathers before sending them.
const CHUNK: usize = 16 << 10;

/// One directory's walk, before it starts.
pub(super) struct Walk<'a> {
    pub plan: &'a Names,
    pub visibility: &'a Visibility<'a>,
    pub threads: usize,
    pub flush_each: bool,
    pub cancelled: &'a AtomicBool,
}

/// How a listing ended.
pub(super) enum Listed {
    /// The walk ran; whether it listed a name and whether any entry failed.
    Done { any: bool, errors: bool },
    /// Output closed (D94): the listing is over, quietly.
    Closed,
    /// The system refused the walk's thread; nothing ran, and the source
    /// comes back for a serial walk.
    Refused(Box<(LiveWalk, Option<live::Policy>)>),
}

enum Found {
    Names(Vec<u8>),
    Diagnostic(String),
    Warnings(Vec<String>),
}

impl Walk<'_> {
    pub(super) fn run(
        &self,
        source: LiveWalk,
        policy: Option<live::Policy>,
        writer: &mut impl Write,
        runtime: &Runtime<'_>,
    ) -> Result<Listed, String> {
        let (sender, receiver) = mpsc::sync_channel(self.threads * 4);
        let mut start = Some((source, policy));
        let quit = AtomicBool::new(false);
        let mut any = false;
        let mut errors = false;
        let mut failed = None;
        let closed = thread::scope(|scope| -> Result<bool, String> {
            let start = &mut start;
            let quit = &quit;
            let walked = thread::Builder::new().spawn_scoped(scope, move || {
                let Some((source, policy)) = start.take() else {
                    return;
                };
                let policy = policy.map(live::Policy::deferring);
                let schedule = Schedule {
                    workers: self.threads,
                    starts: false,
                    quit,
                    on_panic: None,
                    on_refused: None,
                };
                let lister = |_| Lister {
                    walk: self,
                    quit,
                    sender: sender.clone(),
                    buffer: Vec::new(),
                };
                let mut first = lister(0);
                schedule::run(
                    &schedule,
                    source,
                    Task {
                        descend: true,
                        policy,
                    },
                    &mut first,
                    |_| true,
                    lister,
                );
            });
            if walked.is_err() {
                return Ok(false);
            }
            let mut closed = false;
            for found in receiver {
                if closed || failed.is_some() {
                    continue;
                }
                match found {
                    Found::Names(bytes) => {
                        any = true;
                        let written = writer.write_all(&bytes).and_then(|()| {
                            if self.flush_each {
                                writer.flush()
                            } else {
                                Ok(())
                            }
                        });
                        match written {
                            Ok(()) => {}
                            // D94: a closed pipe stops the listing quietly.
                            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                                closed = true;
                            }
                            Err(error) => failed = Some(format!("writing stdout: {error}")),
                        }
                    }
                    Found::Diagnostic(message) => {
                        errors = true;
                        closed = diagnostic(writer, runtime, &message)?;
                    }
                    Found::Warnings(warnings) => {
                        closed = flush_output(writer, self.cancelled)?.is_some();
                        if !closed {
                            for warning in warnings {
                                eprintln!("{warning}");
                            }
                        }
                    }
                }
                if closed || failed.is_some() {
                    self.cancelled.store(true, Ordering::Release);
                }
            }
            Ok(closed)
        })?;
        if let Some((source, policy)) = start {
            return Ok(Listed::Refused(Box::new((source, policy))));
        }
        if let Some(error) = failed {
            return Err(error);
        }
        Ok(if closed {
            Listed::Closed
        } else {
            Listed::Done { any, errors }
        })
    }

    fn stopped(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// A walk task's own state: whether to enter the directory it last yielded,
/// and its copy of the live policy stack (D13), cloned when it lends.
struct Task {
    descend: bool,
    policy: Option<live::Policy>,
}

/// One worker: selects the entries its tasks reach and gathers their names.
struct Lister<'w> {
    walk: &'w Walk<'w>,
    quit: &'w AtomicBool,
    sender: SyncSender<Found>,
    buffer: Vec<u8>,
}

impl Lister<'_> {
    fn stopped(&self) -> bool {
        self.walk.stopped() || self.quit.load(Ordering::Acquire)
    }

    /// Sends the names gathered so far, then `found`; false once the
    /// writer has gone.
    fn send(&mut self, found: Option<Found>) -> bool {
        if !self.buffer.is_empty() {
            let names = Found::Names(std::mem::take(&mut self.buffer));
            if self.sender.send(names).is_err() {
                return false;
            }
        }
        found.is_none_or(|found| self.sender.send(found).is_ok())
    }

    fn name(&mut self, path: &Path) {
        let plan = self.walk.plan;
        // Writing to a Vec cannot fail.
        let _ = presentation::path(
            &plan.presentation,
            &mut self.buffer,
            label(path, plan.paths.is_empty()),
        );
        self.buffer
            .push(if plan.presentation.null { b'\0' } else { b'\n' });
    }
}

impl Visitor<LiveWalk, Task> for Lister<'_> {
    fn step(&mut self, walk: &mut LiveWalk, task: &mut Task) -> Step {
        if self.stopped() {
            return Step::Quit;
        }
        let Some(entry) =
            walk.next_with_cancel(task.descend, &mut || Ok(()), &mut || self.stopped())
        else {
            return if self.stopped() {
                Step::Quit
            } else {
                Step::Done
            };
        };
        task.descend = false;
        let mut found = None;
        match entry {
            Ok(entry) => {
                let stopped = || self.stopped();
                let visit = self
                    .walk
                    .visibility
                    .select(entry, task.policy.as_mut(), &stopped);
                let warnings = task
                    .policy
                    .as_mut()
                    .map(live::Policy::take_warnings)
                    .unwrap_or_default();
                if !warnings.is_empty() && !self.send(Some(Found::Warnings(warnings))) {
                    return Step::Quit;
                }
                match visit {
                    Ok(Visit::Descend) => task.descend = true,
                    Ok(Visit::File) => self.name(entry.path()),
                    Ok(Visit::Skip) => {}
                    Err(error) => {
                        found = Some(format!("grep: {}: {error}", entry.path().display()));
                    }
                }
            }
            Err(error) => {
                found = Some(format!("grep: {}: {}", error.path.display(), error.error));
            }
        }
        let full = self.buffer.len() >= CHUNK || self.walk.flush_each;
        if (found.is_some() || full) && !self.send(found.map(Found::Diagnostic)) {
            return Step::Quit;
        }
        Step::Next
    }

    // The lent levels are below a directory on the lender's stack, which the
    // borrower's first entry unwinds to.
    fn lend(&mut self, parent: &mut Task) -> Task {
        Task {
            descend: true,
            policy: parent.policy.clone(),
        }
    }

    fn end(&mut self, _task: &mut Task) -> u64 {
        0
    }
}

impl Drop for Lister<'_> {
    fn drop(&mut self) {
        if !self.stopped() {
            self.send(None);
        }
    }
}
