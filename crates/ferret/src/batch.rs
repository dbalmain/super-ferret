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
use crate::find::FindSource;
use crate::find_json::{Failure, FrameOutput, RawStdout, generation};
use crate::json::Object;
use crate::protocol::{self, Op, Request};
use crate::search::{self, Run, Summary, json_row};
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
    let engine = Engine::open(index).ok().flatten()?;
    engine.open_content(index).ok()?;
    Some(engine)
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
            // The manifest generation is enough to see that the resident
            // engine is stale. The snapshot and change log open only when
            // Engine::open actually reloads.
            if let Some(path) = next.as_deref()
                && let Ok(Some(peek)) = ferret_catalog::Catalog::published_generation(path)
                && engine
                    .as_ref()
                    .is_none_or(|current| current.generation() != peek)
                && let Ok(Some(reloaded)) = Engine::open(path)
            {
                *engine = Some(reloaded);
            }
            if let (Some(path), Some(current)) = (next.as_deref(), engine.as_ref()) {
                current.open_content(path).map_err(io::Error::other)?;
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
            None,
        ),
        Op::Grep => grep_request(
            request,
            engine.as_ref().map(Engine::pin),
            &Destination::Stdout,
            1,
            None,
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
    workers: Option<usize>,
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
    let mut read_error = false;
    let mut first_row = None;
    let mut stats = None;
    let mut bytes_read = 0u64;
    let mut refused = None;
    if let Ok(query) = &parsed {
        if let Some(session) = session.as_ref() {
            let catalog = session.catalog();
            bytes_read = catalog.bytes_read();
            let mut output_error = None;
            // JSON rows reuse these buffers. Plain search appends `path\n`
            // to find's 64 KiB frame buffer and does not build a row object.
            // `rows` counts paths; a newline inside a path is not a row.
            let mut encoded = Vec::with_capacity(256);
            let mut framed = Vec::with_capacity(320);
            let mut raw_out = RawStdout::new(1);
            let raw = destination.raw_output();
            let result = if request.limit == Some(0) {
                Ok(ferret_query::Stats::default())
            } else {
                let bound = (!request
                    .capabilities
                    .iter()
                    .any(|c| c == "scan" || c == "scan-uncovered"))
                .then_some(ferret_query::UNCOVERED_BOUND);
                let threads = workers.unwrap_or_else(|| session.search_threads());
                session.search_content_threads(
                    query,
                    bound,
                    threads,
                    destination.cancellation(),
                    |row: &Row<'_>| {
                        if destination.cancelled() {
                            return ControlFlow::Break(());
                        }
                        #[cfg(debug_assertions)]
                        if request.capabilities.iter().any(|c| c == "test-panic") {
                            panic!("injected query panic");
                        }
                        if raw {
                            if let Err(error) = raw_out
                                .write(destination, &request.id, row.path)
                                .and_then(|()| raw_out.write(destination, &request.id, b"\n"))
                                // The first path goes out alone, as the first
                                // JSON row does; later paths fill parts.
                                .and_then(|()| {
                                    if rows == 0 {
                                        raw_out.send_pending(destination, &request.id)
                                    } else {
                                        Ok(())
                                    }
                                })
                            {
                                output_error = Some(error);
                                return ControlFlow::Break(());
                            }
                        } else {
                            encoded.clear();
                            json_row(&mut encoded, catalog, row);
                            framed.clear();
                            let mut object = Object::new(&mut framed);
                            object.str("id", &request.id).str("event", "row");
                            object.raw_fields(&encoded[1..encoded.len() - 1]);
                            object.end();
                            if let Err(error) = destination.send(&framed) {
                                output_error = Some(error);
                                return ControlFlow::Break(());
                            }
                        }
                        first_row.get_or_insert_with(|| started.elapsed().as_micros() as i128);
                        rows += 1;
                        if request.limit.is_some_and(|limit| rows >= limit) {
                            ControlFlow::Break(())
                        } else {
                            ControlFlow::Continue(())
                        }
                    },
                )
            };
            if raw
                && output_error.is_none()
                && let Err(error) = raw_out.finish(destination, &request.id)
            {
                return Err(error);
            }
            if let Some(error) = output_error {
                return Err(error);
            }
            match result {
                Ok(result) => {
                    stats = Some(result);
                    status = if rows == 0 { 1 } else { 0 };
                }
                Err(error @ ferret_query::RunError::ScanRequired { .. }) => {
                    refused = session.explain_content(query).ok();
                    query_error = Some(error.to_string());
                }
                Err(ferret_query::RunError::IndexIncomplete { uncovered, live }) => {
                    query_error = Some(crate::search::incomplete_message(uncovered, live));
                }
                Err(error) => {
                    read_error = true;
                    query_error = Some(error.to_string());
                }
            }
        } else {
            query_error = Some("cannot open index".to_owned());
        }
    } else {
        status = 2;
        if let Err(error) = &parsed {
            query_error = Some(error.to_string());
        }
    }
    let elapsed = started.elapsed().as_micros() as i128;
    let summary = Summary {
        plan: parsed.as_ref().map_or_else(
            |_| search::Plan::default(),
            |query| search::Plan::new(query, stats.as_ref(), refused.as_ref()),
        ),
        bytes_read,
        size: session.as_ref().map(|session| {
            (
                session.catalog().name_count(),
                session.catalog().inode_count(),
            )
        }),
        stats: stats.as_ref(),
        error: query_error.as_deref(),
    };
    if let Some(dirs) = dirs {
        let run = Run {
            limit: request.limit,
            exit: status,
            rows,
            first_row_us: first_row,
            total_us: elapsed,
        };
        let line =
            crate::search::log_line(now, request.args.iter().map(Vec::as_slice), &run, |o| {
                summary.fields(o);
            });
        let _ = crate::log::append(&dirs.state, &line);
    }
    event_to(destination, request, "end", |o| {
        o.int("exit", status)
            .int("rows", rows)
            .bool("read_error", read_error)
            .bool("cancelled", destination.cancelled())
            .int("elapsed_us", elapsed)
            .opt_int("first_row_us", first_row);
        summary.fields(o);
        if let Some(workers) = workers {
            o.int("workers", workers as u64);
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
    // The batch host's own authority: a plan the request's capabilities or
    // stdin do not permit ends the block refused.
    let plan = match Plan::parse_at(&args, &cwd, now) {
        Ok(plan) => match crate::find_json::refusal(&plan, request, protocol_stdin) {
            Some(refusal) => Err(Failure::refused(refusal)),
            None => Ok(plan),
        },
        Err(error) => Err(Failure::new("parse", error.to_string())),
    };
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
    let source = session.map_or(FindSource::Live, |session| {
        FindSource::Pinned(Box::new(session))
    });
    crate::find_json::run_block(host, source, plan, workers, started).map(drop)
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
/// Grep shares find's byte frames and the connection's cancellation flag.
pub(crate) fn grep_request(
    request: &Request,
    session: Option<QuerySession>,
    destination: &Destination,
    requests: usize,
    workers: Option<usize>,
) -> io::Result<()> {
    event_to(destination, request, "begin", |o| {
        generation(o, session.as_ref().map(QuerySession::generation))
    })?;
    let exit =
        crate::grep::remote::serve(request, session.as_ref(), destination, requests, workers)?;
    event_to(destination, request, "end", |o| {
        o.int("exit", exit as u8)
            .bool("cancelled", destination.cancelled());
        if let Some(workers) = workers {
            o.int("workers", workers as u64);
        }
    })
}
