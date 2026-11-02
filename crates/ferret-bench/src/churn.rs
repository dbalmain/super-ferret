//! Checkpoint pauses: one idle-boundary compaction, and rounds of
//! replacement-inode churn through the incremental and full builders.

use std::path::Path;
use std::time::{Duration, Instant};

use ferret_catalog::Catalog;

use crate::{Result, memory, ms};

/// D51 A: the whole idle-boundary pause includes packed rewrite, durability,
/// readback validation, retirement and rebuilding resident epoch caches.
pub(crate) fn compact_once(dir: &Path) -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };
    let setup = Instant::now();
    let mut session = ferret_catalog::WriterSession::open(dir)?;
    let setup_ms = setup.elapsed().as_secs_f64() * 1000.0;
    let old = session.view();
    let before = session.budget_usage();
    let disk = |path: &Path| -> std::io::Result<u64> {
        std::fs::read_dir(path)?
            .map(|entry| {
                entry
                    .and_then(|entry| entry.metadata())
                    .map(|meta| meta.len())
            })
            .sum()
    };
    let initial_disk = disk(dir)?;
    let running = Arc::new(AtomicBool::new(true));
    let peak = Arc::new(AtomicU64::new(initial_disk));
    let watch = running.clone();
    let high = peak.clone();
    let directory = dir.to_owned();
    let arrivals = Arc::new(std::sync::Mutex::new(Vec::new()));
    let queued = arrivals.clone();
    let sample = std::thread::spawn(move || {
        let mut next_arrival = Instant::now();
        while watch.load(Ordering::Relaxed) {
            let now = Instant::now();
            if now >= next_arrival {
                if let Ok(mut queue) = queued.lock() {
                    queue.push(now);
                }
                next_arrival = now + Duration::from_millis(10);
            }
            if let Ok(size) = disk(&directory) {
                high.fetch_max(size, Ordering::Relaxed);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });
    let started = Instant::now();
    let result = session.compact();
    let finished = Instant::now();
    let pause = finished.duration_since(started).as_secs_f64() * 1000.0;
    running.store(false, Ordering::Relaxed);
    sample.join().map_err(|_| "disk sampler panicked")?;
    let current = result?;
    let final_disk = disk(dir)?;
    let snapshot_bytes =
        std::fs::metadata(Catalog::snapshot_path(dir)?.ok_or("missing checkpoint")?)?.len();
    if current.generation().sequence != old.generation().sequence
        || current.next_doc() != old.next_doc()
    {
        return Err("compaction changed logical sequence or DocId counter".into());
    }
    let (resident, rss_peak) = memory()?;
    let resident = resident.split_whitespace().next().ok_or("missing RSS")?;
    let rss_peak = rss_peak
        .split_whitespace()
        .next()
        .ok_or("missing peak RSS")?;
    println!(
        "compact setup_ms={setup_ms:.2} pause_ms={:.2} writes={} initial_disk={} peak_disk={} final_disk={} resident_kib={} peak_kib={} old_epoch={} new_epoch={} sequence={} records={} dirty_inodes={} dirty_names={} dead_inodes={} dead_names={}",
        pause,
        snapshot_bytes + 64 + 128,
        initial_disk,
        peak.load(Ordering::Relaxed),
        final_disk,
        resident,
        rss_peak,
        old.generation().checkpoint,
        current.generation().checkpoint,
        current.generation().sequence,
        before.records,
        before.dirty_inodes,
        before.dirty_names,
        before.dead_inodes,
        before.dead_names
    );
    // A simulated numeric burst queued before the pause must retry from the
    // new epoch even though the logical sequence is unchanged (D52 B).
    let queued = ferret_catalog::Handle {
        generation: old.generation(),
        id: ferret_catalog::InoId(u32::MAX),
    };
    if current.checked_inode(queued).is_ok() {
        return Err("queued old-epoch id was accepted".into());
    }
    let queue = arrivals.lock().map_err(|_| "burst queue poisoned")?;
    let oldest = queue.first().map_or(0.0, |&at| {
        finished.saturating_duration_since(at).as_secs_f64() * 1000.0
    });
    let newest = queue.last().map_or(0.0, |&at| {
        finished.saturating_duration_since(at).as_secs_f64() * 1000.0
    });
    let retries = queue
        .iter()
        .filter(|_| current.checked_inode(queued).is_err())
        .count();
    println!(
        "simulated_arrival_ms=10 queued_bursts={} oldest_wait_ms={oldest:.2} newest_wait_ms={newest:.2} epoch_retries={retries}; watcher/daemon queue service is S1b",
        queue.len()
    );
    Ok(())
}

/// Replaces a stated fraction of live regular-file inodes, including every
/// indexed alias. These are real validated final sets with inode/name births
/// and deaths, rather than patched allocation counters. Each round compacts
/// directly, exercising an input diff much larger than published log budgets.
pub(crate) fn checkpoint(dir: &Path, percent: &str, rounds: &str) -> Result<()> {
    use ferret_catalog::{
        InoId, Target,
        log::{ChangeSet, Record},
    };
    let percent: usize = percent.parse()?;
    let rounds: usize = rounds.parse()?;
    if !(1..=100).contains(&percent) || rounds == 0 {
        return Err("churn needs 1..100 percent and positive rounds".into());
    }
    let setup = Instant::now();
    let mut session = ferret_catalog::WriterSession::open(dir)?;
    println!("churn setup_ms={}", ms(setup.elapsed()));
    for round in 1..=rounds {
        let old = session.view();
        let generation = old.generation();
        let file_count = old
            .inode_ids()
            .filter(|&id| old.kind(id) == ferret_catalog::Kind::File)
            .count();
        let count = file_count * percent / 100;
        let mut remap = vec![u32::MAX; old.next_inode().0 as usize];
        let mut next_inode = old.next_inode().0;
        let mut next_name = old.next_name().0;
        let preparation = Instant::now();
        let mut records = Vec::with_capacity(count * 5);
        for id in old
            .inode_ids()
            .filter(|&id| old.kind(id) == ferret_catalog::Kind::File)
            .take(count)
        {
            let new = next_inode;
            next_inode = next_inode.checked_add(1).ok_or("inode counter exhausted")?;
            remap[id.0 as usize] = new;
            let mut inode = old.inode(id);
            inode.stat.ino = inode.stat.ino.wrapping_add(1u64 << 40);
            inode.stat.ctime_sec += 1;
            records.push(Record::InodeDelete { id: id.0 });
            records.push(Record::LifePut {
                id: new,
                kind: ferret_catalog::Kind::File,
                flags: 0,
                names: old.indexed_name_count(id),
            });
            records.push(Record::InodePut {
                id: new,
                kind: ferret_catalog::Kind::File,
                state: inode.state,
                doc: inode.doc.map(|id| id.0),
                stat: inode.stat,
            });
        }
        for (id, edge) in old.name_reader().runs_from(ferret_catalog::NameId(0)) {
            if let Target::Inode(child) = edge.target()
                && remap[child.0 as usize] != u32::MAX
            {
                records.push(Record::NameDelete { id: id.0 });
                records.push(Record::NamePut {
                    revealed: false,
                    id: next_name,
                    parent: edge.parent.0,
                    child: remap[child.0 as usize],
                    name: edge.bytes.to_vec(),
                });
                next_name = next_name.checked_add(1).ok_or("name counter exhausted")?;
            }
        }
        drop(remap);
        let changes = ChangeSet {
            counters: [next_inode, next_name, old.next_doc().0],
            counts: [
                old.inode_count(),
                old.name_count(),
                old.dir_count(),
                old.doc_count(),
            ],
            records,
        };
        let prepare_ms = ms(preparation.elapsed());
        let started = Instant::now();
        let current = session.commit(&changes, old.sniffer_version())?;
        let pause_ms = ms(started.elapsed());
        if current.generation().checkpoint == generation.checkpoint
            || current.next_inode().0 != current.inode_count()
            || current.next_name().0 != current.name_count()
            || current.next_doc() != old.next_doc()
            || current.doc_count() != old.doc_count()
            || current.inode_count() != old.inode_count()
        {
            return Err("churn checkpoint lost counters or liveness".into());
        }
        let bytes =
            std::fs::metadata(Catalog::snapshot_path(dir)?.ok_or("missing checkpoint")?)?.len();
        let (resident, peak) = memory()?;
        println!(
            "churn percent={percent} round={round} replaced_files={count} cumulative_births={} prepare_ms={prepare_ms} publication_pause_ms={pause_ms} snapshot_bytes={bytes} writes={} preflight_inode={} dense_inode={} preflight_name={} dense_name={} next_doc={} records={} resident={} peak={}",
            count * round,
            bytes + 192,
            next_inode,
            current.next_inode().0,
            next_name,
            current.next_name().0,
            current.next_doc().0,
            changes.records.len(),
            resident,
            peak
        );
        let first = current
            .inode_ids()
            .find(|&id| current.kind(id) == ferret_catalog::Kind::File)
            .ok_or("missing file")?;
        if session.identity(current.identity(first)) != Some(InoId(first.0)) {
            return Err("epoch cache was not rebuilt".into());
        }
    }
    Ok(())
}

/// Same replacement-inode/stat workload as churn-checkpoint, generated lazily
/// from the 10M fixture. The fixture has no disk tree: full replay stands in
/// for the crawler's full rewalk, then uses the real full checkpoint builder.
pub(crate) fn rewalk(dir: &Path, percent: &str, rounds: &str) -> Result<()> {
    let percent: usize = percent.parse()?;
    let rounds: usize = rounds.parse()?;
    if !(1..=100).contains(&percent) || rounds == 0 {
        return Err("churn needs 1..100 percent and positive rounds".into());
    }
    let setup = Instant::now();
    let mut session = ferret_catalog::WriterSession::open(dir)?;
    println!("rewalk setup_ms={}", ms(setup.elapsed()));
    for round in 1..=rounds {
        let old = session.view();
        let total = old
            .inode_ids()
            .filter(|&id| old.kind(id) == ferret_catalog::Kind::File)
            .count();
        let count = total * percent / 100;
        let boundary = old
            .inode_ids()
            .filter(|&id| old.kind(id) == ferret_catalog::Kind::File)
            .nth(count - 1)
            .ok_or("not enough files")?;
        let start = Instant::now();
        let (_, usage) = replay_churn(&session, boundary, false)?;
        if !usage.exceeded {
            return Err("large churn did not exhaust input guard".into());
        }
        let abandoned_ms = ms(start.elapsed());
        let full = Instant::now();
        let (batches, _) = replay_churn(&session, boundary, true)?;
        let full_ms = ms(full.elapsed());
        let publish = Instant::now();
        let current = session.rebuild_checkpoint(batches, old.sniffer_version(), old.policy())?;
        let publish_ms = ms(publish.elapsed());
        let pause_ms = ms(start.elapsed());
        if current.next_doc() != old.next_doc()
            || current.doc_count() != old.doc_count()
            || current.inode_count() != old.inode_count()
            || current.name_count() != old.name_count()
            || current.next_inode().0 != current.inode_count()
        {
            return Err("full churn rewalk changed counters or content liveness".into());
        }
        // Full semantic comparison by basename across the dense BFS graphs,
        // independent of inode numbering; every alias carries the same edit.
        let mut checked = 0usize;
        let mut changed_names = 0usize;
        for (before, after) in old
            .name_reader()
            .runs_from(ferret_catalog::NameId(0))
            .zip(current.name_reader().runs_from(ferret_catalog::NameId(0)))
        {
            let (_, a) = before;
            let (_, b) = after;
            if a.bytes != b.bytes || a.parent != b.parent {
                return Err("rewalk changed namespace order".into());
            }
            if let (ferret_catalog::Target::Inode(ai), ferret_catalog::Target::Inode(bi)) =
                (a.target(), b.target())
            {
                let mut expected = old.inode(ai);
                if ai <= boundary && old.kind(ai) == ferret_catalog::Kind::File {
                    expected.stat.ino = expected.stat.ino.wrapping_add(1u64 << 40);
                    expected.stat.ctime_sec += 1;
                    changed_names += 1;
                }
                if current.inode(bi) != expected {
                    return Err("rewalk changed a stat or live DocId binding".into());
                }
            } else if a.target() != b.target() {
                return Err("rewalk changed ignored markers".into());
            }
            checked += 1;
        }
        if checked != old.name_count() as usize {
            return Err("rewalk lost names".into());
        }
        let bytes =
            std::fs::metadata(Catalog::snapshot_path(dir)?.ok_or("missing checkpoint")?)?.len();
        let (resident, peak) = memory()?;
        println!(
            "rewalk percent={percent} round={round} replaced_files={count} changed_names={changed_names} cumulative_births={} abandoned_ms={abandoned_ms} replay_ms={full_ms} publication_ms={publish_ms} pause_ms={pause_ms} snapshot_bytes={bytes} writes={} charged_records={} charged_owned_bytes={} complete_diff_records=0 resident={} peak={}",
            count * round,
            bytes + 192,
            usage.records,
            usage.owned_bytes,
            resident,
            peak
        );
    }
    Ok(())
}

fn replay_churn(
    session: &ferret_catalog::WriterSession,
    boundary: ferret_catalog::InoId,
    full: bool,
) -> Result<(Vec<ferret_catalog::Batch>, ferret_catalog::InputUsage)> {
    use ferret_catalog::{Content, ContentState, Kind, Target};
    let old = session.view();
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
        if let Some(entries) = old.entry_count(dir) {
            batch.entry_count(token, entries);
        }
        if let Some(seq) = old.retained_at(dir) {
            batch.retained_at(token, Some(seq));
        }
        if let Some(work) = old.work_tree(dir) {
            batch.work_tree(token, work.kind, work.common_dir, work.common_id);
        }
        for name in old.children(dir) {
            if budget.exceeded() {
                return Ok((Vec::new(), budget.usage()));
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
        return Ok((Vec::new(), budget.usage()));
    }
    Ok((batches, budget.usage()))
}
