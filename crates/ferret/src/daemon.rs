//! Socket lifecycle and retained writer ownership. Endpoint security lives in
//! `endpoint`, ordinary CLI fallback/rendering in `client`; query events use
//! batch/find_json unchanged. No query holds the engine-selection mutex during
//! evaluation or socket writes.

#[cfg(panic = "abort")]
compile_error!("ferretd requires panic unwinding for query isolation");

mod client;
mod endpoint;
mod writer;
pub(crate) use client::{
    daemon_stats as stats_json, daemon_stats_text as stats_text, daemon_status as status, find,
    grep, search, spawn_resident, writer_command as write,
};

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufReader, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
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
    connections: std::collections::BTreeMap<usize, mpsc::SyncSender<ClientMessage>>,
    next_connection: usize,
}
enum Loaded {
    Loading,
    Ready(Arc<Engine>),
    Failed(String),
}
struct Host {
    engine: Mutex<Loaded>,
    engine_ready: Condvar,
    lifecycle: Mutex<Lifecycle>,
    lifecycle_changed: Condvar,
    clients: AtomicUsize,
    index: PathBuf,
    server_send: mpsc::Sender<ServerEvent>,
    identity: String,
    context: String,
    build: String,
    query_limit: usize,
    permits: Permits,
    format: u64,
    writer_send: mpsc::SyncSender<writer::Message>,
    writer_status: Mutex<writer::Status>,
    writer_running: AtomicBool,
    writer_pending: AtomicUsize,
    /// The writer service ended after the engine was ready. The host drains
    /// and exits rather than serve a catalog nothing refreshes.
    writer_failed: AtomicBool,
    stop: AtomicBool,
}
enum ServerEvent {
    Accepted(io::Result<UnixStream>),
    Wake,
}
enum ClientMessage {
    Line(Vec<u8>),
    Wake,
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

// Linux errno. `ErrorKind` maps EMFILE, ENFILE, ENOBUFS and EPROTO to
// Uncategorized, the same bucket as fatal accept failures such as EBADF.
const EINTR: i32 = 4;
const ENOMEM: i32 = 12;
const ENFILE: i32 = 23;
const EMFILE: i32 = 24;
const EPROTO: i32 = 71;
const ECONNABORTED: i32 = 103;
const ENOBUFS: i32 = 105;
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

// EPROTO matches ECONNABORTED: Linux has already dropped one bad queued
// connection, and one handshake must not take the daemon down with it.
fn transient_accept(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(EINTR | ENOMEM | ENFILE | EMFILE | EPROTO | ECONNABORTED | ENOBUFS)
    )
}
fn next_accepted(listener: &UnixListener) -> io::Result<UnixStream> {
    if let Some(error) = take_test_accept_error() {
        return Err(error);
    }
    listener.accept().map(|(stream, _)| stream)
}
fn take_test_accept_error() -> Option<io::Error> {
    // The value is a path. The accept thread injects one error when that file
    // holds an errno name, then removes it. Release builds have no hook.
    #[cfg(debug_assertions)]
    {
        let path = std::env::var_os("FERRET_DAEMON_TEST_ACCEPT_ERROR")?;
        let text = fs::read_to_string(&path).ok()?;
        let code = test_accept_errno(text.trim())?;
        let _ = fs::remove_file(&path);
        Some(io::Error::from_raw_os_error(code))
    }
    #[cfg(not(debug_assertions))]
    None
}
#[cfg(debug_assertions)]
fn test_accept_errno(name: &str) -> Option<i32> {
    Some(match name {
        "EINTR" => EINTR,
        "ENOMEM" => ENOMEM,
        "ENFILE" => ENFILE,
        "EMFILE" => EMFILE,
        "EPROTO" => EPROTO,
        "ECONNABORTED" => ECONNABORTED,
        "ENOBUFS" => ENOBUFS,
        "EBADF" => 9,
        _ => return None,
    })
}
fn run(args: impl Iterator<Item = OsString>) -> io::Result<()> {
    let mut index = std::env::var_os("FERRET_INDEX")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::xdg::Dirs::from_env().ok().map(|dirs| dirs.data));
    // Zero: stay up and keep the index fresh between bursts of use (D116).
    let mut idle = duration("FERRET_DAEMON_IDLE_MS", 0);
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
    let index = index.ok_or_else(|| io::Error::other("no index directory"))?;
    // The endpoint is named after the directory's identity, so a daemon
    // started before the first index creates it, as the writer would.
    fs::create_dir_all(&index)?;
    let index = fs::canonicalize(index)?;
    let endpoint = Endpoint::open(&index)?;
    let lock = endpoint.private_file("lock")?;
    let until = Instant::now() + duration("FERRET_DAEMON_STARTUP_MS", 10_000);
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) => {
                if owner_responds(&endpoint.socket()) {
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
    let socket = endpoint.socket();
    let listener = UnixListener::bind(&socket)?;
    let own_socket = fs::symlink_metadata(endpoint.socket())?;
    let cleanup = SocketCleanup(&endpoint, own_socket.dev(), own_socket.ino());
    fs::set_permissions(endpoint.socket(), fs::Permissions::from_mode(0o600))?;
    let (server_send, server_receive) = mpsc::channel();
    let accept_send = server_send.clone();
    let accept_lock = lock.try_clone()?;
    std::thread::spawn(move || {
        let _lock = &accept_lock;
        // One line per streak. A persistent EMFILE would otherwise fill stderr.
        let mut reported = false;
        loop {
            let accepted = match next_accepted(&listener) {
                Err(error) if transient_accept(&error) => {
                    if !reported {
                        crate::cli::warn(&format!("accept: {error}; continuing"));
                        reported = true;
                    }
                    // EMFILE and ENFILE leave the connection queued. Retrying
                    // immediately pegs a core until a descriptor frees.
                    if matches!(error.raw_os_error(), Some(ENFILE | EMFILE)) {
                        std::thread::sleep(ACCEPT_BACKOFF);
                    }
                    continue;
                }
                other => {
                    reported = false;
                    other
                }
            };
            if accept_send.send(ServerEvent::Accepted(accepted)).is_err() {
                break;
            }
        }
    });
    let (writer_send, writer_receive) = mpsc::sync_channel(32);
    let host = Arc::new(Host {
        engine: Mutex::new(Loaded::Loading),
        engine_ready: Condvar::new(),
        lifecycle: Mutex::new(Lifecycle {
            draining: false,
            queries: 0,
            connections: std::collections::BTreeMap::new(),
            next_connection: 0,
        }),
        lifecycle_changed: Condvar::new(),
        clients: AtomicUsize::new(0),
        index,
        server_send,
        identity: endpoint.identity.clone(),
        context: advertised_context()?,
        build: test_build(),
        query_limit: ferret_query::line_grep::IoBudget::request_limit(
            QUERIES.min(ferret_crawl::default_workers()),
        ),
        format: advertised_format(),
        writer_send,
        writer_status: Mutex::new(writer::Status::default()),
        writer_running: AtomicBool::new(false),
        writer_pending: AtomicUsize::new(0),
        writer_failed: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        permits: Permits::new(ferret_crawl::default_workers()),
    });
    let writer = writer::start(host.clone(), writer_receive)?;
    let mut idle_since = Instant::now();
    // Every exit, including a fatal accept and a dead accept thread, joins
    // the writer before the socket is removed (D110).
    let mut exit_error = None;
    loop {
        let draining = host
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .draining;
        let clients = host.clients.load(Ordering::Acquire);
        // A dead writer's pending count can be stale, so a failed writer
        // waits only for connections, which its drain has already woken.
        if host.writer_failed.load(Ordering::Acquire) && clients == 0 {
            exit_error = Some(io::Error::other("writer stopped"));
            break;
        }
        let busy = writer::busy(&host);
        if clients != 0 {
            idle_since = Instant::now();
        }
        if (draining && clients == 0 && !busy)
            || (!idle.is_zero() && clients == 0 && !busy && idle_since.elapsed() >= idle)
        {
            break;
        }
        // Once work blocks an expired deadline, its completion notification
        // supplies the next wakeup; a zero timeout would spin.
        let event = if idle.is_zero() || clients != 0 || (busy && idle_since.elapsed() >= idle) {
            match server_receive.recv() {
                Ok(event) => event,
                Err(mpsc::RecvError) => {
                    exit_error = Some(io::Error::other("accept loop stopped"));
                    break;
                }
            }
        } else {
            match server_receive.recv_timeout(idle.saturating_sub(idle_since.elapsed())) {
                Ok(event) => event,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    exit_error = Some(io::Error::other("accept loop stopped"));
                    break;
                }
            }
        };
        match event {
            ServerEvent::Wake => {}
            ServerEvent::Accepted(Ok(stream)) if !draining && clients < CLIENTS => {
                host.clients.fetch_add(1, Ordering::AcqRel);
                let host = host.clone();
                std::thread::spawn(move || {
                    let _client = ClientCount(&host);
                    let _ = connection(stream, &host);
                });
            }
            ServerEvent::Accepted(Ok(_)) => {}
            ServerEvent::Accepted(Err(error)) => {
                exit_error = Some(error);
                break;
            }
        }
    }
    host.stop.store(true, Ordering::Release);
    let _ = host.writer_send.try_send(writer::Message::Intake);
    let _ = writer.join();
    if let Loaded::Ready(engine) = &*host
        .engine
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        engine.close_writer();
    }
    // The lifetime-held endpoint lock excludes starters through cleanup. Never
    // unlink a replacement created by somebody else after our socket vanished.
    let removed = if let Ok(current) = fs::symlink_metadata(endpoint.socket())
        && current.dev() == own_socket.dev()
        && current.ino() == own_socket.ino()
    {
        endpoint.remove_socket()
    } else {
        Ok(())
    };
    drop(cleanup);
    drop(lock);
    // The accept error is why this host left. A removal failure still runs
    // through SocketCleanup above; surfacing it instead would hide the cause.
    if let Some(error) = exit_error {
        return Err(error);
    }
    removed?;
    Ok(())
}
fn owner_responds(socket: &Path) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket) else {
        return false;
    };
    if stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .is_err()
    {
        return false;
    }
    let mut byte = [0];
    matches!(stream.read(&mut byte), Ok(1))
}
fn wake_listener(host: &Host) {
    let _ = host.server_send.send(ServerEvent::Wake);
}
fn cancel(host: &Host, cancelled: &AtomicBool) {
    let _engine = host
        .engine
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _lifecycle = host
        .lifecycle
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cancelled.store(true, Ordering::Release);
    host.engine_ready.notify_all();
    host.lifecycle_changed.notify_all();
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
    // Check the endpoint's index directory before advertising a checked pin.
    // Publication recovery never invalidates the last checked query view.
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
                [
                    b"query-only".as_slice(),
                    b"cancel",
                    b"drain",
                    b"writer",
                    b"raw-output-v1",
                ],
            )
            .object("limits", |o| {
                o.int("line", protocol::MAX_LINE_BYTES as u64)
                    .int("argv", protocol::MAX_ARGV_ELEMENTS as u64)
                    .int("part", 64 * 1024)
                    .int("queries", host.query_limit as u64)
                    .int("workers", host.permits.budget as u64);
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
        self.0.lifecycle_changed.notify_all();
        wake_listener(self.0);
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
        self.0.lifecycle_changed.notify_one();
    }
}

/// One pool of query-worker permits, `min(16, CPUs)` via
/// [`ferret_crawl::default_workers`]. Find, search and grep all draw from it.
/// Admission ([`QUERIES`]) is a separate limit.
struct Permits {
    budget: usize,
    state: Mutex<PermitState>,
}
struct PermitState {
    free: usize,
    /// Parallelism of each in-flight grant, in the order it was taken.
    running: Vec<usize>,
}
/// One query's permits. Drop returns them after that query's workers join.
struct Grant<'a> {
    permits: &'a Permits,
    /// Returned to the pool. Zero when the pool was empty and the query kept
    /// its connection thread without charging past the budget.
    charged: usize,
    workers: usize,
}
impl Permits {
    fn new(budget: usize) -> Self {
        let budget = budget.max(1);
        Self {
            budget,
            state: Mutex::new(PermitState {
                free: budget,
                running: Vec::new(),
            }),
        }
    }

    /// Grants `min(asked, free)` workers, and never fewer than one. Does not
    /// wait. An empty pool still grants the connection thread; that worker is
    /// not charged, so the charged total stays inside the budget. Waiting
    /// could deadlock: an admitted query blocked on a permit another query
    /// holds, while that query is blocked on this connection.
    fn take(&self, asked: usize) -> Grant<'_> {
        let asked = asked.max(1);
        let mut state = self.lock();
        let charged = asked.min(state.free);
        let workers = charged.max(1);
        state.free -= charged;
        state.running.push(workers);
        Grant {
            permits: self,
            charged,
            workers,
        }
    }

    /// Charged permits, and the parallelism of each in-flight grant.
    fn snapshot(&self) -> (usize, Vec<usize>) {
        let state = self.lock();
        (self.budget - state.free, state.running.clone())
    }

    fn lock(&self) -> MutexGuard<'_, PermitState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
impl Grant<'_> {
    fn workers(&self) -> usize {
        self.workers
    }
}
impl Drop for Grant<'_> {
    fn drop(&mut self) {
        let mut state = self.permits.lock();
        state.free += self.charged;
        if let Some(index) = state
            .running
            .iter()
            .position(|&workers| workers == self.workers)
        {
            state.running.swap_remove(index);
        }
        self.charged = 0;
        debug_assert!(state.free <= self.permits.budget);
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
        state = host
            .lifecycle_changed
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(state);
    }
}
/// Selects the ready engine and pins its current generation, after checking
/// that the index directory is still the one this host serves.
///
/// The identity check stats the index directory, which can be slow. It runs
/// on a clone of the selected engine with the selection mutex released, so
/// only this request waits (D32). Selection never leaves `Ready`, and
/// admission and drain do not consult the engine, so the clone admits
/// nothing the held guard would have refused.
fn pin(host: &Host) -> io::Result<QuerySession> {
    let engine = match &*host
        .engine
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        Loaded::Ready(engine) => engine.clone(),
        Loaded::Loading | Loaded::Failed(_) => {
            return Err(io::Error::other("engine is not ready"));
        }
    };
    #[cfg(debug_assertions)]
    identity_checkpoint();
    let identity = fs::metadata(&host.index)?;
    if format!("{:x}-{:x}", identity.dev(), identity.ino()) != host.identity {
        return Err(io::Error::other("index directory identity changed"));
    }
    Ok(engine.pin())
}
/// Holds one selection at the identity check, for a test that needs a request
/// parked there. `FERRET_DAEMON_TEST_PIN_GATE` names a directory; the first
/// selection to rename its `hold` file to `held` writes `reached` and waits
/// until the test removes `held`. Every other selection passes straight
/// through, so the test can drive a second request past the parked one.
#[cfg(debug_assertions)]
fn identity_checkpoint() {
    let Some(gate) = std::env::var_os("FERRET_DAEMON_TEST_PIN_GATE").map(PathBuf::from) else {
        return;
    };
    let held = gate.join("held");
    if fs::rename(gate.join("hold"), &held).is_err() {
        return;
    }
    let _ = fs::write(gate.join("reached"), b"reached");
    while held.exists() {
        std::thread::sleep(Duration::from_millis(1));
    }
}
struct ConnectionRegistration<'a>(&'a Host, usize);
impl Drop for ConnectionRegistration<'_> {
    fn drop(&mut self) {
        unregister(self.0, self.1);
    }
}
fn unregister(host: &Host, id: usize) {
    host.lifecycle
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .connections
        .remove(&id);
}
fn drain(host: &Host) {
    // Match the engine -> lifecycle lock order used by cancellation and the
    // loading waiter, so drain cannot be lost between its check and wait.
    let _loaded = host
        .engine
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut state = host
        .lifecycle
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.draining = true;
    for send in state.connections.values() {
        let _ = send.try_send(ClientMessage::Wake);
    }
    host.engine_ready.notify_all();
    host.lifecycle_changed.notify_all();
    wake_listener(host);
}
fn connection(stream: UnixStream, host: &Arc<Host>) -> io::Result<()> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let destination = Destination::socket(stream.try_clone()?, cancelled.clone())?;
    let (send, receive) = mpsc::sync_channel(1);
    let registration = {
        let mut state = host
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.draining {
            return Ok(());
        }
        let id = state.next_connection;
        state.next_connection += 1;
        state.connections.insert(id, send.clone());
        ConnectionRegistration(host, id)
    };
    let id = registration.1;
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
                    cancel(&reader_host, &reader_cancel);
                    let _ = reader.get_ref().shutdown(std::net::Shutdown::Write);
                    break;
                }
                Some("drain") => {
                    drain(&reader_host);
                }
                _ => {
                    if send.try_send(ClientMessage::Line(line.clone())).is_err() {
                        break;
                    }
                }
            }
        }
        cancel(&reader_host, &reader_cancel);
        // Release the drain registry's sender with this one, so the handler's
        // channel disconnects once it has taken any queued line. A wake alone
        // is lost when that line fills the channel, and the handler would
        // then answer it and block forever.
        unregister(&reader_host, id);
        drop(send);
        // Widens the window in which the handler can still answer a queued
        // line before the socket shuts down.
        #[cfg(debug_assertions)]
        std::thread::sleep(duration("FERRET_DAEMON_TEST_READER_EXIT_DELAY_MS", 0));
        // Wake a blocked writer on hangup/cancel; a partial frame is a broken
        // transport and cannot be followed by a fabricated successful end.
        let _ = reader.get_ref().shutdown(std::net::Shutdown::Both);
    });
    let result = (|| {
        if !hello(&destination, host)? {
            let mut loaded = host
                .engine
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while matches!(*loaded, Loaded::Loading)
                && !cancelled.load(Ordering::Acquire)
                && !host
                    .lifecycle
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .draining
            {
                loaded = host
                    .engine_ready
                    .wait(loaded)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if cancelled.load(Ordering::Acquire)
                || host
                    .lifecycle
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .draining
            {
                return Ok(());
            }
            drop(loaded);
            hello(&destination, host)?;
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
            let line = match receive.recv() {
                Ok(ClientMessage::Line(line)) => line,
                Ok(ClientMessage::Wake) => {
                    if cancelled.load(Ordering::Acquire)
                        || host
                            .lifecycle
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .draining
                    {
                        break;
                    }
                    continue;
                }
                Err(_) => break,
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
            let _permit = if matches!(request.op, Op::Search | Op::Find | Op::Grep) {
                let Some(permit) = admit(host, &cancelled) else {
                    break;
                };
                Some(permit)
            } else {
                None
            };
            let output = destination.clone().with_raw_output(
                matches!(request.op, Op::Find | Op::Grep | Op::Search)
                    && request
                        .capabilities
                        .iter()
                        .any(|capability| capability == "raw-output-v1"),
            );
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                execute(host, &request, &output)
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
    match request.op {
        Op::Find => 1,
        Op::Grep => 2,
        _ => 3,
    }
}
/// Human stats for `--stats-text`. The census is the pin's. The log budget
/// is the resident writer's, read after the pin: a commit in between can
/// move that line ahead of the census. The optional path is the client's
/// index display, so the header matches a local `ferret stats`.
///
/// The budget read waits for any writer operation in progress, which can be
/// a long refresh or follow. It runs on a clone of the selected engine with
/// the selection mutex released, so only this request waits (D32).
fn stats_report(host: &Host, request: &Request, session: &QuerySession) -> Option<String> {
    if request
        .args
        .first()
        .is_none_or(|arg| arg.as_slice() != b"--stats-text")
    {
        return None;
    }
    let engine = match &*host
        .engine
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        Loaded::Ready(engine) => engine.clone(),
        Loaded::Loading | Loaded::Failed(_) => return None,
    };
    // Lets a test hold the writer and know this request has reached the
    // budget read before it checks that other requests still run.
    #[cfg(debug_assertions)]
    if let Some(reached) = std::env::var_os("FERRET_DAEMON_TEST_STATS_REACHED") {
        let _ = fs::write(reached, b"reached");
    }
    let usage = engine.log_budget()?;
    let shown = request
        .args
        .get(1)
        .map(|bytes| PathBuf::from(OsStr::from_bytes(bytes)))
        .unwrap_or_else(|| host.index.clone());
    Some(crate::stats::text(&shown, &usage, session.catalog()))
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
        Op::Search => {
            // Dropped when this arm returns. `search_request` joins its
            // verification workers before then, including cancellation, client
            // disconnect and a broken socket write.
            let grant = host.permits.take(session.search_threads());
            let result = crate::batch::search_request(
                request,
                Some(session),
                None,
                destination,
                Some(grant.workers()),
            );
            drop(grant);
            result
        }
        Op::Grep => {
            // The client's thread count is only an upper bound. The resident
            // search-thread cap still applies, then the shared pool. `serve`
            // sizes every pool from the grant and joins those workers before
            // `grep_request` returns, including a broken stdout pipe.
            let asked = request
                .grep
                .as_ref()
                .map_or(1, |options| options.threads.min(session.search_threads()));
            let grant = host.permits.take(asked);
            let result = crate::batch::grep_request(
                request,
                Some(session),
                destination,
                host.query_limit,
                Some(grant.workers()),
            );
            drop(grant);
            result
        }
        Op::Find => {
            // Clear all action grants. Even a direct socket caller cannot ask
            // the daemon to execute, prompt, delete or open an output file.
            // Find has no job-count flag; the ask is the in-process default.
            // `find_request` joins the walk before it returns. A transport
            // error counts as cancellation and stops that walk first.
            let mut request = request.clone();
            request
                .capabilities
                .retain(|capability| capability == "test-find-panic");
            let grant = host.permits.take(ferret_crawl::default_workers());
            let result = crate::batch::find_request(
                &request,
                Path::new("/"),
                Some(session),
                true,
                destination,
                grant.workers(),
            );
            drop(grant);
            result
        }
        Op::Status | Op::Reload => {
            let report = if request.op == Op::Status {
                stats_report(host, request, &session)
            } else {
                None
            };
            emit_to(
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
                    let (held, running) = host.permits.snapshot();
                    o.int("worker_budget", host.permits.budget as u64)
                        .int("worker_permits", held as u64)
                        .integers("worker_grants", running.into_iter().map(|n| n as u64));
                    crate::status::resources(o);
                    crate::status::catalog_fields(o, &session);
                    let engine = host
                        .engine
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Loaded::Ready(engine) = &*engine {
                        o.integers("pinned_internal_epochs", engine.pinned_epochs());
                    }
                    if request.args == [b"--stats".to_vec()] {
                        crate::stats::json_fields(o, &session);
                    }
                    if let Some(report) = &report {
                        o.str("report", report);
                    }
                },
            )
        }
    }
}
