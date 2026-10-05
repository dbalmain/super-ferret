//! Sequential JSON-lines host for one resident engine.

use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::ops::ControlFlow;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use ferret_query::Query;
use ferret_query::find::{Effects, OutputBuffer, Plan, WalkError};

use crate::cli;
use crate::engine::{Engine, QuerySession};
use crate::json::Object;
use crate::protocol::{self, Op, Request};
use crate::search::json_row;
use crate::xdg::Dirs;

const PART: usize = 64 * 1024;

pub(crate) fn main(args: impl Iterator<Item = OsString>) -> cli::Exit {
    let mut args = args;
    let input = match args.next() {
        None => None,
        Some(flag) if flag == "--input" => match args.next() {
            Some(path) if args.next().is_none() => Some(PathBuf::from(path)),
            _ => return cli::Exit::Usage,
        },
        _ => return cli::Exit::Usage,
    };
    let file = match input {
        Some(path) => match std::fs::File::open(path) {
            Ok(file) => Some(file),
            Err(error) => {
                cli::error(&format!("batch: {error}"));
                return cli::Exit::Error;
            }
        },
        None => None,
    };
    let result = match file {
        Some(file) => process(io::BufReader::new(file)),
        None => process(io::stdin().lock()),
    };
    if let Err(error) = result {
        cli::error(&format!("batch: {error}"));
        cli::Exit::Error
    } else {
        cli::Exit::Ok
    }
}

fn process(reader: impl BufRead) -> io::Result<()> {
    let dirs = Dirs::from_env().ok();
    let index = std::env::var_os("FERRET_INDEX")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs.as_ref().map(|dirs| dirs.data.clone()));
    let mut engine = index.as_deref().and_then(open_engine);
    let launch_cwd = std::env::current_dir()?;
    let mut line = Vec::new();
    let mut reader = reader;
    loop {
        line.clear();
        let count = read_line_bounded(&mut reader, &mut line)?;
        if count == 0 {
            break;
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.len() > protocol::MAX_LINE_BYTES {
            emit_request_error(None, "LineTooLong")?;
            continue;
        }
        match protocol::parse_request(&line) {
            Ok(request) => handle(&request, &launch_cwd, &mut engine, dirs.as_ref())?,
            Err(error) => emit_request_error(error.id.as_deref(), &error.kind.to_string())?,
        }
    }
    Ok(())
}

fn open_engine(index: &Path) -> Option<Engine> {
    Engine::open(index).ok().flatten()
}

fn read_line_bounded(reader: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<usize> {
    let mut total = 0;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(total);
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(buffer.len(), |index| index + 1);
        let remaining = protocol::MAX_LINE_BYTES
            .saturating_add(2)
            .saturating_sub(line.len());
        line.extend_from_slice(&buffer[..consumed.min(remaining)]);
        total += consumed;
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(total);
        }
    }
}

fn handle(
    request: &Request,
    launch_cwd: &Path,
    engine: &mut Option<Engine>,
    dirs: Option<&Dirs>,
) -> io::Result<()> {
    match request.op {
        Op::Status => {
            let mut line = Vec::new();
            let mut object = Object::new(&mut line);
            object.str("id", &request.id).str("event", "status");
            if let Some(engine) = engine {
                let session = engine.pin();
                generation(&mut object, Some(session.generation()));
                object.int("bytes", session.name_index().bytes() as u64);
            } else {
                object.null("generation").int("bytes", 0);
            }
            object.end();
            send(&line)
        }
        Op::Reload => {
            let next = std::env::var_os("FERRET_INDEX")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| dirs.map(|dirs| dirs.data.clone()));
            *engine = next.as_deref().and_then(open_engine);
            event(request, "reload", |o| {
                if let Some(engine) = engine {
                    generation(o, Some(engine.generation()));
                } else {
                    o.null("generation");
                }
            })
        }
        Op::Search => search_request(request, engine.as_ref(), dirs),
        Op::Find => find_request(request, launch_cwd, engine.as_ref()),
    }
}

fn search_request(
    request: &Request,
    engine: Option<&Engine>,
    dirs: Option<&Dirs>,
) -> io::Result<()> {
    let started = Instant::now();
    let now = SystemTime::now();
    let session = engine.map(Engine::pin);
    event(request, "begin", |o| {
        generation(o, session.as_ref().map(QuerySession::generation))
    })?;
    let atoms: Vec<_> = request
        .args
        .iter()
        .cloned()
        .map(OsString::from_vec)
        .collect();
    let parsed = Query::from_args(atoms.iter().map(|atom| atom.as_bytes()), now);
    let mut rows = 0u64;
    let mut status = 3u8;
    let mut query_error = None;
    if let Ok(query) = parsed {
        if let Some(session) = session.as_ref() {
            let catalog = session.catalog();
            let result = session.search(&query, |row| {
                let mut bytes = Vec::new();
                json_row(&mut bytes, catalog, row);
                let mut output = Vec::new();
                let mut object = Object::new(&mut output);
                object.str("id", &request.id).str("event", "row");
                object.raw_fields(&bytes[1..bytes.len() - 1]);
                object.end();
                if send(&output).is_err() {
                    return ControlFlow::Break(());
                }
                rows += 1;
                if request.limit.is_some_and(|limit| rows >= limit) {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            });
            match result {
                Ok(_) => status = if rows == 0 { 1 } else { 0 },
                Err(error) => query_error = Some(error.to_string()),
            }
        } else {
            query_error = Some("cannot open index".to_owned());
        }
    } else {
        status = 2;
        if let Err(error) = parsed {
            query_error = Some(error.to_string());
        }
    }
    let elapsed = started.elapsed().as_micros() as i128;
    log_search(dirs, request, now, status, rows, elapsed);
    event(request, "end", |o| {
        o.int("exit", status)
            .int("rows", rows)
            .bool("cancelled", false)
            .int("elapsed_us", elapsed);
        if let Some(error) = query_error.as_deref() {
            o.str("error", error);
        }
    })
}

fn find_request(request: &Request, launch_cwd: &Path, engine: Option<&Engine>) -> io::Result<()> {
    let started = Instant::now();
    let now = SystemTime::now();
    let session = engine.map(Engine::pin);
    event(request, "begin", |o| {
        generation(o, session.as_ref().map(QuerySession::generation))
    })?;
    let cwd = request
        .cwd
        .as_deref()
        .map(|bytes| PathBuf::from(OsString::from_vec(bytes.to_vec())))
        .unwrap_or_else(|| launch_cwd.to_owned());
    let args: Vec<_> = request
        .args
        .iter()
        .cloned()
        .map(OsString::from_vec)
        .collect();
    let parsed = Plan::parse_at(&args, &cwd, now);
    let mut status = 1u8;
    let host = FrameOutput {
        id: &request.id,
        record: 0,
    };
    match parsed {
        Ok(plan) if plan.has_side_effects() => {
            diagnostic(&request.id, "actions_unavailable", "error", None)?;
            event(request, "end", |o| {
                o.int("exit", 1)
                    .bool("cancelled", false)
                    .str("error", "find actions are not available until M2c");
            })?;
            return Ok(());
        }
        Ok(plan) => {
            if plan.permission_warning() {
                diagnostic(&request.id, "permission", "warning", None)?;
            }
            if let Some(feature) = plan.unsupported() {
                diagnostic(&request.id, &feature.to_string_lossy(), "error", None)?;
                event(request, "end", |o| {
                    o.int("exit", 1).bool("cancelled", false);
                })?;
                return Ok(());
            }
            let workers = ferret_crawl::default_workers();
            let result = if let Some(session) = session {
                session.find(&plan, host.clone(), workers)
            } else {
                plan.run_parallel(plan.live_source(), host.clone(), workers)
            };
            match result {
                Ok(outcome) => status = if outcome.errors == 0 { 0 } else { 1 },
                Err(error) => diagnostic(
                    &request.id,
                    "runtime",
                    "error",
                    Some(error.to_string().as_bytes()),
                )?,
            }
        }
        Err(error) => {
            diagnostic(&request.id, "parse", "error", None)?;
            let _ = error;
        }
    }
    event(request, "end", |o| {
        o.int("exit", status)
            .bool("cancelled", false)
            .int("elapsed_us", started.elapsed().as_micros() as i128);
    })
}

#[derive(Clone)]
struct FrameOutput<'a> {
    id: &'a str,
    record: u64,
}
impl Effects for FrameOutput<'_> {
    fn print(&mut self, path: &Path, nul: bool) -> io::Result<()> {
        let mut bytes = path.as_os_str().as_bytes().to_vec();
        bytes.push(if nul { 0 } else { b'\n' });
        self.emit(&bytes)
    }
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.emit(bytes)
    }
    fn error(&mut self, error: &WalkError) {
        let _ = diagnostic(
            self.id,
            "walk",
            "error",
            Some(error.path.as_os_str().as_bytes()),
        );
    }
    fn output(&mut self, buffer: &mut OutputBuffer) -> io::Result<()> {
        let mut bytes = Vec::new();
        buffer.write_to(&mut bytes)?;
        self.emit(&bytes)
    }
}
impl FrameOutput<'_> {
    fn emit(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.record += 1;
        for (part, chunk) in bytes.chunks(PART).enumerate() {
            frame(
                self.id,
                self.record,
                part as u64,
                chunk,
                (part + 1) * PART >= bytes.len(),
            )?;
        }
        Ok(())
    }
}

fn frame(id: &str, record: u64, part: u64, bytes: &[u8], last: bool) -> io::Result<()> {
    let mut line = Vec::new();
    let mut object = Object::new(&mut line);
    object
        .str("id", id)
        .str("event", "stdout")
        .int("record", record)
        .int("part", part)
        .bool("last", last)
        .str("bytes_base64", &base64(bytes));
    object.end();
    send(&line)
}
fn base64(bytes: &[u8]) -> String {
    let mut out = Vec::new();
    crate::json::encode_base64(&mut out, bytes);
    String::from_utf8(out).unwrap_or_default()
}
fn generation(object: &mut Object<'_>, value: Option<ferret_catalog::Generation>) {
    if let Some(value) = value {
        object.object("generation", |o| {
            o.str("incarnation", &hex(&value.incarnation))
                .int("checkpoint", value.checkpoint)
                .int("sequence", value.sequence);
        });
    } else {
        object.null("generation");
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn event(id: &Request, name: &str, fill: impl FnOnce(&mut Object<'_>)) -> io::Result<()> {
    let mut line = Vec::new();
    let mut o = Object::new(&mut line);
    o.str("id", &id.id).str("event", name);
    fill(&mut o);
    o.end();
    send(&line)
}
fn diagnostic(id: &str, code: &str, severity: &str, path: Option<&[u8]>) -> io::Result<()> {
    let mut line = Vec::new();
    let mut o = Object::new(&mut line);
    o.str("id", id)
        .str("event", "diagnostic")
        .str("code", code)
        .str("severity", severity);
    if let Some(path) = path {
        o.bytes("path", path);
    }
    o.end();
    send(&line)
}
fn emit_request_error(id: Option<&str>, message: &str) -> io::Result<()> {
    let mut line = Vec::new();
    let mut o = Object::new(&mut line);
    if let Some(id) = id {
        o.str("id", id);
    } else {
        o.null("id");
    }
    o.str("event", "error").str("message", message);
    o.end();
    send(&line)
}
fn send(line: &[u8]) -> io::Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(line)?;
    out.write_all(b"\n")?;
    out.flush()
}
fn log_search(
    dirs: Option<&Dirs>,
    request: &Request,
    at: SystemTime,
    exit: u8,
    rows: u64,
    elapsed: i128,
) {
    if let Some(dirs) = dirs {
        let mut line = Vec::new();
        let mut o = crate::log::line(&mut line, "search", at);
        o.byte_strings("query", request.args.iter().map(Vec::as_slice))
            .opt_int("limit", request.limit)
            .int("exit", exit)
            .int("rows", rows)
            .int("total_us", elapsed);
        o.end();
        let _ = crate::log::append(&dirs.state, &line);
    }
}
