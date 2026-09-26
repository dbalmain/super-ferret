//! `cargo run --release -p ferret-crawl --example walk -- <root> [global-file]
//! [workers]`
//!
//! Walks one root and prints decision counts, the error count, how many
//! directories were entered, how many devices the walk touched, and the wall
//! time. The global file defaults to [`ferret_policy::DEFAULT_IGNORE`]. The
//! size cap is [`ferret_policy::DEFAULT_SIZE_CAP`].
//!
//! `entered` is the root plus every [`ferret_policy::Decision::Descend`].
//! Traversed directories are listed under `traverse` and are not part of
//! `entered`. `devices` is the number of distinct `st_dev` values among
//! entries that were not skipped.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use ferret_crawl::{Event, EventVisitor, walk_parallel};
use ferret_policy::{Config, DEFAULT_IGNORE, Decision, Reason};

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(root) = args.next() else {
        return usage();
    };
    let global = match args.next() {
        Some(path) => match read_lossy(Path::new(&path)) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("walk: {}: {error}", Path::new(&path).display());
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
        None => ferret_crawl::default_workers(),
    };
    if args.next().is_some() {
        return usage();
    }

    let mut counts = Counts::default();
    let mut devices = HashSet::new();
    let started = Instant::now();
    let visitors = walk_parallel(
        Path::new(&root),
        Some(&global),
        Config::default(),
        workers,
        CountsVisitor::default,
    );
    for visitor in visitors {
        counts.skip += visitor.counts.skip;
        counts.descend += visitor.counts.descend;
        counts.traverse += visitor.counts.traverse;
        counts.index += visitor.counts.index;
        counts.too_large += visitor.counts.too_large;
        counts.symlink += visitor.counts.symlink;
        counts.errors += visitor.counts.errors;
        devices.extend(visitor.devices);
    }
    let wall = started.elapsed();

    println!("skip {}", counts.skip);
    println!("descend {}", counts.descend);
    println!("traverse {}", counts.traverse);
    println!("index {}", counts.index);
    println!("too-large {}", counts.too_large);
    println!("symlink {}", counts.symlink);
    println!("errors {}", counts.errors);
    println!("entered {}", counts.descend + 1);
    println!("devices {}", devices.len());
    println!("wall_s {:.3}", wall.as_secs_f64());
    ExitCode::SUCCESS
}

#[derive(Default)]
struct CountsVisitor {
    counts: Counts,
    devices: HashSet<u64>,
}

impl EventVisitor for CountsVisitor {
    fn visit(&mut self, event: Event<'_>) {
        match event {
            Event::Decided(decided) => {
                if let Some(stat) = decided.stat {
                    self.devices.insert(stat.dev);
                }
                match decided.decision {
                    Decision::Skip => self.counts.skip += 1,
                    Decision::Descend => self.counts.descend += 1,
                    Decision::Traverse => self.counts.traverse += 1,
                    Decision::Index => self.counts.index += 1,
                    Decision::Catalog(Reason::TooLarge) => self.counts.too_large += 1,
                    Decision::Catalog(Reason::Symlink) => self.counts.symlink += 1,
                }
            }
            Event::Io { .. } | Event::Pattern(_) => self.counts.errors += 1,
        }
    }
}

#[derive(Default)]
struct Counts {
    skip: u64,
    descend: u64,
    traverse: u64,
    index: u64,
    too_large: u64,
    symlink: u64,
    errors: u64,
}

fn read_lossy(path: &Path) -> io::Result<String> {
    let bytes = fs::read(path)?;
    Ok(String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()))
}

fn usage() -> ExitCode {
    let _ = writeln!(
        io::stderr(),
        "usage: walk <root> [global-file] [workers]\n\
         \n\
         Walks <root> and prints decision counts and wall time. Without a\n\
         global file, the built-in default ignore text is used. Without a\n\
         worker count, the crate's default is used (D24)."
    );
    ExitCode::from(2)
}
