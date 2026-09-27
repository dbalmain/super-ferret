//! `cargo run --release -p ferret-crawl --example index -- <catalog-dir>
//! <root>... [--workers N] [--global FILE]`
//!
//! Indexes the roots into the catalog directory, refreshing all of them, and
//! prints the report: counts, faults, timings, and the process's peak RSS
//! (`VmHWM`). The global ignore file defaults to
//! [`ferret_policy::DEFAULT_IGNORE`].

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ferret_crawl::{IndexError, IndexOptions, Refresh, Report, index};

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(dir) = args.next() else {
        return usage();
    };
    let mut roots = Vec::new();
    let mut options = IndexOptions {
        global: Some(ferret_policy::DEFAULT_IGNORE.to_owned()),
        ..IndexOptions::default()
    };
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--workers") => match args.next().and_then(|v| v.to_str()?.parse().ok()) {
                Some(n) => options.workers = n,
                None => return usage(),
            },
            Some("--global") => match args.next() {
                Some(path) => match std::fs::read(&path) {
                    Ok(bytes) => options.global = Some(String::from_utf8_lossy(&bytes).into()),
                    Err(e) => {
                        eprintln!("index: {}: {e}", Path::new(&path).display());
                        return ExitCode::from(1);
                    }
                },
                None => return usage(),
            },
            _ => match std::path::absolute(PathBuf::from(&arg)) {
                Ok(root) => roots.push(root),
                Err(e) => {
                    eprintln!("index: {}: {e}", Path::new(&arg).display());
                    return ExitCode::from(1);
                }
            },
        }
    }
    if roots.is_empty() {
        return usage();
    }
    match index(Path::new(&dir), &roots, Refresh::All, &options) {
        Ok(report) => {
            print_report(&report);
            ExitCode::SUCCESS
        }
        Err(IndexError::Coverage { faults, report }) => {
            print_report(&report);
            eprintln!("coverage faults: {} (nothing published)", faults.len());
            for fault in faults.iter().take(20) {
                eprintln!("  {fault}");
            }
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("index: {error}");
            eprintln!("peak rss: {}", peak_rss());
            ExitCode::from(1)
        }
    }
}

fn print_report(report: &Report) {
    let c = &report.counts;
    let list = |paths: &[PathBuf]| {
        paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    };
    println!("refreshed: {}", list(&report.refreshed));
    println!("kept:      {}", list(&report.kept));
    println!("dropped:   {}", list(&report.dropped));
    println!(
        "dirs {} traversed {} files {} symlinks {}",
        c.dirs, c.traversed, c.files, c.symlinks
    );
    println!(
        "indexed {}: carried {} aliased {} content faults {}",
        c.indexed, c.carried, c.aliased, c.content_faults
    );
    println!(
        "read {} files, {} bytes; cache held {} inodes",
        c.files_read, c.bytes_read, c.cached_inodes
    );
    println!(
        "vanished {} boundaries {} pattern errors {}",
        c.vanished, c.boundaries, c.pattern_errors
    );
    for (path, fault) in report.content_faults.iter().take(20) {
        println!("  content fault {}: {fault}", path.display());
    }
    if let Some(p) = report.published {
        println!(
            "published: {} dirs, {} inodes, {} names, {} docs",
            p.dirs, p.inodes, p.names, p.docs
        );
    }
    println!(
        "walk+hash {:.3} s, commit {:.3} s, peak rss {}",
        report.walk_time.as_secs_f64(),
        report.commit_time.as_secs_f64(),
        peak_rss()
    );
}

/// `VmHWM` from `/proc/self/status`.
fn peak_rss() -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("VmHWM:"))
                .map(|v| v.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

fn usage() -> ExitCode {
    eprintln!("usage: index <catalog-dir> <root>... [--workers N] [--global FILE]");
    ExitCode::from(2)
}
