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
    pub retained_roots: std::collections::BTreeSet<PathBuf>,
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
    std::thread::spawn(move || {
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| serve(&host, receive)));
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
        host.writer_running.store(false, Ordering::Release);
    })
}
fn serve(host: &Arc<Host>, receive: mpsc::Receiver<Message>) -> io::Result<()> {
    #[cfg(debug_assertions)]
    std::thread::sleep(duration("FERRET_DAEMON_LOAD_DELAY_MS", 0));
    let session = WriterSession::open(&host.index).map_err(io::Error::other)?;
    let engine = Arc::new(Engine::from_writer(session));
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
    if let Some(watch) = &watch {
        let watch = watch.clone();
        let send = host.writer_send.clone();
        std::thread::spawn(move || {
            watch.run_intake(|| {
                let _ = send.try_send(Message::Intake);
            })
        });
    }
    let context = crate::cli::Context {
        index: host.index.clone(),
        dirs: crate::xdg::Dirs::from_env().ok(),
    };
    let mut options = IndexOptions {
        watch: watch.clone(),
        ..IndexOptions::default()
    };
    let hourly = duration("FERRET_BACKSTOP_MS", 60 * 60 * 1000).max(Duration::from_millis(1));
    let polling = duration("FERRET_POLL_MS", 5 * 60 * 1000).max(Duration::from_millis(1));
    let mut full_due = Instant::now() + hourly;
    let mut poll_due = Instant::now() + polling;
    let mut retry_due = None;
    let mut initial = watch.is_none();
    loop {
        if host.stop.load(Ordering::Acquire) {
            break;
        }
        let now = Instant::now();
        let mut deadline = full_due.min(poll_due);
        if let Some(retry) = retry_due {
            deadline = deadline.min(retry);
        }
        if let Some(due) = watch.as_ref().and_then(|w| w.next_due()) {
            deadline = deadline.min(due);
        }
        let message = receive.recv_timeout(deadline.saturating_duration_since(now));
        match message {
            Ok(Message::Command(command)) => {
                host.writer_status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .operation = Some(if command.request.op == Op::Index {
                    "index"
                } else {
                    "roots-remove"
                });
                host.writer_running.store(true, Ordering::Release);
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
                    options.global = global;
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
                    engine.index_change(change, refresh, &options)
                };
                if let Ok(report) = &result {
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
                host.writer_status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .operation = None;
                host.writer_running.store(false, Ordering::Release);
                super::wake_listener(host, 1);
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
            }
            poll_due = Instant::now() + polling;
        }
        if retry_due.is_some_and(|retry| Instant::now() < retry) {
            continue;
        }
        let burst = watch.as_ref().and_then(|w| w.take());
        if burst.is_none() && !initial {
            continue;
        }
        host.writer_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .operation = Some("refresh");
        host.writer_running.store(true, Ordering::Release);
        #[cfg(debug_assertions)]
        std::thread::sleep(duration("FERRET_WATCH_TEST_REFRESH_DELAY_MS", 0));
        {
            let pin = engine.pin();
            if let (Some(w), Some(dirs)) = (&watch, &context.dirs) {
                for (_, root) in pin.catalog().roots() {
                    w.policy_path(
                        std::path::Path::new(OsStr::from_bytes(root)),
                        &dirs.ignore_file(),
                    );
                    w.policy_path(
                        std::path::Path::new(OsStr::from_bytes(root)),
                        &dirs.config.join("config"),
                    );
                }
            }
        }
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
        match &result {
            Ok(report) => {
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
            }
            Err(error) => {
                host.writer_status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .error = Some(error.to_string());
                retry_due = Some(Instant::now() + Duration::from_secs(1));
            }
        }
        if let Some(burst) = burst
            && let Some(watch) = &watch
        {
            watch.finish(burst, result.is_ok());
        }
        host.writer_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .operation = None;
        host.writer_running.store(false, Ordering::Release);
        super::wake_listener(host, 1);
    }
    Ok(())
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
}
pub(super) fn busy(host: &Host) -> bool {
    host.writer_running.load(Ordering::Acquire)
        || host.writer_pending.load(Ordering::Acquire) != 0
        || host
            .writer_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .watch
            .as_ref()
            .is_some_and(|w| w.status().busy)
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
            .int("pending_scopes", 0)
            .int("pending_bytes", 0)
            .null("polling_roots")
            .null("oldest_pending_ms")
            .null("backstop_reason");
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
    if let Some(error) = &s.error {
        o.str("refresh_error", error);
    }
}
