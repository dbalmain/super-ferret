//! Socket lifecycle and retained writer ownership. Endpoint security lives in
//! `endpoint`, ordinary CLI fallback/rendering in `client`; query events use
//! batch/find_json unchanged. No query holds the engine-selection mutex during
//! evaluation or socket writes.

#[cfg(panic = "abort")]
compile_error!("ferretd requires panic unwinding for query isolation");

mod client;
mod endpoint;
mod writer;
pub(crate) use client::{daemon_status as status, find, search, writer_command as write};

use std::ffi::OsString;
use std::fs;
use std::io::{self, BufReader};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::engine::{Engine, QuerySession};
use crate::find_json::{emit_to, generation};
use crate::protocol::{self, Op, Request};
use crate::transport::Destination;
use endpoint::Endpoint;

const BUILD: &str = env!("FERRET_BUILD");
const MAJOR: u64 = 1;
const MINOR: u64 = 1;
const FORMAT: u64 = ferret_catalog::FORMAT_VERSION as u64;
const QUERIES: usize = 4;
const CLIENTS: usize = 32;

struct Lifecycle {
    draining: bool,
    queries: usize,
}
enum Loaded {
    Loading,
    Ready(Arc<Engine>),
    Failed(String),
}
struct Host {
    engine: Mutex<Loaded>,
    lifecycle: Mutex<Lifecycle>,
    clients: AtomicUsize,
    index: PathBuf,
    identity: String,
    context: String,
    build: String,
    query_limit: usize,
    workers: usize,
    format: u64,
    writer_send: mpsc::SyncSender<writer::Command>,
    writer_status: Mutex<writer::Status>,
    writer_running: AtomicBool,
    writer_pending: AtomicUsize,
    stop: AtomicBool,
}

/// Runs ferretd. An unusable endpoint or catalog is an operational error; a
/// singleton loser connects to the winning host and exits successfully.
pub fn main() -> ExitCode {
    match run(std::env::args_os().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            crate::cli::error(&format!("daemon: {error}"));
            ExitCode::from(3)
        }
    }
}
fn duration(name: &str, default: u64) -> Duration {
    Duration::from_millis(
        std::env::var(name)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default),
    )
}
fn run(args: impl Iterator<Item = OsString>) -> io::Result<()> {
    let mut index = std::env::var_os("FERRET_INDEX")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::xdg::Dirs::from_env().ok().map(|dirs| dirs.data));
    let mut idle = duration("FERRET_DAEMON_IDLE_MS", 15 * 60 * 1000);
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--index") => index = args.next().map(PathBuf::from),
            Some("--idle-ms") => {
                idle = Duration::from_millis(
                    args.next()
                        .and_then(|s| s.to_str().and_then(|s| s.parse().ok()))
                        .ok_or_else(|| io::Error::other("--idle-ms needs milliseconds"))?,
                )
            }
            _ => {
                return Err(io::Error::other(
                    "usage: ferretd [--index DIR] [--idle-ms N]",
                ));
            }
        }
    }
    let index = fs::canonicalize(index.ok_or_else(|| io::Error::other("no index directory"))?)?;
    let endpoint = Endpoint::open(&index)?;
    let lock = endpoint.private_file("lock")?;
    let until = Instant::now() + duration("FERRET_DAEMON_STARTUP_MS", 10_000);
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) => {
                if UnixStream::connect(endpoint.socket()).is_ok() {
                    return Ok(());
                }
                // A draining owner may have unlinked just before releasing
                // its lock. Retry acquisition so that starter can become the
                // new winner instead of waiting for a departed host to bind.
                if Instant::now() >= until {
                    return Err(io::Error::other("singleton winner did not bind"));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(fs::TryLockError::Error(error)) => return Err(error),
        }
    }
    if UnixStream::connect(endpoint.socket()).is_ok() {
        return Ok(());
    }
    match endpoint.remove_socket() {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(endpoint.socket())?;
    let own_socket = fs::symlink_metadata(endpoint.socket())?;
    let cleanup = SocketCleanup(&endpoint, own_socket.dev(), own_socket.ino());
    fs::set_permissions(endpoint.socket(), fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let (writer_send, writer_receive) = mpsc::sync_channel(32);
    let host = Arc::new(Host {
        engine: Mutex::new(Loaded::Loading),
        lifecycle: Mutex::new(Lifecycle {
            draining: false,
            queries: 0,
        }),
        clients: AtomicUsize::new(0),
        index,
        identity: endpoint.identity.clone(),
        context: advertised_context()?,
        build: test_build(),
        query_limit: QUERIES.min(ferret_crawl::default_workers()),
        format: advertised_format(),
        writer_send,
        writer_status: Mutex::new(writer::Status::default()),
        writer_running: AtomicBool::new(true),
        writer_pending: AtomicUsize::new(0),
        stop: AtomicBool::new(false),
        workers: ferret_crawl::default_workers().min(16)
            / QUERIES.min(ferret_crawl::default_workers()),
    });
    let writer = writer::start(host.clone(), writer_receive);
    let mut idle_since = Instant::now();
    loop {
        let draining = host
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .draining;
        let clients = host.clients.load(Ordering::Acquire);
        if clients != 0 || writer::busy(&host) {
            idle_since = Instant::now();
        }
        if (draining
            && clients == 0
            && !host.writer_running.load(Ordering::Acquire)
            && host.writer_pending.load(Ordering::Acquire) == 0)
            || (!idle.is_zero() && clients == 0 && idle_since.elapsed() >= idle)
        {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) if !draining && clients < CLIENTS => {
                host.clients.fetch_add(1, Ordering::AcqRel);
                let host = host.clone();
                std::thread::spawn(move || {
                    let _client = ClientCount(&host);
                    let _ = connection(stream, &host);
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => return Err(error),
        }
    }
    host.stop.store(true, Ordering::Release);
    let _ = writer.join();
    // The lifetime-held endpoint lock excludes starters through cleanup. Never
    // unlink a replacement created by somebody else after our socket vanished.
    if let Ok(current) = fs::symlink_metadata(endpoint.socket())
        && current.dev() == own_socket.dev()
        && current.ino() == own_socket.ino()
    {
        endpoint.remove_socket()?;
    }
    drop(cleanup);
    drop(listener);
    drop(lock);
    Ok(())
}
fn test_build() -> String {
    #[cfg(debug_assertions)]
    if let Ok(build) = std::env::var("FERRET_DAEMON_TEST_BUILD") {
        return build;
    }
    BUILD.to_owned()
}
fn advertised_context() -> io::Result<String> {
    #[cfg(debug_assertions)]
    if let Ok(context) = std::env::var("FERRET_DAEMON_TEST_CONTEXT") {
        return Ok(context);
    }
    endpoint::context()
}
fn advertised_format() -> u64 {
    #[cfg(debug_assertions)]
    if let Ok(format) = std::env::var("FERRET_DAEMON_TEST_FORMAT")
        && let Ok(format) = format.parse()
    {
        return format;
    }
    FORMAT
}
fn hello(destination: &Destination, host: &Host) -> io::Result<bool> {
    let (state, selected, error) = {
        let loaded = host
            .engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*loaded {
            Loaded::Loading => ("loading", None, None),
            Loaded::Ready(engine) => ("ready", Some(engine.generation()), None),
            Loaded::Failed(error) => ("failed", None, Some(error.clone())),
        }
    };
    // A replaced catalog incarnation must not strand clients on an old
    // hello forever. Refresh ready state before reporting its identity.
    let (state, selected, error) = if state == "ready" {
        let selected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pin(host)))
            .unwrap_or_else(|_| Err(io::Error::other("engine selection panicked")));
        match selected {
            Ok(session) => ("ready", Some(session.generation()), None),
            Err(error) => ("failed", None, Some(error.to_string())),
        }
    } else {
        (state, selected, error)
    };
    emit_to(destination, "hello", "hello", |o| {
        o.int("major", MAJOR)
            .int("minor", MINOR)
            .str("build", &host.build)
            .int("format", host.format)
            .str("index", &host.identity)
            .str("context", &host.context)
            .int("pid", std::process::id())
            .byte_strings(
                "capabilities",
                [b"query-only".as_slice(), b"cancel", b"drain", b"writer"],
            )
            .object("limits", |o| {
                o.int("line", protocol::MAX_LINE_BYTES as u64)
                    .int("argv", protocol::MAX_ARGV_ELEMENTS as u64)
                    .int("part", 64 * 1024)
                    .int("queries", host.query_limit as u64)
                    .int("workers", host.workers as u64);
            });
        o.str("state", state);
        generation(o, selected);
        if state == "loading" {
            o.str(
                "meaning",
                "endpoint bound; catalog validation is still running",
            );
        } else if state == "ready" {
            o.str("meaning", "checked resident engine can admit queries");
        }
        if let Some(error) = error.as_deref() {
            o.str("error", error);
        }
    })?;
    Ok(state != "loading")
}

// Created after binding and before fallible setup; dropped before the
// lifetime-held endpoint lock, including on fatal startup/accept errors.
struct SocketCleanup<'a>(&'a Endpoint, u64, u64);
impl Drop for SocketCleanup<'_> {
    fn drop(&mut self) {
        if let Ok(current) = fs::symlink_metadata(self.0.socket())
            && current.dev() == self.1
            && current.ino() == self.2
        {
            let _ = self.0.remove_socket();
        }
    }
}
struct ClientCount<'a>(&'a Host);
impl Drop for ClientCount<'_> {
    fn drop(&mut self) {
        self.0.clients.fetch_sub(1, Ordering::AcqRel);
    }
}
struct QueryPermit<'a>(&'a Host);
impl Drop for QueryPermit<'_> {
    fn drop(&mut self) {
        self.0
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queries -= 1;
    }
}
fn admit<'a>(host: &'a Host, cancelled: &AtomicBool) -> Option<QueryPermit<'a>> {
    loop {
        let mut state = host
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.draining || cancelled.load(Ordering::Acquire) {
            return None;
        }
        if state.queries < host.query_limit {
            state.queries += 1;
            return Some(QueryPermit(host));
        }
        drop(state);
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn pin(host: &Host) -> io::Result<QuerySession> {
    let mut state = host
        .engine
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Loaded::Ready(engine) = &mut *state else {
        return Err(io::Error::other("engine is not ready"));
    };
    let identity = fs::metadata(&host.index)?;
    if format!("{:x}-{:x}", identity.dev(), identity.ino()) != host.identity {
        return Err(io::Error::other("index directory identity changed"));
    }
    Ok(engine.pin())
}
fn connection(stream: UnixStream, host: &Arc<Host>) -> io::Result<()> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let destination = Destination::Socket {
        writer: Arc::new(Mutex::new(stream.try_clone()?)),
        cancelled: cancelled.clone(),
    };
    let (send, receive) = mpsc::sync_channel(1);
    let input = stream.try_clone()?;
    let reader_host = host.clone();
    let reader_cancel = cancelled.clone();
    let reader = std::thread::spawn(move || {
        let mut reader = BufReader::new(input);
        let mut line = Vec::new();
        loop {
            line.clear();
            match crate::batch::read_line_bounded(&mut reader, &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let op = protocol::parse_object(&line).and_then(|value| {
                value
                    .field("op")
                    .and_then(protocol::Value::text)
                    .map(str::to_owned)
            });
            match op.as_deref() {
                Some("cancel") => {
                    reader_cancel.store(true, Ordering::Release);
                    let _ = reader.get_ref().shutdown(std::net::Shutdown::Write);
                    break;
                }
                Some("drain") => {
                    reader_host
                        .lifecycle
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .draining = true;
                }
                _ => {
                    if send.try_send(line.clone()).is_err() {
                        break;
                    }
                }
            }
        }
        reader_cancel.store(true, Ordering::Release);
        // Wake a blocked writer on hangup/cancel; a partial frame is a broken
        // transport and cannot be followed by a fabricated successful end.
        let _ = reader.get_ref().shutdown(std::net::Shutdown::Both);
    });
    let result = (|| {
        if !hello(&destination, host)? {
            loop {
                if cancelled.load(Ordering::Acquire) {
                    return Ok(());
                }
                if !matches!(
                    *host
                        .engine
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                    Loaded::Loading
                ) {
                    hello(&destination, host)?;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        loop {
            if host
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .draining
            {
                break;
            }
            let line = match receive.recv_timeout(Duration::from_millis(20)) {
                Ok(line) => line,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if line.len() > protocol::MAX_LINE_BYTES {
                let id = protocol::recover_id(&line);
                crate::batch::request_error(&destination, id.as_deref(), "LineTooLong")?;
                continue;
            }
            let request = match protocol::parse_request(&line) {
                Ok(request) => request,
                Err(error) => {
                    crate::batch::request_error(
                        &destination,
                        error.id.as_deref(),
                        &error.kind.to_string(),
                    )?;
                    continue;
                }
            };
            let Some(_permit) = admit(host, &cancelled) else {
                break;
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                execute(host, &request, &destination)
            }));
            match result {
                Ok(result) => result?,
                Err(_) => emit_to(&destination, &request.id, "end", |o| {
                    o.int("exit", runtime_status(&request))
                        .bool("cancelled", false)
                        .str("error", "RuntimeError")
                        .str("message", "query panicked");
                })?,
            }
            if cancelled.load(Ordering::Acquire) {
                break;
            }
        }
        Ok(())
    })();
    drop(receive);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = reader.join();
    result
}
fn runtime_status(request: &Request) -> u8 {
    if request.op == Op::Find { 1 } else { 3 }
}
fn execute(host: &Host, request: &Request, destination: &Destination) -> io::Result<()> {
    if matches!(request.op, Op::Index | Op::RootsRemove) {
        return writer::execute(host, request, destination);
    }
    #[cfg(debug_assertions)]
    if request.op == Op::Status {
        let status = host
            .writer_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(watch) = &status.watch {
            if request
                .capabilities
                .iter()
                .any(|c| c == "test-watch-overflow")
            {
                watch.inject_overflow();
            }
            if request.capabilities.iter().any(|c| c == "test-watch-move")
                && request.args.len() == 4
            {
                use std::os::unix::ffi::OsStrExt;
                let path = Path::new(std::ffi::OsStr::from_bytes(&request.args[0]));
                let cookie = std::str::from_utf8(&request.args[2])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                watch.inject_move(path, &request.args[1], cookie, request.args[3] == b"from");
            }
        }
    }
    // Freshness preparation can itself panic before batch emits begin. Turn
    // that failure into an ordinary null-generation runtime-error block.
    let selected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pin(host)))
        .unwrap_or_else(|_| Err(io::Error::other("engine selection panicked")));
    let session = match selected {
        Ok(session) => session,
        Err(error) => {
            emit_to(destination, &request.id, "begin", |o| {
                o.null("generation");
            })?;
            return emit_to(destination, &request.id, "end", |o| {
                o.int("exit", runtime_status(request))
                    .bool("cancelled", false)
                    .str("error", "RuntimeError")
                    .str("message", &error.to_string());
            });
        }
    };
    match request.op {
        Op::Index | Op::RootsRemove => {
            unreachable!("writer requests routed before query selection")
        }
        Op::Search => crate::batch::search_request(request, Some(session), None, destination),
        Op::Find => {
            // Clear all action grants. Even a direct socket caller cannot ask
            // the daemon to execute, prompt, delete or open an output file.
            let mut request = request.clone();
            request
                .capabilities
                .retain(|capability| capability == "test-find-panic");
            crate::batch::find_request(
                &request,
                Path::new("/"),
                Some(session),
                true,
                destination,
                host.workers,
            )
        }
        Op::Status | Op::Reload => emit_to(
            destination,
            &request.id,
            if request.op == Op::Status {
                "status"
            } else {
                "reload"
            },
            |o| {
                generation(o, Some(session.generation()));
                o.int("engine_opens", Engine::open_count())
                    .int("bytes", session.resident_bytes());
                writer::fields(host, o);
            },
        ),
    }
}
