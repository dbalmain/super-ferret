//! The D54 name index: its construction phases, query timings, and the
//! cost of adopting an update or a compaction.

use std::hint::black_box;
use std::ops::ControlFlow;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use ferret_catalog::Catalog;
use ferret_query::Query;

use crate::{Result, memory, open_catalog};

/// D54: the same catalog conversion and name index used by Engine, with each
/// construction phase separate. No CLI logging or extra engine dependency.
pub(crate) fn once(dir: &Path) -> Result<()> {
    let started = Instant::now();
    let catalog = open_catalog(dir)?;
    catalog.load_all()?;
    let load = started.elapsed();
    let projection_started = Instant::now();
    let catalog = catalog.into_resident()?;
    let projection = projection_started.elapsed();
    let index_started = Instant::now();
    let index = ferret_query::NameIndex::new(&catalog);
    index.warm(&catalog).map_err(|_| "stale query index")?;
    let index_time = index_started.elapsed();
    let full = started.elapsed();
    let (rss, peak) = memory()?;
    let rss_kib: u64 = rss.split_whitespace().next().ok_or("RSS")?.parse()?;
    let names = catalog.resident_names().ok_or("resident projection")?;
    println!(
        "name-open names={} distinct={} load_ms={:.3} projection_ms={:.3} index_ms={:.3} full_ms={:.3} rss_kib={} peak={} bytes_per_name={:.3} projection_bytes={} index_payload_bytes={}",
        catalog.name_count(),
        names.distinct_count(),
        duration_ms(load),
        duration_ms(projection),
        duration_ms(index_time),
        duration_ms(full),
        rss_kib,
        peak.replace(' ', ""),
        rss_kib as f64 * 1024.0 / f64::from(catalog.name_count()),
        names.bytes(),
        index.bytes()
    );
    for text in [
        "case:Flamegraph",
        "test",
        "*.rs",
        "name-term:catalog",
        "name-term:cache",
        "name-term:test",
        "",
    ] {
        name_query_samples(&catalog, &index, text, None, "global")?;
    }
    // These are corpus scopes, not live filesystem lookups. Report absent
    // shapes instead of substituting a synthetic distribution for nixpkgs.
    for (label, paths) in [
        (
            "w",
            &[
                "/synthetic/p0/w",
                "/synthetic/p0.m3/transcend-offload.m3/dave-work-laptop.m3/dave.m3/w",
                "/synthetic/p13.m3/transcend-offload.m3/dave-work-laptop.m3/dave.m3/w",
            ][..],
        ),
        ("super-ferret", &["/synthetic/p0/w/super-ferret"][..]),
        ("nixpkgs", &["/synthetic/p0/nixpkgs"][..]),
        (
            "large-monorepo",
            &["/synthetic/p0/sandpit/large-monorepo"][..],
        ),
    ] {
        let scope = catalog.dir_ids().find(|&dir| {
            let mut path = Vec::new();
            catalog.dir_path(dir, &mut path);
            paths.iter().any(|expected| path == expected.as_bytes())
        });
        let Some(scope) = scope else {
            println!("scope-unavailable {label}");
            continue;
        };
        let mut path = Vec::new();
        catalog.dir_path(scope, &mut path);
        println!("scope-path {}", path.escape_ascii());
        for text in ["case:package.json", "*.rs", "case:default.nix", "*ripgrep*"] {
            name_query_samples(&catalog, &index, text, Some(scope), label)?;
        }
    }
    println!(
        "name-index-build scope_ms={:?} term_ms={:?}",
        index.scope_build_time().map(duration_ms),
        index.term_build_time().map(duration_ms)
    );
    println!("raw_scan_bytes={}", names.raw_scan_bytes());
    Ok(())
}

fn name_query_samples(
    catalog: &Catalog,
    index: &ferret_query::NameIndex,
    text: &str,
    scope: Option<ferret_catalog::InoId>,
    label: &str,
) -> Result<()> {
    let query = Query::parse(text, SystemTime::now())?;
    let scope = scope.map(|id| ferret_catalog::Handle {
        generation: catalog.generation(),
        id,
    });
    let estimate = query
        .name_selection(catalog, index, scope)
        .map_err(|_| "stale query index")?
        .estimate;
    let (mut times, mut firsts, mut count) = (Vec::new(), Vec::new(), 0);
    for sample in 0..6 {
        let started = Instant::now();
        let mut first = None;
        let stats = query.run_indexed(catalog, index, scope, |row| {
            first.get_or_insert_with(|| started.elapsed());
            black_box(row.path);
            ControlFlow::Continue(())
        })?;
        if sample != 0 {
            times.push(duration_ms(started.elapsed()));
            if let Some(first) = first {
                firsts.push(duration_ms(first));
            }
        }
        count = stats.rows;
    }
    times.sort_by(f64::total_cmp);
    firsts.sort_by(f64::total_cmp);
    println!(
        "name-query scope={} query={:?} plan={:?} hits={} scope_rows={:?} rows={} median_ms={:.3} first_row_ms={:?} samples_ms={:?}",
        label,
        text,
        estimate.plan,
        estimate.hits,
        estimate.scope_rows,
        count,
        times[times.len() / 2],
        firsts.get(firsts.len() / 2),
        times
    );
    Ok(())
}

pub(crate) fn query(dir: &Path, scope_path: &str, text: &str, plan: &str) -> Result<()> {
    use ferret_catalog::InoId;
    let plan = match plan {
        "postings" => ferret_query::NamePlan::Postings,
        "walk" => ferret_query::NamePlan::ScopeWalk,
        _ => return Err("--plan must be postings or walk".into()),
    };
    let catalog = open_catalog(dir)?.into_resident()?;
    let index = ferret_query::NameIndex::new(&catalog);
    let scope = if scope_path == "-" {
        None
    } else {
        catalog
            .dir_ids()
            .find(|&id| {
                let mut path = Vec::new();
                catalog.dir_path(id, &mut path);
                path == scope_path.as_bytes()
            })
            .ok_or("scope path not found")?
            .into()
    };
    let scope = scope.map(|id: InoId| ferret_catalog::Handle {
        generation: catalog.generation(),
        id,
    });
    let query = Query::parse(text, SystemTime::now())?;
    let start = Instant::now();
    let mut rows = 0;
    let stats = query.run_indexed_plan(&catalog, &index, scope, Some(plan), |_| {
        rows += 1;
        ControlFlow::Continue(())
    })?;
    println!(
        "name-query query={text:?} plan={plan:?} rows={rows} candidates={} elapsed_ms={:.3}",
        stats.candidates,
        duration_ms(start.elapsed())
    );
    Ok(())
}

/// Name replacements and inode field changes, as in S1+'s overlay fixture.
/// Publication and per-view cache adoption use real APIs. The fixture has no
/// live filesystem, so kernel walking and hashing are deliberately excluded.
pub(crate) fn update(dir: &Path, count: &str) -> Result<()> {
    use ferret_catalog::log::{ChangeSet, Record};
    use ferret_catalog::{InoId, NameId, WriterSession};
    let count: u32 = count.parse()?;
    refuse_shared_snapshot(dir)?;
    let setup = Instant::now();
    let mut session = WriterSession::open(dir)?;
    session.set_compaction_limits(ferret_catalog::CompactionLimits {
        log_bytes: u64::MAX,
        records: u64::MAX,
        dirty_percent: 100,
        dead_percent: 100,
    });
    let previous = session.view();
    let old_epoch = previous.generation().checkpoint;
    let writer_setup = setup.elapsed();
    let setup = Instant::now();
    let index = ferret_query::NameIndex::new(&previous);
    index.warm(&previous).map_err(|_| "stale query index")?;
    let query_setup = setup.elapsed();
    if count > previous.base_name_count()
        || count > previous.base_inode_count() - previous.base_dir_count()
    {
        return Err("change exceeds fixture".into());
    }
    let mut records = Vec::new();
    for row in 0..count {
        let name = previous.name(NameId(row));
        let mut bytes = name.bytes.to_vec();
        bytes.extend_from_slice(b".s1b-cache");
        records.push(Record::NamePut {
            revealed: false,
            id: row,
            parent: name.parent.0,
            child: name.child.0,
            name: bytes,
        });
        let inode = InoId(previous.base_dir_count() + row);
        let value = previous.inode(inode);
        let mut stat = value.stat;
        stat.mode ^= 0o100;
        records.push(Record::InodePut {
            id: inode.0,
            kind: previous.kind(inode),
            state: value.state,
            doc: value.doc.map(|id| id.0),
            stat,
        });
    }
    let changes = ChangeSet {
        records,
        counters: [
            previous.next_inode().0,
            previous.next_name().0,
            previous.next_doc().0,
        ],
        counts: [
            previous.inode_count(),
            previous.name_count(),
            previous.dir_count(),
            previous.doc_count(),
        ],
    };
    let started = Instant::now();
    let current = session.commit(&changes, previous.sniffer_version())?;
    let publication = started.elapsed();
    let started = Instant::now();
    let next = ferret_query::NameIndex::adopt(&current, Some(&index));
    next.warm(&current).map_err(|_| "stale query index")?;
    let cache = started.elapsed();
    let (rss, peak) = memory()?;
    println!(
        "name-update rows={} writer_setup_ms={:.3} query_setup_ms={:.3} publish_ms={:.3} cache_ms={:.3} latency_ms={:.3} rss={} peak={} old_epoch={} epoch={}",
        count,
        duration_ms(writer_setup),
        duration_ms(query_setup),
        duration_ms(publication),
        duration_ms(cache),
        duration_ms(publication + cache),
        rss.replace(' ', ""),
        peak.replace(' ', ""),
        old_epoch,
        current.generation().checkpoint
    );
    name_query_samples(&current, &next, "name-term:cache", None, "updated")?;
    Ok(())
}

pub(crate) fn compact(dir: &Path) -> Result<()> {
    refuse_shared_snapshot(dir)?;
    let setup = Instant::now();
    let mut session = ferret_catalog::WriterSession::open(dir)?;
    let previous = session.view();
    let old_epoch = previous.generation().checkpoint;
    let index = ferret_query::NameIndex::new(&previous);
    index.warm(&previous).map_err(|_| "stale query index")?;
    let setup_ms = duration_ms(setup.elapsed());
    let started = Instant::now();
    let current = session.compact()?;
    let writer_pause = started.elapsed();
    let started = Instant::now();
    let next = ferret_query::NameIndex::adopt(&current, Some(&index));
    next.warm(&current).map_err(|_| "stale query index")?;
    let query_cache = started.elapsed();
    let names = current.resident_names().ok_or("resident compaction view")?;
    let (before_drop_rss, peak) = memory()?;
    drop((index, previous));
    let (after_drop_rss, _) = memory()?;
    drop(session);
    let (after_writer_drop_rss, _) = memory()?;
    println!(
        "name-compact setup_ms={setup_ms:.3} writer_pause_ms={:.3} projection_build_ms={:.3} query_cache_ms={:.3} pause_ms={:.3} rss_before_drop={} rss_after_drop={} rss_after_writer_drop={} peak={} old_epoch={} epoch={} sequence={} index_payload_bytes={}",
        duration_ms(writer_pause),
        duration_ms(names.build_time()),
        duration_ms(query_cache),
        duration_ms(writer_pause + query_cache),
        before_drop_rss.replace(' ', ""),
        after_drop_rss.replace(' ', ""),
        after_writer_drop_rss.replace(' ', ""),
        peak.replace(' ', ""),
        old_epoch,
        current.generation().checkpoint,
        current.generation().sequence,
        next.bytes()
    );
    Ok(())
}

fn refuse_shared_snapshot(dir: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let path = Catalog::snapshot_path(dir)?.ok_or("no snapshot")?;
    let links = std::fs::metadata(path)?.nlink();
    if links > 1 {
        return Err(format!("refusing shared snapshot with {links} hard links").into());
    }
    Ok(())
}

fn duration_ms(time: Duration) -> f64 {
    time.as_secs_f64() * 1000.0
}
