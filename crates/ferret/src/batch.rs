//! Sequential JSON-lines host for one resident engine.

use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::ops::ControlFlow;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use ferret_query::find::Plan;

use ferret_query::{Query, Row};

use crate::cli;
use crate::engine::{Engine, QuerySession};
use crate::find_json::{FrameOutput, diagnostic_to, generation};
use crate::json::Object;
use crate::protocol::{self, Op, Request};
use crate::search::json_row;
use crate::transport::Destination;
use crate::xdg::Dirs;

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
        Some(file) => process(io::BufReader::new(file), false),
        None => process(io::stdin().lock(), true),
    };
    if let Err(error) = result {
        cli::error(&format!("batch: {error}"));
        cli::Exit::Error
    } else {
        cli::Exit::Ok
    }
}

fn process(reader: impl BufRead, protocol_stdin: bool) -> io::Result<()> {
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
            let id = protocol::recover_id(&line);
            emit_request_error(id.as_deref(), "LineTooLong")?;
            continue;
        }
        match protocol::parse_request(&line) {
            Ok(request) => handle(
                &request,
                &launch_cwd,
                &mut engine,
                dirs.as_ref(),
                protocol_stdin,
            )?,
            Err(error) => emit_request_error(error.id.as_deref(), &error.kind.to_string())?,
        }
    }
    Ok(())
}

fn open_engine(index: &Path) -> Option<Engine> {
    Engine::open(index).ok().flatten()
}

pub(crate) fn read_line_bounded(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
) -> io::Result<usize> {
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
    protocol_stdin: bool,
) -> io::Result<()> {
    match request.op {
        Op::Index | Op::RootsRemove => request_error(
            &Destination::Stdout,
            Some(&request.id),
            "writer commands require the daemon socket",
        ),
        Op::Status => {
            let mut line = Vec::new();
            let mut object = Object::new(&mut line);
            object.str("id", &request.id).str("event", "status");
            if let Some(engine) = engine {
                let session = engine.pin();
                generation(&mut object, Some(session.generation()));
                object
                    .int("bytes", session.resident_bytes())
                    .int("engine_opens", Engine::open_count());
            } else {
                object
                    .null("generation")
                    .int("bytes", 0)
                    .int("engine_opens", Engine::open_count());
            }
            object.end();
            send(&line)
        }
        Op::Reload => {
            let next = std::env::var_os("FERRET_INDEX")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| dirs.map(|dirs| dirs.data.clone()));
            // A lazy open reads only the header and log, so an unchanged
            // generation costs no resident rebuild.
            if let Some(path) = next.as_deref()
                && let Ok(Some(peek)) = ferret_catalog::Catalog::open(path)
                && engine
                    .as_ref()
                    .is_none_or(|current| current.generation() != peek.generation())
                && let Ok(Some(reloaded)) = Engine::open(path)
            {
                *engine = Some(reloaded);
            }
            event(request, "reload", |o| {
                if let Some(engine) = engine {
                    generation(o, Some(engine.generation()));
                } else {
                    o.null("generation");
                }
                o.int("engine_opens", Engine::open_count());
            })
        }
        Op::Search => search_request(
            request,
            engine.as_ref().map(Engine::pin),
            dirs,
            &Destination::Stdout,
        ),
        Op::Find => find_request(
            request,
            launch_cwd,
            engine.as_ref().map(Engine::pin),
            protocol_stdin,
            &Destination::Stdout,
            ferret_crawl::default_workers(),
        ),
    }
}

pub(crate) fn search_request(
    request: &Request,
    session: Option<QuerySession>,
    dirs: Option<&Dirs>,
    destination: &Destination,
) -> io::Result<()> {
    let started = Instant::now();
    let now = request.start_unix_ns.map_or_else(SystemTime::now, |ns| {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_nanos(ns)
    });
    event_to(destination, request, "begin", |o| {
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
    let mut first_row = None;
    let mut stats = None;
    let mut bytes_read = 0u64;
    let mut plan_text = String::new();
    let mut strategy = String::new();
    if let Ok(query) = parsed {
        plan_text = query.explain();
        strategy = format!("{:?}", query.strategy());
        if let Some(session) = session.as_ref() {
            let catalog = session.catalog();
            bytes_read = catalog.bytes_read();
            let mut output_error = None;
            let result = if request.limit == Some(0) {
                Ok(ferret_query::Stats::default())
            } else {
                session.search_until(&query, destination.cancellation(), |row: &Row<'_>| {
                    if destination.cancelled() {
                        return ControlFlow::Break(());
                    }
                    #[cfg(debug_assertions)]
                    if request.capabilities.iter().any(|c| c == "test-panic") {
                        panic!("injected query panic");
                    }
                    let mut bytes = Vec::new();
                    json_row(&mut bytes, catalog, row);
                    let mut output = Vec::new();
                    let mut object = Object::new(&mut output);
                    object.str("id", &request.id).str("event", "row");
                    object.raw_fields(&bytes[1..bytes.len() - 1]);
                    object.end();
                    if let Err(error) = destination.send(&output) {
                        output_error = Some(error);
                        return ControlFlow::Break(());
                    }
                    first_row.get_or_insert_with(|| started.elapsed().as_micros() as i128);
                    rows += 1;
                    if request.limit.is_some_and(|limit| rows >= limit) {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                })
            };
            if let Some(error) = output_error {
                return Err(error);
            }
            match result {
                Ok(result) => {
                    stats = Some(result);
                    status = if rows == 0 { 1 } else { 0 };
                }
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
    if let Some(estimate) = stats.as_ref().and_then(|stats| stats.name_plan) {
        plan_text = format!(
            "{:?}: {} global name candidates, scope rows {:?}; exact evaluation",
            estimate.plan, estimate.hits, estimate.scope_rows
        );
        strategy = format!("{:?}", estimate.plan);
    }
    let elapsed = started.elapsed().as_micros() as i128;
    let log = SearchLog {
        status,
        rows,
        elapsed,
        first_row,
        bytes_read,
        plan: &plan_text,
        strategy: &strategy,
        stats: stats.clone(),
        error: query_error.as_deref(),
    };
    log_search(dirs, request, now, &log);
    event_to(destination, request, "end", |o| {
        o.int("exit", status)
            .int("rows", rows)
            .bool("cancelled", destination.cancelled())
            .int("elapsed_us", elapsed)
            .str("plan", &plan_text)
            .str("strategy", &strategy);
        o.opt_int("first_row_us", first_row)
            .int("bytes_read", bytes_read);
        if let Some(session) = session.as_ref() {
            o.int("names", session.catalog().name_count())
                .int("inodes", session.catalog().inode_count());
        }
        if let Some(stats) = &stats {
            o.object("stats", |o| {
                o.int("candidates", stats.candidates)
                    .int("rows", stats.rows);
            });
        }
        if let Some(error) = query_error.as_deref() {
            o.str("error", error);
        }
    })
}

pub(crate) fn find_request(
    request: &Request,
    launch_cwd: &Path,
    session: Option<QuerySession>,
    protocol_stdin: bool,
    destination: &Destination,
    workers: usize,
) -> io::Result<()> {
    let started = Instant::now();
    let now = request.start_unix_ns.map_or_else(SystemTime::now, |ns| {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_nanos(ns)
    });
    event_to(destination, request, "begin", |o| {
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
    let host = FrameOutput::new(
        &request.id,
        request.child_stdin.unwrap_or(protocol::ChildStdin::Null),
    )
    .with_destination(destination.clone());
    #[cfg(debug_assertions)]
    let host = host.with_panic(
        request
            .capabilities
            .iter()
            .any(|capability| capability == "test-find-panic"),
    );
    let mut query_error = None;
    match parsed {
        Ok(plan) => {
            if let Some(error) = crate::find_json::refusal(&plan, request, protocol_stdin) {
                event_to(destination, request, "end", |o| {
                    o.int("exit", 1)
                        .bool("cancelled", destination.cancelled())
                        .str("error", error.code())
                        .int("elapsed_us", started.elapsed().as_micros() as i128);
                })?;
                return Ok(());
            }
            if plan.permission_warning() {
                diagnostic_to(destination, &request.id, "permission", "warning", None)?;
            }
            if let Some(feature) = plan.unsupported() {
                diagnostic_to(
                    destination,
                    &request.id,
                    &feature.to_string_lossy(),
                    "error",
                    None,
                )?;
                event_to(destination, request, "end", |o| {
                    o.int("exit", 1)
                        .bool("cancelled", destination.cancelled())
                        .str("error", &feature.to_string_lossy())
                        .int("elapsed_us", started.elapsed().as_micros() as i128);
                })?;
                return Ok(());
            }
            let result = if let Some(session) = session {
                session.find(&plan, host.clone(), workers)
            } else {
                plan.run_parallel(plan.live_source(), host.clone(), workers)
            };
            match result {
                Ok(outcome) => status = if outcome.errors == 0 { 0 } else { 1 },
                Err(error) => {
                    diagnostic_to(destination, &request.id, "runtime", "error", None)?;
                    query_error = Some(error.to_string());
                }
            }
        }
        Err(error) => {
            diagnostic_to(destination, &request.id, "parse", "error", None)?;
            query_error = Some(error.to_string());
        }
    }
    host.check_transport()?;
    event_to(destination, request, "end", |o| {
        o.int("exit", status)
            .bool("cancelled", destination.cancelled())
            .int("elapsed_us", started.elapsed().as_micros() as i128);
        if let Some(error) = query_error.as_deref() {
            o.str("error", error);
        }
    })
}

fn event(id: &Request, name: &str, fill: impl FnOnce(&mut Object<'_>)) -> io::Result<()> {
    event_to(&Destination::Stdout, id, name, fill)
}
fn event_to(
    destination: &Destination,
    id: &Request,
    name: &str,
    fill: impl FnOnce(&mut Object<'_>),
) -> io::Result<()> {
    crate::find_json::emit_to(destination, &id.id, name, fill)
}

fn emit_request_error(id: Option<&str>, message: &str) -> io::Result<()> {
    request_error(&Destination::Stdout, id, message)
}
pub(crate) fn request_error(
    destination: &Destination,
    id: Option<&str>,
    message: &str,
) -> io::Result<()> {
    let mut line = Vec::new();
    let mut o = Object::new(&mut line);
    if let Some(id) = id {
        o.str("id", id);
    } else {
        o.null("id");
    }
    o.str("event", "error").str("message", message);
    o.end();
    destination.send(&line)
}
fn send(line: &[u8]) -> io::Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(line)?;
    out.write_all(b"\n")?;
    out.flush()
}
struct SearchLog<'a> {
    status: u8,
    rows: u64,
    elapsed: i128,
    first_row: Option<i128>,
    bytes_read: u64,
    plan: &'a str,
    strategy: &'a str,
    stats: Option<ferret_query::Stats>,
    error: Option<&'a str>,
}

fn log_search(dirs: Option<&Dirs>, request: &Request, at: SystemTime, log: &SearchLog<'_>) {
    let SearchLog {
        status,
        rows,
        elapsed,
        first_row,
        bytes_read,
        plan,
        strategy,
        stats,
        error,
    } = log;
    if let Some(dirs) = dirs {
        let mut line = Vec::new();
        let mut o = crate::log::line(&mut line, "search", at);
        o.byte_strings("query", request.args.iter().map(Vec::as_slice))
            .str("plan", plan)
            .str("strategy", strategy)
            .opt_int("limit", request.limit)
            .int("exit", *status)
            .int("rows", *rows)
            .opt_int("first_row_us", *first_row)
            .int("total_us", *elapsed)
            .int("bytes_read", *bytes_read);
        if let Some(stats) = stats {
            o.object("stats", |o| {
                o.int("candidates", stats.candidates)
                    .int("rows", stats.rows);
            });
        }
        if let Some(error) = error {
            o.str("error", error);
        }
        o.end();
        let _ = crate::log::append(&dirs.state, &line);
    }
}
