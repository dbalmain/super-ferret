//! The command lane: explicit root and policy commands run one at a time on
//! their own thread, which keeps the process's starting nice, with the
//! writer waiting on each as a serial barrier.

use ferret_crawl::{Refresh, RootChange};

use super::*;

struct CommandJob {
    request: Request,
    options: IndexOptions,
    done: mpsc::SyncSender<std::thread::Result<CommandReply>>,
}
/// Drops the job sender before joining, so teardown cannot detach the thread
/// or wait on a recv that still has a live sender.
pub(super) struct CommandLane {
    jobs: Option<mpsc::Sender<CommandJob>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for CommandLane {
    fn drop(&mut self) {
        self.jobs.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl CommandLane {
    pub(super) fn spawn(
        host: &Arc<Host>,
        engine: &Arc<Engine>,
        scheduler: &Arc<crate::scheduler::Scheduler>,
        watch: &Option<Arc<Watch>>,
    ) -> io::Result<Self> {
        let (job_send, job_recv) = mpsc::channel();
        let command_host = host.clone();
        let command_engine = engine.clone();
        let command_scheduler = scheduler.clone();
        let command_watch = watch.clone();
        let thread = std::thread::Builder::new()
            .name("ferret-command".into())
            .spawn(move || {
                command_loop(
                    command_host,
                    command_engine,
                    command_scheduler,
                    command_watch,
                    job_recv,
                )
            })?;
        Ok(Self {
            jobs: Some(job_send),
            thread: Some(thread),
        })
    }
}

/// Runs one explicit command on the command lane, at the process's starting
/// nice, and replies to its client. Returns whether the command asks for a
/// fallback backstop.
pub(super) fn run_command(
    host: &Host,
    lane: &CommandLane,
    options: &IndexOptions,
    scheduler: &Arc<crate::scheduler::Scheduler>,
    command: Command,
) -> io::Result<bool> {
    let mut command_options = options.clone();
    command_options.workers = scheduler.configured_workers();
    command_options.bulk = Some(scheduler.command_control());
    let _operation = Operation::start(
        host,
        if command.request.op == Op::Index {
            "index"
        } else {
            "roots-remove"
        },
    );
    // The test delay stays on this thread, before the crawl, so a
    // queued command is already `writer_running`.
    #[cfg(debug_assertions)]
    std::thread::sleep(duration("FERRET_WRITER_TEST_COMMAND_DELAY_MS", 0));
    let (done, outcome) = mpsc::sync_channel(1);
    lane.jobs
        .as_ref()
        .ok_or_else(|| io::Error::other("command thread stopped"))?
        .send(CommandJob {
            request: command.request,
            options: command_options,
            done,
        })
        .map_err(|_| io::Error::other("command thread stopped"))?;
    let reply = match outcome
        .recv()
        .map_err(|_| io::Error::other("command thread stopped"))?
    {
        Ok(reply) => reply,
        Err(payload) => std::panic::resume_unwind(payload),
    };
    let arm_fallback = reply.arm_fallback;
    // The reply is detached from socket backpressure. Publication,
    // watch adoption and the unpaced content follow finish before
    // the next command begins.
    let _ = command.reply.send(reply);
    release_pending(&host.writer_pending);
    release_freed_memory();
    Ok(arm_fallback)
}

fn command_loop(
    host: Arc<Host>,
    engine: Arc<Engine>,
    scheduler: Arc<crate::scheduler::Scheduler>,
    watch: Option<Arc<Watch>>,
    jobs: mpsc::Receiver<CommandJob>,
) {
    let context = crate::cli::Context {
        index: host.index.clone(),
        dirs: crate::xdg::Dirs::from_env().ok(),
    };
    while let Ok(job) = jobs.recv() {
        let CommandJob {
            request,
            mut options,
            done,
        } = job;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_explicit(
                &host,
                &engine,
                &scheduler,
                watch.as_ref(),
                &context,
                &request,
                &mut options,
            )
        }));
        if done.send(outcome).is_err() {
            break;
        }
    }
}
fn run_explicit(
    host: &Host,
    engine: &Engine,
    scheduler: &crate::scheduler::Scheduler,
    watch: Option<&Arc<Watch>>,
    context: &crate::cli::Context,
    request: &Request,
    options: &mut IndexOptions,
) -> CommandReply {
    let result = change_roots(engine, scheduler, request, options);
    let mut arm_fallback = false;
    match &result {
        Ok(report) => {
            global_inputs(watch, engine, context);
            successful(host, report, false);
            if let Some(w) = watch {
                w.adopt_aliases(engine.pin().catalog());
                w.reconcile(engine.pin().catalog());
            }
        }
        Err(error) => arm_fallback = explicit_failed(host, watch, error),
    }
    let content = if result.is_ok() {
        content::follow_explicit(host, engine)
    } else {
        Ok(())
    };
    CommandReply {
        result,
        content,
        arm_fallback,
    }
}

/// Adds (`Op::Index`) or removes the roots `request` names and refreshes
/// them, or every root when an index request names none. A bulk pass
/// deferred for memory reports the scheduler's figures.
fn change_roots(
    engine: &Engine,
    scheduler: &crate::scheduler::Scheduler,
    request: &Request,
    options: &mut IndexOptions,
) -> Result<ferret_crawl::Report, ferret_crawl::IndexError> {
    // The first argv item is the originating client's rules. Paths are
    // absolute bytes, checked again by the real producer.
    let global = request
        .args
        .first()
        .map(|s| String::from_utf8_lossy(s).into_owned());
    let paths: Vec<_> = request
        .args
        .iter()
        .skip(1)
        .map(|s| PathBuf::from(OsStr::from_bytes(s)))
        .collect();
    if global.is_none() {
        return Err(ferret_crawl::IndexError::BadRoot(PathBuf::new()));
    }
    options.global = global;
    let (change, refresh) = match request.op {
        Op::Index => (
            RootChange {
                add: &paths,
                remove: &[],
            },
            if paths.is_empty() {
                Refresh::All
            } else {
                Refresh::Only(&paths)
            },
        ),
        _ => (
            RootChange {
                add: &[],
                remove: &paths,
            },
            Refresh::Only(&[]),
        ),
    };
    engine
        .index_change(change, refresh, options)
        .map_err(|error| match error {
            ferret_crawl::IndexError::DeferredBulk(ferret_catalog::bulk::Blocked::Memory) => {
                let status = scheduler.status();
                ferret_crawl::IndexError::DeferredMemory {
                    required: status.required_memory,
                    available: status.sample.memory.unwrap_or(0),
                }
            }
            other => other,
        })
}

/// Records an explicit command's `error` in the writer status and arms the
/// watch's backstop for a deferred or failed write. Returns whether a
/// deferral without a watch needs the fallback backstop.
fn explicit_failed(
    host: &Host,
    watch: Option<&Arc<Watch>>,
    error: &ferret_crawl::IndexError,
) -> bool {
    let mut arm_fallback = false;
    let deferred = match error {
        ferret_crawl::IndexError::DeferredBulk(reason) => Some(*reason),
        ferret_crawl::IndexError::DeferredMemory { .. } => {
            Some(ferret_catalog::bulk::Blocked::Memory)
        }
        _ => None,
    };
    if let Some(reason) = deferred {
        lock(&host.writer_status).blocked = Some(reason);
        if let Some(w) = watch {
            w.backstop(RefreshReason::Backstop);
        } else {
            arm_fallback = true;
            lock(&host.writer_status).fallback_backstop = true;
        }
    }
    if let Some(w) = watch {
        w.abort_policy_roots();
    }
    lock(&host.writer_status).error = Some(error.to_string());
    if matches!(
        error,
        ferret_crawl::IndexError::Update(_)
            | ferret_crawl::IndexError::Commit(_)
            | ferret_crawl::IndexError::Undurable { .. }
    ) && let Some(w) = watch
    {
        w.backstop(RefreshReason::Backstop);
    }
    arm_fallback
}
