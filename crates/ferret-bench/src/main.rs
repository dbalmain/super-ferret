//! `ferret-bench`: measurements that no product crate should carry.
//!
//! ```text
//! ferret-bench scan  <catalog-dir> [needle...]   scanner arms against memmem
//! ferret-bench open  <catalog-dir>               open and load, by section set
//! ferret-bench sections <catalog-dir>            bytes per name, per section
//! ferret-bench query <catalog-dir> [query...]    the D40 query mix
//! ```
//!
//! Build it in release (`cargo build --release -p ferret-bench`). Every
//! command prints a Markdown table on stdout. Run one at a time on the
//! machine: the numbers are wall-clock.
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
            ("scan", [dir, needles @ ..]) => scan(Path::new(dir), needles),
            ("open", [dir]) => open(Path::new(dir)),
            ("open-once", [dir, set]) => open_once(Path::new(dir), set),
            ("checksum", [dir]) => checksum(Path::new(dir)),
            ("log-fill", [dir, transactions, rows]) => log_fill(Path::new(dir), transactions, rows),
            ("log-append-once", [dir, rows]) => log_append_once(Path::new(dir), rows),
            ("log-open-once", [dir]) => log_open_once(Path::new(dir)),
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
         ferret-bench open <catalog-dir>\n       \
         ferret-bench open-once <catalog-dir> names|metadata|full|legacy-full\n       \
         ferret-bench log-fill <catalog-dir> <transactions> <records>\n       \
         ferret-bench log-append-once <catalog-dir> <records>\n       \
         ferret-bench log-open-once <catalog-dir>\n       \
         ferret-bench checksum <catalog-dir>\n       \
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
