//! `ferret index` and `ferret roots`: the configured roots are the ones the
//! catalog holds (D34), changed only through an index run, so a root is
//! added or removed exactly when a generation that has or lacks it is
//! published.
//!
//! `index DIR...` adds and refreshes each; bare `index` refreshes them all;
//! `roots remove DIR...` drops each, which re-walks any root it was inside
//! (ferret-crawl widens the run). The change is applied under the writer
//! lock ([`ferret_crawl::index_change`]), so concurrent runs cannot lose
//! each other's roots.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufRead, IsTerminal};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, SystemTime};

use ferret_catalog::{Catalog, OpenError, Section};
use ferret_crawl::{IndexError, IndexOptions, Refresh, Report, RootChange, index_change};
use ferret_policy::DEFAULT_IGNORE;

use crate::cli::{Context, Exit, error, note, print, warn};
use crate::setup::{self, Written};

/// At most this many faults of each kind are printed; the rest are counted.
const SHOWN: usize = 20;

/// `ferret index [DIR...]`.
pub fn index(context: &Context, dirs: &[PathBuf]) -> Exit {
    let mut added = Vec::with_capacity(dirs.len());
    for dir in dirs {
        match new_root(dir) {
            Ok(root) => added.push(root),
            Err(message) => {
                error(&message);
                return Exit::Error;
            }
        }
    }
    if added.is_empty() {
        match configured(context) {
            Ok(roots) if !roots.is_empty() => {}
            Ok(_) => match ask_for_root() {
                Some(Ok(root)) => added.push(root),
                Some(Err(message)) => {
                    error(&message);
                    return Exit::Error;
                }
                None => {
                    error("no roots to index: run `ferret index DIR`");
                    return Exit::Usage;
                }
            },
            Err(e) => {
                error(&format!("{}: {e}", context.index.display()));
                return Exit::Error;
            }
        }
    }
    let refresh = match added.is_empty() {
        true => Refresh::All,
        false => Refresh::Only(&added),
    };
    let change = RootChange {
        add: &added,
        remove: &[],
    };
    run(context, "index", change, refresh)
}

/// `ferret roots remove DIR...`.
pub fn remove(context: &Context, dirs: &[PathBuf]) -> Exit {
    let mut removed = Vec::with_capacity(dirs.len());
    for dir in dirs {
        match std::path::absolute(dir) {
            Ok(path) => removed.push(normalise(&path)),
            Err(e) => {
                error(&format!("{}: {e}", dir.display()));
                return Exit::Error;
            }
        }
    }
    let change = RootChange {
        add: &[],
        remove: &removed,
    };
    run(context, "roots-remove", change, Refresh::Only(&[]))
}

/// `ferret roots list`: each root's path, one per line, as raw bytes.
pub fn list(context: &Context) -> Exit {
    match configured(context) {
        Ok(roots) => {
            let mut text = Vec::new();
            for root in roots {
                text.extend_from_slice(root.as_os_str().as_bytes());
                text.push(b'\n');
            }
            print("roots", &text)
        }
        Err(e) => {
            error(&format!("{}: {e}", context.index.display()));
            Exit::Error
        }
    }
}

/// The roots the current generation holds; none before the first index.
fn configured(context: &Context) -> Result<Vec<PathBuf>, OpenError> {
    let Some(catalog) = Catalog::open(&context.index)? else {
        return Ok(Vec::new());
    };
    catalog.load(&[Section::Roots])?;
    Ok(catalog
        .roots()
        .map(|(_, path)| PathBuf::from(OsStr::from_bytes(path)))
        .collect())
}

/// A directory named on the command line, as the root the catalog stores:
/// absolute, without `.` or trailing slashes, and a directory. A path with
/// `..` is resolved by the kernel (symlinks too), because resolving `..`
/// lexically can name a different directory.
fn new_root(dir: &Path) -> Result<PathBuf, String> {
    let fail = |e: io::Error| format!("{}: {e}", dir.display());
    let absolute = std::path::absolute(dir).map_err(fail)?;
    let root = match absolute.components().any(|c| c == Component::ParentDir) {
        true => fs::canonicalize(&absolute).map_err(fail)?,
        false => normalise(&absolute),
    };
    match fs::metadata(&root) {
        Ok(meta) if meta.is_dir() => Ok(root),
        Ok(_) => Err(format!("{}: not a directory", dir.display())),
        Err(e) => Err(fail(e)),
    }
}

/// `path` without `.` components or a trailing slash, as ferret-crawl
/// compares roots.
fn normalise(path: &Path) -> PathBuf {
    path.components().collect()
}

/// Asks at a terminal for the first root. `None` when stdin or stderr is
/// not a terminal, or the answer is empty.
fn ask_for_root() -> Option<Result<PathBuf, String>> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return None;
    }
    note("ferret: no roots yet. Directory to index (empty to cancel): ");
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer).ok()?;
    let answer = answer.trim_end_matches(['\n', '\r']);
    (!answer.is_empty()).then(|| new_root(Path::new(answer)))
}

/// The global ignore file's text: seeded with the defaults on first use
/// (`setup`), and the built-in defaults when it cannot be read.
fn global_ignore(context: &Context) -> String {
    let Some(dirs) = &context.dirs else {
        warn("no config directory; using the default ignore rules");
        return DEFAULT_IGNORE.to_owned();
    };
    let path = dirs.ignore_file();
    match setup::write_ignore_file(&path) {
        Ok(Written::Created) => {
            note(&format!(
                "ferret: wrote the default ignore rules to {}\n",
                path.display()
            ));
        }
        Ok(Written::Kept) => {}
        Err(e) => warn(&format!("{}: {e}", path.display())),
    }
    match fs::read(&path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) => {
            warn(&format!(
                "{}: {e}; using the default ignore rules",
                path.display()
            ));
            DEFAULT_IGNORE.to_owned()
        }
    }
}

/// One index run: publish, report, log.
fn run(context: &Context, command: &str, change: RootChange<'_>, refresh: Refresh<'_>) -> Exit {
    let started = Instant::now();
    let now = SystemTime::now();
    let options = IndexOptions {
        global: Some(global_ignore(context)),
        ..IndexOptions::default()
    };
    let result = index_change(&context.index, change, refresh, &options);
    let total = started.elapsed();

    // The outcome is decided before anything is printed, and printing
    // cannot panic: a published generation is logged even when the reader
    // of the report has gone away.
    let (exit, outcome) = match &result {
        Ok(report) => {
            print_content_faults(report);
            (
                print("the report", report_text(report).as_bytes()),
                "published",
            )
        }
        Err(IndexError::Coverage { faults, report }) => {
            print_content_faults(report);
            error(&format!(
                "nothing published: {} directory or entry could not be read, so the walk may \
                 have missed entries; the previous index is unchanged",
                faults.len()
            ));
            let mut text = String::new();
            for fault in faults.iter().take(SHOWN) {
                let _ = writeln!(text, "  {fault}");
            }
            if faults.len() > SHOWN {
                let _ = writeln!(text, "  … and {} more", faults.len() - SHOWN);
            }
            if faults.iter().any(|f| f.on_root) {
                text.push_str(
                    "  a root that is gone can be dropped with `ferret roots remove DIR`\n",
                );
            }
            note(&text);
            (Exit::Error, "coverage")
        }
        Err(IndexError::Commit(e)) if e.published() => {
            warn(&e.to_string());
            (Exit::Ok, "undurable")
        }
        Err(e) => {
            error(&e.to_string());
            (Exit::Error, "error")
        }
    };

    let mut line = Vec::new();
    let mut object = crate::log::line(&mut line, command, now);
    object
        .str("outcome", outcome)
        .int("exit", exit as u8)
        .int("total_us", total.as_micros() as i128)
        .opt_int("peak_rss_kb", peak_rss_kb());
    let report = match &result {
        Ok(report) => Some(report),
        Err(IndexError::Coverage { faults, report }) => {
            object.int("coverage_faults", faults.len() as u64);
            Some(&**report)
        }
        Err(_) => None,
    };
    if let Some(report) = report {
        log_report(&mut object, report);
    }
    object.end();
    context.log(&line);
    exit
}

fn log_report(object: &mut crate::json::Object<'_>, report: &Report) {
    let c = &report.counts;
    object
        .int("refreshed", report.refreshed.len() as u64)
        .int("kept", report.kept.len() as u64)
        .int("dropped", report.dropped.len() as u64)
        .int("walk_us", report.walk_time.as_micros() as i128)
        .int("hash_us", report.hash_time.as_micros() as i128)
        .int("commit_us", report.commit_time.as_micros() as i128)
        .int("faults_us", report.fault_time.as_micros() as i128)
        .object("counts", |o| {
            o.int("dirs", c.dirs)
                .int("traversed", c.traversed)
                .int("files", c.files)
                .int("symlinks", c.symlinks)
                .int("indexed", c.indexed)
                .int("carried", c.carried)
                .int("aliased", c.aliased)
                .int("deferred", c.deferred)
                .int("content_faults", c.content_faults)
                .int("files_read", c.files_read)
                .int("bytes_read", c.bytes_read)
                .int("vanished", c.vanished)
                .int("boundaries", c.boundaries)
                .int("pattern_errors", c.pattern_errors)
                .int("cached_inodes", c.cached_inodes);
        });
    if let Some(p) = report.published {
        object.object("published", |o| {
            o.int("dirs", p.dirs)
                .int("inodes", p.inodes)
                .int("names", p.names)
                .int("docs", p.docs);
        });
    }
}

/// The human report of a published run, for stdout. Ignore patterns that
/// were skipped go to stderr as warnings.
fn report_text(report: &Report) -> String {
    let mut text = String::new();
    let mut list = |label: &str, roots: &[PathBuf]| {
        for root in roots {
            let _ = writeln!(text, "{label} {}", root.display());
        }
    };
    list("indexed", &report.refreshed);
    list("kept   ", &report.kept);
    list("dropped", &report.dropped);
    let c = &report.counts;
    let _ = writeln!(
        text,
        "{} directories, {} files, {} symlinks; read {} files ({}), {} unchanged",
        c.dirs,
        c.files,
        c.symlinks,
        c.files_read,
        bytes(c.bytes_read),
        c.carried
    );
    if let Some(p) = report.published {
        let _ = writeln!(
            text,
            "published {} names, {} inodes, {} documents in {:.2} s \
             (walk {:.2} s, commit {:.2} s)",
            p.names,
            p.inodes,
            p.docs,
            (report.walk_time + report.commit_time + report.fault_time).as_secs_f64(),
            report.walk_time.as_secs_f64(),
            report.commit_time.as_secs_f64(),
        );
    }
    for pattern in &report.pattern_errors {
        warn(&format!("ignore pattern skipped: {pattern}"));
    }
    text
}

/// Content faults are warnings: the file is indexed without its content and
/// read again next run.
fn print_content_faults(report: &Report) {
    let faults = &report.content_faults;
    if faults.is_empty() {
        return;
    }
    warn(&format!(
        "{} file(s) indexed without their content, to be read again next run:",
        faults.len()
    ));
    let mut text = String::new();
    for (path, fault) in faults.iter().take(SHOWN) {
        let _ = writeln!(text, "  {}: {fault}", path.display());
    }
    if faults.len() > SHOWN {
        let _ = writeln!(text, "  … and {} more", faults.len() - SHOWN);
    }
    note(&text);
}

/// `n` bytes, in the largest unit that keeps it at least 1.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{n} B"),
        _ => format!("{value:.1} {}", UNITS[unit]),
    }
}

/// The process's peak resident set (`VmHWM`), in KiB.
fn peak_rss_kb() -> Option<u64> {
    fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))?
        .trim()
        .strip_suffix("kB")?
        .trim()
        .parse()
        .ok()
}
