//! The daemon's one writer queue. Explicit root/policy commands are serial
//! barriers on a thread that keeps the process's starting nice; inotify drains
//! on another thread while the producer publishes. Queries use Engine's
//! immutable pins and never enter this queue.

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ferret_crawl::watch::{Config, Limits, Watch};
use ferret_crawl::{IndexOptions, Refresh, RefreshReason, RootChange};

use super::{Host, Loaded, duration};
use crate::engine::Engine;
use crate::find_json::{emit_to, generation};
use crate::lock;
use crate::protocol::{Op, Request};
use crate::transport::Destination;

mod background;
mod content;

use background::Background;

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

/// The writer's inbox. Each command takes a slot; wakeups coalesce into at
/// most one queued [`Message::Intake`]. Intake wakes the writer once per
/// inotify event, so uncoalesced, a burst of saves while the writer was busy
/// filled the queue and turned the next command away as `WriterUnavailable`.
pub(super) struct Queue {
    send: mpsc::SyncSender<Message>,
    /// Whether a wakeup is pending: an `Intake` queued and not yet received,
    /// or one that found the queue full and is delivered by
    /// [`Queue::receive`] once the queue empties.
    woken: AtomicBool,
}

impl Queue {
    pub fn new(capacity: usize) -> (Self, mpsc::Receiver<Message>) {
        let (send, receive) = mpsc::sync_channel(capacity);
        let queue = Self {
            send,
            woken: AtomicBool::new(false),
        };
        (queue, receive)
    }

    /// Wakes the writer, unless a wakeup is already pending. A full queue
    /// leaves it pending: the writer has messages to take first, and asks
    /// for the wakeup before it next blocks.
    pub fn wake(&self) {
        if !self.woken.swap(true, Ordering::AcqRel) {
            let _ = self.send.try_send(Message::Intake);
        }
    }

    /// Queues a command; it is refused when the queue is full or the writer
    /// has stopped.
    pub fn command(&self, command: Command) -> Result<(), ()> {
        self.send
            .try_send(Message::Command(Box::new(command)))
            .map_err(|_| ())
    }

    /// The writer's next message, waiting at most `timeout`, or for ever
    /// when it is `None`. A pending wakeup that found the queue full is
    /// delivered as an `Intake` once the queue is empty, before blocking.
    /// Taking a wakeup clears it, so an event after this wakes the writer
    /// again.
    fn receive(
        &self,
        receive: &mpsc::Receiver<Message>,
        timeout: Option<Duration>,
    ) -> Result<Message, mpsc::RecvTimeoutError> {
        let message = match receive.try_recv() {
            Ok(message) => Ok(message),
            Err(mpsc::TryRecvError::Disconnected) => Err(mpsc::RecvTimeoutError::Disconnected),
            Err(mpsc::TryRecvError::Empty) if self.woken.swap(false, Ordering::AcqRel) => {
                return Ok(Message::Intake);
            }
            Err(mpsc::TryRecvError::Empty) => match timeout {
                Some(timeout) => receive.recv_timeout(timeout),
                None => receive
                    .recv()
                    .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
            },
        };
        if let Ok(Message::Intake) = message {
            // Acquires a coalesced waker's event as the swap above does.
            self.woken.swap(false, Ordering::AcqRel);
        }
        message
    }
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
        lock(&host.writer_status).operation = Some(name);
        host.writer_running.store(true, Ordering::Release);
        Self(host)
    }
}
impl Drop for Operation<'_> {
    fn drop(&mut self) {
        lock(&self.0.writer_status).operation = None;
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
    lock(&host.writer_status).scheduler = Some(scheduler.clone());
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
            let Some(error) = stopped_cause(result) else {
                return;
            };
            let mut loaded = lock(&host.engine);
            let loading = matches!(*loaded, Loaded::Loading);
            if loading {
                *loaded = Loaded::Failed(error.clone());
            }
            drop(loaded);
            host.engine_ready.notify_all();
            lock(&host.writer_status).error = Some(error.clone());
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
/// Why the writer thread ended, or None for a clean stop. A panic keeps its
/// message, so the "writer stopped" line names the cause (D122).
fn stopped_cause(result: std::thread::Result<io::Result<()>>) -> Option<String> {
    match result {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error.to_string()),
        Err(panic) => Some(format!("writer {}", panic_message(panic.as_ref()))),
    }
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
/// Opens the catalog, or creates it, as this writer's session, attaches
/// the content index and starts the watch, then publishes the engine to
/// waiting clients. The host reports loading throughout.
fn load(
    host: &Arc<Host>,
    scheduler: &crate::scheduler::Scheduler,
) -> io::Result<(Arc<Engine>, Option<Arc<Watch>>)> {
    let loading = Operation::start(host, "loading");
    #[cfg(debug_assertions)]
    std::thread::sleep(duration("FERRET_DAEMON_LOAD_DELAY_MS", 0));
    // First-use tests must observe loading before releasing it, regardless
    // of how long their local query takes to run under CPU contention.
    #[cfg(debug_assertions)]
    if let Some(gate) = std::env::var_os("FERRET_DAEMON_TEST_LOAD_GATE").map(PathBuf::from) {
        while gate.exists() && !host.stop.load(Ordering::Acquire) && !lock(&host.lifecycle).draining
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
    let watch = open_watch();
    scheduler.watch(watch.clone());
    lock(&host.writer_status).watch = watch.clone();
    if let Some(watch) = &watch {
        watch.backstop(RefreshReason::Backstop);
    }
    *lock(&host.engine) = Loaded::Ready(engine.clone());
    host.engine_ready.notify_all();
    drop(loading);
    Ok((engine, watch))
}
fn serve(
    host: &Arc<Host>,
    receive: mpsc::Receiver<Message>,
    scheduler: Arc<crate::scheduler::Scheduler>,
) -> io::Result<()> {
    let (engine, watch) = load(host, &scheduler)?;
    if let Some(watch) = &watch {
        spawn_intake(host, watch.clone())?;
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
    let lane = CommandLane::spawn(host, &engine, &scheduler, &watch)?;
    ferret_crawl::lower_index_priority()?;
    let mut background = Background::new(host, &engine, &scheduler, &watch, context);
    loop {
        if host.stop.load(Ordering::Acquire) {
            break;
        }
        let now = Instant::now();
        let deadline = background.deadline(now);
        let draining = lock(&host.lifecycle).draining;
        // Drain forbids background work, so its deadlines never advance and
        // an expired one would spin until stop. Only a command or the stop
        // intake can change anything.
        let message = host.writer_send.receive(
            &receive,
            (!draining).then(|| deadline.saturating_duration_since(now)),
        );
        match message {
            Ok(Message::Command(command)) => {
                if run_command(host, &lane, &background.options, &scheduler, *command)? {
                    background.initial = true;
                }
                background.content_due = Instant::now();
                continue;
            }
            Ok(Message::Intake) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if lock(&host.lifecycle).draining {
            continue;
        }
        background.backstop_if_due();
        background.poll_if_due();
        background.turn()?;
    }
    drop(background);
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
/// The inotify watch sized from the process's limits, or `None` when they
/// cannot be read or the watch cannot start, which leaves the writer polling.
fn open_watch() -> Option<Arc<Watch>> {
    Limits::read().ok().and_then(|limits| {
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
    })
}

/// Starts the thread that turns inotify events into writer wakeups.
fn spawn_intake(host: &Arc<Host>, watch: Arc<Watch>) -> io::Result<()> {
    let intake_host = host.clone();
    std::thread::Builder::new()
        .name("ferret-intake".into())
        .spawn(move || {
            // Without intake the catalog would go stale behind a ready
            // host, refreshed only by the hourly backstop. A panic or a
            // read error stops the host as a failed writer does.
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                watch.run_intake(|| {
                    intake_host.writer_send.wake();
                    !intake_host.stop.load(Ordering::Acquire)
                })
            }));
            let cause = match ran {
                Ok(Ok(())) => return,
                Ok(Err(error)) => io::Error::from(error).to_string(),
                Err(panic) => panic_message(panic.as_ref()),
            };
            super::fatal(&intake_host, "watch intake stopped", &cause);
        })
        .map(drop)
}

impl CommandLane {
    fn spawn(
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
fn run_command(
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
    let mut status = lock(&host.writer_status);
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
    matches!(*lock(&host.engine), Loaded::Loading)
        || host.writer_running.load(Ordering::Acquire)
        || host.writer_pending.load(Ordering::Acquire) != 0
}
pub(super) fn execute(host: &Host, request: &Request, destination: &Destination) -> io::Result<()> {
    let (reply, receive) = mpsc::sync_channel(1);
    host.writer_pending.fetch_add(1, Ordering::AcqRel);
    if host
        .writer_send
        .command(Command {
            request: request.clone(),
            reply,
        })
        .is_err()
    {
        release_pending(&host.writer_pending);
        return unavailable(destination, &request.id, None);
    }
    let started = Instant::now();
    let reply = match receive.recv() {
        Ok(reply) => reply,
        Err(_) => {
            return unavailable(
                destination,
                &request.id,
                Some(
                    "writer failed; retry the command, and if ferretd is still running, \
                     inspect ferret status --json and restart it",
                ),
            );
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
    let log = log_line(request, &reply, rendered.outcome, exit, started);
    emit_to(destination, &request.id, "log", |o| {
        o.bytes("bytes", &log);
    })?;
    emit_to(destination, &request.id, "end", |o| {
        o.int("exit", exit).str("outcome", rendered.outcome);
    })
}
/// Answers a writer command the writer cannot take, with `message` saying
/// what to do when the writer failed rather than was gone.
fn unavailable(destination: &Destination, id: &str, message: Option<&str>) -> io::Result<()> {
    emit_to(destination, id, "begin", |o| {
        o.null("generation");
    })?;
    emit_to(destination, id, "end", |o| {
        o.int("exit", 3).str("error", "WriterUnavailable");
        if let Some(message) = message {
            o.str("message", message);
        }
    })
}
/// The query-log line for writer command `request`, which `reply` answered.
fn log_line(
    request: &Request,
    reply: &CommandReply,
    outcome: &str,
    exit: u8,
    started: Instant,
) -> Vec<u8> {
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
        .str("outcome", outcome)
        .int("exit", exit)
        .int("total_us", started.elapsed().as_micros() as i128);
    crate::index::log_result(&mut object, &reply.result);
    object.end();
    log
}
pub(super) fn fields(host: &Host, o: &mut crate::json::Object<'_>) {
    let s = lock(&host.writer_status);
    s.watch_fields(o);
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
        .int("refreshes", s.refreshes)
        .opt_str(
            "last_refresh_reason",
            s.last_reason.map(|reason| format!("{reason:?}")).as_deref(),
        );
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
        o.object("controller", |o| controller_fields(scheduler, s.blocked, o));
    } else {
        o.null("controller");
    }
    o.opt_str("refresh_error", s.error.as_deref());
}
impl Status {
    /// The watch's coverage and backlog, or what the fallback backstop
    /// stands in for them.
    fn watch_fields(&self, o: &mut crate::json::Object<'_>) {
        let Some(w) = &self.watch else {
            o.bool("watch_uncovered", true)
                .int("watch_installed", 0)
                .int("watch_needed", 0)
                .int("watch_failed", 0)
                .int("pending_scopes", u64::from(self.fallback_backstop))
                .int("pending_bytes", 0)
                .null("polling_roots")
                .null("oldest_pending_ms")
                .opt_str(
                    "backstop_reason",
                    self.fallback_backstop.then_some("Backstop"),
                );
            return;
        };
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
            .bool("watch_uncovered", w.uncovered)
            .opt_str(
                "backstop_reason",
                w.backstop.map(|reason| format!("{reason:?}")).as_deref(),
            );
    }
}
/// The politeness controller's samples, targets and admission.
fn controller_fields(
    scheduler: &crate::scheduler::Scheduler,
    blocked: Option<ferret_catalog::bulk::Blocked>,
    o: &mut crate::json::Object<'_>,
) {
    let c = scheduler.status();
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
        .bool("no_reuse_advice", false)
        .opt_str(
            "blocked_reason",
            c.paused.or(blocked).map(|reason| reason.status()),
        );
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
}

#[cfg(test)]
mod tests;
