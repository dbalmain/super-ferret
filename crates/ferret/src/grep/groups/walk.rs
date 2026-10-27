//! The parallel live walk (D105): workers lend subtrees and search the files
//! they reach.

use super::job::{Job, Observed, Scratch};
use super::publish::{Handoff, Search, weight};
use super::source::{Filter, Source};
use super::*;
use ferret_query::find::LiveWalk;
use ferret_query::find::schedule::{self, Schedule, Step, Visitor};
use ferret_query::{Lane, Shortfall};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Searches `source`'s operands with the walk itself spread over `threads`
/// workers (D105): find's walk schedule lends subtrees of each live
/// directory operand between workers, each searching the files it reaches
/// and handing groups to this thread in completion order, under the same
/// handoff and input bounds as [`run`]. File operands are searched in order
/// by the first worker. Sets `searched` if any walk selected a file (D99).
/// If the walk's own thread cannot start, returns [`Walk::Refused`] having
/// taken no operand from `source`, for the caller to search them itself.
pub(in super::super) fn walk<W: Write>(
    host: &mut Host<'_, W>,
    source: &mut Source<'_>,
    threads: usize,
    cancelled: &AtomicBool,
    searched: &mut bool,
) -> io::Result<Walk> {
    let search = Search::new(host, cancelled, threads);
    let found = AtomicBool::new(false);
    let mut current = None;
    // Borrowed, not moved, into the walk's thread: a refused thread drops
    // its closure unrun and leaves every operand here.
    let inputs = &mut source.inputs;
    let filter = &source.filter;
    let live = source.live;
    let refused = Mutex::new(None);
    let on_refused = |shortfall: Shortfall| {
        if buffered_path_output(search.plan, search.tty) {
            *lock(&refused) = Some(shortfall);
        } else {
            // Keep live-policy warning order unchanged; only buffered path
            // output defers this publisher-owned diagnostic.
            let _publication = lock(&search.publication);
            let _ = report_shortfall(search.runtime, &shortfall);
        }
    };
    let started = ferret_query::lane_groups(
        threads,
        cancelled,
        |lanes, stopped| {
            let walker = |i: usize| Walker {
                search: &search,
                filter,
                lane: &lanes[i],
                stopped,
                found: &found,
                live: live.is_some(),
                scratch: Scratch::new(search.plan.clone()),
            };
            let mut first = walker(0);
            // A walker's panic stops the lanes too: a sibling may be waiting
            // for input bytes, or reading until the run stops.
            let stop = || lanes[0].stop();
            for input in inputs {
                if stopped() {
                    break;
                }
                let go = match input {
                    Input::File(path, size) => {
                        first.search(size, Job::Direct(path.clone(), filter.cwd.join(path)))
                    }
                    Input::Failed(path, error) => {
                        first.search(0, Job::Failed(path, error.to_string()))
                    }
                    Input::Directory(path) => {
                        let policy = {
                            // The root's rules may warn, as in the serial
                            // source.
                            let _publication = live.is_some().then(|| lock(&search.publication));
                            live.map(|anchors| anchors.start(&path, stopped))
                                .transpose()
                        };
                        match policy {
                            Ok(policy) => {
                                let quit = AtomicBool::new(false);
                                let schedule = Schedule {
                                    workers: threads,
                                    starts: false,
                                    quit: &quit,
                                    on_panic: Some(&stop),
                                    on_refused: Some(&on_refused),
                                };
                                schedule::run(
                                    &schedule,
                                    sources::live(&path, &filter.cwd, policy.is_some())
                                        .following_links(filter.traversal.follow)
                                        .retaining_parents(),
                                    Walked {
                                        descend: true,
                                        policy: policy.map(live::Policy::deferring),
                                        selected: None,
                                    },
                                    &mut first,
                                    |_| true,
                                    walker,
                                );
                                !quit.load(Ordering::Acquire)
                            }
                            Err(error) => first.search(0, Job::Failed(path, error.to_string())),
                        }
                    }
                    // The parent splits operands at stdin barriers.
                    Input::Stream(_) => false,
                };
                if !go {
                    break;
                }
            }
            for lane in &lanes {
                lane.flush(stopped());
            }
        },
        |group| search.publish(host, &mut current, group),
    );
    if let Some(shortfall) = lock(&refused).take() {
        host.writer.flush()?;
        host.report_shortfall(&shortfall)?;
    }
    if let Err(cause) = started {
        return Ok(Walk::Refused(cause));
    }
    *searched |= found.load(Ordering::Acquire);
    search.finish(host).map(|()| Walk::Done)
}

/// How [`walk`] ended.
pub(in super::super) enum Walk {
    /// The operands were searched.
    Done,
    /// The system refused the walk's thread; nothing was searched.
    Refused(io::Error),
}

/// A walk task's own state: whether to enter the directory it last
/// yielded, its copy of the live policy stack (D13), cloned when the task
/// lends part of its walk, and the file its last step selected. That file is
/// searched by the next step, so the scheduler can lend the task's remaining
/// siblings before a search that may take seconds.
struct Walked {
    descend: bool,
    policy: Option<live::Policy>,
    selected: Option<(u64, Job)>,
}

/// One worker of [`walk`]: filters each entry its task reaches and searches
/// the selected files.
struct Walker<'w, 's> {
    search: &'w Search<'s>,
    filter: &'w Filter<'w>,
    lane: &'w Lane<'w, Handoff>,
    stopped: &'w (dyn Fn() -> bool + Sync),
    found: &'w AtomicBool,
    live: bool,
    scratch: Scratch,
}

impl Walker<'_, '_> {
    /// Searches one job and hands its group over; false once the run stops.
    fn search(&mut self, size: u64, job: Job) -> bool {
        if matches!(job, Job::Failed(..)) {
            return self
                .lane
                .hand(Handoff::Complete(job, Observed::Skipped), 0)
                .is_continue();
        }
        let Some(reservation) = self.lane.reserve(size, self.stopped) else {
            return false;
        };
        let handoff = |group| self.lane.handoff(group, self.stopped);
        let group = self
            .search
            .work(job, self.stopped, &handoff, true, |job, target| {
                self.scratch.prepare(
                    None,
                    job,
                    self.search.filename,
                    &self.search.cache,
                    false,
                    (self.stopped, target, Some(&self.search.capture)),
                )
            });
        drop(reservation);
        if (self.stopped)() {
            return false;
        }
        let bytes = weight(&group);
        self.lane.hand(group, bytes).is_continue()
    }
}

impl Visitor<LiveWalk, Walked> for Walker<'_, '_> {
    fn step(&mut self, walk: &mut LiveWalk, task: &mut Walked) -> Step {
        let stopped = self.stopped;
        if stopped() {
            return Step::Quit;
        }
        if let Some((size, job)) = task.selected.take() {
            return if self.search(size, job) {
                Step::Next
            } else {
                Step::Quit
            };
        }
        let Some(_descriptors) = self.search.descriptors.acquire(stopped) else {
            return Step::Quit;
        };
        let Some(entry) = walk.next_with_cancel(task.descend, &mut || Ok(()), &mut || stopped())
        else {
            return if stopped() { Step::Quit } else { Step::Done };
        };
        task.descend = true;
        let job = match entry {
            Ok(entry) => {
                let mut selected = false;
                let job = self.filter.entry(
                    entry,
                    &mut task.descend,
                    task.policy.as_mut(),
                    &mut selected,
                    self.live.then_some(&self.search.publication),
                    stopped,
                );
                if selected && !self.found.load(Ordering::Relaxed) {
                    self.found.store(true, Ordering::Relaxed);
                }
                job
            }
            Err(error) => Some((0, Job::Failed(error.path, error.error.to_string()))),
        };
        task.selected = job;
        Step::Next
    }

    fn holding(&self, task: &Walked) -> bool {
        task.selected.is_some()
    }

    // The lent levels are below a directory on the lender's stack, which the
    // borrower's first entry unwinds to.
    fn lend(&mut self, parent: &mut Walked) -> Walked {
        Walked {
            descend: true,
            policy: parent.policy.clone(),
            selected: None,
        }
    }

    // Errors are published as groups.
    fn end(&mut self, _task: &mut Walked) -> u64 {
        0
    }
}
