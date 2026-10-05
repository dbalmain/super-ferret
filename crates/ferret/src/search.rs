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

use ferret_catalog::{Catalog, Kind, Section};
use ferret_query::{Query, Row, Stats};

use crate::cli::{Context, Exit, error};
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
pub fn run(context: &Context, atoms: &[OsString], json: bool, limit: Option<u64>) -> Exit {
    let started = Instant::now();
    let now = SystemTime::now();
    let query = match Query::from_args(atoms.iter().map(|a| a.as_bytes()), now) {
        Ok(query) => query,
        Err(e) => {
            error(&e.to_string());
            return Exit::Usage;
        }
    };
    let mut outcome = Outcome::default();
    let exit = search(context, &query, json, limit, started, &mut outcome);
    let total = started.elapsed();

    let mut line = Vec::new();
    let mut object = crate::log::line(&mut line, "search", now);
    object
        .byte_strings("query", atoms.iter().map(|a| a.as_bytes()))
        .str("plan", &query.explain())
        .str("strategy", &format!("{:?}", query.strategy()))
        .opt_int("limit", limit)
        .int("exit", exit as u8)
        .int("rows", outcome.rows)
        .opt_int(
            "first_row_us",
            outcome.first_row.map(|d| d.as_micros() as i128),
        )
        .int("total_us", total.as_micros() as i128)
        .int("bytes_read", outcome.bytes_read);
    if let Some(stats) = outcome.stats {
        object.object("stats", |o| {
            o.int("candidates", stats.candidates)
                .int("rows", stats.rows);
        });
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

fn search(
    context: &Context,
    query: &Query,
    json: bool,
    limit: Option<u64>,
    started: Instant,
    outcome: &mut Outcome,
) -> Exit {
    let catalog = match Catalog::open(&context.index) {
        Ok(Some(catalog)) => catalog,
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
    outcome.size = Some((catalog.name_count(), catalog.inode_count()));
    // A plain listing prints only paths, so it reads no inode column.
    if json && let Err(e) = catalog.load(&JSON_SECTIONS) {
        error(&format!("{}: {e}", context.index.display()));
        outcome.error = Some("read");
        return Exit::Error;
    }

    let stdout = io::stdout();
    let tty = stdout.is_terminal();
    let mut out = BufWriter::with_capacity(64 << 10, stdout.lock());
    let mut line = Vec::new();
    let mut failed: Option<io::Error> = None;
    let result = query.run(&catalog, |row| {
        line.clear();
        if json {
            json_row(&mut line, &catalog, row);
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
    });
    if failed.is_none() {
        failed = out.flush().err();
    }
    outcome.bytes_read = catalog.bytes_read();
    match result {
        Ok(stats) => outcome.stats = Some(stats),
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

/// What [`json_row`] reads beyond the row itself.
const JSON_SECTIONS: [Section; 3] = [Section::Size, Section::Mtime, Section::Doc];

/// One `--json` row: `path` (and `path_base64` when it is not UTF-8; see
/// [`crate::json`]), `type`, `size` in bytes, `mtime` in Unix seconds, and
/// `doc`, the content's document id, or null when the file has none
/// (a directory, a symlink, a binary or unread file). Document ids are
/// stable across index runs (D4); inode and name ids are not (D27), so
/// they are not printed.
fn json_row(out: &mut Vec<u8>, catalog: &Catalog, row: &Row<'_>) {
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
