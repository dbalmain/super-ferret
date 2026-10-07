//! The daemon's one writer queue. Explicit root/policy commands are serial
//! barriers; inotify drains on another thread while the producer publishes.
//! Queries use Engine's immutable pins and never enter this queue.

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ferret_catalog::WriterSession;
use ferret_crawl::watch::{Config, Limits, Watch};
use ferret_crawl::{IndexOptions, Refresh, RefreshReason, RootChange};

use super::{Host, Loaded, duration};
use crate::engine::Engine;
use crate::find_json::{emit_to, generation};
use crate::protocol::{Op, Request};
use crate::transport::Destination;

pub(super) struct Command {
    request: Request,
    reply: mpsc::SyncSender<Result<ferret_crawl::Report, ferret_crawl::IndexError>>,
}
pub(super) enum Message {
    Command(Command),
    Intake,
}
#[derive(Default)]
pub(super) struct Status {
    pub watch: Option<Arc<Watch>>,
    pub last_refresh: Option<u64>,
    pub last_backstop: Option<u64>,
    pub fault_retained: bool,
    pub refreshes: u64,
    pub last_reason: Option<RefreshReason>,
    pub operation: Option<&'static str>,
    pub input_usage: ferret_catalog::InputUsage,
    pub error: Option<String>,
    pub scheduler: Option<Arc<crate::scheduler::Scheduler>>,
    pub blocked: Option<ferret_catalog::bulk::Blocked>,
    pub fallback_backstop: bool,
    pub retained_roots: std::collections::BTreeSet<PathBuf>,
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
) -> std::thread::JoinHandle<()> {
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
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            serve(&host, receive, scheduler)
        }));
        let error = match result {
            Ok(Ok(())) => return,
            Ok(Err(error)) => error.to_string(),
            Err(_) => "writer panicked; restart ferretd to recover".into(),
        };
        let mut loaded = host
            .engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*loaded, Loaded::Loading) {
            *loaded = Loaded::Failed(error.clone());
        }
        drop(loaded);
        host.engine_ready.notify_all();
        host.writer_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .error = Some(error);
        host.writer_pending.store(0, Ordering::Release);
        super::wake_listener(&host);
    })
}
fn serve(
    host: &Arc<Host>,
    receive: mpsc::Receiver<Message>,
    scheduler: Arc<crate::scheduler::Scheduler>,
) -> io::Result<()> {
    let loading = Operation::start(host, "loading");
    #[cfg(debug_assertions)]
    std::thread::sleep(duration("FERRET_DAEMON_LOAD_DELAY_MS", 0));
    let session = WriterSession::open(&host.index).map_err(io::Error::other)?;
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
        std::thread::spawn(move || {
            watch.run_intake(|| {
                let _ = send.try_send(Message::Intake);
                !intake_host.stop.load(Ordering::Acquire)
            })
        });
    }
    let context = crate::cli::Context {
        index: host.index.clone(),
        dirs: crate::xdg::Dirs::from_env().ok(),
    };
    // Intake and sampler were spawned at normal priority; only this retained
    // index owner and its subsequently created index workers are lowered.
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
    let mut retry_due: Option<Duration> = None;
    let mut initial = watch.is_none();
    host.writer_status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .fallback_backstop = initial;
    let mut content_due = Instant::now();
    loop {
        if host.stop.load(Ordering::Acquire) {
            break;
        }
        let now = Instant::now();
        let mut deadline = full_due.min(poll_due).min(content_due);
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
                #[cfg(debug_assertions)]
                std::thread::sleep(duration("FERRET_WRITER_TEST_COMMAND_DELAY_MS", 0));
                // The first argv item is the originating client's rules. Paths
                // are absolute bytes, checked again by the real producer.
                let global = command
                    .request
                    .args
                    .first()
                    .map(|s| String::from_utf8_lossy(s).into_owned());
                let paths: Vec<_> = command
                    .request
                    .args
                    .iter()
                    .skip(1)
                    .map(|s| PathBuf::from(OsStr::from_bytes(s)))
                    .collect();
                let result = if global.is_none() {
                    Err(ferret_crawl::IndexError::BadRoot(PathBuf::new()))
                } else {
                    command_options.global = global;
                    let (change, refresh) = match command.request.op {
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
                        .index_change(change, refresh, &command_options)
                        .map_err(|error| match error {
                            ferret_crawl::IndexError::DeferredBulk(
                                ferret_catalog::bulk::Blocked::Memory,
                            ) => {
                                let status = scheduler.status();
                                ferret_crawl::IndexError::DeferredMemory {
                                    required: status.required_memory,
                                    available: status.sample.memory.unwrap_or(0),
                                }
                            }
                            other => other,
                        })
                };
                if let Ok(report) = &result {
                    global_inputs(watch.as_ref(), &engine, &context);
                    successful(host, report, false);
                    host.writer_status
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .retained_roots = report
                        .coverage_faults
                        .iter()
                        .map(|fault| fault.root.clone())
                        .collect();
                    if let Some(w) = &watch {
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
                        if let Some(w) = &watch {
                            w.backstop(RefreshReason::Backstop);
                        } else {
                            initial = true;
                            host.writer_status
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .fallback_backstop = true;
                        }
                    }
                    if let Some(w) = &watch {
                        w.abort_policy_roots();
                    }
                    host.writer_status
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .error = Some(error.to_string());
                    if matches!(
                        error,
                        ferret_crawl::IndexError::Update(_) | ferret_crawl::IndexError::Commit(_)
                    ) && let Some(w) = &watch
                    {
                        w.backstop(RefreshReason::Backstop);
                    }
                }
                // The reply is detached from socket backpressure. Publication
                // and watch adoption finish before the next command begins.
                let _ = command.reply.send(result);
                host.writer_pending.fetch_sub(1, Ordering::AcqRel);
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
                let roots = w.polling_roots(pin.catalog());
                let retained = host
                    .writer_status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retained_roots
                    .clone();
                let scopes = roots
                    .into_iter()
                    .chain(retained)
                    .collect::<std::collections::BTreeSet<_>>();
                w.scoped_roots(scopes.into_iter().collect());
            } else {
                initial = true;
                host.writer_status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .fallback_backstop = true;
            }
            poll_due = Instant::now() + polling;
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
                content_due = Instant::now() + content_turn(host, &engine, &scheduler)?;
            }
            continue;
        }
        if paused.is_none() {
            host.writer_status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .blocked = None;
        }
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
                host.writer_status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retained_roots = report
                    .report
                    .coverage_faults
                    .iter()
                    .map(|fault| fault.root.clone())
                    .collect();
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
    Ok(())
}
// Each idle turn is bounded by bytes, postings scratch and elapsed time. A
// queued explicit command or admitted watch burst interrupts at the next
// document/term.
fn content_turn(
    host: &Host,
    engine: &Engine,
    scheduler: &crate::scheduler::Scheduler,
) -> io::Result<Duration> {
    use ferret_catalog::bulk::Control;
    let started = Instant::now();
    let should_interrupt = || {
        host.stop.load(Ordering::Acquire)
            || host.writer_pending.load(Ordering::Acquire) != 0
            || scheduler.status().paused.is_some()
            || host
                .writer_status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .watch
                .as_ref()
                .and_then(|w| w.next_due())
                .is_some_and(|due| due <= Instant::now())
            || host
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .draining
    };
    #[cfg(debug_assertions)]
    let checks = std::cell::Cell::new(0u64);
    let interrupted = || {
        #[cfg(debug_assertions)]
        {
            checks.set(checks.get() + 1);
            content_checkpoint(host, checks.get(), &should_interrupt);
        }
        should_interrupt()
    };
    let cancelled = || interrupted() || started.elapsed() >= Duration::from_millis(100);
    if cancelled() {
        return Ok(Duration::from_secs(1));
    }
    let limiter = scheduler.limiter();
    let pace = |bytes: usize| {
        for chunk in (0..bytes).step_by(64 << 10) {
            limiter.transfer((bytes - chunk).min(64 << 10), || Ok(()))?;
        }
        Ok(())
    };
    let budget = ferret_index::Budget {
        bytes: 256 << 10,
        buffer: 1 << 20,
        pace: &pace,
        cancelled: &cancelled,
    };
    let operation = Operation::start(host, "follow");
    let followed = engine
        .follow_content(&budget, Some(limiter.clone()))
        .map_err(io::Error::other)?;
    drop(operation);
    if cancelled() || followed.remaining > 0 {
        return Ok(Duration::ZERO);
    }
    let pin = engine.pin();
    let Some((memory, disk)) = pin.content().and_then(|v| v.merge_resources(pin.live())) else {
        return Ok(Duration::from_secs(3600));
    };
    if let Err(reason) = scheduler.admit_merge(pin.catalog(), memory, disk) {
        host.writer_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .blocked = Some(reason);
        return Ok(Duration::from_secs(1));
    }
    let _operation = Operation::start(host, "merge");
    // Merge input size is admitted separately; cooperative cancellation is
    // checked during streaming and before cutover, never during publication.
    let budget = ferret_index::Budget {
        bytes: u64::MAX,
        cancelled: &interrupted,
        ..budget
    };
    match engine.merge_content(&budget) {
        Ok(Some(_)) => Ok(Duration::ZERO),
        Ok(None) => Ok(Duration::from_secs(3600)),
        Err(crate::engine::Error::Content(ferret_index::Error::Cancelled)) => Ok(Duration::ZERO),
        Err(error) => Err(io::Error::other(error)),
    }
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
    status.fault_retained = !report.coverage_faults.is_empty();
    status.error = None;
    status.blocked = None;
    status.fallback_backstop = false;
}
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
        .try_send(Message::Command(Command {
            request: request.clone(),
            reply,
        }))
        .is_err()
    {
        host.writer_pending.fetch_sub(1, Ordering::AcqRel);
        emit_to(destination, &request.id, "begin", |o| {
            o.null("generation");
        })?;
        return emit_to(destination, &request.id, "end", |o| {
            o.int("exit", 3).str("error", "WriterUnavailable");
        });
    }
    let started = Instant::now();
    let result = match receive.recv() {
        Ok(result) => result,
        Err(_) => {
            emit_to(destination, &request.id, "begin", |o| {
                o.null("generation");
            })?;
            return emit_to(destination, &request.id, "end", |o| {
                o.int("exit", 3).str("error", "WriterUnavailable").str(
                    "message",
                    "writer failed; inspect ferret status --json and restart ferretd",
                );
            });
        }
    };
    let rendered = crate::index::render(&result);
    let session = super::pin(host)?;
    emit_to(destination, &request.id, "begin", |o| {
        generation(o, Some(session.generation()));
    })?;
    for (event, text) in [
        ("stdout", &rendered.stdout),
        ("stderr", &rendered.diagnostics),
    ] {
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
        .int("exit", rendered.exit as u8)
        .int("total_us", started.elapsed().as_micros() as i128);
    if let Ok(report) = &result {
        crate::index::log_report(&mut object, report);
    }
    object.end();
    emit_to(destination, &request.id, "log", |o| {
        o.bytes("bytes", &log);
    })?;
    emit_to(destination, &request.id, "end", |o| {
        o.int("exit", rendered.exit as u8)
            .str("outcome", rendered.outcome);
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
        .bool("writer_busy", host.writer_running.load(Ordering::Acquire))
        .bool("fault_retained", s.fault_retained);
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

/// Deterministic integration barrier on production cancellation checkpoints.
/// A command, battery transition or drain releases it through the real
/// predicate.
#[cfg(debug_assertions)]
fn content_checkpoint(host: &Host, checks: u64, interrupted: &dyn Fn() -> bool) {
    let Some(gate) = std::env::var_os("FERRET_CONTENT_TEST_GATE").map(PathBuf::from) else {
        return;
    };
    let phase = host
        .writer_status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .operation;
    let Some(phase @ ("follow" | "merge")) = phase else {
        return;
    };
    let held = gate.join(phase);
    if !held.exists() || checks < 10 {
        return;
    }
    if phase == "merge" {
        let written = std::fs::read_dir(host.index.join(crate::engine::CONTENT_DIR))
            .ok()
            .is_some_and(|entries| {
                entries.filter_map(Result::ok).any(|entry| {
                    entry.file_name().to_string_lossy().starts_with("tmp-")
                        && entry.path().extension().is_some_and(|e| e == "seg")
                        && entry.metadata().is_ok_and(|m| m.len() > 0)
                })
            });
        if !written {
            return;
        }
    }
    let _ = std::fs::write(gate.join(format!("{phase}.reached")), b"checkpoint");
    while held.exists() && !interrupted() {
        std::thread::sleep(Duration::from_millis(1));
    }
}
