//! Ordinary client transport, native event rendering and pre-query fallback.
//! Sending a query commits to this transport: failures afterwards never replay.

use std::ffi::OsString;
use std::fs;
use std::io::{self, BufReader, BufWriter, Write};
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
        },
        now,
        Some(context),
    )
}
pub(crate) fn find(index: &Path, args: &[OsString], json: bool, now: SystemTime) -> Option<Exit> {
    query(index, args, "find", json, Options::default(), now, None)
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
fn connect(
    endpoint: &Endpoint,
    index: &Path,
    can_spawn: bool,
    writer: bool,
) -> Result<BufReader<UnixStream>, ConnectError> {
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
) -> io::Result<BufReader<UnixStream>> {
    let incarnation = ferret_catalog::Catalog::open(index)
        .ok()
        .flatten()
        .map(|catalog| crate::find_json::hex(&catalog.generation().incarnation));
    let until = Instant::now() + duration("FERRET_DAEMON_STARTUP_MS", 10_000);
    let context = endpoint::context()?;
    let mut draining_socket = None;
    while Instant::now() < until {
        if let Some(identity) = draining_socket
            && fs::symlink_metadata(endpoint.socket())
                .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == identity)
        {
            std::thread::sleep(Duration::from_millis(20));
            continue;
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
            UnixStream::connect(endpoint.socket())
        });
        match connected {
            Ok(stream) => {
                *observed_owner = true;
                stream.set_read_timeout(Some(
                    until
                        .saturating_duration_since(Instant::now())
                        .max(Duration::from_millis(1)),
                ))?;
                let mut reader = BufReader::new(stream);
                let mut line = Vec::new();
                // Writer commands wait out loading on this connection, within
                // the startup deadline: answering in-process would race the
                // daemon's writer ownership. Queries answer in-process instead.
                let hello = loop {
                    let hello = event(&mut reader, &mut line)?;
                    if !(writer && field_text(&hello, "state") == Some("loading")) {
                        break hello;
                    }
                };
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
                        return Ok(reader);
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
#[derive(Default)]
struct Options {
    limit: Option<u64>,
    scan_uncovered: bool,
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
    } = options;
    let started = Instant::now();
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
        scan_uncovered.then_some(b"scan-uncovered".as_slice()),
    );
    object.end();
    protocol::parse_request(&request).ok()?;
    request.push(b'\n');
    let mut reader = match connect(&endpoint, &index, matches!(op, "search" | "find"), writer) {
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
        let mut out = BufWriter::with_capacity(64 * 1024, io::stdout().lock());
        let mut err = io::stderr().lock();
        loop {
            let value = event(&mut reader, &mut line)?;
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
                    write(&mut out, &line, &mut output_failure)?;
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
                Some("stdout" | "stderr") if matches!(op, "find" | "index" | "roots-remove") => {
                    if !json {
                        let data = bytes(&value, "bytes", "bytes_base64")?;
                        if field_text(&value, "event") == Some("stdout") {
                            write(&mut out, &data, &mut output_failure)?;
                            if first_frame {
                                out.flush().inspect_err(|_| output_failure = true)?;
                                first_frame = false;
                            }
                        } else {
                            out.flush().inspect_err(|_| output_failure = true)?;
                            err.write_all(&data)?;
                            err.flush()?;
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
                    let exit = status(
                        field_number(&value, "exit")
                            .ok_or_else(|| io::Error::other("missing native status"))?,
                    )?;
                    if (op == "search" || !json)
                        && let Some(error) = field_text(&value, "error")
                    {
                        let message = field_text(&value, "message").unwrap_or(error);
                        if op == "find" {
                            cli::error(&format!("find: {message}"));
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
            if op == "search" && error.kind() == io::ErrorKind::BrokenPipe {
                if rows > 0 { Exit::Ok } else { Exit::NoMatch }
            } else {
                cli::error(&format!("{op}: writing stdout: {error}"));
                if op == "find" {
                    Exit::NoMatch
                } else {
                    Exit::Error
                }
            }
        }
        Err(error) => {
            cancel(reader.get_mut());
            cli::error(&format!("daemon transport failure: {error}"));
            Exit::Error
        }
    };
    if let Some(context) = context.filter(|_| op == "search") {
        let mut line = Vec::new();
        let mut object = crate::log::line(&mut line, "search", now);
        object
            .byte_strings("query", args.iter().map(|a| a.as_bytes()))
            .str("host", "socket")
            .opt_int("limit", limit)
            .int("exit", exit as u8)
            .int("rows", rows)
            .int("total_us", started.elapsed().as_micros() as i128);
        object.opt_int("first_row_us", first_row);
        if let Some(summary) = summary {
            object
                .str("plan", field_text(&summary, "plan").unwrap_or(""))
                .str("strategy", field_text(&summary, "strategy").unwrap_or(""))
                .int(
                    "server_us",
                    field_number(&summary, "elapsed_us").unwrap_or(0),
                )
                .int(
                    "bytes_read",
                    field_number(&summary, "bytes_read").unwrap_or(0),
                );
            if let (Some(names), Some(inodes)) = (
                field_number(&summary, "names"),
                field_number(&summary, "inodes"),
            ) {
                object.int("names", names).int("inodes", inodes);
            }
            if let Some(stats) = summary.field("stats") {
                object.object("stats", |o| {
                    o.int("candidates", field_number(stats, "candidates").unwrap_or(0))
                        .int("rows", field_number(stats, "rows").unwrap_or(0));
                });
            }
            if let Some(error) = field_text(&summary, "error") {
                object.str("error", error);
            }
        } else {
            object.str("error", "transport");
        }
        object.end();
        context.log(&line);
    }
    Some(exit)
}

fn write(writer: &mut impl Write, bytes: &[u8], failed: &mut bool) -> io::Result<()> {
    writer.write_all(bytes).inspect_err(|_| *failed = true)
}
