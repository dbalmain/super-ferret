//! Ordinary client transport, native event rendering and pre-query fallback.
//! Sending a query commits to this transport: failures afterwards never replay.

use std::ffi::OsString;
use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use super::{
    BUILD, FORMAT, MAJOR, duration,
    endpoint::{self, Endpoint},
};
use crate::cli::{self, Context, Exit};
use crate::json::Object;
use crate::protocol::{self, Value};

pub(crate) fn search(
    context: &Context,
    atoms: &[OsString],
    json: bool,
    limit: Option<u64>,
    now: SystemTime,
    scan_uncovered: bool,
) -> Option<Exit> {
    query(
        &context.index,
        atoms,
        "search",
        json,
        Options {
            limit,
            scan_uncovered,
            ..Options::default()
        },
        now,
        Some(context),
    )
    .or_else(|| stale_notice(&context.index, now))
}
pub(crate) fn find(index: &Path, args: &[OsString], json: bool, now: SystemTime) -> Option<Exit> {
    query(index, args, "find", json, Options::default(), now, None)
        .or_else(|| stale_notice(index, now))
}

pub(crate) fn grep(
    index: &Path,
    args: &[OsString],
    options: crate::protocol::GrepOptions,
) -> Option<Exit> {
    query(
        index,
        args,
        "grep",
        false,
        Options {
            grep: Some(options),
            ..Options::default()
        },
        SystemTime::now(),
        None,
    )
    .or_else(|| stale_notice(index, SystemTime::now()))
}

/// How old a publication may be before a command that answers it in-process
/// says so (D117 B). Above the 1 s debounce and the 10 s startup deadline, so
/// an index a daemon or `ferret index` just wrote stays quiet.
const STALE_AFTER: Duration = Duration::from_secs(60);

/// Says on stderr that no daemon answered and the catalog in `index` was
/// published more than [`STALE_AFTER`] before `now`, so the answer may miss
/// recent changes. Every publication renames `current` into place, so its
/// mtime is the publication time. `FERRET_STALE_NOTICE=0` silences it. Always
/// returns `None`: the caller answers in-process either way.
fn stale_notice(index: &Path, now: SystemTime) -> Option<Exit> {
    if std::env::var_os("FERRET_STALE_NOTICE").is_some_and(|value| value == "0") {
        return None;
    }
    let published = fs::metadata(index.join("current")).ok()?.modified().ok()?;
    let age = now.duration_since(published).ok()?;
    if age > STALE_AFTER {
        cli::warn(&format!(
            "no daemon answered; this answer comes from the index as published {} ago and may miss later changes (FERRET_STALE_NOTICE=0 silences this)",
            approximate(age)
        ));
    }
    None
}
/// `age` to the nearest whole minute, hour or day.
fn approximate(age: Duration) -> String {
    let minutes = (age.as_secs() + 30) / 60;
    match minutes {
        0..=89 => format!("{minutes} min"),
        90..=2159 => format!("{} h", (minutes + 30) / 60),
        _ => format!("{} days", (minutes + 720) / 1440),
    }
}

pub(crate) fn writer_command(
    context: &Context,
    command: &str,
    change: ferret_crawl::RootChange<'_>,
    global: &str,
) -> Option<Exit> {
    let mut args = vec![OsString::from(global)];
    args.extend(
        change
            .add
            .iter()
            .chain(change.remove)
            .map(|p| p.as_os_str().to_owned()),
    );
    query(
        &context.index,
        &args,
        command,
        false,
        Options::default(),
        SystemTime::now(),
        Some(context),
    )
}
pub(crate) fn daemon_status(context: &Context) -> Exit {
    query(
        &context.index,
        &[],
        "status",
        false,
        Options::default(),
        SystemTime::now(),
        None,
    )
    .unwrap_or_else(|| crate::status::local(context, false))
}

pub(crate) fn daemon_stats(context: &Context) -> Exit {
    query(
        &context.index,
        &["--stats".into()],
        "status",
        false,
        Options::default(),
        SystemTime::now(),
        None,
    )
    .unwrap_or_else(|| crate::status::local(context, true))
}

/// Text `ferret stats`. Asks a running daemon and does not start one.
/// The report is the daemon pin's census; no daemon reads the published
/// catalog locally, same as `stats --json`'s fallback.
pub(crate) fn daemon_stats_text(context: &Context) -> Exit {
    query(
        &context.index,
        &[
            OsString::from("--stats-text"),
            context.index.as_os_str().to_owned(),
        ],
        "status",
        false,
        Options {
            text_report: true,
            ..Options::default()
        },
        SystemTime::now(),
        None,
    )
    .unwrap_or_else(|| crate::stats::run(context))
}

fn spawn(endpoint: &Endpoint, index: &Path) -> io::Result<()> {
    let binary = std::env::var_os("FERRET_DAEMON_BIN")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_exe()?.with_file_name("ferretd"));
    let log = endpoint.private_file("log")?;
    let mut child = Command::new(binary)
        .arg("--index")
        .arg(index)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()?;
    // Reap direct-spawn failures while this client is alive; a resident daemon
    // is reparented when its short-lived originating client exits.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Starts ferretd for `index` and returns without waiting. The caller must
/// already have dropped the catalog writer lock: this is the same `spawn` a
/// search uses on a miss, and a spawn that overlaps that lock races the new
/// process for it. `FERRET_NO_DAEMON`, or an endpoint that cannot be opened,
/// does not spawn. A spawn error is ignored so a finished index still
/// reports its own outcome.
pub(crate) fn spawn_resident(index: &Path) {
    if std::env::var_os("FERRET_NO_DAEMON").is_some() {
        return;
    }
    let Ok(index) = fs::canonicalize(index) else {
        return;
    };
    let Ok(endpoint) = Endpoint::open(&index) else {
        return;
    };
    let _ = spawn(&endpoint, &index);
}

fn field_text<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.field(field)?.text()
}
fn field_number(value: &Value, field: &str) -> Option<u64> {
    value.field(field)?.number()
}
fn event(reader: &mut BufReader<UnixStream>, line: &mut Vec<u8>) -> io::Result<Value> {
    line.clear();
    if crate::batch::read_line_bounded(reader, line)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "daemon closed the connection",
        ));
    }
    protocol::parse_object(line)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid daemon event"))
}
enum ConnectError {
    NoOwner(io::Error),
    Owner(io::Error),
    OwnerTimeout,
}
impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoOwner(error) | Self::Owner(error) => error.fmt(f),
            Self::OwnerTimeout => f.write_str("daemon owner did not become ready before the startup deadline; it may still be loading or draining; check ferret status --json and retry"),
        }
    }
}
struct Connected {
    reader: BufReader<UnixStream>,
    raw_output: bool,
}
fn connect(
    endpoint: &Endpoint,
    index: &Path,
    can_spawn: bool,
    writer: bool,
) -> Result<Connected, ConnectError> {
    let mut observed_owner = false;
    connect_owner(endpoint, index, can_spawn, writer, &mut observed_owner).map_err(|error| {
        if !observed_owner {
            ConnectError::NoOwner(error)
        } else if matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ) {
            ConnectError::OwnerTimeout
        } else {
            ConnectError::Owner(error)
        }
    })
}
fn connect_owner(
    endpoint: &Endpoint,
    index: &Path,
    can_spawn: bool,
    writer: bool,
    observed_owner: &mut bool,
) -> io::Result<Connected> {
    // The incarnation lives in the fixed-length manifest. Opening the catalog
    // also walks every change-log transaction, which a long-lived index
    // accumulates without bound.
    let incarnation = ferret_catalog::Catalog::published_generation(index)
        .ok()
        .flatten()
        .map(|generation| crate::find_json::hex(&generation.incarnation));
    let until = Instant::now() + duration("FERRET_DAEMON_STARTUP_MS", 10_000);
    let context = endpoint::context()?;
    let mut draining_socket = None;
    while Instant::now() < until {
        if let Some(identity) = draining_socket {
            if fs::symlink_metadata(endpoint.socket())
                .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == identity)
            {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            // The leaving owner is gone: carry on as if it had never been
            // seen, so a writer with nobody listening runs in-process.
            *observed_owner = false;
        }
        let connected = fs::symlink_metadata(endpoint.socket()).and_then(|metadata| {
            if !metadata.file_type().is_socket()
                || metadata.uid() != endpoint::uid()?
                || metadata.mode() & 0o7777 != 0o600
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "unusable daemon socket",
                ));
            }
            let identity = (metadata.dev(), metadata.ino());
            UnixStream::connect(endpoint.socket()).map(|stream| (stream, identity))
        });
        match connected {
            Ok((stream, identity)) => {
                *observed_owner = true;
                stream.set_read_timeout(Some(
                    until
                        .saturating_duration_since(Instant::now())
                        .max(Duration::from_millis(1)),
                ))?;
                let mut reader = BufReader::new(stream);
                let mut line = Vec::new();
                // A close before any hello is an owner that is exiting,
                // draining or full. A writer waits for its socket to go, as
                // for a build-mismatch drain: the daemon releases its catalog
                // lock before removing the socket (D110). Queries answer
                // in-process at once instead.
                let mut hello = match event(&mut reader, &mut line) {
                    Err(error)
                        if writer
                            && matches!(
                                error.kind(),
                                io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                            ) =>
                    {
                        draining_socket = Some(identity);
                        continue;
                    }
                    hello => hello?,
                };
                // Writer commands wait out loading on this connection, within
                // the startup deadline: answering in-process would race the
                // daemon's writer ownership.
                while writer && field_text(&hello, "state") == Some("loading") {
                    hello = event(&mut reader, &mut line)?;
                }
                if field_text(&hello, "event") != Some("hello")
                    || field_number(&hello, "major") != Some(MAJOR)
                    || field_text(&hello, "index") != Some(&endpoint.identity)
                    || field_text(&hello, "context") != Some(&context)
                {
                    return Err(io::Error::other("incompatible daemon context or protocol"));
                }
                if field_text(&hello, "build") != Some(BUILD)
                    || field_number(&hello, "format") != Some(FORMAT)
                    || !supports_queries(&hello)
                    || writer && !has_capability(&hello, "writer")
                {
                    if !can_spawn {
                        return Err(io::Error::other(
                            "incompatible writer owner; use ferret status --json",
                        ));
                    }
                    reader.get_mut().write_all(b"{\"op\":\"drain\"}\n")?;
                    // Wait for the owning lock and socket to be released.
                    // Active queries finish; the same deadline bounds
                    // fallback.
                    draining_socket = fs::symlink_metadata(endpoint.socket())
                        .ok()
                        .map(|metadata| (metadata.dev(), metadata.ino()));
                    continue;
                }
                match field_text(&hello, "state") {
                    Some("loading") => {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "daemon is still loading; using the in-process engine",
                        ));
                    }
                    Some("ready") => {
                        if let Some(incarnation) = &incarnation
                            && hello
                                .field("generation")
                                .and_then(|generation| field_text(generation, "incarnation"))
                                != Some(incarnation.as_str())
                        {
                            return Err(io::Error::other("incompatible catalog incarnation"));
                        }
                        reader.get_ref().set_read_timeout(None)?;
                        return Ok(Connected {
                            reader,
                            raw_output: has_capability(&hello, "raw-output-v1"),
                        });
                    }
                    _ => return Err(io::Error::other("daemon could not open the catalog")),
                }
            }
            Err(error) if !can_spawn || *observed_owner && writer => return Err(error),
            Err(_) => {
                spawn(endpoint, index)?;
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "daemon started; using the in-process engine for this query",
                ));
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "daemon startup deadline",
    ))
}
fn has_capability(hello: &Value, capability: &str) -> bool {
    matches!(hello.field("capabilities"), Some(Value::Arr(values)) if values.iter().any(|v| v.text() == Some(capability)))
}
fn supports_queries(hello: &Value) -> bool {
    let Some(Value::Arr(capabilities)) = hello.field("capabilities") else {
        return false;
    };
    ["query-only", "cancel", "drain"].iter().all(|required| {
        capabilities
            .iter()
            .any(|value| value.text() == Some(*required))
    })
}
fn cancel(stream: &mut UnixStream) {
    let _ = stream.write_all(b"{\"op\":\"cancel\"}\n");
    let _ = stream.shutdown(std::net::Shutdown::Both);
}
fn status(value: u64) -> io::Result<Exit> {
    match value {
        0 => Ok(Exit::Ok),
        1 => Ok(Exit::NoMatch),
        2 => Ok(Exit::Usage),
        3 => Ok(Exit::Error),
        4 => Ok(Exit::Partial),
        _ => Err(io::Error::other("invalid native status")),
    }
}
fn bytes(value: &Value, plain: &str, base64: &str) -> io::Result<Vec<u8>> {
    if let Some(encoded) = field_text(value, base64) {
        protocol::base64_decode(encoded)
            .ok_or_else(|| io::Error::other("invalid base64 in daemon event"))
    } else {
        field_text(value, plain)
            .map(|s| s.as_bytes().to_vec())
            .ok_or_else(|| io::Error::other("missing byte field in daemon event"))
    }
}
/// Consume a bounded exact-length payload before looking for another JSON
/// envelope. EOF in a payload is a transport failure, never a partial success.
fn read_payload(
    reader: &mut impl Read,
    value: &Value,
    negotiated: bool,
    payload: &mut Vec<u8>,
) -> io::Result<()> {
    if let Some(length) = value.field("length") {
        let length = length
            .number()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|&n| n <= crate::transport::MAX_RAW_BYTES && negotiated)
            .ok_or_else(|| io::Error::other("invalid or unnegotiated raw output length"))?;
        payload.resize(length, 0);
        reader.read_exact(payload)
    } else {
        *payload = bytes(value, "bytes", "bytes_base64")?;
        Ok(())
    }
}

#[derive(Default)]
struct Options {
    limit: Option<u64>,
    scan_uncovered: bool,
    grep: Option<crate::protocol::GrepOptions>,
    /// Print the status event's `report` text instead of its JSON line.
    text_report: bool,
}
fn query(
    index: &Path,
    args: &[OsString],
    op: &str,
    json: bool,
    options: Options,
    now: SystemTime,
    context: Option<&Context>,
) -> Option<Exit> {
    let Options {
        limit,
        scan_uncovered,
        grep,
        text_report,
    } = options;
    let started = Instant::now();
    let grep_tty = grep.as_ref().is_some_and(|options| options.tty);
    if std::env::var_os("FERRET_NO_DAEMON").is_some() {
        return None;
    }
    let index = fs::canonicalize(index).ok()?;
    let endpoint = Endpoint::open(&index).ok()?;
    let writer = matches!(op, "index" | "roots-remove");
    // Build and validate before connecting: a request the protocol rejects
    // (an argv past its line or element limit) answers in-process, and must
    // neither reach a daemon nor start one.
    let cwd = std::env::current_dir().ok()?;
    let mut request = Vec::new();
    let mut object = Object::new(&mut request);
    object
        .str("id", op)
        .str("op", op)
        .byte_strings("args", args.iter().map(|s| s.as_bytes()))
        .opt_int("limit", limit)
        .int(
            "start_unix_ns",
            now.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_nanos() as u64,
        )
        .opt_byte_value("cwd", Some(cwd.as_os_str().as_bytes()));
    object.byte_strings(
        "capabilities",
        scan_uncovered
            .then_some(b"scan-uncovered".as_slice())
            .into_iter()
            .chain(
                (matches!(op, "find" | "grep" | "search") && !json)
                    .then_some(b"raw-output-v1".as_slice()),
            ),
    );
    if let Some(grep) = &grep {
        grep.encode(&mut object);
    }
    object.end();
    protocol::parse_request(&request).ok()?;
    request.push(b'\n');
    let Connected {
        mut reader,
        raw_output,
    } = match connect(
        &endpoint,
        &index,
        matches!(op, "search" | "find" | "grep"),
        writer,
    ) {
        Ok(reader) => reader,
        Err(ConnectError::NoOwner(_)) => return None,
        Err(error) if writer => {
            cli::error(&format!("writer command: {error}"));
            return Some(Exit::Error);
        }
        Err(_) => return None,
    };
    let mut first_row = None;
    let mut summary = None;
    let mut rows = 0u64;
    let mut output_failure = false;
    let mut first_frame = true;
    let result = (|| -> io::Result<Exit> {
        reader.get_mut().write_all(&request)?;
        let mut line = Vec::new();
        let mut payload = Vec::new();
        let mut out = BufWriter::with_capacity(64 * 1024, io::stdout().lock());
        let mut err = io::stderr().lock();
        loop {
            line.clear();
            if crate::batch::read_line_bounded(&mut reader, &mut line)? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "daemon closed the connection",
                ));
            }
            let value = protocol::parse_object(&line).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid daemon event")
            })?;
            if field_text(&value, "id") != Some(op) {
                return Err(io::Error::other("wrong daemon query tag"));
            }
            if op == "find" && json {
                write(&mut out, &line, &mut output_failure)?;
                let event_name = field_text(&value, "event");
                if (first_frame && matches!(event_name, Some("row" | "stdout")))
                    || matches!(event_name, Some("diagnostic" | "stderr" | "end"))
                {
                    out.flush().inspect_err(|_| output_failure = true)?;
                }
                if matches!(event_name, Some("row" | "stdout")) {
                    first_frame = false;
                }
            }
            match field_text(&value, "event") {
                Some("begin") => {}
                Some("log") if matches!(op, "index" | "roots-remove") => {
                    if let Some(context) = context {
                        context.log(&bytes(&value, "bytes", "bytes_base64")?);
                    }
                }
                Some("status") if op == "status" => {
                    if text_report {
                        let report = field_text(&value, "report")
                            .ok_or_else(|| io::Error::other("missing stats report"))?;
                        write(&mut out, report.as_bytes(), &mut output_failure)?;
                    } else {
                        write(&mut out, &line, &mut output_failure)?;
                    }
                    out.flush().inspect_err(|_| output_failure = true)?;
                    return Ok(Exit::Ok);
                }
                Some("row") if op == "search" => {
                    rows += 1;
                    if json {
                        // The shared encoder prefixes native row fields with
                        // the fixed id/event envelope. Preserve those fields
                        // verbatim rather than encoding a second row schema.
                        let fields = line
                            .strip_prefix(br#"{"id":"search","event":"row","#)
                            .ok_or_else(|| io::Error::other("invalid row envelope"))?;
                        let mut native = Vec::with_capacity(fields.len() + 1);
                        native.push(b'{');
                        native.extend_from_slice(fields);
                        write(&mut out, &native, &mut output_failure)?;
                    } else {
                        write(
                            &mut out,
                            &bytes(&value, "path", "path_base64")?,
                            &mut output_failure,
                        )?;
                        write(&mut out, b"\n", &mut output_failure)?;
                    }
                    first_row.get_or_insert_with(|| started.elapsed().as_micros() as i128);
                    if rows == 1 {
                        out.flush().inspect_err(|_| output_failure = true)?;
                    }
                }
                Some("stdout" | "stderr")
                    if matches!(op, "find" | "grep" | "search" | "index" | "roots-remove") =>
                {
                    if !json {
                        read_payload(&mut reader, &value, raw_output, &mut payload)?;
                        let data = &payload;
                        if field_text(&value, "event") == Some("stdout") {
                            // A payload is not a row count: a path may contain
                            // a newline, and a long path splits across parts.
                            // `end.rows` is the count. A broken pipe arrives
                            // before that event, so the first payload still
                            // counts as a match.
                            if op == "search" && !data.is_empty() {
                                if rows == 0 {
                                    rows = 1;
                                }
                                first_row
                                    .get_or_insert_with(|| started.elapsed().as_micros() as i128);
                            }
                            write(&mut out, data, &mut output_failure)?;
                            if first_frame
                                || grep_tty
                                || matches!(value.field("flush"), Some(Value::Bool(true)))
                            {
                                out.flush().inspect_err(|_| output_failure = true)?;
                                if !data.is_empty() {
                                    first_frame = false;
                                }
                            }
                        } else {
                            out.flush().inspect_err(|_| output_failure = true)?;
                            if op == "grep" {
                                // Local grep diagnostics ignore a closed
                                // stderr.
                                let _ = err.write_all(data).and_then(|()| err.flush());
                            } else {
                                err.write_all(data)?;
                                err.flush()?;
                            }
                        }
                    }
                }
                Some("diagnostic") if op == "find" => {
                    if field_text(&value, "code") == Some("warning") {
                        cli::error(&format!(
                            "find: {}",
                            field_text(&value, "message").unwrap_or("find warning")
                        ));
                    }
                    if !json {
                        out.flush().inspect_err(|_| output_failure = true)?;
                        match field_text(&value, "code") {
                            Some("permission") => cli::error(crate::find::PERMISSION_WARNING),
                            Some("warning") => {}
                            Some("walk") => {
                                let path = PathBuf::from(OsString::from_vec(bytes(
                                    &value,
                                    "path",
                                    "path_base64",
                                )?));
                                cli::error(&format!(
                                    "find: {}: {}",
                                    path.display(),
                                    field_text(&value, "message").unwrap_or("walk error")
                                ));
                            }
                            _ => {}
                        }
                    }
                }
                Some("end") => {
                    out.flush().inspect_err(|_| output_failure = true)?;
                    if op == "search"
                        && let Some(reported) = field_number(&value, "rows")
                    {
                        rows = reported;
                    }
                    let exit = status(
                        field_number(&value, "exit")
                            .ok_or_else(|| io::Error::other("missing native status"))?,
                    )?;
                    if (op == "search" || !json)
                        && let Some(error) = field_text(&value, "error")
                    {
                        let message = field_text(&value, "message").unwrap_or(error);
                        if op == "search"
                            && matches!(value.field("read_error"), Some(Value::Bool(true)))
                            && let Some(context) = context
                        {
                            cli::error(&format!("{}: {message}", context.index.display()));
                        } else if matches!(op, "find" | "grep") {
                            cli::error(&format!("{op}: {message}"));
                        } else {
                            cli::error(message);
                        }
                    }
                    summary = Some(value);
                    return Ok(exit);
                }
                _ => return Err(io::Error::other("unexpected daemon event")),
            }
        }
    })();
    let exit = match result {
        Ok(exit) => exit,
        Err(error) if output_failure => {
            cancel(reader.get_mut());
            if (op == "grep" || op == "find" && !json) && error.kind() == io::ErrorKind::BrokenPipe
            {
                Exit::Ok
            } else if op == "search" && error.kind() == io::ErrorKind::BrokenPipe {
                if rows > 0 { Exit::Ok } else { Exit::NoMatch }
            } else {
                cli::error(&format!("{op}: writing stdout: {error}"));
                if op == "grep" {
                    Exit::Usage
                } else if op == "find" {
                    Exit::NoMatch
                } else {
                    Exit::Error
                }
            }
        }
        Err(error) => {
            cancel(reader.get_mut());
            cli::error(&format!("daemon transport failure: {error}"));
            if op == "grep" {
                Exit::Usage
            } else {
                Exit::Error
            }
        }
    };
    if let Some(context) = context.filter(|_| op == "search") {
        let run = crate::search::Run {
            limit,
            exit: exit as u8,
            rows,
            first_row_us: first_row,
            total_us: started.elapsed().as_micros() as i128,
        };
        let line = crate::search::log_line(now, args.iter().map(|a| a.as_bytes()), &run, |o| {
            o.str("host", "socket");
            if let Some(end) = &summary {
                o.int("server_us", field_number(end, "elapsed_us").unwrap_or(0));
                crate::search::copy_summary(o, end);
            } else {
                o.str("error", "transport");
            }
        });
        context.log(&line);
    }
    Some(exit)
}

fn write(writer: &mut impl Write, bytes: &[u8], failed: &mut bool) -> io::Result<()> {
    writer.write_all(bytes).inspect_err(|_| *failed = true)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A truncated or unnegotiated raw frame must never turn its prefix into a
    // successful query or make the client allocate an advertised giant length.
    #[test]
    fn output_payloads_are_bounded_negotiated_and_exact() -> io::Result<()> {
        let mut payload = Vec::new();
        for (header, negotiated, input, expected) in [
            (
                br#"{"length":3}"#.as_slice(),
                true,
                b"a\0\xff".as_slice(),
                None,
            ),
            (
                br#"{"length":3}"#,
                true,
                b"a",
                Some(io::ErrorKind::UnexpectedEof),
            ),
            (
                br#"{"length":3}"#,
                false,
                b"abc",
                Some(io::ErrorKind::Other),
            ),
            (
                br#"{"length":65537}"#,
                true,
                b"",
                Some(io::ErrorKind::Other),
            ),
            (br#"{"length":true}"#, true, b"", Some(io::ErrorKind::Other)),
            (br#"{"bytes_base64":"YQD/"}"#, false, b"", None),
        ] {
            let value = protocol::parse_object(header).ok_or_else(|| io::Error::other("header"))?;
            let result = read_payload(&mut &input[..], &value, negotiated, &mut payload);
            assert_eq!(result.err().map(|error| error.kind()), expected);
            if expected.is_none() {
                assert_eq!(payload, b"a\0\xff");
            }
        }
        Ok(())
    }
}
