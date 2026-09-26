//! `cargo run --release -p ferret-crawl --example walk -- <root> [global-file]`
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

use ferret_crawl::{Event, walk};
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
    if args.next().is_some() {
        return usage();
    }

    let mut counts = Counts::default();
    let mut devices = HashSet::new();
    let started = Instant::now();
    walk(
        Path::new(&root),
        Some(&global),
        Config::default(),
        |event| match event {
            Event::Decided(decided) => {
                if let Some(stat) = decided.stat {
                    devices.insert(stat.dev);
                }
                match decided.decision {
                    Decision::Skip => counts.skip += 1,
                    Decision::Descend => counts.descend += 1,
                    Decision::Traverse => counts.traverse += 1,
                    Decision::Index => counts.index += 1,
                    Decision::Catalog(Reason::TooLarge) => counts.too_large += 1,
                    Decision::Catalog(Reason::Symlink) => counts.symlink += 1,
                }
            }
            Event::Io { .. } | Event::Pattern(_) => counts.errors += 1,
        },
    );
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
        "usage: walk <root> [global-file]\n\
         \n\
         Walks <root> and prints decision counts and wall time. Without a\n\
         global file, the built-in default ignore text is used."
    );
    ExitCode::from(2)
}
