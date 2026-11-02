//! One client connection: reading requests, pinning a generation, and
//! answering each request on the selected engine.

use std::ffi::OsStr;
use std::io::BufReader;
use std::os::unix::ffi::OsStrExt;

use super::permits::admit;
use super::*;
use crate::protocol::{Op, Request};

struct ConnectionRegistration<'a>(&'a Host, usize);
impl Drop for ConnectionRegistration<'_> {
    fn drop(&mut self) {
        unregister(self.0, self.1);
    }
}
fn unregister(host: &Host, id: usize) {
    lock(&host.lifecycle).connections.remove(&id);
}
pub(super) fn connection(stream: UnixStream, host: &Arc<Host>) -> io::Result<()> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let destination = Destination::socket(stream.try_clone()?, cancelled.clone())?;
    let (send, receive) = mpsc::sync_channel(1);
    let registration = {
        let mut state = lock(&host.lifecycle);
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
    let reader = {
        let (host, cancelled) = (host.clone(), cancelled.clone());
        std::thread::spawn(move || read_requests(&host, &cancelled, input, send, id))
    };
    let result = answer_requests(host, &destination, &receive, &cancelled);
    drop(receive);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = reader.join();
    result
}
/// Reads a connection's request lines until hangup or `cancel`, handing
/// each to the handler; `cancel` and `drain` act at once.
fn read_requests(
    host: &Arc<Host>,
    cancelled: &AtomicBool,
    input: UnixStream,
    send: mpsc::SyncSender<ClientMessage>,
    id: usize,
) {
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
                cancel(host, cancelled);
                let _ = reader.get_ref().shutdown(std::net::Shutdown::Write);
                break;
            }
            Some("drain") => {
                drain(host);
            }
            _ => {
                if send.try_send(ClientMessage::Line(line.clone())).is_err() {
                    break;
                }
            }
        }
    }
    cancel(host, cancelled);
    // Release the drain registry's sender with this one, so the handler's
    // channel disconnects once it has taken any queued line. A wake alone
    // is lost when that line fills the channel, and the handler would
    // then answer it and block forever.
    unregister(host, id);
    drop(send);
    // Widens the window in which the handler can still answer a queued
    // line before the socket shuts down.
    #[cfg(debug_assertions)]
    std::thread::sleep(duration("FERRET_DAEMON_TEST_READER_EXIT_DELAY_MS", 0));
    // Wake a blocked writer on hangup/cancel; a partial frame is a broken
    // transport and cannot be followed by a fabricated successful end.
    let _ = reader.get_ref().shutdown(std::net::Shutdown::Both);
}

/// Greets the client, then answers its requests in order until it hangs
/// up, cancels or the daemon drains.
fn answer_requests(
    host: &Host,
    destination: &Destination,
    receive: &mpsc::Receiver<ClientMessage>,
    cancelled: &AtomicBool,
) -> io::Result<()> {
    if !greet(host, destination, cancelled)? {
        return Ok(());
    }
    loop {
        if lock(&host.lifecycle).draining {
            break;
        }
        let line = match receive.recv() {
            Ok(ClientMessage::Line(line)) => line,
            Ok(ClientMessage::Wake) => {
                if cancelled.load(Ordering::Acquire) || lock(&host.lifecycle).draining {
                    break;
                }
                continue;
            }
            Err(_) => break,
        };
        match answer(host, destination, cancelled, &line)? {
            Answer::Rejected => continue,
            Answer::Ran if cancelled.load(Ordering::Acquire) => break,
            Answer::Ran => {}
            Answer::Stop => break,
        }
    }
    Ok(())
}

/// Says hello, waiting for a loading engine when the first hello could
/// not; `false` when cancel or drain came first.
fn greet(host: &Host, destination: &Destination, cancelled: &AtomicBool) -> io::Result<bool> {
    if !hello(destination, host)? {
        let mut loaded = lock(&host.engine);
        while matches!(*loaded, Loaded::Loading)
            && !cancelled.load(Ordering::Acquire)
            && !lock(&host.lifecycle).draining
        {
            loaded = host
                .engine_ready
                .wait(loaded)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if cancelled.load(Ordering::Acquire) || lock(&host.lifecycle).draining {
            return Ok(false);
        }
        drop(loaded);
        hello(destination, host)?;
    }
    Ok(true)
}

/// How one request line ended, which decides whether the connection checks
/// for cancellation before reading the next.
enum Answer {
    /// Refused before running: too long or unparsable. The next line is read
    /// without a cancellation check.
    Rejected,
    /// Ran to its end block.
    Ran,
    /// Cancelled or draining before it was admitted.
    Stop,
}

/// Answers one request line.
fn answer(
    host: &Host,
    destination: &Destination,
    cancelled: &AtomicBool,
    line: &[u8],
) -> io::Result<Answer> {
    if line.len() > protocol::MAX_LINE_BYTES {
        let id = protocol::recover_id(line);
        crate::batch::request_error(destination, id.as_deref(), "LineTooLong")?;
        return Ok(Answer::Rejected);
    }
    let request = match protocol::parse_request(line) {
        Ok(request) => request,
        Err(error) => {
            crate::batch::request_error(destination, error.id.as_deref(), &error.kind.to_string())?;
            return Ok(Answer::Rejected);
        }
    };
    let _permit = if matches!(request.op, Op::Search | Op::Find | Op::Grep) {
        let Some(permit) = admit(host, cancelled) else {
            return Ok(Answer::Stop);
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
        Err(_) => emit_to(destination, &request.id, "end", |o| {
            o.int("exit", runtime_status(&request))
                .bool("cancelled", false)
                .str("error", "RuntimeError")
                .str("message", "query panicked");
        })?,
    }
    Ok(Answer::Ran)
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
    let engine = match &*lock(&host.engine) {
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
        inject_watch_faults(host, request);
    }
    let Some(session) = select(host, request, destination)? else {
        return Ok(());
    };
    match request.op {
        Op::Index | Op::RootsRemove => {
            unreachable!("writer requests routed before query selection")
        }
        Op::Search => serve_search(host, request, session, destination),
        Op::Grep => serve_grep(host, request, session, destination),
        Op::Find => serve_find(host, request, session, destination),
        Op::Status | Op::Reload => serve_report(host, request, &session, destination),
    }
}

/// Applies a debug build's `test-watch-*` capabilities to the live watch.
#[cfg(debug_assertions)]
fn inject_watch_faults(host: &Host, request: &Request) {
    let status = lock(&host.writer_status);
    if let Some(watch) = &status.watch {
        if request
            .capabilities
            .iter()
            .any(|c| c == "test-watch-overflow")
        {
            watch.inject_overflow();
        }
        for (capability, panic) in [
            ("test-watch-intake-panic", true),
            ("test-watch-intake-read-error", false),
        ] {
            if request.capabilities.iter().any(|c| c == capability) {
                watch.inject_intake_fault(panic);
            }
        }
        if request.capabilities.iter().any(|c| c == "test-watch-move") && request.args.len() == 4 {
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

/// Pins the engine for one query; `None` once a selection failure has been
/// answered with a null-generation runtime-error block.
fn select(
    host: &Host,
    request: &Request,
    destination: &Destination,
) -> io::Result<Option<QuerySession>> {
    // Freshness preparation can itself panic before batch emits begin. Turn
    // that failure into an ordinary null-generation runtime-error block.
    let selected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pin(host)))
        .unwrap_or_else(|_| Err(io::Error::other("engine selection panicked")));
    match selected {
        Ok(session) => Ok(Some(session)),
        Err(error) => {
            emit_to(destination, &request.id, "begin", |o| {
                o.null("generation");
            })?;
            emit_to(destination, &request.id, "end", |o| {
                o.int("exit", runtime_status(request))
                    .bool("cancelled", false)
                    .str("error", "RuntimeError")
                    .str("message", &error.to_string());
            })?;
            Ok(None)
        }
    }
}

fn serve_search(
    host: &Host,
    request: &Request,
    session: QuerySession,
    destination: &Destination,
) -> io::Result<()> {
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

fn serve_grep(
    host: &Host,
    request: &Request,
    session: QuerySession,
    destination: &Destination,
) -> io::Result<()> {
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

fn serve_find(
    host: &Host,
    request: &Request,
    session: QuerySession,
    destination: &Destination,
) -> io::Result<()> {
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

/// Answers `status` and `reload` with the engine, writer and worker fields.
fn serve_report(
    host: &Host,
    request: &Request,
    session: &QuerySession,
    destination: &Destination,
) -> io::Result<()> {
    let report = if request.op == Op::Status {
        stats_report(host, request, session)
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
            o.int("worker_budget", host.permits.budget() as u64)
                .int("worker_permits", held as u64)
                .integers("worker_grants", running.into_iter().map(|n| n as u64));
            crate::status::resources(o);
            crate::status::catalog_fields(o, session);
            let engine = lock(&host.engine);
            if let Loaded::Ready(engine) = &*engine {
                o.integers("pinned_internal_epochs", engine.pinned_epochs());
            }
            if request.args == [b"--stats".to_vec()] {
                crate::stats::json_fields(o, session);
            }
            if let Some(report) = &report {
                o.str("report", report);
            }
        },
    )
}
