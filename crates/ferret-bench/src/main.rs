//! `ferret-bench`: measurements that no product crate should carry.
//!
//! ```text
//! ferret-bench scan  <catalog-dir> [needle...]   scanner arms against memmem
//! ferret-bench open  <catalog-dir>               open and load, by section set
//! ferret-bench sections <catalog-dir>            bytes per name, per section
//! ferret-bench query <catalog-dir> [query...]    the D40 query mix
//! ferret-bench census <catalog-dir>              Hashed documents: bytes, sizes, extensions
//! ferret-bench corpus-sample <catalog-dir> <out-dir> <n>   copy a reproducible sample of ~n documents
//! ferret-bench tokenize <corpus-dir>             tokenizer throughput and term counts
//! ferret-bench terms <catalog-dir>               full-tree term census: df, hapax, pairs
//! ferret-bench segment-build <catalog-dir> <out-dir> [--breakdown]  real segments: bytes by section, CPU, RSS
//! ferret-bench index-build <catalog-dir> <index-dir>  follow, merge to steady state, full merge (S2 M3)
//! ferret-bench index-refresh <index-dir>   512 × 64 KiB, 40 paired follow refreshes
//! ferret-bench trigram-build <catalog-dir> <out-dir>  offline trigrams at M3 boundaries
//! ferret-bench trigram-census <catalog-dir>  distinct-key histogram and D69 counts
//! ferret-bench trigram-memory <catalog-dir> <out-dir> a <tri-MiB> | b  D70 paired buffers
//! ferret-bench filter-screen <catalog-dir> <term-segment> <tri-dir> <out-dir>  D68: fuse filters against postings
//! ferret-bench saturation <catalog-dir> <term-segment> <tri-dir> <out-dir>  D69: saturated large documents
//! ferret-bench index-churn <index-dir> <rounds>   synthetic churn: merge write amplification
//! ferret-bench verify-100k [--smoke]  checked reads and follow on 100k files
//! ferret-bench steady-content-follow [--smoke]  steady follow after real refresh
//! ferret-bench content-query <catalog-dir> <index-dir> [class...]  text: latency by class, phrase verification (S2 M4b)
//! ferret-bench blob-screen sample|run <catalog-dir> <index-dir> <snapshot-dir> ...  T1: routed blob text: screens
//! ferret-bench rank-fp <catalog-dir> <out-dir>   static df≥2 dictionary: 32-bit sparse fingerprints vs 20-bit buckets
//! ferret-bench sparse-latency <catalog-dir> <index-dir> <snapshot-dir> <scratch-dir> [variant]  cold/warm regex screens
//! ferret-bench screens <catalog-dir> <control-index> <t1-index> <snapshot-dir> <scratch-dir> <arm,...>  screens against rg (SCREENS.md)
//! ferret-bench regex-volume <catalog-dir> <index-dir> <snapshot-dir>  M5 candidate-byte diagnostic
//! ferret-bench chunk-census <catalog-dir> <index-dir>   D66 heuristic chunk map census (S3 M6)
//! ferret-bench overlay-fill <catalog-dir> <rows>   mixed name/inode overrides
//! ferret-bench resident-once <catalog-dir> <query> resident query time and RSS
//! ferret-bench name-index-once <catalog-dir> open phases and query timings
//! ferret-bench name-index-update <catalog-dir> <rows> publish that many name and inode rows and adopt
//! ferret-bench name-index-compact <catalog-dir> compact and rebuild the index
//! ferret-bench name-index-query <catalog-dir> <scope-path|-> <query> --plan postings|walk
//! ferret-bench overlay-carry <catalog-dir> <count> one-inode geometric carries
//! ferret-bench overlay-carry-boundary <catalog-dir> <rows> a large carry
//! ```
//!
//! Build it in release (`cargo build --release -p ferret-bench`). Every
//! command prints a Markdown table or `key: value` lines on stdout. Run one at
//! a time on the machine: the numbers are wall-clock.
//!
//! **Warm** means the catalog file is in the page cache: each case runs once
//! unmeasured, then the median of several runs is reported. **Evicted**
//! means the file was dropped from the page cache (`posix_fadvise`
//! `DONTNEED`) before each run, so every byte the run needs comes from the
//! device; that is the first query after the cache has moved on, not a
//! cold boot.
//!
//! `scan` times each arm of [`ferret_verify::Finder`] over the whole name
//! heap, finding every match (resuming one byte past each), against
//! `memchr::memmem` on the same heap. For a folded search memmem has no
//! folding, so its row runs on a lower-cased copy of the heap, the second
//! heap D41 option A would need; the copy's cost is not in its time.
//!
//! `query` times each query from `Catalog::open` to the last row: parse,
//! plan, section loads, scan, tests and path resolution, with the rows
//! discarded. Time to first row is when the first row reached the callback.

use std::error::Error;
use std::fs::File;
use std::hint::black_box;
use std::ops::ControlFlow;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime};

use ferret_catalog::{Catalog, Section};
use ferret_query::Query;
use ferret_verify::{Arm, Finder};

mod blob_screen;
mod census;
mod chunk_census;
mod churn;
mod content_engine;
mod content_query;
mod content_snapshot;
mod corpus;
mod fuse;
mod index_build;
mod name_index;
mod paired;
mod rank_fp;
mod regex_volume;
mod saturation;
mod screen;
mod segment_build;
mod sparse;
mod terms;
mod tokenize;
mod trigram;

#[cfg(test)]
mod support;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// The D40 mix, chosen against `$HOME`'s names (the synthetic catalogs are
/// copies of the same tree): `Flamegraph` matches 4 names there and
/// `flamegraph` 5, `test` about 6,400 and 6,900, `.JPG` 6,000 and `.jpg`
/// folded 15,600.
const MIX: &[&str] = &[
    "case:Flamegraph",
    "flamegraph",
    "case:test",
    "test",
    "case:ext:JPG",
    "ext:jpg",
    "src/**/*.rs",
    r"re:^test_.*\.py$",
    "re:^[0-9a-f]{8}$",
    "mtime:<1d",
    "size:>100M",
    "ext:rs size:>10k",
    "",
];

/// The scanner's needles: rare, common, an extension.
const NEEDLES: &[&str] = &["Flamegraph", "test", ".JPG"];

const WARM_RUNS: usize = 7;
const EVICTED_RUNS: usize = 3;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.split_first() {
        Some((command, rest)) => match (command.as_str(), rest) {
            ("verify-100k", []) => content_engine::verify(false),
            ("verify-100k", [flag]) if flag == "--smoke" => content_engine::verify(true),
            ("steady-content-follow", []) => content_engine::follow(false),
            ("steady-content-follow", [flag]) if flag == "--smoke" => content_engine::follow(true),
            ("scan", [dir, needles @ ..]) => scan(Path::new(dir), needles),
            ("open", [dir]) => open(Path::new(dir)),
            ("open-once", [dir, set]) => open_once(Path::new(dir), set),
            ("checksum", [dir]) => checksum(Path::new(dir)),
            ("census", [dir]) => census::run(Path::new(dir)),
            ("corpus-sample", [dir, out, n]) => corpus::run(Path::new(dir), Path::new(out), n),
            ("tokenize", [dir]) => tokenize::run(Path::new(dir)),
            ("terms", [dir]) => terms::run(Path::new(dir)),
            ("segment-build", [dir, out]) => {
                segment_build::run(Path::new(dir), Path::new(out), false)
            }
            ("segment-build", [dir, out, flag]) if flag == "--breakdown" => {
                segment_build::run(Path::new(dir), Path::new(out), true)
            }
            ("index-freeze", [dir, out]) => {
                content_snapshot::capture(Path::new(dir), Path::new(out))
            }
            ("index-build", [dir, out, options @ ..]) => {
                index_build::run(Path::new(dir), Path::new(out), options)
            }
            ("index-refresh", [out]) => index_build::refresh_trace(Path::new(out)),
            ("trigram-build", [dir, out]) => {
                trigram::build(Path::new(dir), Path::new(out), trigram::Policy::M3)
            }
            ("trigram-boundaries", [dir, out]) => {
                trigram::boundaries(Path::new(dir), Path::new(out))
            }
            ("trigram-census", [dir]) => trigram::census(Path::new(dir)),
            ("trigram-memory", [dir, out, policy]) if policy == "b" => {
                trigram::build(Path::new(dir), Path::new(out), trigram::Policy::Shared)
            }
            ("trigram-memory", [dir, out, policy, mib]) if policy == "a" => {
                mib.parse::<usize>().map_err(Into::into).and_then(|mib| {
                    let trigram_bound = mib.checked_mul(1 << 20).ok_or("bound overflow")?;
                    trigram::build(
                        Path::new(dir),
                        Path::new(out),
                        trigram::Policy::Own { trigram_bound },
                    )
                })
            }
            ("filter-screen", [dir, terms, tri, out]) => screen::run(
                Path::new(dir),
                Path::new(terms),
                Path::new(tri),
                Path::new(out),
                &screen::CONFIG,
            )
            .map(drop),
            ("saturation", [dir, terms, tri, out]) => saturation::run(
                Path::new(dir),
                Path::new(terms),
                Path::new(tri),
                Path::new(out),
                &saturation::config(),
            )
            .map(drop),
            ("index-churn", [out, rounds, options @ ..]) => {
                index_build::churn(Path::new(out), rounds, options)
            }
            ("sparse-latency", [dir, index, snapshot, scratch]) => sparse::latency::run(
                Path::new(dir),
                Path::new(index),
                Path::new(snapshot),
                Path::new(scratch),
                None,
            ),
            ("sparse-latency", [dir, index, snapshot, scratch, only]) => sparse::latency::run(
                Path::new(dir),
                Path::new(index),
                Path::new(snapshot),
                Path::new(scratch),
                Some(only),
            ),
            ("sparse", [dir, index, snapshot]) => {
                sparse::run(Path::new(dir), Path::new(index), Path::new(snapshot), None)
            }
            ("sparse", [dir, index, snapshot, only]) => sparse::run(
                Path::new(dir),
                Path::new(index),
                Path::new(snapshot),
                Some(only),
            ),
            ("regex-volume", [dir, index, snapshot]) => {
                regex_volume::run(Path::new(dir), Path::new(index), Path::new(snapshot))
            }
            ("regex-query", [dir, index, snapshot]) => {
                content_query::run_regex(Path::new(dir), Path::new(index), Path::new(snapshot))
            }
            ("content-query", [dir, index, only @ ..]) => {
                content_query::run(Path::new(dir), Path::new(index), only)
            }
            ("blob-screen", args) => blob_screen::run(args),
            ("screens", args) => sparse::screens::run(args),
            ("pure-scan", args) => sparse::screens::scan::run(args),
            ("rank-fp", [dir, out]) => rank_fp::run(Path::new(dir), Path::new(out)),
            ("chunk-census", [dir, index]) => chunk_census::run(Path::new(dir), Path::new(index)),
            ("log-fill", [dir, transactions, rows]) => log_fill(Path::new(dir), transactions, rows),
            ("log-append-once", [dir, rows]) => log_append_once(Path::new(dir), rows),
            ("log-open-once", [dir]) => log_open_once(Path::new(dir)),
            ("overlay-fill", [dir, rows]) => overlay_fill(Path::new(dir), rows),
            ("resident-once", [dir, text]) => resident_once(Path::new(dir), text),
            ("name-index-once", [dir]) => name_index::once(Path::new(dir)),
            ("name-index-update", [dir, rows]) => name_index::update(Path::new(dir), rows),
            ("name-index-compact", [dir]) => name_index::compact(Path::new(dir)),
            ("name-index-query", [dir, scope, query, flag, plan]) if flag == "--plan" => {
                name_index::query(Path::new(dir), scope, query, plan)
            }
            ("overlay-rename-once", [dir]) => overlay_rename_once(Path::new(dir)),
            ("overlay-carry", [dir, count]) => overlay_carry(Path::new(dir), count),
            ("overlay-carry-boundary", [dir, rows]) => overlay_carry_boundary(Path::new(dir), rows),
            ("recrawl-once", [dir, rows, producer]) => {
                recrawl_once(Path::new(dir), rows, Path::new(producer))
            }
            ("compact-once", [dir]) => churn::compact_once(Path::new(dir)),
            ("churn-rewalk", [dir, percent, rounds]) => {
                churn::rewalk(Path::new(dir), percent, rounds)
            }
            ("churn-checkpoint", [dir, percent, rounds]) => {
                churn::checkpoint(Path::new(dir), percent, rounds)
            }
            ("sections", [dir]) => sections(Path::new(dir)),
            ("query", [dir, queries @ ..]) => query(Path::new(dir), queries),
            _ => return usage(),
        },
        None => return usage(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ferret-bench: {error}");
            ExitCode::from(1)
        }
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: ferret-bench scan <catalog-dir> [needle...]\n       \
         ferret-bench verify-100k [--smoke]\n       \
         ferret-bench steady-content-follow [--smoke]\n       \
         ferret-bench census <catalog-dir>\n       \
         ferret-bench corpus-sample <catalog-dir> <out-dir> <n>\n       \
         ferret-bench tokenize <corpus-dir>\n       \
         ferret-bench terms <catalog-dir>\n       \
         ferret-bench segment-build <catalog-dir> <out-dir> [--breakdown]\n       \
         ferret-bench index-freeze <catalog-dir> <snapshot-dir>\n       \
         ferret-bench index-build <catalog-dir> <index-dir> [--codec 1|h] [--snapshots] [--control-bytes N] [--corpus snapshot-dir]\n       \
         ferret-bench index-refresh <index-dir>\n       \
         ferret-bench trigram-build <catalog-dir> <out-dir>\n       \
         ferret-bench trigram-census <catalog-dir>\n       \
         ferret-bench trigram-memory <catalog-dir> <out-dir> a <tri-MiB> | b\n       \
         ferret-bench filter-screen <catalog-dir> <term-segment> <tri-dir> <out-dir>\n       \
         ferret-bench saturation <catalog-dir> <term-segment> <tri-dir> <out-dir>\n       \
         ferret-bench index-churn <index-dir> <rounds> [--codec 1|h]\n       \
         ferret-bench sparse-latency <catalog-dir> <index-dir> <snapshot-dir> <scratch-dir> [variant]\n       \
         ferret-bench sparse <catalog-dir> <index-dir> <snapshot-dir> [variant]\n       \
         ferret-bench screens <catalog-dir> <control-index> <t1-index> <snapshot-dir> <scratch-dir> <arm,...>\n       \
         ferret-bench regex-volume <catalog-dir> <index-dir> <snapshot-dir>\n       \
         ferret-bench regex-query <catalog-dir> <index-dir> <snapshot-dir>\n       \
         ferret-bench content-query <catalog-dir> <index-dir> [--terms control-full] [--compare control-state] [--corpus snapshot-dir] [--covered] [class...]\n       \
         ferret-bench blob-screen sample <catalog-dir> <index-dir> <snapshot-dir> <out-file>\n       \
         ferret-bench blob-screen run <catalog-dir> <index-dir> <snapshot-dir> <queries-file> <out-tsv>\n       \
         ferret-bench rank-fp <catalog-dir> <out-dir>\n       \
         ferret-bench chunk-census <catalog-dir> <index-dir>\n       \
         ferret-bench recrawl-once <catalog-dir> <changed-files> <crawl-producer>\n       \
         ferret-bench open <catalog-dir>\n       \
         ferret-bench open-once <catalog-dir> names|metadata|full|legacy-full\n       \
         ferret-bench log-fill <catalog-dir> <transactions> <records>\n       \
         ferret-bench log-append-once <catalog-dir> <records>\n       \
         ferret-bench log-open-once <catalog-dir>\n       \
         ferret-bench overlay-fill <catalog-dir> <rows>\n       \
         ferret-bench resident-once <catalog-dir> <query>\n       \
         ferret-bench name-index-once <catalog-dir>\n       \
         ferret-bench name-index-update <catalog-dir> <rows>\n       \
         ferret-bench name-index-compact <catalog-dir>\n       \
         ferret-bench name-index-query <catalog-dir> <scope-path|-> <query> --plan postings|walk\n       \
         ferret-bench overlay-rename-once <catalog-dir>\n       \
         ferret-bench overlay-carry <catalog-dir> <count>\n       \
         ferret-bench overlay-carry-boundary <catalog-dir> <rows>\n       \
         ferret-bench checksum <catalog-dir>\n       \
         ferret-bench churn-checkpoint <catalog-dir> <percent> <rounds>\n       \
         ferret-bench churn-rewalk <catalog-dir> <percent> <rounds>\n       \
         ferret-bench compact-once <catalog-dir>\n       \
         ferret-bench sections <catalog-dir>\n       \
         ferret-bench query <catalog-dir> [query...]"
    );
    ExitCode::from(2)
}

fn open_catalog(dir: &Path) -> Result<Catalog> {
    Ok(Catalog::open(dir)?.ok_or_else(|| format!("no catalog in {}", dir.display()))?)
}

/// Drops the catalog file from the page cache.
fn evict(dir: &Path) -> Result<()> {
    let file = File::open(Catalog::snapshot_path(dir)?.ok_or("no snapshot")?)?;
    rustix::fs::fadvise(&file, 0, None, rustix::fs::Advice::DontNeed)?;
    Ok(())
}

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len() / 2]
}

fn ms(d: Duration) -> String {
    format!("{:.2}", d.as_secs_f64() * 1e3)
}

fn describe(dir: &Path, catalog: &Catalog) -> Result<()> {
    let size = std::fs::metadata(Catalog::snapshot_path(dir)?.ok_or("no snapshot")?)?.len();
    println!(
        "catalog {}: {} names, {} inodes, {} docs, file {:.1} MB",
        dir.display(),
        catalog.name_count(),
        catalog.inode_count(),
        catalog.doc_count(),
        size as f64 / 1e6
    );
    Ok(())
}

// ── scan ──

fn scan(dir: &Path, needles: &[String]) -> Result<()> {
    let catalog = open_catalog(dir)?;
    describe(dir, &catalog)?;
    catalog.load(&[Section::NameHeap])?;
    let heap = catalog.name_heap();
    let lowered = heap.to_ascii_lowercase();
    println!(
        "heap {:.1} MB, warm, median of {WARM_RUNS}\n",
        heap.len() as f64 / 1e6
    );
    println!("| needle | case | hits | arm | ms | GB/s |");
    println!("|---|---|---:|---|---:|---:|");
    let needles: Vec<&str> = if needles.is_empty() {
        NEEDLES.to_vec()
    } else {
        needles.iter().map(String::as_str).collect()
    };
    for needle in needles {
        for fold in [false, true] {
            let case = if fold { "folded" } else { "exact" };
            let mut rows = Vec::new();
            for arm in [Arm::Avx2, Arm::Swar] {
                if arm.is_available() {
                    let finder = Finder::with_arm(needle.as_bytes(), fold, arm);
                    let (hits, time) = time_all(|from| finder.find_from(heap, from));
                    rows.push((format!("{arm:?}"), hits, time));
                }
            }
            let (subject, pattern) = if fold {
                (lowered.as_slice(), needle.to_ascii_lowercase())
            } else {
                (heap, needle.to_string())
            };
            let finder = memchr::memmem::Finder::new(pattern.as_bytes());
            let (hits, time) = time_all(|from| finder.find(&subject[from..]).map(|at| at + from));
            let label = if fold {
                "memmem, lower-cased copy"
            } else {
                "memmem"
            };
            rows.push((label.to_string(), hits, time));
            for (arm, hits, time) in rows {
                println!(
                    "| `{needle}` | {case} | {hits} | {arm} | {} | {:.2} |",
                    ms(time),
                    heap.len() as f64 / time.as_secs_f64() / 1e9
                );
            }
        }
    }
    Ok(())
}

/// Every match through `find` (resuming one past each), warm: one unmeasured
/// pass, then the median of [`WARM_RUNS`].
fn time_all(find: impl Fn(usize) -> Option<usize>) -> (u64, Duration) {
    let count = || {
        let (mut hits, mut from) = (0, 0);
        while let Some(at) = find(from) {
            hits += 1;
            from = at + 1;
        }
        hits
    };
    let hits = black_box(count());
    let times = (0..WARM_RUNS)
        .map(|_| {
            let start = Instant::now();
            black_box(count());
            start.elapsed()
        })
        .collect();
    (hits, median(times))
}

// ── sections ──

/// Every section's exact size and its bytes per name, and the file's: the
/// figure a format change moves. Reads only the section table.
fn sections(dir: &Path) -> Result<()> {
    let catalog = open_catalog(dir)?;
    describe(dir, &catalog)?;
    let names = f64::from(catalog.name_count());
    let file = std::fs::metadata(Catalog::snapshot_path(dir)?.ok_or("no snapshot")?)?.len();
    println!("\n| section | bytes | B/name |");
    println!("|---|---:|---:|");
    for (section, len) in catalog.section_sizes() {
        println!("| {section:?} | {len} | {:.2} |", len as f64 / names);
    }
    let other = file - catalog.section_sizes().map(|(_, len)| len).sum::<u64>();
    println!(
        "| header, table and padding | {other} | {:.2} |",
        other as f64 / names
    );
    println!("| **file** | {file} | {:.2} |", file as f64 / names);
    Ok(())
}

/// One fresh process for warm load/RSS measurement, using the same section
/// sets as `open`. RSS is the process high-water mark, in KiB, on Linux.
fn open_once(dir: &Path, set: &str) -> Result<()> {
    let start = Instant::now();
    let catalog = open_catalog(dir)?;
    let names = [
        Section::Names,
        Section::DirNames,
        Section::Roots,
        Section::Traversed,
        Section::Links,
    ];
    match set {
        "names" => catalog.load(&names)?,
        "metadata" => {
            catalog.load(&names)?;
            catalog.load(&Section::INODE)?;
        }
        "full" => catalog.load_all()?,
        "legacy-full" => catalog.load(
            &catalog
                .section_sizes()
                .map(|(s, _)| s)
                .filter(|s| !matches!(s, Section::DocRefs | Section::RetainedAt | Section::Policy))
                .collect::<Vec<_>>(),
        )?,
        _ => return Err(format!("unknown open section set {set}").into()),
    }
    let elapsed = start.elapsed();
    let status = std::fs::read_to_string("/proc/self/status")?;
    let rss = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .ok_or("no VmHWM")?
        .trim();
    println!(
        "{set}: {} ms, {} bytes read, RSS {rss}",
        ms(elapsed),
        catalog.bytes_read()
    );
    Ok(())
}

/// Hashes every persisted section through the catalog's real checksum
/// primitive. Reading/warming is outside the timed interval.
fn checksum(dir: &Path) -> Result<()> {
    let catalog = open_catalog(dir)?;
    let bytes = std::fs::read(Catalog::snapshot_path(dir)?.ok_or("no snapshot")?)?;
    let mut start = catalog.head_len() as usize;
    let mut total = 0;
    let before = Instant::now();
    for (_, len) in catalog.section_sizes() {
        let end = start + len as usize;
        std::hint::black_box(ferret_catalog::checkpoint_checksum(&bytes[start..end]));
        total += len;
        start = end;
    }
    let elapsed = before.elapsed();
    println!(
        "checksum: {} bytes, {} ms, {:.3} GB/s",
        total,
        ms(elapsed),
        total as f64 / elapsed.as_secs_f64() / 1e9
    );
    Ok(())
}

// ── open ──

fn open(dir: &Path) -> Result<()> {
    describe(dir, &open_catalog(dir)?)?;
    let name_sections = [
        Section::Names,
        Section::NameHeap,
        Section::DirNames,
        Section::Roots,
        Section::Traversed,
        Section::Links,
    ];
    println!("\n| open | cache | ms | bytes read |");
    println!("|---|---|---:|---:|");
    let mut metadata_sections = name_sections.to_vec();
    metadata_sections.extend(Section::INODE);
    let cases: [(&str, &[Section]); 4] = [
        ("header and table", &[]),
        ("name sections (a name query's load)", &name_sections),
        ("name and inode metadata", &metadata_sections),
        ("every section", &[]),
    ];
    for (i, (label, sections)) in cases.into_iter().enumerate() {
        let run = || -> Result<(Duration, u64)> {
            let start = Instant::now();
            let catalog = open_catalog(dir)?;
            if i == 3 {
                catalog.load_all()?;
            } else {
                catalog.load(sections)?;
            }
            Ok((start.elapsed(), catalog.bytes_read()))
        };
        for evicted in [false, true] {
            let runs = if evicted { EVICTED_RUNS } else { WARM_RUNS };
            if !evicted {
                run()?;
            }
            let mut times = Vec::new();
            let mut bytes = 0;
            for _ in 0..runs {
                if evicted {
                    evict(dir)?;
                }
                let (time, read) = run()?;
                times.push(time);
                bytes = read;
            }
            let cache = if evicted { "evicted" } else { "warm" };
            println!("| {label} | {cache} | {} | {bytes} |", ms(median(times)));
        }
    }
    fault_split(dir, &name_sections)
}

/// Splits a warm name-section load into its page faults and its copy: the
/// same number of bytes read from the file into a fresh buffer, whose pages
/// the kernel faults in as the read fills them, and again into the same
/// buffer, now resident.
fn fault_split(dir: &Path, name_sections: &[Section]) -> Result<()> {
    use std::os::unix::fs::FileExt;
    let catalog = open_catalog(dir)?;
    catalog.load(name_sections)?;
    let len = catalog.bytes_read() as usize;
    drop(catalog);
    let file = File::open(Catalog::snapshot_path(dir)?.ok_or("no snapshot")?)?;
    let (mut fresh, mut resident) = (Vec::new(), Vec::new());
    let mut buffer = Vec::new();
    for _ in 0..WARM_RUNS {
        buffer = vec![0u8; len];
        let start = Instant::now();
        file.read_exact_at(&mut buffer, 0)?;
        fresh.push(start.elapsed());
        let start = Instant::now();
        file.read_exact_at(&mut buffer, 0)?;
        resident.push(start.elapsed());
    }
    black_box(&buffer);
    println!(
        "| {len} B into a fresh buffer | warm | {} | {len} |",
        ms(median(fresh))
    );
    println!(
        "| {len} B into a resident buffer | warm | {} | {len} |",
        ms(median(resident))
    );
    Ok(())
}

// ── query ──

fn query(dir: &Path, queries: &[String]) -> Result<()> {
    describe(dir, &open_catalog(dir)?)?;
    let queries: Vec<&str> = if queries.is_empty() {
        MIX.to_vec()
    } else {
        queries.iter().map(String::as_str).collect()
    };
    let now = SystemTime::now();
    println!(
        "warm: median of {WARM_RUNS}; evicted: median of {EVICTED_RUNS}; \
         times in ms from open to the row\n"
    );
    println!(
        "| query | plan | rows | warm first | warm all | evicted first | evicted all | MB read |"
    );
    println!("|---|---|---:|---:|---:|---:|---:|---:|");
    for text in queries {
        let plan = Query::parse(text, now)?.explain();
        let mut cells = Vec::new();
        let mut rows = 0;
        let mut read = 0;
        for evicted in [false, true] {
            let runs = if evicted { EVICTED_RUNS } else { WARM_RUNS };
            if !evicted {
                run_query(dir, text, now)?;
            }
            let (mut firsts, mut alls) = (Vec::new(), Vec::new());
            for _ in 0..runs {
                if evicted {
                    evict(dir)?;
                }
                let timed = run_query(dir, text, now)?;
                firsts.push(timed.first.unwrap_or(timed.all));
                alls.push(timed.all);
                (rows, read) = (timed.rows, timed.read);
            }
            cells.push(ms(median(firsts)));
            cells.push(ms(median(alls)));
        }
        println!(
            "| `{text}` | {plan} | {rows} | {} | {:.1} |",
            cells.join(" | "),
            read as f64 / 1e6
        );
    }
    Ok(())
}

struct Timed {
    first: Option<Duration>,
    all: Duration,
    rows: u64,
    read: u64,
}

fn run_query(dir: &Path, text: &str, now: SystemTime) -> Result<Timed> {
    let start = Instant::now();
    let catalog = open_catalog(dir)?;
    let query = Query::parse(text, now)?;
    let mut first = None;
    let mut sink = 0usize;
    let stats = query.run(&catalog, |row| {
        if first.is_none() {
            first = Some(start.elapsed());
        }
        // Touch the materialised path and the row, then discard them.
        sink = sink.wrapping_add(row.path.len()) ^ row.inode.0 as usize;
        ControlFlow::Continue(())
    })?;
    let all = start.elapsed();
    black_box(sink);
    Ok(Timed {
        first,
        all,
        rows: stats.rows,
        read: catalog.bytes_read(),
    })
}

// Log benchmarks use real metadata replacements, preserving document bindings.
// Preparing rows and locked recovery are outside the append timer.
fn log_changes(
    p: &ferret_catalog::log::Published,
    rows: usize,
) -> Result<ferret_catalog::log::ChangeSet> {
    use ferret_catalog::log::{ChangeSet, Record};
    let base = p.checkpoint();
    base.load(&Section::INODE)?;
    if rows > (base.inode_count() - base.dir_count()) as usize {
        return Err("not enough file rows".into());
    }
    let mut records = Vec::new();
    for id in base.dir_count()..base.dir_count() + rows as u32 {
        let inode = base.inode(ferret_catalog::InoId(id));
        let mut stat = inode.stat;
        stat.mode ^= 0o100;
        records.push(Record::InodePut {
            id,
            kind: ferret_catalog::Kind::from_mode(stat.mode),
            state: inode.state,
            doc: inode.doc.map(|d| d.0),
            stat,
        });
    }
    Ok(ChangeSet {
        records,
        counters: p.counters(),
        counts: p.counts(),
    })
}
fn log_fill(dir: &Path, transactions: &str, rows: &str) -> Result<()> {
    let count: usize = transactions.parse()?;
    let rows: usize = rows.parse()?;
    let mut writer = ferret_catalog::log::Writer::open(dir)?;
    let p = ferret_catalog::log::Published::open(dir)?.ok_or("no checkpoint")?;
    let changes = log_changes(&p, rows)?;
    for _ in 0..count {
        writer.commit(writer.generation(), &changes)?;
    }
    println!(
        "filled {count} transactions of {rows} records, sequence {}",
        writer.generation().sequence
    );
    Ok(())
}
fn log_append_once(dir: &Path, rows: &str) -> Result<()> {
    let rows: usize = rows.parse()?;
    let mut writer = ferret_catalog::log::Writer::open(dir)?;
    let p = ferret_catalog::log::Published::open(dir)?.ok_or("no checkpoint")?;
    let changes = log_changes(&p, rows)?;
    let path = dir.join(format!("changes.{}", p.generation().checkpoint));
    let before = std::fs::metadata(&path)?.len();
    let start = Instant::now();
    writer.commit(writer.generation(), &changes)?;
    let elapsed = start.elapsed();
    let bytes = std::fs::metadata(path)?.len() - before;
    println!(
        "log-append: {rows} records, {bytes} log bytes + {} manifest bytes, {:.3} ms",
        if rows == 0 { 0 } else { 128 },
        elapsed.as_secs_f64() * 1e3
    );
    Ok(())
}
fn log_open_once(dir: &Path) -> Result<()> {
    let start = Instant::now();
    let p = ferret_catalog::log::Published::open(dir)?.ok_or("no checkpoint")?;
    let elapsed = start.elapsed();
    let status = std::fs::read_to_string("/proc/self/status")?;
    let rss = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .ok_or("no VmHWM")?
        .trim();
    println!(
        "log-open: {} transactions, {:.3} ms, {} checkpoint + {} log bytes read, RSS {rss}",
        p.log().transaction_count(),
        elapsed.as_secs_f64() * 1e3,
        p.checkpoint().bytes_read(),
        p.log().bytes_read()
    );
    Ok(())
}

// M3 uses the product writer and effective reader, with fixture preparation
// outside query timers. Both names and inode fields change at the stated rate.
fn overlay_fill(dir: &Path, rows: &str) -> Result<()> {
    use ferret_catalog::log::{ChangeSet, Record, Writer};
    use ferret_catalog::{InoId, NameId};
    let rows: u32 = rows.parse()?;
    let mut writer = Writer::open(dir)?;
    let view = writer.view();
    if rows > view.base_name_count() || rows > view.base_inode_count() - view.base_dir_count() {
        return Err("overlay exceeds fixture".into());
    }
    let mut records = Vec::new();
    for id in 0..rows {
        let n = view.name(NameId(id));
        let mut name = n.bytes.to_vec();
        name.extend_from_slice(b".m3");
        records.push(Record::NamePut {
            revealed: false,
            id,
            parent: n.parent.0,
            child: n.child.0,
            name,
        });
    }
    for id in view.base_dir_count()..view.base_dir_count() + rows {
        let inode = view.inode(InoId(id));
        let mut stat = inode.stat;
        stat.size = stat.size.saturating_add(1);
        stat.mode ^= 0o100;
        records.push(Record::InodePut {
            id,
            kind: view.kind(InoId(id)),
            state: inode.state,
            doc: inode.doc.map(|d| d.0),
            stat,
        });
    }
    let changes = ChangeSet {
        records,
        counters: [view.next_inode().0, view.next_name().0, view.next_doc().0],
        counts: [
            view.inode_count(),
            view.name_count(),
            view.dir_count(),
            view.doc_count(),
        ],
    };
    writer.commit(writer.generation(), &changes)?;
    println!(
        "overlay {rows} names + {rows} inode fields; sequence {}",
        writer.generation().sequence
    );
    Ok(())
}
fn memory() -> Result<(String, String)> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .map(str::trim)
            .map(str::to_owned)
            .ok_or_else(|| format!("no {name}"))
    };
    Ok((field("VmRSS:")?, field("VmHWM:")?))
}
fn resident_once(dir: &Path, text: &str) -> Result<()> {
    let catalog = open_catalog(dir)?;
    catalog.load_all()?;
    let query = Query::parse(text, SystemTime::now())?;
    let mut samples = Vec::new();
    let mut count = 0;
    for _ in 0..14 {
        let mut sink = 0usize;
        let start = Instant::now();
        let stats = query.run(&catalog, |row| {
            sink = sink.wrapping_add(row.path.len()) ^ row.inode.0 as usize;
            ControlFlow::Continue(())
        })?;
        let time = start.elapsed();
        black_box(sink);
        count = stats.rows;
        samples.push(ms(time));
    }
    samples.remove(0);
    let (rss, peak) = memory()?;
    println!(
        "resident {:?}: rows {count}, samples {:?} ms, RSS {rss}, peak {peak}, runs {}",
        text,
        samples,
        catalog.overlay_run_count()
    );
    Ok(())
}
fn overlay_carry(dir: &Path, count: &str) -> Result<()> {
    use ferret_catalog::{
        InoId,
        log::{ChangeSet, Record, Writer},
    };
    let count: u32 = count.parse()?;
    let mut writer = Writer::open(dir)?;
    let mut view = writer.view();
    let mut times = Vec::new();
    for step in 0..count {
        let id = view.base_dir_count() + step;
        let inode = view.inode(InoId(id));
        let mut stat = inode.stat;
        stat.size = stat.size.saturating_add(7);
        let changes = ChangeSet {
            records: vec![Record::InodePut {
                id,
                kind: view.kind(InoId(id)),
                state: inode.state,
                doc: inode.doc.map(|d| d.0),
                stat,
            }],
            counters: [view.next_inode().0, view.next_name().0, view.next_doc().0],
            counts: [
                view.inode_count(),
                view.name_count(),
                view.dir_count(),
                view.doc_count(),
            ],
        };
        let before = view.overlay_run_count();
        let start = Instant::now();
        let candidate = view.advance(view.generation(), &changes)?;
        let elapsed = ms(start.elapsed());
        let after = candidate.overlay_run_count();
        writer.commit(writer.generation(), &changes)?;
        view = writer.view();
        times.push((step + 1, before, after, elapsed));
    }
    let (rss, peak) = memory()?;
    println!("carry {:?}; RSS {rss}, peak {peak}", times);
    Ok(())
}

/// Seed geometric runs in bursts, then time the individual updates on both
/// sides of a large carry. Every burst and timed update is also published by
/// the real writer; durable I/O is outside the resident advance timer.
fn overlay_carry_boundary(dir: &Path, rows: &str) -> Result<()> {
    use ferret_catalog::{
        InoId,
        log::{ChangeSet, Record, Writer},
    };
    let rows: u32 = rows.parse()?;
    let mut writer = Writer::open(dir)?;
    let mut view = writer.view();
    if !rows.is_power_of_two()
        || rows < 4
        || rows + 1 > view.base_inode_count() - view.base_dir_count()
    {
        return Err("rows must be a power of two within the base file count".into());
    }
    let changes = |view: &Catalog, offset: u32, count: u32| {
        let records = (offset..offset + count)
            .map(|offset| {
                let id = view.base_dir_count() + offset;
                let inode = view.inode(InoId(id));
                let mut stat = inode.stat;
                stat.size = stat.size.saturating_add(7);
                Record::InodePut {
                    id,
                    kind: view.kind(InoId(id)),
                    state: inode.state,
                    doc: inode.doc.map(|d| d.0),
                    stat,
                }
            })
            .collect();
        ChangeSet {
            records,
            counters: [view.next_inode().0, view.next_name().0, view.next_doc().0],
            counts: [
                view.inode_count(),
                view.name_count(),
                view.dir_count(),
                view.doc_count(),
            ],
        }
    };
    let mut offset = 0;
    let mut burst = rows / 2;
    while burst >= 2 {
        let c = changes(&view, offset, burst);
        writer.commit(writer.generation(), &c)?;
        view = writer.view();
        offset += burst;
        burst /= 2;
    }
    let mut times = Vec::new();
    for _ in 0..3 {
        let c = changes(&view, offset, 1);
        let before = view.overlay_run_count();
        let start = Instant::now();
        let candidate = view.advance(view.generation(), &c)?;
        let elapsed = ms(start.elapsed());
        let after = candidate.overlay_run_count();
        writer.commit(writer.generation(), &c)?;
        view = writer.view();
        offset += 1;
        times.push((offset, before, after, elapsed));
    }
    let (rss, peak) = memory()?;
    println!("boundary {:?}; RSS {rss}, peak {peak}", times);
    Ok(())
}

/// Namespace publication also builds a latest-name heap and suppression
/// stream. Measure that sparse work separately from metadata run carries.
fn overlay_rename_once(dir: &Path) -> Result<()> {
    use ferret_catalog::log::{ChangeSet, Record, Writer};
    let mut writer = Writer::open(dir)?;
    let view = writer.view();
    let (id, _) = view.names().next().ok_or("empty namespace")?;
    let old = view.name(id);
    let mut name = old.bytes.to_vec();
    name.extend_from_slice(b".next");
    let c = ChangeSet {
        records: vec![Record::NamePut {
            revealed: false,
            id: id.0,
            parent: old.parent.0,
            child: old.child.0,
            name,
        }],
        counters: [view.next_inode().0, view.next_name().0, view.next_doc().0],
        counts: [
            view.inode_count(),
            view.name_count(),
            view.dir_count(),
            view.doc_count(),
        ],
    };
    let before = view.overlay_run_count();
    let start = Instant::now();
    let candidate = view.advance(view.generation(), &c)?;
    let elapsed = ms(start.elapsed());
    let after = candidate.overlay_run_count();
    writer.commit(writer.generation(), &c)?;
    let (rss, peak) = memory()?;
    println!("rename {elapsed} ms; runs {before} -> {after}; RSS {rss}, peak {peak}");
    Ok(())
}

/// The producer lives in crawl so the bench crate acquires no layering edge.
/// Its output separates session setup, synthetic observation replay, production
/// diff and durable publication. All timings come from that one child process.
fn recrawl_once(dir: &Path, rows: &str, producer: &Path) -> Result<()> {
    let status = std::process::Command::new(producer)
        .arg(dir)
        .arg(rows)
        .status()?;
    if !status.success() {
        return Err(format!("recrawl producer exited with {status}").into());
    }
    Ok(())
}
