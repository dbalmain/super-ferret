//! The daemon's one writer queue. Explicit root/policy commands are serial
//! barriers on a thread that keeps the process's starting nice; inotify drains
//! on another thread while the producer publishes. Queries use Engine's
//! immutable pins and never enter this queue.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ferret_crawl::watch::{Config, Limits, Watch};
use ferret_crawl::{IndexOptions, Refresh, RefreshReason, RootChange};

use super::{Host, Loaded, duration};
use crate::engine::Engine;
use crate::find_json::{emit_to, generation};
use crate::protocol::{Op, Request};
use crate::transport::Destination;

mod content;

pub(super) struct Command {
    request: Request,
    reply: mpsc::SyncSender<CommandReply>,
}
struct CommandJob {
    request: Request,
    options: IndexOptions,
    done: mpsc::SyncSender<std::thread::Result<CommandReply>>,
}
pub(super) struct CommandReply {
    result: Result<ferret_crawl::Report, ferret_crawl::IndexError>,
    /// `Ok` when the catalog change failed or content follow covered every
    /// document. Drain and shutdown leave `Err` while documents remain.
    content: Result<(), String>,
    /// A deferred command with no watch must arm the fallback backstop again.
    arm_fallback: bool,
}
/// Drops the job sender before joining, so teardown cannot detach the thread
/// or wait on a recv that still has a live sender.
struct CommandLane {
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
pub(super) enum Message {
    Command(Box<Command>),
    Intake,
}
#[derive(Default)]
pub(super) struct Status {
    pub watch: Option<Arc<Watch>>,
    pub last_refresh: Option<u64>,
    pub last_backstop: Option<u64>,
    pub refreshes: u64,
    pub last_reason: Option<RefreshReason>,
    pub operation: Option<&'static str>,
    pub merge_running: bool,
    pub input_usage: ferret_catalog::InputUsage,
    pub error: Option<String>,
    pub scheduler: Option<Arc<crate::scheduler::Scheduler>>,
    pub blocked: Option<ferret_catalog::bulk::Blocked>,
    pub fallback_backstop: bool,
}
struct Operation<'a>(&'a Host);
impl<'a> Operation<'a> {
    fn start(host: &'a Host, name: &'static str) -> Self {
        host.writer_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .operation = Some(name);
        host.writer_running.store(true, Ordering::Release);
        Self(host)
    }
}
impl Drop for Operation<'_> {
    fn drop(&mut self) {
        self.0
            .writer_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .operation = None;
        self.0.writer_running.store(false, Ordering::Release);
        super::wake_listener(self.0);
    }
}
fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub(super) fn start(
    host: Arc<Host>,
    receive: mpsc::Receiver<Message>,
) -> io::Result<std::thread::JoinHandle<()>> {
    let scheduler = Arc::new(crate::scheduler::Scheduler::new(
        crate::config::Controller::from_env(),
        std::thread::available_parallelism().map_or(1, |n| n.get()),
        host.index.clone(),
        Box::new(crate::politeness::Linux::from_env()),
        Arc::new(ferret_catalog::bulk::Monotonic::default()),
    ));
    host.writer_status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .scheduler = Some(scheduler.clone());
    let signal_host = host.clone();
    let signals = scheduler.clone();
    std::thread::spawn(move || {
        while !signal_host.stop.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_secs(1));
            signals.sample();
        }
    });
    // Named so that threads it starts without a name of their own inherit
    // "ferret-writer" and can be told from an explicit command's workers,
    // which inherit "ferret-command".
    std::thread::Builder::new()
        .name("ferret-writer".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                serve(&host, receive, scheduler)
            }));
            let error = match result {
                Ok(Ok(())) => return,
                Ok(Err(error)) => error.to_string(),
                Err(_) => "writer panicked".into(),
            };
            let mut loaded = host
                .engine
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let loading = matches!(*loaded, Loaded::Loading);
            if loading {
                *loaded = Loaded::Failed(error.clone());
            }
            drop(loaded);
            host.engine_ready.notify_all();
            host.writer_status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .error = Some(error.clone());
            host.writer_pending.store(0, Ordering::Release);
            if loading {
                super::wake_listener(&host);
                return;
            }
            // A ready engine would otherwise keep answering from a catalog
            // nothing refreshes, while holding the catalog lock a replacement
            // needs. A poisoned index writer is not retried: the host leaves
            // through drain, so the next client runs in process or starts a
            // fresh daemon (D110).
            super::fatal(&host, "writer stopped", &error);
        })
}
fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .map_or_else(|| "panicked".to_owned(), |s| format!("panicked: {s}"))
}
/// Returns heap freed by a rewalk or a command to the kernel. glibc keeps it
/// otherwise: after a `/nix/store` rewalk, RSS stayed about 0.5 GB above the
/// live heap for as long as the daemon ran (D116).
fn release_freed_memory() {
    // D11: on geekom's /nix/store (5.4M entries, past the watch cap) the
    // idle daemon held 715 MB anonymous without this and 637 MB with it,
    // flat for 20 minutes after the startup rewalk (2026-10-23).
    // SAFETY: malloc_trim takes no pointers and locks each arena itself; it
    // only returns whole free pages to the kernel.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[allow(unsafe_code)]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        unsafe {
            malloc_trim(0);
        }
    }
}
fn serve(
    host: &Arc<Host>,
    receive: mpsc::Receiver<Message>,
    scheduler: Arc<crate::scheduler::Scheduler>,
) -> io::Result<()> {
    let loading = Operation::start(host, "loading");
    #[cfg(debug_assertions)]
    std::thread::sleep(duration("FERRET_DAEMON_LOAD_DELAY_MS", 0));
    // First-use tests must observe loading before releasing it, regardless
    // of how long their local query takes to run under CPU contention.
    #[cfg(debug_assertions)]
    if let Some(gate) = std::env::var_os("FERRET_DAEMON_TEST_LOAD_GATE").map(PathBuf::from) {
        while gate.exists()
            && !host.stop.load(Ordering::Acquire)
            && !host
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .draining
        {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    // A daemon started before the first `ferret index` publishes an empty
    // catalog, holding the lock until its session owns it.
    let session = ferret_crawl::open_or_create(&host.index, &IndexOptions::default())
        .map_err(io::Error::other)?;
    let engine = Arc::new(Engine::from_writer(session));
    engine
        .attach_content(&host.index)
        .map_err(io::Error::other)?;
    let watch = Limits::read().ok().and_then(|limits| {
        let mut config = Config::from_limits(&limits);
        if let Ok(cap) = std::env::var("FERRET_WATCH_CAP")
            && let Ok(cap) = cap.parse()
        {
            config.watch_cap = cap;
        }
        #[cfg(debug_assertions)]
        if let Ok(cap) = std::env::var("FERRET_WATCH_TEST_SCOPES")
            && let Ok(cap) = cap.parse()
        {
            config.scopes = cap;
        }
        Watch::new_blocking(config).ok().map(Arc::new)
    });
    scheduler.watch(watch.clone());
    host.writer_status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .watch = watch.clone();
    if let Some(watch) = &watch {
        watch.backstop(RefreshReason::Backstop);
    }
    *host
        .engine
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Loaded::Ready(engine.clone());
    host.engine_ready.notify_all();
    drop(loading);
    if let Some(watch) = &watch {
        let watch = watch.clone();
        let send = host.writer_send.clone();
        let intake_host = host.clone();
        std::thread::Builder::new()
            .name("ferret-intake".into())
            .spawn(move || {
                // Without intake the catalog would go stale behind a ready
                // host, refreshed only by the hourly backstop. A panic or a
                // read error stops the host as a failed writer does.
                let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    watch.run_intake(|| {
                        let _ = send.try_send(Message::Intake);
                        !intake_host.stop.load(Ordering::Acquire)
                    })
                }));
                let cause = match ran {
                    Ok(Ok(())) => return,
                    Ok(Err(error)) => io::Error::from(error).to_string(),
                    Err(panic) => panic_message(panic.as_ref()),
                };
                super::fatal(&intake_host, "watch intake stopped", &cause);
            })?;
    }
    let context = crate::cli::Context {
        index: host.index.clone(),
        dirs: crate::xdg::Dirs::from_env().ok(),
    };
    // Intake and the sampler were spawned at the process's starting nice.
    // This thread is too. An unprivileged process cannot raise a thread's
    // nice later (RLIMIT_NICE is a hard ceiling), so an explicit command
    // runs here and its crawl workers inherit this nice. The writer then
    // lowers itself to nice 19 for background refresh and follow, and blocks
    // here until the command finishes, which keeps a watch refresh queued.
    let (job_send, job_recv) = mpsc::channel();
    let command_host = host.clone();
    let command_engine = engine.clone();
    let command_scheduler = scheduler.clone();
    let command_watch = watch.clone();
    let command_thread = std::thread::Builder::new()
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
    let lane = CommandLane {
        jobs: Some(job_send),
        thread: Some(command_thread),
    };
    ferret_crawl::lower_index_priority()?;
    let mut options = IndexOptions {
        watch: watch.clone(),
        bulk: Some(scheduler.clone()),
        ..IndexOptions::default()
    };
    let hourly = duration("FERRET_BACKSTOP_MS", 60 * 60 * 1000).max(Duration::from_millis(1));
    let polling = duration("FERRET_POLL_MS", 5 * 60 * 1000).max(Duration::from_millis(1));
    let mut full_due = Instant::now() + hourly;
    let mut poll_due = Instant::now() + polling;
    // Whether a poll was issued that no refresh has taken yet.
    let mut poll_issued = false;
    let mut retry_due: Option<Duration> = None;
    let mut initial = watch.is_none();
    host.writer_status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .fallback_backstop = initial;
    let mut content_due = Instant::now();
    let mut content_state = content::State::default();
    loop {
        if host.stop.load(Ordering::Acquire) {
            break;
        }
        let now = Instant::now();
        let mut deadline = full_due.min(poll_due);
        // A catalog retry owns this boundary. An expired content deadline
        // must not wake repeatedly while that retry is cooling down.
        if retry_due.is_none() {
            deadline = deadline.min(content_due);
        }
        if let Some(retry) = retry_due {
            deadline = deadline.min(now + retry.saturating_sub(scheduler.now()));
        }
        if retry_due.is_none()
            && let Some(due) = watch.as_ref().and_then(|w| w.next_due())
        {
            deadline = deadline.min(due);
        }
        let draining = host
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .draining;
        // Drain forbids background work, so its deadlines never advance and
        // an expired one would spin until stop. Only a command or the stop
        // intake can change anything.
        let message = if draining {
            receive
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        } else {
            receive.recv_timeout(deadline.saturating_duration_since(now))
        };
        match message {
            Ok(Message::Command(command)) => {
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
                if reply.arm_fallback {
                    initial = true;
                }
                // The reply is detached from socket backpressure. Publication,
                // watch adoption and the unpaced content follow finish before
                // the next command begins.
                let _ = command.reply.send(reply);
                release_pending(&host.writer_pending);
                release_freed_memory();
                content_due = Instant::now();
                continue;
            }
            Ok(Message::Intake) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if host
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .draining
        {
            continue;
        }
        if Instant::now() >= full_due {
            if let Some(w) = &watch {
                w.backstop(RefreshReason::Backstop);
            } else {
                initial = true;
                host.writer_status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .fallback_backstop = true;
            }
            full_due = Instant::now() + hourly;
        }
        if Instant::now() >= poll_due {
            if let Some(w) = &watch {
                let pin = engine.pin();
                let scopes = w
                    .polling_roots(pin.catalog())
                    .into_iter()
                    .chain(retained_roots(pin.catalog()))
                    .collect::<BTreeSet<_>>();
                w.scoped_roots(scopes.into_iter().collect());
            } else {
                initial = true;
                host.writer_status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .fallback_backstop = true;
            }
            poll_due = Instant::now() + polling;
            poll_issued = true;
        }
        if retry_due.is_some_and(|retry| scheduler.now() < retry) {
            continue;
        }
        // An expired retry is spent here. Left set, it would keep hiding the
        // watch's next due time from the deadline, and a later `continue`
        // would spin on the past deadline.
        retry_due = None;
        options.workers = scheduler.status().workers;
        let paused = scheduler.status().paused;
        let burst = watch
            .as_ref()
            .and_then(|w| w.take_admitted(paused.is_none()));
        if paused.is_some() && burst.is_none() {
            content_due = Instant::now() + Duration::from_secs(1);
            if initial
                || watch
                    .as_ref()
                    .is_some_and(|w| w.status().backstop.is_some())
            {
                retry_due = Some(scheduler.now() + Duration::from_secs(1));
            }
            continue;
        }
        if burst.is_none() && !initial {
            if Instant::now() >= content_due {
                content_due =
                    Instant::now() + content::turn(host, &engine, &scheduler, &mut content_state)?;
            }
            continue;
        }
        if paused.is_none() {
            host.writer_status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .blocked = None;
        }
        let started = Instant::now();
        // The burst taken above holds any poll issued before it.
        let polled = std::mem::take(&mut poll_issued);
        let _operation = Operation::start(host, "refresh");
        #[cfg(debug_assertions)]
        std::thread::sleep(duration("FERRET_WATCH_TEST_REFRESH_DELAY_MS", 0));
        global_inputs(watch.as_ref(), &engine, &context);
        let mut reason = RefreshReason::Backstop;
        let result = crate::index::global_ignore(&context)
            .map_err(io::Error::other)
            .and_then(|global| {
                options.global = Some(global);
                let pin = engine.pin();
                let request = burst.as_ref().map_or_else(
                    || ferret_crawl::RefreshRequest {
                        expected_generation: pin.generation(),
                        scopes: vec![],
                        rename_hints: vec![],
                        reason: RefreshReason::Backstop,
                    },
                    |b| b.request(pin.catalog()),
                );
                reason = request.reason;
                engine.refresh(request, &options).map_err(io::Error::other)
            });
        // Past the watch cap a poll rewalks a whole root, which can outlast
        // the poll interval and would then run back to back (D116). A long
        // refresh spaces the next poll by four times its duration, so polling
        // holds the writer at most a fifth of the time. Only a refresh that
        // walked the polled roots, a poll's or a full one, spaces them: a slow
        // scoped burst elsewhere must not starve them.
        let elapsed = started.elapsed();
        let spacing = elapsed.saturating_mul(4);
        if (polled || reason != RefreshReason::Burst) && spacing > polling {
            poll_due = poll_due.max(Instant::now() + spacing);
        }
        if elapsed >= Duration::from_secs(1) {
            release_freed_memory();
        }
        let deferred = result
            .as_ref()
            .ok()
            .and_then(|report| match report.outcome {
                ferret_crawl::RefreshOutcome::DeferredBulk(reason) => Some(reason),
                _ => None,
            });
        let retry_current = result.as_ref().is_ok_and(|report| {
            matches!(
                report.outcome,
                ferret_crawl::RefreshOutcome::RetryFromCurrent(_)
            )
        });
        match &result {
            Ok(_) if retry_current => {
                // A budget checkpoint may keep sequence unchanged. It did not
                // observe the queued work; resolve the complete marker again.
                initial = true;
                host.writer_status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .fallback_backstop = watch.is_none();
                retry_due = Some(scheduler.now());
            }
            Ok(_) if deferred.is_some() => {
                host.writer_status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .blocked = deferred;
                if watch.is_none() {
                    initial = true;
                    host.writer_status
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .fallback_backstop = true;
                }
                retry_due = Some(scheduler.now() + Duration::from_secs(1));
            }
            Ok(report) => {
                global_inputs(watch.as_ref(), &engine, &context);
                let complete = burst
                    .as_ref()
                    .is_none_or(|b| b.reason() != RefreshReason::Burst)
                    && report.report.coverage_faults.is_empty();
                successful(host, &report.report, complete);
                host.writer_status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .last_reason = Some(reason);
                if let Some(w) = &watch {
                    w.adopt_aliases(&report.view);
                    if burst.as_ref().is_none_or(|b| b.reconcile_watches()) {
                        w.reconcile(&report.view);
                    }
                }
                initial = false;
                retry_due = None;
                content_due = Instant::now();
            }
            Err(error) => {
                host.writer_status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .error = Some(error.to_string());
                retry_due = Some(scheduler.now() + Duration::from_secs(1));
            }
        }
        if let Some(burst) = burst
            && let Some(watch) = &watch
        {
            if deferred.is_some() {
                watch.defer(burst);
            } else {
                watch.finish(burst, result.is_ok() && !retry_current);
            }
        }
    }
    drop(content_state);
    // The merge worker is joined. This thread still holds the catalog lock
    // until it returns. A test can hold that window open (D110).
    #[cfg(debug_assertions)]
    if host.stop.load(Ordering::Acquire) {
        let delay = duration("FERRET_DAEMON_TEST_STOP_DELAY_MS", 0);
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
    }
    Ok(())
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
    let result = if global.is_none() {
        Err(ferret_crawl::IndexError::BadRoot(PathBuf::new()))
    } else {
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
    };
    let mut arm_fallback = false;
    if let Ok(report) = &result {
        global_inputs(watch, engine, context);
        successful(host, report, false);
        if let Some(w) = watch {
            w.adopt_aliases(engine.pin().catalog());
            w.reconcile(engine.pin().catalog());
        }
    } else if let Err(error) = &result {
        let deferred = match error {
            ferret_crawl::IndexError::DeferredBulk(reason) => Some(*reason),
            ferret_crawl::IndexError::DeferredMemory { .. } => {
                Some(ferret_catalog::bulk::Blocked::Memory)
            }
            _ => None,
        };
        if let Some(reason) = deferred {
            host.writer_status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .blocked = Some(reason);
            if let Some(w) = watch {
                w.backstop(RefreshReason::Backstop);
            } else {
                arm_fallback = true;
                host.writer_status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .fallback_backstop = true;
            }
        }
        if let Some(w) = watch {
            w.abort_policy_roots();
        }
        host.writer_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .error = Some(error.to_string());
        if matches!(
            error,
            ferret_crawl::IndexError::Update(_)
                | ferret_crawl::IndexError::Commit(_)
                | ferret_crawl::IndexError::Undurable { .. }
        ) && let Some(w) = watch
        {
            w.backstop(RefreshReason::Backstop);
        }
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
/// Roots whose effective catalog still holds a retained directory (D26's
/// S1+ checked retention), which polling retries until a refresh repairs them.
/// The catalog is the authority: a report names only the faults in the scopes
/// it observed, so it can neither add nor clear another root's protection, and
/// the markers survive a daemon restart. One pass over the directories, taken
/// once per poll interval rather than per burst.
fn retained_roots(catalog: &ferret_catalog::Catalog) -> BTreeSet<PathBuf> {
    let names = catalog.name_reader();
    let mut tops = BTreeSet::new();
    for dir in catalog.dir_ids() {
        if catalog.retained_at(dir).is_none() {
            continue;
        }
        let mut at = dir;
        while let Some(name) = catalog.dir_name(at) {
            at = names.edge(name).0;
        }
        tops.insert(at);
    }
    catalog
        .roots()
        .filter(|(id, _)| tops.contains(id))
        .map(|(_, path)| PathBuf::from(OsStr::from_bytes(path)))
        .collect()
}
/// A client's failed send can follow the failed writer's reset to zero, so
/// the count saturates rather than wrapping into a permanently busy host.
fn release_pending(pending: &std::sync::atomic::AtomicUsize) {
    let _ = pending.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
        Some(n.saturating_sub(1))
    });
}
fn global_inputs(watch: Option<&Arc<Watch>>, engine: &Engine, context: &crate::cli::Context) {
    if let (Some(w), Some(dirs)) = (watch, &context.dirs) {
        let pin = engine.pin();
        for (_, root) in pin.catalog().roots() {
            let root = std::path::Path::new(OsStr::from_bytes(root));
            w.policy_path(root, &dirs.ignore_file());
            w.policy_path(root, &dirs.config.join("config"));
        }
    }
}
fn successful(host: &Host, report: &ferret_crawl::Report, backstop: bool) {
    let mut status = host
        .writer_status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    status.last_refresh = Some(timestamp());
    status.refreshes += 1;
    status.input_usage = report.input_usage;
    if backstop {
        status.last_backstop = status.last_refresh;
    }
    status.error = None;
    status.blocked = None;
    status.fallback_backstop = false;
}
// A merge does not keep an otherwise idle host alive: writer teardown
// cancels and joins it before close_writer and socket removal (D110).
pub(super) fn busy(host: &Host) -> bool {
    matches!(
        *host
            .engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        Loaded::Loading
    ) || host.writer_running.load(Ordering::Acquire)
        || host.writer_pending.load(Ordering::Acquire) != 0
}
pub(super) fn execute(host: &Host, request: &Request, destination: &Destination) -> io::Result<()> {
    let (reply, receive) = mpsc::sync_channel(1);
    host.writer_pending.fetch_add(1, Ordering::AcqRel);
    if host
        .writer_send
        .try_send(Message::Command(Box::new(Command {
            request: request.clone(),
            reply,
        })))
        .is_err()
    {
        release_pending(&host.writer_pending);
        emit_to(destination, &request.id, "begin", |o| {
            o.null("generation");
        })?;
        return emit_to(destination, &request.id, "end", |o| {
            o.int("exit", 3).str("error", "WriterUnavailable");
        });
    }
    let started = Instant::now();
    let reply = match receive.recv() {
        Ok(reply) => reply,
        Err(_) => {
            emit_to(destination, &request.id, "begin", |o| {
                o.null("generation");
            })?;
            return emit_to(destination, &request.id, "end", |o| {
                o.int("exit", 3).str("error", "WriterUnavailable").str(
                    "message",
                    "writer failed; retry the command, and if ferretd is still running, \
                     inspect ferret status --json and restart it",
                );
            });
        }
    };
    let rendered = crate::index::render(&reply.result);
    // Daemon stderr frames are written raw, so the prefix matches `cli::error`.
    let content_failed = reply.content.is_err();
    let exit = if content_failed {
        crate::cli::Exit::Error as u8
    } else {
        rendered.exit as u8
    };
    let mut stderr = rendered.diagnostics;
    if let Err(error) = &reply.content {
        stderr.push_str("ferret: content index: ");
        stderr.push_str(error);
        stderr.push('\n');
    }
    let stdout = if content_failed {
        String::new()
    } else {
        rendered.stdout
    };
    let session = super::pin(host)?;
    emit_to(destination, &request.id, "begin", |o| {
        generation(o, Some(session.generation()));
    })?;
    for (event, text) in [("stdout", &stdout), ("stderr", &stderr)] {
        for bytes in text.as_bytes().chunks(64 << 10) {
            emit_to(destination, &request.id, event, |o| {
                o.bytes("bytes", bytes);
            })?;
        }
    }
    let mut log = Vec::new();
    let mut object = crate::log::line(
        &mut log,
        if request.op == Op::Index {
            "index"
        } else {
            "roots-remove"
        },
        SystemTime::now(),
    );
    object
        .str("outcome", rendered.outcome)
        .int("exit", exit)
        .int("total_us", started.elapsed().as_micros() as i128);
    if let Ok(report) = &reply.result {
        crate::index::log_report(&mut object, report);
    }
    object.end();
    emit_to(destination, &request.id, "log", |o| {
        o.bytes("bytes", &log);
    })?;
    emit_to(destination, &request.id, "end", |o| {
        o.int("exit", exit).str("outcome", rendered.outcome);
    })
}
pub(super) fn fields(host: &Host, o: &mut crate::json::Object<'_>) {
    let s = host
        .writer_status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(w) = &s.watch {
        let w = w.status();
        o.int("watch_installed", w.installed as u64)
            .int("watch_needed", w.needed as u64)
            .int("watch_failed", w.failed as u64)
            .int("pending_scopes", w.pending as u64)
            .int("pending_bytes", w.bytes as u64)
            .byte_strings(
                "polling_roots",
                w.polling_roots.iter().map(|p| p.as_os_str().as_bytes()),
            )
            .opt_int("oldest_pending_ms", w.oldest.map(|d| d.as_millis() as i128))
            .bool("watch_uncovered", w.uncovered);
        if let Some(reason) = w.backstop {
            o.str("backstop_reason", &format!("{reason:?}"));
        } else {
            o.null("backstop_reason");
        }
    } else {
        o.bool("watch_uncovered", true)
            .int("watch_installed", 0)
            .int("watch_needed", 0)
            .int("watch_failed", 0)
            .int("pending_scopes", u64::from(s.fallback_backstop))
            .int("pending_bytes", 0)
            .null("polling_roots")
            .null("oldest_pending_ms");
        if s.fallback_backstop {
            o.str("backstop_reason", "Backstop");
        } else {
            o.null("backstop_reason");
        }
    }
    o.bool("host_running", true)
        .str(
            "current_operation",
            if host.writer_running.load(Ordering::Acquire) {
                s.operation.unwrap_or("refresh")
            } else {
                "idle"
            },
        )
        .bool("merge_running", s.merge_running)
        .int("refreshes", s.refreshes);
    if let Some(reason) = s.last_reason {
        o.str("last_refresh_reason", &format!("{reason:?}"));
    } else {
        o.null("last_refresh_reason");
    }
    o.object("writer_input_usage", |o| {
        o.int("records", s.input_usage.records as u64)
            .int("owned_bytes", s.input_usage.owned_bytes as u64)
            .bool("exceeded", s.input_usage.exceeded);
    });
    o.opt_int("last_successful_refresh", s.last_refresh)
        .opt_int("last_complete_backstop", s.last_backstop)
        .int(
            "writer_commands",
            host.writer_pending.load(Ordering::Acquire) as u64,
        )
        .bool("writer_busy", host.writer_running.load(Ordering::Acquire));
    if let Some(scheduler) = &s.scheduler {
        let c = scheduler.status();
        o.object("controller", |o| {
            for (key, value) in [
                ("cpu_psi_some_avg10", c.sample.cpu),
                ("io_psi_some_avg10", c.sample.io),
                ("load", c.sample.load),
            ] {
                if let Some(value) = value {
                    o.number(key, value);
                } else {
                    o.str(key, "unavailable");
                }
            }
            if let Some(battery) = c.sample.battery {
                o.bool("on_battery", battery);
            } else {
                o.str("on_battery", "unavailable");
            }
            match c.sample.idle {
                crate::politeness::Idle::Unknown => {
                    o.str("idle", "unknown");
                }
                crate::politeness::Idle::Headless => {
                    o.str("idle", "headless");
                }
                crate::politeness::Idle::Desktop(duration) => {
                    o.int("idle_seconds", duration.as_secs());
                }
            }
            o.int("worker_target", c.workers as u64)
                .opt_int("available_memory_bytes", c.sample.memory)
                .opt_int("available_disk_bytes", c.available_disk)
                .int("required_memory_bytes", c.required_memory)
                .int("required_disk_bytes", c.required_disk)
                .str("priority", "per-thread nice 19")
                .bool("no_reuse_advice", false);
            if let Some(reason) = c.paused.or(s.blocked) {
                o.str("blocked_reason", reason.status());
            } else {
                o.null("blocked_reason");
            }
            if let Some((kind, result)) = c.admission {
                o.str(
                    "admission_kind",
                    match kind {
                        ferret_catalog::bulk::Kind::Checkpoint => "checkpoint",
                        ferret_catalog::bulk::Kind::FullRewalk => "full-rewalk",
                    },
                );
                o.str(
                    "admission",
                    result.err().map_or("admitted", |reason| reason.status()),
                );
            } else {
                o.null("admission_kind").null("admission");
            }
            let rate = scheduler.rate_status();
            o.object("rate_limit", |o| {
                o.int("bytes_per_second", rate.bytes_per_second)
                    .int("reserved_bytes", rate.reserved_bytes)
                    .int("waits", rate.waits)
                    .int("waiting", rate.waiting)
                    .int("sequential_advice_failures", rate.advice_failures);
            });
        });
    } else {
        o.null("controller");
    }
    if let Some(error) = &s.error {
        o.str("refresh_error", error);
    } else {
        o.null("refresh_error");
    }
}

#[cfg(test)]
mod tests;
