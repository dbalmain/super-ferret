//! Synthetic whole-recrawl observations over the existing 10M checkpoint.
//! The fixture has no files on disk: replay borrows its names/stat/content into
//! real batches, then calls the production reconciler and durable writer. This
//! measures publication and whole-batch memory, not filesystem enumeration or
//! hashing throughput. Fault modes use real root-open errors and the public
//! crawl API; preparation makes one old leaf a nested configured root so small
//! and large protected scopes can be timed without unrelated enumeration.
//! `ferret-bench recrawl-once` invokes this producer.

use std::collections::{BTreeSet, HashMap};
use std::error::Error;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ferret_catalog::{
    Catalog, Content, ContentState, DirToken, InoId, Kind, Target, WriterSession,
};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [dir, changed] = args.as_slice() else {
        return Err("usage: recrawl <catalog-dir> <changed-file-inodes|fault-prepare|fault-small|fault-large>".into());
    };
    match changed.as_str() {
        "fault-prepare" => prepare_fault_roots(Path::new(dir)),
        "fault-small" | "fault-large" => fault_root(Path::new(dir), changed == "fault-small"),
        "churn-50-fault" => churn_fault(Path::new(dir), 50),
        "churn-90-fault" => churn_fault(Path::new(dir), 90),
        "resident-1" => resident(Path::new(dir), 1, 1),
        "resident-100000" => resident(Path::new(dir), 100000, 1),
        "resident-repeat" => resident(Path::new(dir), 1, 1000),
        _ => run(Path::new(dir), changed.parse()?),
    }
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
    let mut observed_files = 0usize;
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
                Kind::Symlink => {
                    observed_files += 1;
                    batch.symlink(
                        token,
                        edge.bytes,
                        stat,
                        old.link_target(id).unwrap_or_default(),
                    );
                }
                _ => {
                    observed_files += 1;
                    batch.file(token, edge.bytes, stat, content);
                }
            }
        }
    }
    for batch in &mut batches {
        batch.seal();
    }
    if batches
        .iter()
        .map(|batch| batch.file_count() + batch.reused_file_count())
        .sum::<usize>()
        != observed_files
    {
        return Err("observation reduction lost file rows".into());
    }
    let observation_rows: usize = batches.iter().map(|b| b.observation_peak().0).sum();
    let observation_bytes: usize = batches.iter().map(|b| b.observation_peak().1).sum();
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
    println!(
        "temporary_observation_rows={observation_rows} temporary_observation_bytes={observation_bytes} cap_rows_per_worker={} cap_name_bytes_per_worker={}",
        ferret_catalog::batch::OBSERVATION_ROWS,
        ferret_catalog::batch::OBSERVATION_BYTES
    );
    Ok(())
}

// Split one real fixture leaf into a nested configured root using the durable
// writer. This changes only its incoming edge/root boundary, not its subtree.
// Both fault cases then start from exactly this same checked 10M view.
fn prepare_fault_roots(dir: &Path) -> Result<(), Box<dyn Error>> {
    use ferret_catalog::Catalog;
    use ferret_catalog::log::{ChangeSet, Record};
    let policy_tree = dir.join("policy-tree");
    let policy_catalog = dir.join("policy-catalog");
    fs::create_dir_all(&policy_tree)?;
    let options = ferret_crawl::IndexOptions::default();
    ferret_crawl::index(
        &policy_catalog,
        &[policy_tree],
        ferret_crawl::Refresh::All,
        &options,
    )?;
    let policy = Catalog::open(&policy_catalog)?.ok_or("missing policy probe")?;
    policy.load_all()?;
    let mut session = WriterSession::open(dir)?;
    let old = session.view();
    if old.roots().count() != 1 {
        return Err("fault preparation requires original single-root fixture".into());
    }
    let mut path = Vec::new();
    let small = old
        .dir_ids()
        .find(|&id| {
            if old.is_traversed(id) || old.entry_count(id) != Some(1) || old.dir_name(id).is_none()
            {
                return false;
            }
            let mut children = old.children(id);
            let Some(name) = children.next() else {
                return false;
            };
            let Target::Inode(child) = old.name(name).target() else {
                return false;
            };
            if children.next().is_some()
                || old.kind(child) != Kind::File
                || old.state(child) != ContentState::Hashed
            {
                return false;
            }
            path.clear();
            old.dir_path(id, &mut path);
            std::str::from_utf8(&path).is_ok()
        })
        .ok_or("no one-file leaf scope")?;
    let name = old.dir_name(small).ok_or("leaf has no incoming edge")?;
    let mut changes = ChangeSet {
        counters: [old.next_inode().0, old.next_name().0, old.next_doc().0],
        counts: [
            old.inode_count(),
            old.name_count() - 1,
            old.dir_count(),
            old.doc_count(),
        ],
        records: vec![
            Record::NameDelete { id: name.0 },
            Record::LifePut {
                id: small.0,
                kind: Kind::Dir,
                flags: 0,
                names: 0,
            },
            Record::DirPut {
                id: small.0,
                name: None,
                entries: Some(1),
                flags: 4,
                retained_at: None,
            },
            Record::RootPut {
                id: small.0,
                path: path.clone(),
            },
        ],
    };
    if old.policy() != policy.policy() {
        changes.records.push(Record::PolicyPut {
            hash: policy.policy(),
        });
    }
    let current = session.commit(&changes, options.sniffer)?;
    // The old leaf contains exactly one name. Its detached incoming name is
    // removed, and its child remains owned by the new nested root.
    let large_names = current.name_count() - 1;
    fs::write(
        dir.join("coverage-scopes"),
        format!("{}\n{}\n", small.0, large_names),
    )?;
    println!(
        "prepared small_root={} path={} names=1; large_root=/synthetic names={large_names}; sequence={}",
        small.0,
        std::str::from_utf8(&path)?,
        current.generation().sequence
    );
    Ok(())
}

fn fault_root(dir: &Path, small: bool) -> Result<(), Box<dyn Error>> {
    use ferret_catalog::{Catalog, Section};
    let setup = Instant::now();
    let mut session = WriterSession::open(dir)?;
    let setup_ms = setup.elapsed().as_secs_f64() * 1000.0;
    let (setup_rss, setup_peak) = rss()?;
    let old = session.view();
    let scopes = fs::read_to_string(dir.join("coverage-scopes"))?;
    let mut values = scopes.lines();
    let leaf = InoId(values.next().ok_or("missing small scope")?.parse()?);
    let large_names: u32 = values.next().ok_or("missing large scope")?.parse()?;
    let (id, path) = old
        .roots()
        .find(|(id, path)| {
            if small {
                *id == leaf
            } else {
                *path == b"/synthetic"
            }
        })
        .ok_or("missing fault root")?;
    let scope = PathBuf::from(std::ffi::OsStr::from_bytes(path));
    if fs::metadata(&scope).is_ok() {
        return Err("synthetic fault path unexpectedly exists".into());
    }
    let roots: Vec<_> = old
        .roots()
        .map(|(_, path)| PathBuf::from(std::ffi::OsStr::from_bytes(path)))
        .collect();
    let generation = old.generation();
    let snapshot = dir.join(format!("snapshot.{}", generation.checkpoint));
    let snapshot_before = fs::metadata(&snapshot)?;
    let log = dir.join(format!("changes.{}", generation.checkpoint));
    let before = fs::metadata(&log)?.len();
    let started = Instant::now();
    let report = ferret_crawl::recrawl(
        &mut session,
        &roots,
        ferret_crawl::Refresh::Only(std::slice::from_ref(&scope)),
        &ferret_crawl::IndexOptions::default(),
    )?;
    let total_ms = started.elapsed().as_secs_f64() * 1000.0;
    let current = session.view();
    if report.protected_scopes != 1
        || report.coverage_faults.len() != 1
        || report.coverage_faults[0].op != ferret_crawl::IoOp::OpenDir
        || report.coverage_faults[0].error.kind() != std::io::ErrorKind::NotFound
        || current.entry_count(id).is_some()
        || current.retained_at(id) != Some(generation.sequence)
        || current.generation().checkpoint != generation.checkpoint
        || current.inode_count() != old.inode_count()
        || current.name_count() != old.name_count()
        || current.dir_count() != old.dir_count()
        || current.doc_count() != old.doc_count()
    {
        return Err(
            "fault recrawl changed retained namespace or failed to publish its marker".into(),
        );
    }
    let after = fs::metadata(&snapshot)?;
    if after.len() != snapshot_before.len() || after.modified()? != snapshot_before.modified()? {
        return Err("fault recrawl rewrote checkpoint".into());
    }
    let bytes = fs::metadata(&log)?.len() - before;
    let disk = Catalog::open(dir)?.ok_or("missing replayed fault view")?;
    disk.load(&[
        Section::Names,
        Section::Roots,
        Section::Entries,
        Section::RetainedAt,
    ])?;
    if disk.entry_count(id).is_some()
        || disk.retained_at(id) != Some(generation.sequence)
        || disk.generation() != current.generation()
    {
        return Err("disk replay lost fault coverage".into());
    }
    drop(disk);
    // Repeat the same actual kernel error through the same API. Zero append
    // and unchanged generation prove that retained-at is not a retry counter.
    let repeated = ferret_crawl::recrawl(
        &mut session,
        &roots,
        ferret_crawl::Refresh::Only(std::slice::from_ref(&scope)),
        &ferret_crawl::IndexOptions::default(),
    )?;
    if repeated.published.is_some()
        || session.view().generation() != current.generation()
        || fs::metadata(&log)?.len() - before != bytes
    {
        return Err("identical root fault published again".into());
    }
    let (final_rss, peak) = rss()?;
    println!(
        "| protected names | setup ms | setup current/peak KiB | walk ms | reconciliation/publication ms | run ms | final/peak KiB | log/manifest bytes | scopes | retained sequence |"
    );
    println!("| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    println!(
        "| {} | {setup_ms:.2} | {setup_rss}/{setup_peak} | {:.2} | {:.2} | {total_ms:.2} | {final_rss}/{peak} | {bytes}/128 | {} | {} |",
        if small { 1 } else { large_names },
        report.walk_time.as_secs_f64() * 1000.0,
        report.commit_time.as_secs_f64() * 1000.0,
        report.protected_scopes,
        generation.sequence
    );
    Ok(())
}

// Scoped synthetic observations use the exact batch preservation, reducer,
// final-set reconciler and durable session used by refresh. The fixture has no
// filesystem tree: enumeration, handle opening and content hashing are
// excluded.
fn resident(dir: &Path, changed: usize, bursts: usize) -> Result<(), Box<dyn Error>> {
    let setup = Instant::now();
    let mut session = WriterSession::open(dir)?;
    let setup_ms = setup.elapsed().as_secs_f64() * 1000.0;
    let (setup_rss, setup_peak) = rss()?;
    let old = session.view();
    let generation = old.generation();
    let snapshot = dir.join(format!("snapshot.{}", generation.checkpoint));
    let snapshot_before = fs::metadata(&snapshot)?;
    let log = dir.join(format!("changes.{}", generation.checkpoint));
    let initial_log = fs::metadata(&log)?.len();
    // Selecting the simulated watcher events belongs to the input fixture,
    // outside burst latency. Ancestor resolution is timed as part of replay.
    let chosen: BTreeSet<_> = old
        .inode_ids()
        .filter(|&id| old.kind(id) == Kind::File && old.state(id) == ContentState::Hashed)
        .take(changed)
        .collect();
    if chosen.len() != changed {
        return Err("not enough scoped regular files".into());
    }
    let mut latencies = Vec::with_capacity(bursts);
    for _ in 0..bursts {
        let old = session.view();
        let generation = old.generation();
        let log_before = fs::metadata(&log)?.len();
        let input = Instant::now();
        let mut wanted = BTreeSet::new();
        for &id in &chosen {
            for name in session.names_for(id) {
                let mut parent = old.name(name).parent;
                loop {
                    if !wanted.insert(parent) {
                        break;
                    }
                    let Some(name) = old.dir_name(parent) else {
                        break;
                    };
                    parent = old.name(name).parent;
                }
            }
        }
        let mut batch = session.batch();
        let mut queue = Vec::new();
        let roots: Vec<_> = old
            .roots()
            .map(|(_, p)| PathBuf::from(std::ffi::OsStr::from_bytes(p)))
            .collect();
        for (id, path) in old.roots() {
            if wanted.contains(&id) {
                queue.push((id, batch.root(path, old.inode(id).stat)));
            }
        }
        let mut observed = 0;
        while let Some((parent, token)) = queue.pop() {
            if let Some(count) = old.entry_count(parent) {
                batch.entry_count(token, count);
            }
            if let Some(work) = old.work_tree(parent) {
                batch.work_tree(token, work.kind, work.common_dir, work.common_id);
            }
            for name in old.children(parent) {
                let edge = old.name(name);
                let Target::Inode(id) = edge.target() else {
                    batch.preserve(token, edge.bytes);
                    continue;
                };
                if old.is_directory(id) && wanted.contains(&id) {
                    let stat = old.inode(id).stat;
                    let next = if old.is_traversed(id) {
                        batch.traversed_dir(token, edge.bytes, stat)
                    } else {
                        batch.dir(token, edge.bytes, stat)
                    };
                    queue.push((id, next));
                } else if chosen.contains(&id) {
                    let mut stat = old.inode(id).stat;
                    stat.mtime_sec += 1;
                    stat.ctime_sec += 1;
                    let mut hash = blake3::Hasher::new();
                    hash.update(b"M6 scoped changed bytes\0");
                    hash.update(&id.0.to_le_bytes());
                    hash.update(&generation.sequence.to_le_bytes());
                    let mut bytes = [0; 16];
                    bytes.copy_from_slice(&hash.finalize().as_bytes()[..16]);
                    batch.file(token, edge.bytes, stat, Content::Hashed(bytes));
                    observed += 1;
                } else {
                    batch.preserve(token, edge.bytes);
                }
            }
        }
        batch.seal();
        let observation_peak = batch.observation_peak();
        let replay_ms = input.elapsed().as_secs_f64() * 1000.0;
        let diff = Instant::now();
        let changes = ferret_crawl::reconcile::changes(
            &session,
            &[batch],
            &roots,
            &[],
            old.policy(),
            old.sniffer_version(),
        )?
        .ok_or("incomplete scoped observations")?;
        let diff_ms = diff.elapsed().as_secs_f64() * 1000.0;
        let commit = Instant::now();
        let current = session.commit(&changes, old.sniffer_version())?;
        let commit_ms = commit.elapsed().as_secs_f64() * 1000.0;
        let total_ms = input.elapsed().as_secs_f64() * 1000.0;
        let (final_rss, peak) = rss()?;
        let after = fs::metadata(&snapshot)?;
        if after.len() != snapshot_before.len()
            || after.modified()? != snapshot_before.modified()?
            || current.generation().checkpoint != generation.checkpoint
            || current.inode_count() != old.inode_count()
            || current.name_count() != old.name_count()
        {
            return Err("scoped refresh changed checkpoint or namespace".into());
        }
        let log_bytes = fs::metadata(&log)?.len() - log_before;
        latencies.push(total_ms);
        if bursts == 1 {
            println!(
                "| scoped changed files | setup ms | setup current/peak KiB | replay ms | diff ms | commit ms | latency ms | final/peak KiB | log/manifest bytes | observed names | local peak rows/bytes |"
            );
            println!(
                "| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
            );
            println!(
                "| {changed} | {setup_ms:.2} | {setup_rss}/{setup_peak} | {replay_ms:.2} | {diff_ms:.2} | {commit_ms:.2} | {total_ms:.2} | {final_rss}/{peak} | {log_bytes}/128 | {observed} | {}/{} |",
                observation_peak.0, observation_peak.1
            );
        }
    }
    if bursts > 1 {
        latencies.sort_by(f64::total_cmp);
        let (final_rss, peak) = rss()?;
        println!(
            "resident_bursts={bursts} setup_ms={setup_ms:.2} setup_amortized_ms={:.3} latency_median_ms={:.2} latency_min_ms={:.2} latency_max_ms={:.2} log_bytes={} manifest_bytes={} final_kib={final_rss} peak_kib={peak}",
            setup_ms / bursts as f64,
            latencies[bursts / 2],
            latencies[0],
            latencies[bursts - 1],
            fs::metadata(log)?.len() - initial_log,
            128 * bursts
        );
    }
    Ok(())
}

// Same lazy replacements as churn-rewalk, with two typed listing faults sent
// through the production full-root preparation seam. The synthetic fixture has
// no filesystem tree; this cannot measure getdents/stat/hash or syscall races.
fn churn_fault(dir: &Path, percent: usize) -> Result<(), Box<dyn Error>> {
    let setup = Instant::now();
    let mut session = WriterSession::open(dir)?;
    println!(
        "fault_rewalk setup_ms={:.2}",
        setup.elapsed().as_secs_f64() * 1000.0
    );
    for round in 1..=2 {
        let old = session.view();
        let total = old
            .inode_ids()
            .filter(|&id| old.kind(id) == Kind::File)
            .count();
        let count = total * percent / 100;
        let boundary = old
            .inode_ids()
            .filter(|&id| old.kind(id) == Kind::File)
            .nth(count - 1)
            .ok_or("not enough files")?;
        let leaf: Vec<_> = old
            .dir_ids()
            .filter(|&id| {
                old.entry_count(id).is_some_and(|count| count > 0)
                    && old.is_traversed(id) == old.is_search_suppressed(id)
                    && old.children(id).all(|name| match old.name(name).target() {
                        Target::Inode(child) => !old.is_directory(child),
                        _ => true,
                    })
            })
            .take(2)
            .collect();
        if leaf.len() != 2 {
            return Err("need two nonempty leaves".into());
        }
        let faults = [(leaf[0], 13), (leaf[1], 5)];
        let names_removed = old.children(leaf[0]).count();
        let start = Instant::now();
        let (_, usage, _) = replay_fault_churn(&session, boundary, false, &[])?;
        if !usage.exceeded {
            return Err("input guard did not trip".into());
        }
        let abandoned_ms = start.elapsed().as_secs_f64() * 1000.0;
        let replay = Instant::now();
        let (batches, _, faults) = replay_fault_churn(&session, boundary, true, &faults)?;
        let replay_ms = replay.elapsed().as_secs_f64() * 1000.0;
        let prepare = Instant::now();
        let roots: Vec<_> = old
            .roots()
            .map(|(_, p)| PathBuf::from(std::ffi::OsStr::from_bytes(p)))
            .collect();
        let (batches, scopes) = ferret_crawl::reconcile::checkpoint_batches(
            &session,
            batches,
            &faults,
            &roots,
            old.sniffer_version(),
            old.policy(),
        );
        let batches = batches.ok_or("synthetic coverage cannot be represented")?;
        let prepare_ms = prepare.elapsed().as_secs_f64() * 1000.0;
        let publish = Instant::now();
        let current = session.rebuild_checkpoint(batches, old.sniffer_version(), old.policy())?;
        let publication_ms = publish.elapsed().as_secs_f64() * 1000.0;
        let pause_ms = start.elapsed().as_secs_f64() * 1000.0;
        if current.name_count() as usize != old.name_count() as usize - names_removed
            || current.next_doc() != old.next_doc()
        {
            return Err("fault fallback changed namespace or DocId high-water incorrectly".into());
        }
        for (id, hash) in current.docs() {
            if old.doc_hash(id) != Some(hash) {
                return Err("live DocId changed".into());
            }
        }
        for fault in &faults {
            let path = fault.root.join(&fault.path);
            let mut found = None;
            let mut bytes = Vec::new();
            for id in current.dir_ids() {
                bytes.clear();
                current.dir_path(id, &mut bytes);
                if bytes == path.as_os_str().as_bytes() {
                    found = Some(id);
                    break;
                }
            }
            let id = found.ok_or("fault directory disappeared")?;
            if current.entry_count(id).is_some() {
                return Err("fault count became known".into());
            }
            if fault.error.raw_os_error() == Some(13) {
                if current.children(id).next().is_some() || current.retained_at(id).is_some() {
                    return Err("EACCES was not opaque".into());
                }
            } else if current.retained_at(id).is_none() || current.children(id).next().is_none() {
                return Err("EIO was not retained".into());
            }
        }
        let bytes = fs::metadata(Catalog::snapshot_path(dir)?.ok_or("missing checkpoint")?)?.len();
        let (resident, peak) = rss()?;
        println!(
            "fault_rewalk percent={percent} round={round} replaced_files={count} denied_names={names_removed} retained_scopes={scopes} abandoned_ms={abandoned_ms:.2} replay_ms={replay_ms:.2} prepare_ms={prepare_ms:.2} publication_ms={publication_ms:.2} pause_ms={pause_ms:.2} writes={} snapshot_bytes={bytes} charged_records={} charged_owned_bytes={} complete_diff_records=0 resident_kib={resident} peak_kib={peak}",
            bytes + 192,
            usage.records,
            usage.owned_bytes
        );
    }
    Ok(())
}

type FaultReplay = (
    Vec<ferret_catalog::Batch>,
    ferret_catalog::InputUsage,
    Vec<ferret_crawl::CoverageFault>,
);

fn replay_fault_churn(
    session: &ferret_catalog::WriterSession,
    boundary: ferret_catalog::InoId,
    full: bool,
    fault_dirs: &[(InoId, i32)],
) -> Result<FaultReplay, Box<dyn Error>> {
    use ferret_catalog::{Content, ContentState, Kind, Target};
    let old = session.view();
    let roots: Vec<_> = old
        .roots()
        .map(|(_, p)| PathBuf::from(std::ffi::OsStr::from_bytes(p)))
        .collect();
    let mut faults = Vec::new();
    let budget = std::sync::Arc::new(ferret_catalog::InputBudget::new(session.input_limits()));
    let mut batches: Vec<_> = (0..16)
        .map(|_| {
            if full {
                session.checkpoint_batch(
                    (old.name_count().saturating_sub(old.dir_count()) as usize).div_ceil(16),
                )
            } else {
                session.batch().with_input_budget(budget.clone())
            }
        })
        .collect();
    let mut queue = Vec::new();
    for (id, path) in old.roots() {
        let token = batches[id.0 as usize % 16].root(path, old.inode(id).stat);
        queue.push((id, token));
    }
    let mut at = 0;
    while at < queue.len() {
        let (dir, token) = queue[at];
        at += 1;
        let batch = &mut batches[dir.0 as usize % 16];
        if full && let Some(&(_, errno)) = fault_dirs.iter().find(|&&(id, _)| id == dir) {
            let mut path = Vec::new();
            old.dir_path(dir, &mut path);
            let path = Path::new(std::ffi::OsStr::from_bytes(&path));
            let root = roots
                .iter()
                .find(|root| path.starts_with(root))
                .ok_or("fault outside configured roots")?;
            faults.push(ferret_crawl::CoverageFault {
                root: root.clone(),
                path: path.strip_prefix(root)?.to_owned(),
                op: ferret_crawl::IoOp::List,
                on_root: false,
                context: ferret_crawl::CoverageContext::Directory(token),
                error: std::io::Error::from_raw_os_error(errno),
            });
            continue;
        }
        if let Some(entries) = old.entry_count(dir) {
            batch.entry_count(token, entries);
        }

        if let Some(work) = old.work_tree(dir) {
            batch.work_tree(token, work.kind, work.common_dir, work.common_id);
        }
        for name in old.children(dir) {
            if budget.exceeded() {
                return Ok((Vec::new(), budget.usage(), faults));
            }
            let edge = old.name(name);
            let Target::Inode(id) = edge.target() else {
                if let Target::Ignored(kind) = edge.target() {
                    batch.ignored(token, edge.bytes, kind);
                }
                continue;
            };
            let inode = old.inode(id);
            let mut stat = inode.stat;
            let content = match inode.state {
                ContentState::Unindexed => Content::Unindexed,
                ContentState::Binary => Content::Binary,
                ContentState::Fault => Content::Fault,
                ContentState::Hashed => Content::Hashed(
                    old.doc_hash(inode.doc.ok_or("missing content binding")?)
                        .ok_or("missing live hash")?,
                ),
            };
            if id <= boundary && old.kind(id) == Kind::File {
                stat.ino = stat.ino.wrapping_add(1u64 << 40);
                stat.ctime_sec += 1;
            }
            match old.kind(id) {
                Kind::Dir => {
                    let next = if old.is_traversed(id) {
                        batch.traversed_dir(token, edge.bytes, stat)
                    } else {
                        batch.dir(token, edge.bytes, stat)
                    };
                    queue.push((id, next));
                }
                Kind::Symlink => batch.symlink(
                    token,
                    edge.bytes,
                    stat,
                    old.link_target(id).ok_or("missing target")?,
                ),
                _ => batch.file(token, edge.bytes, stat, content),
            }
        }
        batch.finish_observations();
    }
    for batch in &mut batches {
        batch.seal();
    }
    if budget.exceeded() {
        return Ok((Vec::new(), budget.usage(), faults));
    }
    Ok((batches, budget.usage(), faults))
}
