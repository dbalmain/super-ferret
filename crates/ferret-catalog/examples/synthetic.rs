//! The D40 scale measurement: a walker dump replicated under `copies`
//! prefixes, built through the real [`Transaction`] with no files on disk.
//!
//! ```text
//! cargo run --release -p ferret-catalog --example synthetic -- <dump.tsv> <dir> <copies> [rerun]
//! ```
//!
//! The tree is one root, `/synthetic`, holding `p0 .. p<copies-1>`, each a copy
//! of the dump's tree with its own inode numbers and, for indexed files, its
//! own content hash, so every copy adds documents (the worst case). The
//! entries are spread over 16 batches, one per copy modulo 16, as 16 workers
//! would hand them over. With `rerun`, a previous generation must exist: every
//! file is looked up with [`Transaction::carry`] as the walk would, which is
//! what a re-run holds in memory.
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
    let (dump, dir, copies, rerun) = match words.as_slice() {
        [dump, dir, copies] => (dump, dir, copies, false),
        [dump, dir, copies, "rerun"] => (dump, dir, copies, true),
        _ => {
            eprintln!("usage: synthetic <dump.tsv> <dir> <copies> [rerun]");
            return ExitCode::from(2);
        }
    };
    let Ok(copies) = copies.parse::<usize>() else {
        eprintln!("synthetic: copies must be a number");
        return ExitCode::from(2);
    };
    match run(Path::new(dump), Path::new(dir), copies, rerun) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("synthetic: {error}");
            ExitCode::from(1)
        }
    }
}

fn stat(ino: u64, mode: u32) -> Stat {
    Stat {
        dev: 1,
        ino,
        size: 100,
        mtime_sec: 1_700_000_000,
        mtime_nsec: 0,
        ctime_sec: 1_700_000_000,
        ctime_nsec: 0,
        mode,
        uid: 1000,
        gid: 100,
    }
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

fn run(dump: &Path, dir: &Path, copies: usize, rerun: bool) -> Result<()> {
    let text = std::fs::read(dump)?;
    let mut dirs = Vec::new();
    let mut entries = Vec::new();
    for line in text.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        let tab = line
            .iter()
            .position(|&b| b == b'\t')
            .ok_or("a line without a tab")?;
        let (path, decision) = (&line[..tab], &line[tab + 1..]);
        match decision {
            b"Descend" => dirs.push(path),
            b"Skip" => {}
            _ => entries.push((path, decision)),
        }
    }
    dirs.sort_unstable();
    let per_copy = (dirs.len() + entries.len() + 1) as u64;

    let started = Instant::now();
    let mut txn = Transaction::begin(dir, SNIFFER)?;
    let opened = started.elapsed();
    if rerun && txn.previous().is_none() {
        return Err("rerun needs a previous generation".into());
    }
    let mut batches: Vec<_> = (0..BATCHES).map(|_| txn.batch()).collect();
    let root = batches[0].root(b"/synthetic", stat(1, 0o040_755));
    let (mut carried, mut total) = (0u64, 0u64);
    let mut tokens: HashMap<&[u8], DirToken> = HashMap::with_capacity(dirs.len());
    for copy in 0..copies {
        let batch = &mut batches[copy % BATCHES];
        let base = 2 + copy as u64 * per_copy;
        let top = batch.dir(root, format!("p{copy}").as_bytes(), stat(base, 0o040_755));
        tokens.clear();
        let parent_of = |tokens: &HashMap<&[u8], DirToken>, parent: Option<&[u8]>| match parent {
            None => Ok(top),
            Some(parent) => tokens
                .get(parent)
                .copied()
                .ok_or_else(|| format!("no parent {}", String::from_utf8_lossy(parent))),
        };
        let mut ino = base;
        for path in &dirs {
            let (parent, name) = split(path);
            ino += 1;
            let token = batch.dir(parent_of(&tokens, parent)?, name, stat(ino, 0o040_755));
            tokens.insert(path, token);
        }
        for (path, decision) in &entries {
            let (parent, name) = split(path);
            let parent = parent_of(&tokens, parent)?;
            ino += 1;
            total += 1;
            match *decision {
                b"Index" => {
                    let s = stat(ino, 0o100_644);
                    let content = match rerun.then(|| txn.carry(&s)).flatten() {
                        Some(content) => {
                            carried += 1;
                            content
                        }
                        None => Content::Hashed(hash_of(ino)),
                    };
                    batch.file(parent, name, s, content);
                }
                b"Catalog(Symlink)" => {
                    batch.symlink(parent, name, stat(ino, 0o120_777), b"target");
                }
                _ => batch.file(parent, name, stat(ino, 0o100_644), Content::Unindexed),
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
