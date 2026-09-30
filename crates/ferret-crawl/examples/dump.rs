//! `cargo run --release -p ferret-crawl --example dump -- [--stat] <root>
//! [global-file] [workers]`
//!
//! Walks one root and prints one line per event, sorted by path: the
//! root-relative path and the decision (or `io` / `pattern`). Two builds that
//! print the same lines made the same decisions, whatever order they walked
//! in, so this is the differential check for any change to the walk or the
//! policy.
//!
//! With `--stat` each decided entry's line also carries its `lstat` fields,
//! tab-separated after the decision, for the `synthetic` example in
//! `ferret-catalog` to replay. Two lines come first, unsorted: a header
//! naming the columns (`<tab>columns<tab>size<tab>...`), so a reader takes
//! them by name and a later column is an addition, not a break, and
//! the root's own stat (`<tab>root<tab>...`). Both start with an empty path,
//! which no entry has. Stats change as files do, so the default output stays
//! stat-free: that is the one to diff.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use ferret_crawl::{Event, EventVisitor, Stat, WalkOptions, walk_parallel};
use ferret_policy::{Config, DEFAULT_IGNORE};

fn main() -> ExitCode {
    let mut args: Vec<_> = std::env::args_os().skip(1).collect();
    let stat = args.iter().any(|arg| arg == "--stat");
    args.retain(|arg| arg != "--stat");
    let mut args = args.into_iter();
    let Some(root) = args.next() else {
        return usage();
    };
    let global = match args.next() {
        Some(path) => match fs::read(&path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) => {
                eprintln!("dump: {}: {error}", Path::new(&path).display());
                return ExitCode::from(1);
            }
        },
        None => DEFAULT_IGNORE.to_owned(),
    };
    let workers = match args.next() {
        Some(value) => match value.to_string_lossy().parse::<usize>() {
            Ok(workers) if workers > 0 => workers,
            _ => return usage(),
        },
        None => 1,
    };
    if args.next().is_some() {
        return usage();
    }

    let visitors = walk_parallel(
        Path::new(&root),
        Some(&global),
        Config::default(),
        &WalkOptions {
            workers,
            ..WalkOptions::default()
        },
        || Dump {
            stat,
            ..Dump::default()
        },
    );
    let mut out = io::BufWriter::new(io::stdout().lock());
    if stat {
        let root = visitors.iter().find_map(|visitor| visitor.root.as_deref());
        let header = writeln!(out, "\tcolumns\t{}", COLUMNS.join("\t"));
        let root = writeln!(out, "\troot\t{}", root.unwrap_or_default());
        if header.and(root).is_err() {
            return ExitCode::from(1);
        }
    }
    let mut lines: Vec<_> = visitors
        .into_iter()
        .flat_map(|visitor| visitor.lines)
        .collect();
    lines.sort_unstable();
    for line in lines {
        if writeln!(out, "{line}").is_err() {
            return ExitCode::from(1);
        }
    }
    ExitCode::SUCCESS
}

fn usage() -> ExitCode {
    eprintln!("usage: dump [--stat] <root> [global-file] [workers]");
    ExitCode::from(2)
}

/// The `--stat` columns, in order. Add a column at the end.
const COLUMNS: [&str; 11] = [
    "size",
    "mtime_sec",
    "mtime_nsec",
    "ctime_sec",
    "ctime_nsec",
    "mode",
    "uid",
    "gid",
    "dev",
    "ino",
    "nlink",
];

/// A stat as the tab-separated values of [`COLUMNS`].
fn columns(stat: &Stat<'_>) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        stat.size,
        stat.mtime_sec,
        stat.mtime_nsec,
        stat.ctime_sec,
        stat.ctime_nsec,
        stat.mode,
        stat.uid,
        stat.gid,
        stat.dev,
        stat.ino,
        stat.nlink
    )
}

#[derive(Default)]
struct Dump {
    stat: bool,
    root: Option<String>,
    lines: Vec<String>,
}

impl EventVisitor for Dump {
    type Dir = ();

    fn root(&mut self, stat: Stat<'_>) {
        self.root = Some(columns(&stat));
    }

    fn visit(&mut self, event: Event<'_, ()>) -> Option<()> {
        let line = match event {
            Event::Decided(decided) => {
                let mut line = format!("{}\t{:?}", decided.path.display(), decided.decision);
                if let Some(stat) = decided.stat.filter(|_| self.stat) {
                    line.push('\t');
                    line.push_str(&columns(&stat));
                }
                line
            }
            Event::Entered { .. } => return Some(()),
            Event::Boundary { path, .. } => format!("{}\tboundary", path.display()),
            Event::Io { path, op, .. } => format!("{}\tio {op:?}", path.display()),
            Event::Pattern(error) => format!("-\tpattern {error:?}"),
        };
        self.lines.push(line);
        Some(())
    }
}
