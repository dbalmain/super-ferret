//! A suffix array and an FM-index as regex screens over the D65C corpus
//! (docs/FMINDEX.md). Subcommands, each its own process so peak RSS is per
//! step:
//!
//! ```text
//! fm-screen plan     <corpus> <oracle> <dir>
//! fm-screen build-sa <corpus> <oracle> <dir> <k> <threads>
//! fm-screen build-fm <dir> <k> <rate,...>
//! fm-screen lookup   <corpus> <oracle> <dir> <sa|fm32|fm64|...> <k> <full|rarest> [cap]
//! fm-screen verify   <corpus> <oracle> <dir> <candset> <threads> <line,whole>
//! fm-screen report   <dir>
//! ```
//!
//! Patterns and paths stay in the scratch directory; stdout carries
//! aggregates and per-query numbers only.

mod index;
mod pop;
mod query;
mod report;
mod util;
mod verify;

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use index::{Fm, Sa, Table};
use query::Index;

fn sizes_by_doc(docs: &pop::Docs) -> Vec<u64> {
    let mut v = vec![0u64; docs.ids.last().map_or(0, |&d| d as usize + 1)];
    for (&d, &s) in docs.ids.iter().zip(&docs.sizes) {
        v[d as usize] = s;
    }
    v
}

fn resources(label: &str, cpu0: f64, wall0: f64) {
    println!(
        "{label} wall_s={:.2} cpu_s={:.2} peak_rss_kb={}",
        util::wall() - wall0,
        util::cpu_seconds() - cpu0,
        util::peak_rss_kb()
    );
}

fn plan(corpus: &Path, oracle: &Path, dir: &Path) {
    let (queries, docs) = pop::load(corpus, oracle);
    let segs = index::segments(&docs);
    println!(
        "docs={} bytes={} segments={}",
        docs.ids.len(),
        docs.sizes.iter().sum::<u64>(),
        segs.len()
    );
    for (k, r) in segs.iter().enumerate() {
        println!(
            "segment={k} docs={} bytes={}",
            r.len(),
            docs.sizes[r.clone()].iter().sum::<u64>()
        );
    }
    for class in pop::CLASSES {
        let picked: Vec<_> = queries.iter().filter(|q| q.class == class).collect();
        let mut why = std::collections::BTreeMap::new();
        for q in &picked {
            if let Err(e) = q.lines {
                *why.entry(e).or_insert(0) += 1;
            }
        }
        let leaf_sets: Vec<BTreeSet<Vec<u8>>> = picked
            .iter()
            .map(|q| {
                let mut s = BTreeSet::new();
                query::leaves(&q.facts, &mut s);
                s
            })
            .collect();
        let all = picked
            .iter()
            .filter(|q| matches!(q.facts, ferret_verify::trigram::RequiredLiterals::All))
            .count();
        println!(
            "class={class} queries={} facts_all={all} line_sound={} line_unsound={why:?} leaves_median_p90={:?} leaf_len_median_p90={:?}",
            picked.len(),
            picked.iter().filter(|q| q.lines.is_ok()).count(),
            util::median_p90(leaf_sets.iter().map(|s| s.len() as f64).collect()),
            util::median_p90(leaf_sets.iter().flatten().map(|l| l.len() as f64).collect()),
        );
    }
    fs::create_dir_all(dir.join("rows")).expect("rows");
}

fn lookup(
    corpus: &Path,
    oracle: &Path,
    dir: &Path,
    arm: &str,
    k: usize,
    plan: query::Plan,
    cap: Option<u64>,
) {
    let (queries, docs) = pop::load(corpus, oracle);
    let sizes = sizes_by_doc(&docs);
    let table = Table::read(dir, k);
    let rate: Option<u32> = arm.strip_prefix("fm").map(|r| r.parse().expect("rate"));
    let files: Vec<PathBuf> = match rate {
        None => Sa::files(dir, k).to_vec(),
        Some(r) => Fm::files(dir, k, r).to_vec(),
    };
    let mut set = arm.to_string();
    if plan == query::Plan::Rarest {
        set += "-rarest";
    }
    if let Some(c) = cap {
        set += &format!("-cap{c}");
    }
    if std::env::var("FM_RESIDENT").is_ok() {
        set += "-resident";
    }
    if std::env::var("FM_DRY").is_ok() {
        set += "-dry";
    }
    // FM_CLASS limits a run to one class (rows for the rest are skipped).
    let only = std::env::var("FM_CLASS").ok();
    let cand_dir = dir.join("cand").join(&set);
    fs::create_dir_all(&cand_dir).expect("cand dir");
    fs::create_dir_all(dir.join("rows")).expect("rows");
    // FM_RESIDENT=1: the index is loaded whole before the queries (timed,
    // from cold, as a sequential read) and never evicted, so "cold" rows are
    // only the documents' cold; demand-paged random faults are not measured.
    let resident = std::env::var("FM_RESIDENT").is_ok();
    if resident {
        for f in &files {
            index::evict(f);
        }
        let t = std::time::Instant::now();
        let mut bytes = 0u64;
        for f in &files {
            bytes += fs::read(f).expect("index file").len() as u64;
        }
        println!(
            "resident_load arm={arm} segment={k} bytes={bytes} cold_sequential_ms={:.1}",
            t.elapsed().as_secs_f64() * 1e3
        );
        // fs::read leaves the pages cached; mapped queries now find them.
    }
    let mut rows = String::new();
    let run = |q: &pop::Query| match rate {
        None => {
            let sa = Sa::open(dir, k);
            query::run(
                Index::Sa(&sa),
                &table,
                &sizes,
                &q.facts,
                q.lines.is_ok(),
                cap,
                plan,
            )
        }
        Some(r) => {
            let fm = Fm::open(dir, k, r);
            query::run(
                Index::Fm(&fm),
                &table,
                &sizes,
                &q.facts,
                q.lines.is_ok(),
                cap,
                plan,
            )
        }
    };
    for (i, q) in queries.iter().enumerate() {
        if only.as_deref().is_some_and(|c| c != q.class) {
            continue;
        }
        let (found, _) = run(q);
        let twin = dir
            .join("cand")
            .join(set.replacen(arm, "sa", 1).replace("-resident", ""))
            .join(format!("q{i}.k{k}"));
        if rate.is_some() && twin.exists() && std::env::var("FM_DRY").is_err() {
            // The FM-index must locate exactly what the suffix array did
            // under the same plan and cap.
            let sa = fs::read(&twin).expect("sa cands");
            assert!(
                query::decode(&sa) == found,
                "fm differs from sa: query {i} segment {k}"
            );
        }
        fs::write(cand_dir.join(format!("q{i}.k{k}")), query::encode(&found)).expect("cands");
        if !resident {
            for f in &files {
                index::evict(f);
            }
        }
        let device = util::device_read_bytes();
        let (cold_c, cold) = run(q);
        let device = util::device_read_bytes() - device;
        let (warm_c, warm) = run(q);
        assert!(
            cold_c == found && warm_c == found,
            "candidates moved: query {i}"
        );
        let ns = |d: std::time::Duration| d.as_nanos();
        let _ = writeln!(
            rows,
            "{i}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{device}",
            q.class,
            ns(cold.count),
            ns(cold.locate),
            ns(cold.combine),
            ns(cold.total),
            ns(warm.count),
            ns(warm.locate),
            ns(warm.combine),
            ns(warm.total),
            cold.occurrences,
            cold.located,
            cold.steps,
            cold.capped_leaves,
            found.len(),
        );
        println!(
            "progress arm={set} segment={k} query={} occurrences={} cold_ms={:.2} warm_ms={:.2} locate_warm_ms={:.2}",
            i + 1,
            cold.occurrences,
            cold.total.as_secs_f64() * 1e3,
            warm.total.as_secs_f64() * 1e3,
            warm.locate.as_secs_f64() * 1e3
        );
    }
    let suffix = only.map_or(String::new(), |c| format!("-{}", c.replace(' ', "_")));
    fs::write(
        dir.join("rows")
            .join(format!("lookup-{set}-k{k}{suffix}.tsv")),
        rows,
    )
    .expect("rows");
}

fn verify(corpus: &Path, oracle: &Path, dir: &Path, set: &str, threads: usize, modes: &str) {
    let (queries, docs) = pop::load(corpus, oracle);
    let sizes = sizes_by_doc(&docs);
    let segs = index::segments(&docs).len();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .expect("pool");
    let mut rows = String::new();
    for (i, q) in queries.iter().enumerate() {
        let mut cands = Vec::new();
        for k in 0..segs {
            let bytes =
                fs::read(dir.join("cand").join(set).join(format!("q{i}.k{k}"))).expect("cands");
            cands.extend(query::decode(&bytes));
        }
        let ids: Vec<u32> = cands.iter().map(|c| c.doc).collect();
        let missed = q
            .oracle
            .iter()
            .filter(|d| ids.binary_search(d).is_err())
            .count();
        let cand_bytes: u64 = ids.iter().map(|&d| sizes[d as usize]).sum();
        for mode in modes.split(',') {
            let whole = mode == "whole";
            verify::evict(&docs, &cands);
            let (cold_m, cold) = verify::run(&pool, &q.grep, &docs, &sizes, &cands, whole);
            let (warm_m, warm) = verify::run(&pool, &q.grep, &docs, &sizes, &cands, whole);
            let equal = cold_m == q.oracle && warm_m == q.oracle;
            let _ = writeln!(
                rows,
                "{i}\t{}\t{mode}\t{}\t{cand_bytes}\t{missed}\t{equal}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                q.class,
                ids.len(),
                cold.time.as_nanos(),
                warm.time.as_nanos(),
                cold.read,
                cold.regex,
                cold.device,
                cold.whole_docs,
                cold.line_docs,
                warm.read,
                q.lines.err().unwrap_or("-"),
            );
            println!(
                "progress verify={set} mode={mode} query={} docs={} cand_MB={:.1} missed={missed} equal={equal} read_MB={:.2} cold_ms={:.1} warm_ms={:.1}",
                i + 1,
                ids.len(),
                cand_bytes as f64 / 1e6,
                cold.read as f64 / 1e6,
                cold.time.as_secs_f64() * 1e3,
                warm.time.as_secs_f64() * 1e3
            );
        }
    }
    fs::write(
        dir.join("rows").join(format!(
            "verify-{set}-t{threads}{}.tsv",
            std::env::var("FM_VERIFY_TAG").unwrap_or_default()
        )),
        rows,
    )
    .expect("rows");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cpu0, wall0) = (util::cpu_seconds(), util::wall());
    let p = |i: usize| PathBuf::from(&args[i]);
    match args[0].as_str() {
        "plan" => plan(&p(1), &p(2), &p(3)),
        "build-sa" => {
            let (_, docs) = pop::load(&p(1), &p(2));
            util::check_disk(&p(3));
            let (cpu1, wall1) = (util::cpu_seconds(), util::wall());
            let k = args[4].parse().expect("k");
            println!(
                "{}",
                index::build_sa(&p(3), &docs, k, args[5].parse().expect("threads"))
            );
            resources(&format!("build_sa segment={k}"), cpu1, wall1);
        }
        "build-fm" => {
            util::check_disk(&p(1));
            let k = args[2].parse().expect("k");
            let rates: Vec<u32> = args[3]
                .split(',')
                .map(|r| r.parse().expect("rate"))
                .collect();
            println!("{}", index::build_fm(&p(1), k, &rates));
            resources(&format!("build_fm segment={k}"), cpu0, wall0);
        }
        "lookup" => lookup(
            &p(1),
            &p(2),
            &p(3),
            &args[4],
            args[5].parse().expect("k"),
            match args[6].as_str() {
                "full" => query::Plan::Full,
                "rarest" => query::Plan::Rarest,
                other => panic!("unknown plan {other}"),
            },
            args.get(7).map(|c| c.parse().expect("cap")),
        ),
        "verify" => verify(
            &p(1),
            &p(2),
            &p(3),
            &args[4],
            args[5].parse().expect("threads"),
            &args[6],
        ),
        "report" => report::run(&p(1)),
        other => panic!("unknown subcommand {other}"),
    }
}
