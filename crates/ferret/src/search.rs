//! `ferret search`: parse the atoms, run the query over the catalog, and
//! stream each row to stdout as it is found, then log the run.
//!
//! Rows are written through a buffer that is flushed after every row when
//! stdout is a terminal and in 64 KiB blocks otherwise, as grep does. A
//! closed pipe (`ferret search x | head`) stops the query quietly.

use std::ffi::OsString;
use std::io::{self, BufWriter, IsTerminal, Write};
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::time::{Duration, Instant, SystemTime};

use ferret_catalog::{Catalog, Kind};
use ferret_index::Certainty;
use ferret_query::{ContentReport, Query, Row, RunError, Side, Stats, UNCOVERED_BOUND};

use crate::cli::{Context, Exit, error};
use crate::engine::Engine;
use crate::json::Object;

/// What one run did, for the log.
#[derive(Default)]
struct Outcome {
    stats: Option<Stats>,
    rows: u64,
    first_row: Option<Duration>,
    /// Name and inode counts of the catalog searched.
    size: Option<(u32, u32)>,
    bytes_read: u64,
    error: Option<&'static str>,
}

/// Runs `search` and returns its exit status: [`Exit::Ok`] when it printed a
/// row, [`Exit::NoMatch`] when it printed none.
///
/// Content queries use the daemon's paired pin, with a read-only local
/// fallback.
pub fn run(
    context: &Context,
    atoms: &[OsString],
    json: bool,
    limit: Option<u64>,
    scan_uncovered: bool,
) -> Exit {
    let started = Instant::now();
    let now = SystemTime::now();
    let query = match Query::from_args(atoms.iter().map(|a| a.as_bytes()), now) {
        Ok(query) => query,
        Err(e) => {
            error(&e.to_string());
            return Exit::Usage;
        }
    };
    if let Some(exit) = crate::daemon::search(context, atoms, json, limit, now, scan_uncovered) {
        return exit;
    }
    let mut outcome = Outcome::default();
    let exit = search(
        context,
        &query,
        Options {
            json,
            limit,
            scan_uncovered,
        },
        started,
        &mut outcome,
    );
    let total = started.elapsed();

    let mut line = Vec::new();
    let mut object = crate::log::line(&mut line, "search", now);
    object
        .byte_strings("query", atoms.iter().map(|a| a.as_bytes()))
        .str("plan", &plan_text(&query, outcome.stats.as_ref()))
        .str(
            "strategy",
            &outcome
                .stats
                .as_ref()
                .and_then(|stats| stats.name_plan)
                .map_or_else(
                    || format!("{:?}", query.strategy()),
                    |estimate| format!("{:?}", estimate.plan),
                ),
        )
        .opt_int("limit", limit)
        .int("exit", exit as u8)
        .int("rows", outcome.rows)
        .opt_int(
            "first_row_us",
            outcome.first_row.map(|d| d.as_micros() as i128),
        )
        .int("total_us", total.as_micros() as i128)
        .int("bytes_read", outcome.bytes_read);
    if let Some(stats) = &outcome.stats {
        object.object("stats", |o| {
            o.int("candidates", stats.candidates)
                .int("rows", stats.rows);
        });
        if let Some(content) = &stats.content {
            log_content(&mut object, content);
        }
    }
    if let Some((names, inodes)) = outcome.size {
        object.int("names", names).int("inodes", inodes);
    }
    if let Some(error) = outcome.error {
        object.str("error", error);
    }
    object.end();
    context.log(&line);
    exit
}

/// The query log's `content` object: the driver side, each `text:` atom's
/// estimate and certainty in query order, the uncovered and live counts, and
/// how many documents were probed, verified, and found changed. Counts only:
/// no term, path or id.
fn log_content(object: &mut Object<'_>, content: &ContentReport) {
    object.object("content", |o| {
        o.str(
            "driver",
            match content.driver {
                Side::Content => "content",
                Side::Names => "names",
            },
        )
        .objects("atoms", &content.atoms, |o, atom| {
            o.int("estimate", atom.estimate).str(
                "certainty",
                match atom.certainty {
                    Certainty::Yes => "yes",
                    Certainty::Maybe => "maybe",
                },
            );
        })
        .int("uncovered", content.uncovered)
        .int("live", content.live)
        .int("documents", content.documents)
        .int("verified", content.verified)
        .int("changed", content.changed);
    });
}

/// The query log's `plan`: S1's name plan when one ran, else the static
/// [`Query::explain`], then the content plan as it ran (driver, estimates,
/// coverage, verification) when the query had a `text:` atom.
fn plan_text(query: &Query, stats: Option<&Stats>) -> String {
    let mut text = stats.and_then(|stats| stats.name_plan).map_or_else(
        || query.explain(),
        |estimate| {
            format!(
                "{:?}: {} global name candidates, scope rows {:?}; exact evaluation",
                estimate.plan, estimate.hits, estimate.scope_rows
            )
        },
    );
    if let Some(content) = stats.and_then(|stats| stats.content.as_ref()) {
        text.push_str("; content: ");
        text.push_str(&content.describe());
    }
    text
}

/// `search`'s flags.
struct Options {
    json: bool,
    limit: Option<u64>,
    scan_uncovered: bool,
}

fn search(
    context: &Context,
    query: &Query,
    options: Options,
    started: Instant,
    outcome: &mut Outcome,
) -> Exit {
    let Options {
        json,
        limit,
        scan_uncovered,
    } = options;
    let engine = match Engine::open(&context.index) {
        Ok(Some(engine)) => engine,
        Ok(None) => {
            error(&format!(
                "no index in {}: run `ferret index DIR` first",
                context.index.display()
            ));
            outcome.error = Some("no index");
            return Exit::Error;
        }
        Err(e) => {
            error(&crate::index::open_failed(context, &e));
            outcome.error = Some("open");
            return Exit::Error;
        }
    };
    if query.has_content()
        && let Err(e) = engine.open_content(&context.index)
    {
        error(&format!("{}: content index: {e}", context.index.display()));
        outcome.error = Some("content index");
        return Exit::Error;
    }
    let session = engine.pin();
    let catalog = session.catalog();
    outcome.size = Some((catalog.name_count(), catalog.inode_count()));

    let stdout = io::stdout();
    let tty = stdout.is_terminal();
    let mut out = BufWriter::with_capacity(64 << 10, stdout.lock());
    let mut line = Vec::new();
    let mut failed: Option<io::Error> = None;
    let result = session.search_content(
        query,
        (!scan_uncovered).then_some(UNCOVERED_BOUND),
        None,
        |row| {
            line.clear();
            if json {
                json_row(&mut line, catalog, row);
            } else {
                line.extend_from_slice(row.path);
            }
            line.push(b'\n');
            outcome.first_row.get_or_insert_with(|| started.elapsed());
            outcome.rows += 1;
            let written = out.write_all(&line).and_then(|()| match tty {
                true => out.flush(),
                false => Ok(()),
            });
            if let Err(e) = written {
                failed = Some(e);
                return ControlFlow::Break(());
            }
            match limit {
                Some(limit) if outcome.rows >= limit => ControlFlow::Break(()),
                _ => ControlFlow::Continue(()),
            }
        },
    );
    if failed.is_none() {
        failed = out.flush().err();
    }
    outcome.bytes_read = catalog.bytes_read();
    match result {
        Ok(stats) => outcome.stats = Some(stats),
        Err(RunError::IndexIncomplete { uncovered, live }) => {
            error(&incomplete_message(uncovered, live));
            outcome.error = Some("index incomplete");
            return Exit::Error;
        }
        Err(e) => {
            error(&format!("{}: {e}", context.index.display()));
            outcome.error = Some("read");
            return Exit::Error;
        }
    }
    match failed {
        // The reader went away; what it read was delivered.
        Some(e) if e.kind() == io::ErrorKind::BrokenPipe => {}
        Some(e) => {
            error(&format!("writing results: {e}"));
            outcome.error = Some("write");
            return Exit::Error;
        }
        None => {}
    }
    if outcome.rows > 0 {
        Exit::Ok
    } else {
        Exit::NoMatch
    }
}

/// One `--json` row: `path` (and `path_base64` when it is not UTF-8; see
/// [`crate::json`]), `type`, `size` in bytes, `mtime` in Unix seconds, and
/// `doc`, the content's document id, or null when the file has none
/// (a directory, a symlink, a binary or unread file). Document ids are
/// stable across index runs (D4); inode and name ids are not (D27), so
/// they are not printed.
pub(crate) fn json_row(out: &mut Vec<u8>, catalog: &Catalog, row: &Row<'_>) {
    let kind = match row.kind {
        Kind::Dir => "dir",
        Kind::File => "file",
        Kind::Symlink => "symlink",
        Kind::Fifo => "fifo",
        Kind::Socket => "socket",
        Kind::Block => "block",
        Kind::Character => "character",
    };
    let mut object = Object::new(out);
    object
        .bytes("path", row.path)
        .str("type", kind)
        .int("size", catalog.size(row.inode))
        .int("mtime", catalog.mtime(row.inode))
        .opt_int("doc", catalog.doc(row.inode).map(|d| d.0));
    object.end();
}

/// Shared native diagnostic for local and socket content queries.
pub(crate) fn incomplete_message(uncovered: u32, live: u32) -> String {
    format!(
        "the content index does not yet cover {uncovered} of {live} documents, and reading that many is slow: pass --scan-uncovered to read them anyway"
    )
}
