//! Socket lifecycle and retained writer ownership. Endpoint security lives in
//! `endpoint`, ordinary CLI fallback/rendering in `client`, query admission
//! and worker permits in `permits`, and a connection's requests in
//! `requests`; query events use batch/find_json unchanged. No query holds the
//! engine-selection mutex during evaluation or socket writes.

#[cfg(panic = "abort")]
compile_error!("ferretd requires panic unwinding for query isolation");

mod client;
mod endpoint;
mod permits;
mod requests;
mod writer;
pub(crate) use client::{
    daemon_stats as stats_json, daemon_stats_text as stats_text, daemon_status as status, find,
    grep, search, spawn_resident, writer_command as write,
};

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use endpoint::Endpoint;
use permits::{Permits, admit};
use requests::connection;

use crate::engine::{Engine, QuerySession};
use crate::find_json::{emit_to, generation};
use crate::lock;
use crate::protocol;
use crate::transport::Destination;

const BUILD: &str = env!("FERRET_BUILD");
const MAJOR: u64 = 1;
const MINOR: u64 = 1;
const FORMAT: u64 = ferret_catalog::FORMAT_VERSION as u64;
const QUERIES: usize = 4;
const CLIENTS: usize = 32;

#[derive(Default)]
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
    writer_send: writer::Queue,
    writer_status: Mutex<writer::Status>,
    writer_running: AtomicBool,
    writer_pending: AtomicUsize,
    /// Which service thread ended after the engine was ready: the writer or
    /// watch intake. The host drains and exits rather than serve a catalog
    /// nothing refreshes. Set through [`fatal`].
    fatal: Mutex<Option<&'static str>>,
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
    // While the process has one thread, growing the table costs nothing.
    ferret_query::reserve_descriptor_table();
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
    let (index, idle) = arguments(args)?;
    let index = index.ok_or_else(|| io::Error::other("no index directory"))?;
    // The endpoint is named after the directory's identity, so a daemon
    // started before the first index creates it, as the writer would.
    fs::create_dir_all(&index)?;
    let index = fs::canonicalize(index)?;
    let endpoint = Endpoint::open(&index)?;
    let endpoint_lock = endpoint.private_file("lock")?;
    if !win_singleton(&endpoint, &endpoint_lock)? {
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
    spawn_accept(listener, server_send.clone(), endpoint_lock.try_clone()?);
    let (writer_send, writer_receive) = writer::Queue::new(32);
    let host = Arc::new(Host {
        engine: Mutex::new(Loaded::Loading),
        engine_ready: Condvar::new(),
        lifecycle: Mutex::new(Lifecycle::default()),
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
        fatal: Mutex::new(None),
        stop: AtomicBool::new(false),
        permits: Permits::new(ferret_crawl::default_workers()),
    });
    let writer = writer::start(host.clone(), writer_receive)?;
    // Every exit, including a fatal accept and a dead accept thread, joins
    // the writer before the socket is removed (D110).
    let exit_error = serve(&host, &server_receive, idle);
    host.stop.store(true, Ordering::Release);
    host.writer_send.wake();
    let _ = writer.join();
    if let Loaded::Ready(engine) = &*lock(&host.engine) {
        engine.close_writer();
    }
    // The lifetime-held endpoint lock excludes starters through cleanup.
    let removed = remove_own_socket(&endpoint, &own_socket);
    drop(cleanup);
    drop(endpoint_lock);
    // The accept error is why this host left. A removal failure still runs
    // through SocketCleanup above; surfacing it instead would hide the cause.
    if let Some(error) = exit_error {
        return Err(error);
    }
    removed?;
    Ok(())
}
/// Removes the endpoint's socket if it is still `own`. Never unlink a
/// replacement created by somebody else after our socket vanished.
fn remove_own_socket(endpoint: &Endpoint, own: &fs::Metadata) -> io::Result<()> {
    if let Ok(current) = fs::symlink_metadata(endpoint.socket())
        && current.dev() == own.dev()
        && current.ino() == own.ino()
    {
        endpoint.remove_socket()
    } else {
        Ok(())
    }
}
/// The index directory and idle timeout, from the environment and then
/// `--index` and `--idle-ms`.
fn arguments(args: impl Iterator<Item = OsString>) -> io::Result<(Option<PathBuf>, Duration)> {
    let mut index = crate::xdg::index_dir(None, || {
        crate::xdg::Dirs::from_env().ok().map(|dirs| dirs.data)
    });
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
    Ok((index, idle))
}
/// Takes the endpoint's lifetime lock, or finds the host that holds it.
/// `false` when another host already answers on the socket.
fn win_singleton(endpoint: &Endpoint, endpoint_lock: &fs::File) -> io::Result<bool> {
    let until = Instant::now() + duration("FERRET_DAEMON_STARTUP_MS", 10_000);
    loop {
        match endpoint_lock.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) => {
                if owner_responds(&endpoint.socket()) {
                    return Ok(false);
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
    Ok(UnixStream::connect(endpoint.socket()).is_err())
}
/// Accepts connections for the life of the process, holding a clone of
/// the endpoint lock, and forwards each to the server loop.
fn spawn_accept(
    listener: UnixListener,
    accept_send: mpsc::Sender<ServerEvent>,
    accept_lock: fs::File,
) {
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
}
/// Hands accepted connections to client threads until the host drains,
/// idles out or fails, returning the failure.
fn serve(
    host: &Arc<Host>,
    server_receive: &mpsc::Receiver<ServerEvent>,
    idle: Duration,
) -> Option<io::Error> {
    let mut idle_since = Instant::now();
    loop {
        let draining = lock(&host.lifecycle).draining;
        let clients = host.clients.load(Ordering::Acquire);
        // A dead writer's pending count can be stale, so a failed service
        // waits only for connections, which its drain has already woken.
        let fatal = *lock(&host.fatal);
        if let Some(service) = fatal
            && clients == 0
        {
            return Some(io::Error::other(service));
        }
        let busy = writer::busy(host);
        if clients != 0 {
            idle_since = Instant::now();
        }
        if (draining && clients == 0 && !busy)
            || (!idle.is_zero() && clients == 0 && !busy && idle_since.elapsed() >= idle)
        {
            return None;
        }
        // Once work blocks an expired deadline, its completion notification
        // supplies the next wakeup; a zero timeout would spin.
        let event = if idle.is_zero() || clients != 0 || (busy && idle_since.elapsed() >= idle) {
            match server_receive.recv() {
                Ok(event) => event,
                Err(mpsc::RecvError) => {
                    return Some(io::Error::other("accept loop stopped"));
                }
            }
        } else {
            match server_receive.recv_timeout(idle.saturating_sub(idle_since.elapsed())) {
                Ok(event) => event,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Some(io::Error::other("accept loop stopped"));
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
                return Some(error);
            }
        }
    }
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
    let _engine = lock(&host.engine);
    let _lifecycle = lock(&host.lifecycle);
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
        let loaded = lock(&host.engine);
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
                    .int("workers", host.permits.budget() as u64);
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
/// Selects the ready engine and pins its current generation, after checking
/// that the index directory is still the one this host serves.
///
/// The identity check stats the index directory, which can be slow. It runs
/// on a clone of the selected engine with the selection mutex released, so
/// only this request waits (D32). Selection never leaves `Ready`, and
/// admission and drain do not consult the engine, so the clone admits
/// nothing the held guard would have refused.
fn pin(host: &Host) -> io::Result<QuerySession> {
    let engine = match &*lock(&host.engine) {
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
/// A service thread stopped after the engine was ready. Logs the cause and
/// drains, so the host exits 3 through the ordered teardown (D110) and the
/// next client starts a fresh daemon.
fn fatal(host: &Host, service: &'static str, cause: &dyn std::fmt::Display) {
    crate::cli::error(&format!("daemon: {service}: {cause}"));
    lock(&host.fatal).get_or_insert(service);
    drain(host);
}
fn drain(host: &Host) {
    // Match the engine -> lifecycle lock order used by cancellation and the
    // loading waiter, so drain cannot be lost between its check and wait.
    let _loaded = lock(&host.engine);
    let mut state = lock(&host.lifecycle);
    state.draining = true;
    for send in state.connections.values() {
        let _ = send.try_send(ClientMessage::Wake);
    }
    host.engine_ready.notify_all();
    host.lifecycle_changed.notify_all();
    wake_listener(host);
}
