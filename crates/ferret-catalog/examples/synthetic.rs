//! The D40 scale measurement: a walker dump replicated under `copies`
//! prefixes, built through the real [`Transaction`] with no files on disk.
//!
//! ```text
//! cargo run --release -p ferret-crawl --example dump -- --stat <root> > dump.tsv
//! cargo run --release -p ferret-catalog --example synthetic -- <dump.tsv> <dir> <copies> [rerun] [faults=N]
//! ```
//!
//! The tree is one root, `/synthetic`, holding `p0 .. p<copies-1>`, each a copy
//! of the dump's tree with its own inode numbers and, for indexed files, its
//! own content hash, so every copy adds documents (the worst case). The
//! dump's `--stat` columns give every entry its real size, times, mode, owner,
//! device and inode number, so a packed format meets the spread real data has;
//! a dump without them gets one size, one time and sequential inode numbers,
//! which pack to nothing and flatter any compaction. Copy `c` adds
//! `c * stride` to each real inode number, where the stride is the dump's
//! largest inode number plus one: the values stay as scattered as on the
//! disk they came from, and never collide across copies. Each directory's entry
//! count is the number of its children in the dump, ignored ones included: a
//! lower bound on the real `getdents` count. The entries are spread over 16
//! batches, one per copy modulo 16, as 16 workers would hand them over. With
//! `rerun`, a previous generation must exist: every file is looked up with
//! [`Transaction::carry`] as the walk would, which is what a re-run holds in
//! memory. With `faults=N`, every Nth indexed file is stored as
//! [`Content::Fault`], for timing the crawl's post-commit content-fault pass at
//! scale.
//!
//! Prints the RSS once the batches are filled, the commit time and the peak
//! RSS (`VmHWM`), so batch memory and build memory can be told apart.

use std::collections::HashMap;
use std::error::Error;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use ferret_catalog::{Content, DirToken, Hash, Stat, Transaction};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const SNIFFER: u32 = 1;
const BATCHES: usize = 16;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let usage = || {
        eprintln!("usage: synthetic <dump.tsv> <dir> <copies> [rerun] [faults=N]");
        ExitCode::from(2)
    };
    let [dump, dir, copies, options @ ..] = words.as_slice() else {
        return usage();
    };
    let (mut rerun, mut fault_every) = (false, None);
    for option in options {
        match (*option, option.strip_prefix("faults=")) {
            ("rerun", _) => rerun = true,
            (_, Some(n)) => match n.parse::<u64>() {
                Ok(n) if n > 0 => fault_every = Some(n),
                _ => return usage(),
            },
            _ => return usage(),
        }
    }
    let Ok(copies) = copies.parse::<usize>() else {
        eprintln!("synthetic: copies must be a number");
        return ExitCode::from(2);
    };
    match run(Path::new(dump), Path::new(dir), copies, rerun, fault_every) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("synthetic: {error}");
            ExitCode::from(1)
        }
    }
}

/// A stat for a dump with no `--stat` columns: one size, one time, one owner.
fn plain_stat(mode: u32) -> Stat {
    Stat {
        dev: 1,
        ino: 0,
        size: 100,
        mtime_sec: 1_700_000_000,
        mtime_nsec: 0,
        ctime_sec: 1_700_000_000,
        ctime_nsec: 0,
        mode,
        uid: 1000,
        gid: 100,
        nlink: if mode & 0o170_000 == 0o040_000 { 2 } else { 1 },
    }
}

/// A dump line's field, named by the header's `columns` line.
#[derive(Clone, Copy)]
enum Column {
    Size,
    MtimeSec,
    MtimeNsec,
    CtimeSec,
    CtimeNsec,
    Mode,
    Uid,
    Gid,
    Dev,
    Ino,
    Nlink,
    /// A name this build does not know, such as a column added since.
    Unknown,
}

impl Column {
    fn named(name: &[u8]) -> Column {
        match name {
            b"size" => Column::Size,
            b"mtime_sec" => Column::MtimeSec,
            b"mtime_nsec" => Column::MtimeNsec,
            b"ctime_sec" => Column::CtimeSec,
            b"ctime_nsec" => Column::CtimeNsec,
            b"mode" => Column::Mode,
            b"uid" => Column::Uid,
            b"gid" => Column::Gid,
            b"dev" => Column::Dev,
            b"ino" => Column::Ino,
            b"nlink" => Column::Nlink,
            _ => Column::Unknown,
        }
    }
}

/// The stat a dump line's `values` describe, in the order of `columns`.
fn parse_stat<'a>(
    columns: &[Column],
    values: impl Iterator<Item = &'a [u8]>,
    mut stat: Stat,
) -> Result<Stat> {
    for (column, value) in columns.iter().zip(values) {
        let text = std::str::from_utf8(value)?;
        match column {
            Column::Size => stat.size = text.parse()?,
            Column::MtimeSec => stat.mtime_sec = text.parse()?,
            Column::MtimeNsec => stat.mtime_nsec = text.parse()?,
            Column::CtimeSec => stat.ctime_sec = text.parse()?,
            Column::CtimeNsec => stat.ctime_nsec = text.parse()?,
            Column::Mode => stat.mode = text.parse()?,
            Column::Uid => stat.uid = text.parse()?,
            Column::Gid => stat.gid = text.parse()?,
            Column::Dev => stat.dev = text.parse()?,
            Column::Ino => stat.ino = text.parse()?,
            Column::Nlink => stat.nlink = text.parse()?,
            Column::Unknown => {}
        }
    }
    Ok(stat)
}

/// One dump line that names an entry, with its real stat.
struct Item<'a> {
    path: &'a [u8],
    decision: &'a [u8],
    stat: Stat,
}

fn hash_of(n: u64) -> Hash {
    let mut hash = [0; 16];
    hash[..8].copy_from_slice(&n.to_le_bytes());
    hash[8..].copy_from_slice(&n.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes());
    hash
}

fn split(path: &[u8]) -> (Option<&[u8]>, &[u8]) {
    match path.iter().rposition(|&b| b == b'/') {
        Some(slash) => (Some(&path[..slash]), &path[slash + 1..]),
        None => (None, path),
    }
}

fn rss(field: &str) -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix(field))
                .map(|v| v.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

fn run(
    dump: &Path,
    dir: &Path,
    copies: usize,
    rerun: bool,
    fault_every: Option<u64>,
) -> Result<()> {
    let text = std::fs::read(dump)?;
    let mut columns = Vec::new();
    let mut root_stat = plain_stat(0o040_755);
    let mut dirs = Vec::new();
    let mut entries = Vec::new();
    // Each directory's children as the dump lists them, `Skip` lines
    // included: a lower bound on what `getdents` returned, since the walker
    // prints nothing for a directory it does not read. `None` is the copy's
    // top directory.
    let mut child_counts: HashMap<Option<&[u8]>, u32> = HashMap::new();
    for line in text.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        let mut fields = line.split(|&b| b == b'\t');
        let (path, decision) = (fields.next().unwrap_or_default(), fields.next());
        let decision = decision.ok_or("a line without a tab")?;
        if path.is_empty() {
            match decision {
                b"columns" => columns = fields.map(Column::named).collect(),
                b"root" => root_stat = parse_stat(&columns, fields, root_stat)?,
                _ => return Err("an unknown line with no path".into()),
            }
            continue;
        }
        *child_counts.entry(split(path).0).or_default() += 1;
        let mode = match decision {
            b"Descend" => 0o040_755,
            b"Catalog(Symlink)" => 0o120_777,
            _ => 0o100_644,
        };
        let stat = parse_stat(&columns, fields, plain_stat(mode))?;
        match decision {
            b"Descend" => dirs.push(Item {
                path,
                decision,
                stat,
            }),
            b"Skip" => {}
            _ => entries.push(Item {
                path,
                decision,
                stat,
            }),
        }
    }
    dirs.sort_unstable_by_key(|dir| dir.path);
    // Without inode numbers in the dump, number the entries in order.
    let has_ino = columns.iter().any(|c| matches!(c, Column::Ino));
    if !has_ino {
        for (n, item) in dirs.iter_mut().chain(entries.iter_mut()).enumerate() {
            item.stat.ino = 2 + n as u64;
        }
        root_stat.ino = 1;
    }
    let stride = dirs
        .iter()
        .chain(&entries)
        .map(|item| item.stat.ino)
        .max()
        .unwrap_or(0)
        .max(root_stat.ino)
        + 1;
    // The copy's own numbers: real ones, moved up by whole strides.
    let moved = |stat: &Stat, copy: usize| Stat {
        ino: stat.ino + copy as u64 * stride,
        ..*stat
    };

    let started = Instant::now();
    let mut txn = Transaction::begin(dir, SNIFFER)?;
    let opened = started.elapsed();
    if rerun && txn.previous().is_none() {
        return Err("rerun needs a previous generation".into());
    }
    let mut batches: Vec<_> = (0..BATCHES).map(|_| txn.batch()).collect();
    // The root takes the last stride, after every copy's.
    let root = batches[0].root(b"/synthetic", moved(&root_stat, copies));
    let (mut carried, mut total) = (0u64, 0u64);
    let mut tokens: HashMap<&[u8], DirToken> = HashMap::with_capacity(dirs.len());
    for copy in 0..copies {
        let batch = &mut batches[copy % BATCHES];
        let top = batch.dir(root, format!("p{copy}").as_bytes(), moved(&root_stat, copy));
        batch.entry_count(top, child_counts.get(&None).copied().unwrap_or(0));
        tokens.clear();
        let parent_of = |tokens: &HashMap<&[u8], DirToken>, parent: Option<&[u8]>| match parent {
            None => Ok(top),
            Some(parent) => tokens
                .get(parent)
                .copied()
                .ok_or_else(|| format!("no parent {}", String::from_utf8_lossy(parent))),
        };
        for dir in &dirs {
            let (parent, name) = split(dir.path);
            let stat = moved(&dir.stat, copy);
            let token = batch.dir(parent_of(&tokens, parent)?, name, stat);
            batch.entry_count(
                token,
                child_counts.get(&Some(dir.path)).copied().unwrap_or(0),
            );
            tokens.insert(dir.path, token);
        }
        for entry in &entries {
            let (parent, name) = split(entry.path);
            let parent = parent_of(&tokens, parent)?;
            let s = moved(&entry.stat, copy);
            let ino = s.ino;
            total += 1;
            match entry.decision {
                b"Index" => {
                    let content = match rerun.then(|| txn.carry(&s)).flatten() {
                        Some(content) => {
                            carried += 1;
                            content
                        }
                        None if fault_every.is_some_and(|n| ino.is_multiple_of(n)) => {
                            Content::Fault
                        }
                        None => Content::Hashed(hash_of(ino)),
                    };
                    batch.file(parent, name, s, content);
                }
                b"Catalog(Symlink)" => {
                    batch.symlink(parent, name, s, b"target");
                }
                _ => batch.file(parent, name, s, Content::Unindexed),
            }
        }
    }
    drop(tokens);
    let filled = started.elapsed();
    let filled_rss = rss("VmRSS:");
    for batch in batches {
        txn.add(batch);
    }
    let catalog = txn.commit()?;
    let committed = started.elapsed();
    let size = std::fs::metadata(dir.join("catalog"))?.len();
    println!(
        "copies {copies}: {} names, {} inodes, {} docs; {total} non-dir entries, {carried} carried",
        catalog.name_count(),
        catalog.inode_count(),
        catalog.doc_count()
    );
    println!(
        "file {size} B; begin {} ms, filled at {} ms (rss {filled_rss}), commit {} ms; peak rss {}",
        opened.as_millis(),
        filled.as_millis(),
        (committed - filled).as_millis(),
        rss("VmHWM:")
    );
    Ok(())
}
