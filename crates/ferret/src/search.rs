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
use ferret_query::{
    ContentPlan, ContentReport, Query, Row, RunError, Side, Stats, UNCOVERED_BOUND,
};

use crate::cli::{Context, Exit, error};
use crate::engine::Engine;
use crate::json::Object;
use crate::protocol::Value;

/// What one run did, for the log.
#[derive(Default)]
struct Outcome {
    stats: Option<Stats>,
    /// The content plan of a query refused because it needed a scan.
    refused: Option<ContentPlan>,
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
    let summary = Summary {
        plan: Plan::new(&query, outcome.stats.as_ref(), outcome.refused.as_ref()),
        bytes_read: outcome.bytes_read,
        size: outcome.size,
        stats: outcome.stats.as_ref(),
        error: outcome.error,
    };
    let run = Run {
        limit,
        exit: exit as u8,
        rows: outcome.rows,
        first_row_us: outcome.first_row.map(|d| d.as_micros() as i128),
        total_us: started.elapsed().as_micros() as i128,
    };
    let line = log_line(now, atoms.iter().map(|a| a.as_bytes()), &run, |o| {
        summary.fields(o);
    });
    context.log(&line);
    exit
}

/// How one query ran on the host that answered the user: the part of its
/// log line that only that host can measure.
pub(crate) struct Run {
    pub limit: Option<u64>,
    pub exit: u8,
    pub rows: u64,
    pub first_row_us: Option<i128>,
    pub total_us: i128,
}

/// One `search` query-log line: the query and how it [`Run`], then the
/// summary, which `summary` writes. A host that answered the query itself
/// passes [`Summary::fields`]; the socket client names itself, adds the
/// daemon's time and copies the daemon's summary with [`copy_summary`].
/// Every query gets one line, from the host the user ran.
pub(crate) fn log_line<'q>(
    at: SystemTime,
    query: impl IntoIterator<Item = &'q [u8]>,
    run: &Run,
    summary: impl FnOnce(&mut Object<'_>),
) -> Vec<u8> {
    let mut line = Vec::new();
    let mut object = crate::log::line(&mut line, "search", at);
    object
        .byte_strings("query", query)
        .opt_int("limit", run.limit)
        .int("exit", run.exit)
        .int("rows", run.rows)
        .opt_int("first_row_us", run.first_row_us)
        .int("total_us", run.total_us);
    summary(&mut object);
    object.end();
    line
}

/// What one search reports about its plan and work, wherever it ran: the
/// local query log, and batch's `end` event and log, which the socket
/// client copies into its own. Each host keeps its own `error` wording:
/// short labels locally, the diagnostic itself from batch.
pub(crate) struct Summary<'a> {
    pub plan: Plan,
    pub bytes_read: u64,
    /// Name and inode counts of the catalog searched.
    pub size: Option<(u32, u32)>,
    pub stats: Option<&'a Stats>,
    pub error: Option<&'a str>,
}

/// The fields [`Summary::fields`] can write, which [`copy_summary`] copies.
const SUMMARY_FIELDS: [&str; 8] = [
    "plan",
    "strategy",
    "bytes_read",
    "names",
    "inodes",
    "stats",
    "content",
    "error",
];

impl Summary<'_> {
    /// Writes the summary's fields into `object`, the names in
    /// [`SUMMARY_FIELDS`].
    pub(crate) fn fields(&self, object: &mut Object<'_>) {
        object
            .str("plan", &self.plan.text)
            .str("strategy", &self.plan.strategy)
            .int("bytes_read", self.bytes_read);
        if let Some((names, inodes)) = self.size {
            object.int("names", names).int("inodes", inodes);
        }
        if let Some(stats) = self.stats {
            object.object("stats", |o| {
                o.int("candidates", stats.candidates)
                    .int("rows", stats.rows);
            });
            if let Some(content) = &stats.content {
                log_content(object, content);
            }
        }
        if let Some(error) = self.error {
            object.str("error", error);
        }
    }
}

/// Copies the summary fields of a daemon's `end` event, as the daemon
/// encoded them, so the socket log carries the whole summary rather than a
/// second list of its parts.
pub(crate) fn copy_summary(object: &mut Object<'_>, end: &Value) {
    for key in SUMMARY_FIELDS {
        if let Some(value) = end.field(key) {
            object.value(key, value);
        }
    }
}

/// The query log's `content` object: the driver side, each `text:` atom's
/// estimate and certainty in query order, the uncovered and live counts, and
/// how many documents were probed, verified, and found changed. Counts only:
/// no term, path or id.
pub(crate) fn log_content(object: &mut Object<'_>, content: &ContentReport) {
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
        .int("changed", content.changed)
        .int("possible", content.bounds.possible)
        .int("maybe", content.bounds.maybe)
        .int("snapshot_maybe", content.snapshot_maybe)
        .int("regex_documents", content.matching.regex_documents)
        .int("regex_bytes", content.matching.regex_bytes)
        .int("regex_rejected", content.matching.regex_rejected)
        .int(
            "verification_us",
            content.verification_time.as_micros() as i128,
        )
        .int("regex_us", content.regex_time.as_micros() as i128)
        .int("verify_threads", content.threads as u64);
    });
}

/// The query log's `plan` and `strategy`. Empty when the query did not
/// parse.
#[derive(Default)]
pub(crate) struct Plan {
    text: String,
    strategy: String,
}

impl Plan {
    /// The plan is S1's name plan when one ran, else the static
    /// [`Query::explain`], then the content plan as it ran (driver,
    /// estimates, coverage, verification) when the query had a `text:` atom,
    /// or the content plan that was `refused` because it needed a scan.
    /// The strategy is the name plan that ran, else the query's own.
    pub(crate) fn new(query: &Query, stats: Option<&Stats>, refused: Option<&ContentPlan>) -> Self {
        let name_plan = stats.and_then(|stats| stats.name_plan);
        let mut text = name_plan.map_or_else(
            || query.explain(),
            |estimate| {
                format!(
                    "{:?}: {} global name candidates, scope rows {:?}; exact evaluation",
                    estimate.plan, estimate.hits, estimate.scope_rows
                )
            },
        );
        let content = stats
            .and_then(|stats| stats.content.as_ref())
            .map(ContentReport::describe)
            .or_else(|| refused.map(ContentPlan::describe));
        if let Some(content) = content {
            text.push_str("; content: ");
            text.push_str(&content);
        }
        let strategy = name_plan.map_or_else(
            || format!("{:?}", query.strategy()),
            |estimate| format!("{:?}", estimate.plan),
        );
        Self { text, strategy }
    }
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
        Err(scan_error @ RunError::ScanRequired { .. }) => {
            outcome.refused = session.explain_content(query).ok();
            error(&scan_error.to_string());
            outcome.error = Some("scan required");
            return Exit::Error;
        }
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
    if let Some(offset) = row.offset {
        object.int("offset", offset as u64);
    }
    object.end();
}

/// Shared native diagnostic for local and socket content queries.
pub(crate) fn incomplete_message(uncovered: u32, live: u32) -> String {
    format!(
        "the content index does not yet cover {uncovered} of {live} documents, and reading that many is slow: pass --scan-uncovered to read them anyway"
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ferret_query::{AtomReport, Bounds, NameEstimate, NamePlan};

    use super::*;

    fn query(atom: &str) -> Query {
        Query::from_args([atom.as_bytes()], SystemTime::UNIX_EPOCH).unwrap()
    }

    fn encode(fill: impl FnOnce(&mut Object<'_>)) -> String {
        let mut out = Vec::new();
        let mut object = Object::new(&mut out);
        fill(&mut object);
        object.end();
        String::from_utf8(out).unwrap()
    }

    fn content_report() -> ContentReport {
        let bounds = Bounds {
            possible: 3,
            maybe: 1,
        };
        ContentReport {
            driver: Side::Content,
            bounds,
            plan: ContentPlan {
                root: bounds,
                nodes: Vec::new(),
                greps: Vec::new(),
            },
            snapshot_maybe: 1,
            snapshot_possible: 3,
            verification_time: Duration::from_micros(40),
            regex_time: Duration::from_micros(5),
            atoms: vec![AtomReport {
                estimate: 3,
                certainty: Certainty::Maybe,
            }],
            uncovered: 0,
            live: 7,
            documents: 3,
            verified: 1,
            changed: 0,
            matching: Default::default(),
            threads: 2,
        }
    }

    #[test]
    // Each host's summary survives the daemon's `end` event into the socket
    // log unchanged: copy_summary names every field Summary::fields writes,
    // and keeps none of the transport fields around them.
    fn the_socket_copy_of_a_summary_is_the_summary() {
        let name_only = Stats {
            candidates: 4,
            rows: 2,
            name_plan: Some(NameEstimate {
                plan: NamePlan::Postings,
                hits: 4,
                scope_rows: None,
            }),
            content: None,
        };
        let content = Stats {
            candidates: 3,
            rows: 1,
            name_plan: None,
            content: Some(content_report()),
        };
        let refused = content_report().plan;
        let (names, text) = (query("*.rs"), query("text:main"));
        let cases = [
            Summary {
                plan: Plan::new(&names, Some(&name_only), None),
                bytes_read: 10,
                size: Some((5, 6)),
                stats: Some(&name_only),
                error: None,
            },
            Summary {
                plan: Plan::new(&text, Some(&content), None),
                bytes_read: 20,
                size: Some((5, 6)),
                stats: Some(&content),
                error: None,
            },
            Summary {
                plan: Plan::new(&text, None, Some(&refused)),
                bytes_read: 30,
                size: Some((5, 6)),
                stats: None,
                error: Some("scan required"),
            },
            Summary {
                plan: Plan::default(),
                bytes_read: 0,
                size: None,
                stats: None,
                error: Some("unbalanced parenthesis"),
            },
        ];
        for summary in &cases {
            let expected = encode(|o| summary.fields(o));
            let end = encode(|o| {
                o.str("event", "end").int("exit", 0).int("elapsed_us", 9);
                summary.fields(o);
                o.int("workers", 2);
            });
            let end = crate::protocol::parse_object(end.as_bytes()).unwrap();
            assert_eq!(encode(|o| copy_summary(o, &end)), expected);
        }
        let all = encode(|o| cases[1].fields(o));
        assert!(all.contains(r#""content":{"driver":"content","#), "{all}");
        assert!(all.contains(r#""verification_us":40,"#), "{all}");
    }

    #[test]
    // The plan names the name plan that ran, and a query refused for a scan
    // still logs the content plan it would have run.
    fn plans_name_what_ran_or_what_was_refused() {
        let ran = Stats {
            name_plan: Some(NameEstimate {
                plan: NamePlan::ScopeWalk,
                hits: 4,
                scope_rows: Some(9),
            }),
            ..Stats::default()
        };
        let names = query("*.rs");
        let plan = Plan::new(&names, Some(&ran), None);
        assert_eq!(
            plan.text,
            "ScopeWalk: 4 global name candidates, scope rows Some(9); exact evaluation"
        );
        assert_eq!(plan.strategy, "ScopeWalk");
        let unplanned = Plan::new(&names, None, None);
        assert_eq!(unplanned.text, names.explain());
        assert_eq!(unplanned.strategy, format!("{:?}", names.strategy()));

        let text = query("text:main");
        let refused = content_report().plan;
        let plan = Plan::new(&text, None, Some(&refused));
        assert_eq!(
            plan.text,
            format!("{}; content: {}", text.explain(), refused.describe())
        );
    }
}
