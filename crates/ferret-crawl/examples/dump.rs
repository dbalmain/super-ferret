//! `cargo run --release -p ferret-crawl --example dump -- <root> [global-file]
//! [workers]`
//!
//! Walks one root and prints one line per event, sorted by path: the decision
//! (or `io` / `pattern`) and the root-relative path. Two builds that print the
//! same lines made the same decisions, whatever order they walked in, so this
//! is the differential check for any change to the walk or the policy.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use ferret_crawl::{Event, EventVisitor, walk_parallel};
use ferret_policy::{Config, DEFAULT_IGNORE};

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
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

    let mut lines: Vec<_> = walk_parallel(
        Path::new(&root),
        Some(&global),
        Config::default(),
        workers,
        Dump::default,
    )
    .into_iter()
    .flat_map(|visitor| visitor.lines)
    .collect();
    lines.sort_unstable();
    let mut out = io::BufWriter::new(io::stdout().lock());
    for line in lines {
        if writeln!(out, "{line}").is_err() {
            return ExitCode::from(1);
        }
    }
    ExitCode::SUCCESS
}

fn usage() -> ExitCode {
    eprintln!("usage: dump <root> [global-file] [workers]");
    ExitCode::from(2)
}

#[derive(Default)]
struct Dump {
    lines: Vec<String>,
}

impl EventVisitor for Dump {
    fn visit(&mut self, event: Event<'_>) {
        let line = match event {
            Event::Decided(decided) => {
                format!("{}\t{:?}", decided.path.display(), decided.decision)
            }
            Event::Io { path, .. } => format!("{}\tio", path.display()),
            Event::Pattern(error) => format!("-\tpattern {error:?}"),
        };
        self.lines.push(line);
    }
}
