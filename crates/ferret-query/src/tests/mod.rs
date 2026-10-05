//! Queries run against real catalogs, committed through `ferret-catalog`'s
//! transaction and reopened lazily, as `ferret search` does.

mod grammar;
mod overlay;
mod pattern;
mod run;

use std::fs;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ferret_catalog::{Catalog, Content, Stat, Transaction};

use crate::{Query, Stats};

/// The clock every test query runs at.
const NOW: i64 = 1_800_000_000;
const DAY: i64 = 86_400;

fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(NOW as u64)
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("ferret-query-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stat(ino: u64, mode: u32, size: u64, age: i64) -> Stat {
    Stat {
        dev: 1,
        ino,
        size,
        mtime_sec: NOW - age,
        mtime_nsec: 0,
        ctime_sec: NOW - age,
        ctime_nsec: 0,
        mode,
        uid: 1000,
        gid: 1000,
        nlink: 1,
    }
}

fn dir(ino: u64) -> Stat {
    stat(ino, 0o040_755, 4096, 30 * DAY)
}

fn file(ino: u64, size: u64, age: i64) -> Stat {
    stat(ino, 0o100_644, size, age)
}

/// The tree every query test runs over, committed to `scratch`:
///
/// ```text
/// /r                          root
///   Makefile                  2 KiB, 10 days old
///   docs/  README.md  readme.txt
///   link -> src/main.rs       symlink
///   skip/                     traversed (D29): structural, never a result
///     kept.rs                 re-included under it
///   src/
///     main.rs                 100 B, an hour old, hashed
///     lib.RS
///     parse_HTTP.rs
///     big.bin                 200 MiB, 400 days old
///     notes.rst               holds ".rs", matches no `*.rs`
///     deep/  x.rs  src/y.rs
/// /t                          second root
///   a.rs
///   .rs                       all extension: `*.rs`, but not `ext:rs`
/// ```
fn sample(scratch: &Scratch) -> Catalog {
    let mut txn = Transaction::begin(&scratch.0, 1).unwrap();
    let mut w = txn.batch();
    let r = w.root(b"/r", dir(1));
    w.file(r, b"Makefile", file(10, 2048, 10 * DAY), Content::Unindexed);
    let docs = w.dir(r, b"docs", dir(2));
    w.file(
        docs,
        b"README.md",
        file(11, 500, 5 * DAY),
        Content::Unindexed,
    );
    w.file(
        docs,
        b"readme.txt",
        file(12, 300, 5 * DAY),
        Content::Unindexed,
    );
    w.symlink(r, b"link", stat(13, 0o120_777, 11, DAY), b"src/main.rs");
    let skip = w.traversed_dir(r, b"skip", dir(3));
    w.file(skip, b"kept.rs", file(14, 10, DAY), Content::Unindexed);
    let src = w.dir(r, b"src", dir(4));
    w.file(
        src,
        b"main.rs",
        file(15, 100, 3600),
        Content::Hashed([7; 16]),
    );
    w.file(src, b"lib.RS", file(16, 100, 2 * DAY), Content::Unindexed);
    w.file(
        src,
        b"parse_HTTP.rs",
        file(17, 900, 2 * DAY),
        Content::Unindexed,
    );
    w.file(
        src,
        b"big.bin",
        file(18, 200 << 20, 400 * DAY),
        Content::Binary,
    );
    w.file(src, b"notes.rst", file(22, 50, 2 * DAY), Content::Unindexed);
    let deep = w.dir(src, b"deep", dir(5));
    w.file(deep, b"x.rs", file(19, 1, 3 * DAY), Content::Unindexed);
    let inner = w.dir(deep, b"src", dir(6));
    w.file(inner, b"y.rs", file(20, 1, 3 * DAY), Content::Unindexed);
    let t = w.root(b"/t", dir(7));
    w.file(t, b"a.rs", file(21, 5, DAY), Content::Unindexed);
    w.file(t, b".rs", file(23, 50, DAY), Content::Unindexed);
    txn.add(w);
    txn.commit().unwrap();
    lazy(scratch)
}

/// Reopens the catalog in `scratch` with nothing loaded.
fn lazy(scratch: &Scratch) -> Catalog {
    Catalog::open(&scratch.0).unwrap().unwrap()
}

/// Runs `text` over `catalog`: the paths it emits, in order, and its stats.
fn find(catalog: &Catalog, text: &str) -> (Vec<String>, Stats) {
    let query = Query::parse(text, now()).unwrap();
    let mut paths = Vec::new();
    let stats = query
        .run(catalog, |row| {
            paths.push(String::from_utf8(row.path.to_vec()).unwrap());
            ControlFlow::Continue(())
        })
        .unwrap();
    (paths, stats)
}

/// The paths `text` finds, sorted.
fn paths(catalog: &Catalog, text: &str) -> Vec<String> {
    let mut found = find(catalog, text).0;
    found.sort();
    found
}
