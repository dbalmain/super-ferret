//! Ordinary client transport, native event rendering and pre-query fallback.
//! Sending a query commits to this transport: failures afterwards never replay.

use std::ffi::OsString;
use std::fs;
use std::io::{self, BufReader, Write};
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
) -> Option<Exit> {
    query(
        &context.index,
        atoms,
        "search",
        json,
        limit,
        now,
        Some(context),
    )
}
pub(crate) fn find(index: &Path, args: &[OsString], json: bool, now: SystemTime) -> Option<Exit> {
    query(index, args, "find", json, None, now, None)
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
fn connect(endpoint: &Endpoint, index: &Path) -> io::Result<BufReader<UnixStream>> {
    let until = Instant::now() + duration("FERRET_DAEMON_STARTUP_MS", 10_000);
    let context = endpoint::context()?;
    let mut spawned = false;
    let mut draining = false;
    while Instant::now() < until {
        if draining && fs::symlink_metadata(endpoint.socket()).is_ok() {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        draining = false;
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
                stream.set_read_timeout(Some(
                    until
                        .saturating_duration_since(Instant::now())
                        .max(Duration::from_millis(1)),
                ))?;
                let mut reader = BufReader::new(stream);
                let mut line = Vec::new();
                loop {
                    let hello = event(&mut reader, &mut line)?;
                    if field_text(&hello, "event") != Some("hello")
                        || field_number(&hello, "major") != Some(MAJOR)
                        || field_text(&hello, "index") != Some(&endpoint.identity)
                        || field_text(&hello, "context") != Some(&context)
                    {
                        return Err(io::Error::other("incompatible daemon context or protocol"));
                    }
                    if field_text(&hello, "build") != Some(BUILD)
                        || field_number(&hello, "format") != Some(FORMAT)
                    {
                        reader.get_mut().write_all(b"{\"op\":\"drain\"}\n")?;
                        // Wait for the owning lock and socket to be released.
                        // Active queries finish; the same deadline bounds
                        // fallback.
                        spawned = false;
                        draining = true;
                        break;
                    }
                    match field_text(&hello, "state") {
                        Some("loading") => continue,
                        Some("ready") => {
                            reader.get_ref().set_read_timeout(None)?;
                            return Ok(reader);
                        }
                        _ => return Err(io::Error::other("daemon could not open the catalog")),
                    }
                }
            }
            Err(_) if !spawned => {
                spawn(endpoint, index)?;
                spawned = true;
            }
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "daemon startup deadline",
    ))
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
fn query(
    index: &Path,
    args: &[OsString],
    op: &str,
    json: bool,
    limit: Option<u64>,
    now: SystemTime,
    context: Option<&Context>,
) -> Option<Exit> {
    let started = Instant::now();
    if std::env::var_os("FERRET_NO_DAEMON").is_some() {
        return None;
    }
    let index = fs::canonicalize(index).ok()?;
    let endpoint = Endpoint::open(&index).ok()?;
    let mut reader = connect(&endpoint, &index).ok()?;
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
    object.end();
    if protocol::parse_request(&request).is_err() {
        return None;
    }
    request.push(b'\n');
    let mut first_row = None;
    let mut summary = None;
    let mut rows = 0u64;
    let mut output_failure = false;
    let result = (|| -> io::Result<Exit> {
        reader.get_mut().write_all(&request)?;
        let mut line = Vec::new();
        let mut out = io::stdout().lock();
        let mut err = io::stderr().lock();
        loop {
            let value = event(&mut reader, &mut line)?;
            if field_text(&value, "id") != Some(op) {
                return Err(io::Error::other("wrong daemon query tag"));
            }
            if op == "find" && json {
                write(&mut out, &line, &mut output_failure)?;
            }
            match field_text(&value, "event") {
                Some("begin") => {}
                Some("row") if op == "search" => {
                    if json {
                        let offset = line
                            .windows(8)
                            .position(|bytes| bytes == b",\"path\":")
                            .ok_or_else(|| io::Error::other("missing row path"))?;
                        write(&mut out, b"{", &mut output_failure)?;
                        write(&mut out, &line[offset + 1..], &mut output_failure)?;
                    } else {
                        write(
                            &mut out,
                            &bytes(&value, "path", "path_base64")?,
                            &mut output_failure,
                        )?;
                        write(&mut out, b"\n", &mut output_failure)?;
                    }
                    first_row.get_or_insert_with(|| started.elapsed().as_micros() as i128);
                    rows += 1;
                    out.flush()?;
                }
                Some("stdout" | "stderr") if op == "find" => {
                    if !json {
                        let data = bytes(&value, "", "bytes_base64")?;
                        if field_text(&value, "event") == Some("stdout") {
                            write(&mut out, &data, &mut output_failure)?;
                        } else {
                            err.write_all(&data)?;
                            err.flush()?;
                        }
                    }
                }
                Some("diagnostic") if op == "find" => {
                    if !json {
                        match field_text(&value, "code") {
                            Some("permission") => cli::error(
                                "find: warning: -perm /000 now matches all files; use -perm -000 for the equivalent form",
                            ),
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
                    let exit = status(
                        field_number(&value, "exit")
                            .ok_or_else(|| io::Error::other("missing native status"))?,
                    )?;
                    if !json && let Some(error) = field_text(&value, "error") {
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
    if let Some(context) = context {
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
    writer
        .write_all(bytes)
        .and_then(|()| writer.flush())
        .inspect_err(|_| *failed = true)
}
