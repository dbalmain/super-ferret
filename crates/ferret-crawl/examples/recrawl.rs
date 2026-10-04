//! Synthetic whole-recrawl observations over the existing 10M checkpoint.
//! The fixture has no files on disk: replay borrows its names/stat/content into
//! real batches, then calls the production reconciler and durable writer. This
//! measures publication and whole-batch memory, not filesystem enumeration or
//! hashing throughput. `ferret-bench recrawl-once` invokes this producer.

use std::collections::{BTreeSet, HashMap};
use std::error::Error;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ferret_catalog::{Content, ContentState, DirToken, InoId, Kind, Target, WriterSession};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [dir, changed] = args.as_slice() else {
        return Err("usage: recrawl <catalog-dir> <changed-file-inodes>".into());
    };
    run(Path::new(dir), changed.parse()?)
}

fn rss() -> Result<(u64, u64), Box<dyn Error>> {
    let status = fs::read_to_string("/proc/self/status")?;
    let value = |label| -> Result<u64, Box<dyn Error>> {
        Ok(status
            .lines()
            .find_map(|line| line.strip_prefix(label))
            .ok_or("missing RSS")?
            .split_whitespace()
            .next()
            .ok_or("missing RSS value")?
            .parse()?)
    };
    Ok((value("VmRSS:")?, value("VmHWM:")?))
}

fn run(dir: &Path, changed: usize) -> Result<(), Box<dyn Error>> {
    let setup = Instant::now();
    let mut session = WriterSession::open(dir)?;
    let setup_ms = setup.elapsed().as_secs_f64() * 1000.0;
    let (setup_rss, setup_peak) = rss()?;
    let old = session.view();
    let generation = old.generation();
    let snapshot = dir.join(format!("snapshot.{}", generation.checkpoint));
    let snapshot_before = fs::metadata(&snapshot)?;
    let log = dir.join(format!("changes.{}", generation.checkpoint));
    let log_before = fs::metadata(&log)?.len();
    let started = Instant::now();
    let mut changed_ids = BTreeSet::new();
    for id in old
        .inode_ids()
        .filter(|&id| old.kind(id) == Kind::File && old.state(id) == ContentState::Hashed)
        .take(changed)
    {
        changed_ids.insert(id);
    }
    if changed_ids.len() != changed {
        return Err("not enough regular file inodes".into());
    }
    let mut batches: Vec<_> = (0..16).map(|_| session.batch()).collect();
    let mut tokens: HashMap<InoId, DirToken> = HashMap::with_capacity(old.dir_count() as usize);
    let roots: Vec<PathBuf> = old
        .roots()
        .map(|(_, p)| PathBuf::from(std::ffi::OsStr::from_bytes(p)))
        .collect();
    let mut queue = Vec::new();
    for (id, path) in old.roots() {
        let batch = &mut batches[id.0 as usize % 16];
        let token = batch.root(path, old.inode(id).stat);
        tokens.insert(id, token);
        queue.push(id);
    }
    while let Some(dir) = queue.pop() {
        let token = tokens[&dir];
        let batch = &mut batches[dir.0 as usize % 16];
        if let Some(entries) = old.entry_count(dir) {
            batch.entry_count(token, entries);
        }
        if let Some(work) = old.work_tree(dir) {
            batch.work_tree(token, work.kind, work.common_dir, work.common_id);
        }
        for name in old.children(dir) {
            let edge = old.name(name);
            let Target::Inode(id) = edge.target() else {
                if let Target::Ignored(kind) = edge.target() {
                    batch.ignored(token, edge.bytes, kind);
                }
                continue;
            };
            let inode = old.inode(id);
            let mut stat = inode.stat;
            let mut content = match inode.state {
                ContentState::Unindexed => Content::Unindexed,
                ContentState::Binary => Content::Binary,
                ContentState::Fault => Content::Fault,
                ContentState::Hashed => Content::Hashed(
                    inode
                        .doc
                        .and_then(|doc| old.doc_hash(doc))
                        .ok_or("hashed without document")?,
                ),
            };
            if changed_ids.contains(&id) {
                stat.mtime_sec += 1;
                stat.ctime_sec += 1;
                let mut hash = blake3::Hasher::new();
                hash.update(b"M4 synthetic changed content\0");
                hash.update(&id.0.to_le_bytes());
                let mut bytes = [0; 16];
                bytes.copy_from_slice(&hash.finalize().as_bytes()[..16]);
                content = Content::Hashed(bytes);
            }
            match old.kind(id) {
                Kind::Dir => {
                    let child = if old.is_traversed(id) {
                        batch.traversed_dir(token, edge.bytes, stat)
                    } else {
                        batch.dir(token, edge.bytes, stat)
                    };
                    tokens.insert(id, child);
                    queue.push(id);
                }
                Kind::Symlink => batch.symlink(
                    token,
                    edge.bytes,
                    stat,
                    old.link_target(id).unwrap_or_default(),
                ),
                _ => batch.file(token, edge.bytes, stat, content),
            }
        }
    }
    drop(tokens);
    drop(changed_ids);
    let replay_ms = started.elapsed().as_secs_f64() * 1000.0;
    let (batch_rss, _) = rss()?;
    let diff = Instant::now();
    let changes = ferret_crawl::reconcile::changes(
        &session,
        &batches,
        &roots,
        &[],
        old.policy(),
        old.sniffer_version(),
    )?
    .ok_or("fixture contains incomplete coverage")?;
    let diff_ms = diff.elapsed().as_secs_f64() * 1000.0;
    drop(batches);
    let commit = Instant::now();
    let current = session.commit(&changes, old.sniffer_version())?;
    let commit_ms = commit.elapsed().as_secs_f64() * 1000.0;
    let total_ms = started.elapsed().as_secs_f64() * 1000.0;
    let (final_rss, peak) = rss()?;
    let snapshot_after = fs::metadata(snapshot)?;
    if current.generation().checkpoint != generation.checkpoint
        || snapshot_before.len() != snapshot_after.len()
        || snapshot_before.modified()? != snapshot_after.modified()?
    {
        return Err("recrawl rewrote checkpoint".into());
    }
    let log_bytes = fs::metadata(log)?.len() - log_before;
    let manifest_bytes = if current.generation() == generation {
        0
    } else {
        128
    };
    if changed == 0 && (log_bytes != 0 || manifest_bytes != 0 || !changes.records.is_empty()) {
        return Err("unchanged recrawl published".into());
    }
    println!(
        "| changed files | setup ms | setup current/peak KiB | replay ms | diff ms | commit ms | run ms | batches/final/peak KiB | log/manifest bytes | records |"
    );
    println!("| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    println!(
        "| {changed} | {setup_ms:.2} | {setup_rss}/{setup_peak} | {replay_ms:.2} | {diff_ms:.2} | {commit_ms:.2} | {total_ms:.2} | {batch_rss}/{final_rss}/{peak} | {log_bytes}/{manifest_bytes} | {} |",
        changes.records.len()
    );
    Ok(())
}
