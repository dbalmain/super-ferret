//! The `product` arm: every seed run as `grep:` through the product path,
//! [`Query::run_content_parallel`] (planner, postings, and the pooled
//! verifier), at each verification thread count, cold and warm (D76 C).
//!
//! The reader is the bench's, not the crawl's checked one: the frozen
//! corpus is a copy, so a document is read from its snapshot path (one
//! `open` and a read into the verifier's buffer) rather than through a
//! re-walk of its catalogued directories. Everything else is the product's.
//!
//! Per query: one discovery run at the most threads records the documents
//! read; then the index and those documents are evicted before each cold
//! run. Warm runs follow at every count. Every run's matched documents must
//! equal the oracle's, and every run must read the same documents.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Read as _;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use ferret_catalog::{NameId, Target};
use ferret_index::Pinned;
use ferret_query::{Content, DocNames, NameIndex, Query, ReadRequest, ReadVersion, Workers};
use ferret_verify::GrepFlags;

use super::super::latency::ns;
use super::super::{Corpus, stats};
use super::rg::counters;
use super::{CLASSES, evict_files, readable_oracle};

/// Warm runs at each count; cold at the ends and the middle. Either list
/// can be replaced, comma-separated, by `PRODUCT_THREADS` / `PRODUCT_COLD`
/// (the cold counts must be among the warm ones; empty for none).
const THREADS: [usize; 5] = [1, 4, 8, 16, 32];
const COLD: [usize; 3] = [1, 8, 32];

/// The warm and cold thread counts, from the environment or the defaults.
fn thread_lists() -> crate::Result<(Vec<usize>, Vec<usize>)> {
    let warm = counts("PRODUCT_THREADS", &THREADS)?;
    let cold = counts("PRODUCT_COLD", &COLD)?;
    check_lists(&warm, &cold)?;
    Ok((warm, cold))
}

/// Refuses lists the report cannot label: no warm count, a count of zero, a
/// count given twice, or a cold count with no warm run to go with it.
fn check_lists(warm: &[usize], cold: &[usize]) -> crate::Result<()> {
    if warm.is_empty() {
        return Err("PRODUCT_THREADS: no thread counts".into());
    }
    for (name, list) in [("PRODUCT_THREADS", warm), ("PRODUCT_COLD", cold)] {
        for (i, &threads) in list.iter().enumerate() {
            if threads == 0 {
                return Err(format!("{name}: thread counts must be positive").into());
            }
            if list[..i].contains(&threads) {
                return Err(format!("{name}: {threads} given twice").into());
            }
        }
    }
    if let Some(threads) = cold.iter().find(|t| !warm.contains(t)) {
        return Err(format!("PRODUCT_COLD: {threads} is not in PRODUCT_THREADS").into());
    }
    Ok(())
}

fn counts(name: &str, default: &[usize]) -> crate::Result<Vec<usize>> {
    match std::env::var(name) {
        Err(_) => Ok(default.to_vec()),
        Ok(list) => list
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| Ok(s.parse()?))
            .collect(),
    }
}

#[derive(Clone, Copy, Default)]
struct Run {
    wall: Duration,
    /// From the start until the first document read: planning, postings,
    /// and the candidate rows.
    before_reads: Duration,
    /// Summed busy time of every verifying thread.
    busy: Duration,
    cpu: f64,
    /// On-CPU time of the calling thread, which runs the candidate rows and
    /// emits in order: the pipeline's serial part.
    caller: Duration,
    /// Bytes the kernel fetched from the device.
    device: u64,
}

/// What one run of a query read and matched.
struct Once {
    run: Run,
    matched: Vec<u32>,
    read_docs: Vec<u32>,
}

fn once(
    corpus: &Corpus,
    names: &NameIndex,
    docs: &DocNames,
    query: &Query,
    threads: usize,
) -> crate::Result<Once> {
    let catalog = &corpus.catalog;
    let pinned = Pinned::new(Some(&corpus.view), &corpus.live);
    let content = Content {
        pinned: &pinned,
        docs,
        bound: None,
    };
    let first = Mutex::new(None::<Instant>);
    let read_docs = Mutex::new(Vec::new());
    let fork = || -> Box<ferret_query::Reader<'_>> {
        Box::new(|name: NameId, request: ReadRequest<'_>| {
            let doc = match catalog.name(name).target() {
                Target::Inode(inode) => catalog.doc(inode)?.0,
                _ => return None,
            };
            let path = corpus.snapshot.path(doc);
            match request {
                // The snapshot is frozen: still a filesystem operation, as
                // in the product reader, always the catalogued version.
                ReadRequest::Stat => fs::metadata(path).ok().map(|_| ReadVersion::Catalogued),
                ReadRequest::Bytes(out, _) => {
                    first
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get_or_insert_with(Instant::now);
                    out.clear();
                    File::open(path).ok()?.read_to_end(out).ok()?;
                    read_docs
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(doc);
                    Some(ReadVersion::Catalogued)
                }
            }
        })
    };
    let mut matched = Vec::new();
    let before = counters()?;
    let caller_before = caller_cpu()?;
    let start = Instant::now();
    let stats = query.run_content_parallel(
        catalog,
        names,
        &content,
        Workers {
            fork: &fork,
            threads,
        },
        None,
        |row| {
            if let Some(doc) = catalog.doc(row.inode) {
                matched.push(doc.0);
            }
            ControlFlow::Continue(())
        },
    )?;
    let wall = start.elapsed();
    let caller = caller_cpu()? - caller_before;
    let after = counters()?;
    let report = stats
        .content
        .ok_or("a grep: query reported no content plan")?;
    matched.sort_unstable();
    matched.dedup();
    let mut read_docs = read_docs
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    read_docs.sort_unstable();
    let first = first
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Ok(Once {
        run: Run {
            wall,
            before_reads: first.map_or(wall, |at| at - start),
            busy: report.verification_time,
            cpu: after.self_cpu - before.self_cpu,
            caller,
            device: after.device - before.device,
        },
        matched,
        read_docs,
    })
}

/// This thread's on-CPU time, in nanoseconds since it started.
fn caller_cpu() -> crate::Result<Duration> {
    let stat = fs::read_to_string("/proc/thread-self/schedstat")?;
    let ns = stat
        .split_whitespace()
        .next()
        .ok_or("empty schedstat")?
        .parse()?;
    Ok(Duration::from_nanos(ns))
}

fn evict_index(index: &Path) -> crate::Result<()> {
    for entry in fs::read_dir(index)? {
        let path = entry?.path();
        if path.is_file() {
            let file = File::open(&path)?;
            rustix::fs::fadvise(&file, 0, None, rustix::fs::Advice::DontNeed)?;
        }
    }
    Ok(())
}

struct Row {
    class: &'static str,
    candidates: u64,
    /// Each run with the thread count it ran at.
    cold: Vec<(usize, Run)>,
    warm: Vec<(usize, Run)>,
}

pub(super) fn arm(
    corpus: &Corpus,
    flags: GrepFlags,
    index: &Path,
    private: &mut String,
) -> crate::Result<()> {
    let names = NameIndex::new(&corpus.catalog);
    let docs = DocNames::new(&corpus.catalog)?;
    let inline = if flags.case_insensitive { "(?i)" } else { "" };
    let (threads_list, cold_list) = thread_lists()?;
    let most = *threads_list.iter().max().ok_or("no thread counts")?;
    let mut rows = Vec::new();
    for (q, sample) in corpus.samples.iter().enumerate() {
        crate::trigram::check_disk()?;
        let query = Query::from_args(
            &[format!("grep:{inline}{}", sample.grep.pattern())],
            SystemTime::now(),
        )?;
        let oracle = readable_oracle(corpus, q);
        let discovery = once(corpus, &names, &docs, &query, most)?;
        let check = |once: &Once, what: &str| -> crate::Result<()> {
            if once.matched != oracle {
                return Err(
                    format!("product matches differ from the oracle: query {q} {what}").into(),
                );
            }
            if once.read_docs != discovery.read_docs {
                return Err(format!("product read other documents: query {q} {what}").into());
            }
            Ok(())
        };
        check(&discovery, "discovery")?;
        let mut cold = Vec::new();
        let mut warm = Vec::new();
        for &threads in &threads_list {
            if cold_list.contains(&threads) {
                evict_index(index)?;
                evict_files(corpus, &discovery.read_docs)?;
                let run = once(corpus, &names, &docs, &query, threads)?;
                check(&run, &format!("cold at {threads}"))?;
                cold.push((threads, run.run));
            }
            let run = once(corpus, &names, &docs, &query, threads)?;
            check(&run, &format!("warm at {threads}"))?;
            warm.push((threads, run.run));
        }
        let candidates = corpus_bytes(corpus, &discovery.read_docs);
        let _ = writeln!(
            private,
            "product\t{q}\t{}\t{}\t{candidates}\t{}\t{}",
            sample.class,
            discovery.read_docs.len(),
            cold.iter()
                .map(|(_, r)| ns(r.wall).to_string())
                .collect::<Vec<_>>()
                .join(","),
            warm.iter()
                .map(|(_, r)| ns(r.wall).to_string())
                .collect::<Vec<_>>()
                .join(","),
        );
        println!(
            "progress arm=product query={} class={} read_B={candidates} cold_ms={} warm_ms={}",
            q + 1,
            sample.class,
            cold.iter()
                .map(|(_, r)| format!("{:.1}", r.wall.as_secs_f64() * 1e3))
                .collect::<Vec<_>>()
                .join("/"),
            warm.iter()
                .map(|(_, r)| format!("{:.1}", r.wall.as_secs_f64() * 1e3))
                .collect::<Vec<_>>()
                .join("/"),
        );
        rows.push(Row {
            class: sample.class,
            candidates,
            cold,
            warm,
        });
    }
    report(&rows, &threads_list, &cold_list);
    println!(
        "correctness arm=product queries={} matches_equal_oracle=all reads_stable=all",
        corpus.samples.len()
    );
    Ok(())
}

fn corpus_bytes(corpus: &Corpus, docs: &[u32]) -> u64 {
    docs.iter().map(|&d| corpus.sizes[d as usize]).sum()
}

fn ms(values: Vec<u64>) -> String {
    let (median, p90, _) = stats(values);
    format!("{:.1}/{:.1}", median as f64 / 1e6, p90 as f64 / 1e6)
}

fn report(rows: &[Row], threads_list: &[usize], cold_list: &[usize]) {
    println!(
        "| class | cache | threads | queries | read MB median | wall ms median/p90 | before reads ms median/p90 | busy ms median/p90 | cpu ms median/p90 | caller cpu ms median/p90 | device MB median/p90 | device MB/s median |"
    );
    for class in CLASSES {
        let picked: Vec<&Row> = rows.iter().filter(|r| r.class == class).collect();
        let read = stats(picked.iter().map(|r| r.candidates).collect()).0 as f64 / 1e6;
        for (cache, counts, pick) in [
            (
                "cold",
                cold_list,
                (|r: &Row| &r.cold) as fn(&Row) -> &Vec<(usize, Run)>,
            ),
            ("warm", threads_list, |r: &Row| &r.warm),
        ] {
            for &threads in counts {
                let runs: Vec<Run> = picked
                    .iter()
                    .flat_map(|r| pick(r).iter().filter(|&&(t, _)| t == threads))
                    .map(|&(_, run)| run)
                    .collect();
                let rate: Vec<u64> = runs
                    .iter()
                    .map(|r| (r.device as f64 / r.wall.as_secs_f64().max(1e-9) / 1e6) as u64)
                    .collect();
                println!(
                    "| {class} | {cache} | {threads} | {} | {read:.0} | {} | {} | {} | {} | {} | {} | {} |",
                    runs.len(),
                    ms(runs.iter().map(|r| ns(r.wall)).collect()),
                    ms(runs.iter().map(|r| ns(r.before_reads)).collect()),
                    ms(runs.iter().map(|r| ns(r.busy)).collect()),
                    ms(runs.iter().map(|r| (r.cpu * 1e9) as u64).collect()),
                    ms(runs.iter().map(|r| ns(r.caller)).collect()),
                    {
                        let (m, p, _) = stats(runs.iter().map(|r| r.device).collect());
                        format!("{:.0}/{:.0}", m as f64 / 1e6, p as f64 / 1e6)
                    },
                    stats(rate).0,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::check_lists;

    #[test]
    fn thread_lists_that_would_mislabel_runs_are_refused() {
        assert!(check_lists(&[1, 4, 8], &[1, 8]).is_ok());
        assert!(check_lists(&[1, 8], &[8, 1]).is_ok());
        assert!(check_lists(&[8], &[]).is_ok());
        for (warm, cold) in [
            (&[][..], &[][..]),
            (&[0, 4], &[]),
            (&[1, 4], &[0]),
            (&[1, 4, 1], &[]),
            (&[1, 4], &[4, 4]),
            (&[1, 4], &[8]),
        ] {
            assert!(check_lists(warm, cold).is_err(), "{warm:?} {cold:?}");
        }
    }
}
