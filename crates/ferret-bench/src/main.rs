//! `ferret-bench`: measurements that no product crate should carry.
//!
//! ```text
//! ferret-bench scan  <catalog-dir> [needle...]   scanner arms against memmem
//! ferret-bench open  <catalog-dir>               open and load, by section set
//! ferret-bench sections <catalog-dir>            bytes per name, per section
//! ferret-bench query <catalog-dir> [query...]    the D40 query mix
//! ferret-bench overlay-fill <catalog-dir> <rows>   mixed name/inode overrides
//! ferret-bench resident-once <catalog-dir> <query> resident query time and RSS
//! ferret-bench overlay-carry <catalog-dir> <count> one-inode geometric carries
//! ferret-bench overlay-carry-boundary <catalog-dir> <rows> a large carry
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
            ("overlay-fill", [dir, rows]) => overlay_fill(Path::new(dir), rows),
            ("resident-once", [dir, text]) => resident_once(Path::new(dir), text),
            ("overlay-rename-once", [dir]) => overlay_rename_once(Path::new(dir)),
            ("overlay-carry", [dir, count]) => overlay_carry(Path::new(dir), count),
            ("overlay-carry-boundary", [dir, rows]) => overlay_carry_boundary(Path::new(dir), rows),
            ("recrawl-once", [dir, rows, producer]) => {
                recrawl_once(Path::new(dir), rows, Path::new(producer))
            }
            ("compact-once", [dir]) => compact_once(Path::new(dir)),
            ("churn-rewalk", [dir, percent, rounds]) => {
                churn_rewalk(Path::new(dir), percent, rounds)
            }
            ("churn-checkpoint", [dir, percent, rounds]) => {
                churn_checkpoint(Path::new(dir), percent, rounds)
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
         ferret-bench recrawl-once <catalog-dir> <changed-files> <crawl-producer>\n       \
         ferret-bench open <catalog-dir>\n       \
         ferret-bench open-once <catalog-dir> names|metadata|full|legacy-full\n       \
         ferret-bench log-fill <catalog-dir> <transactions> <records>\n       \
         ferret-bench log-append-once <catalog-dir> <records>\n       \
         ferret-bench log-open-once <catalog-dir>\n       \
         ferret-bench overlay-fill <catalog-dir> <rows>\n       \
         ferret-bench resident-once <catalog-dir> <query>\n       \
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

/// D51 A: the whole idle-boundary pause includes packed rewrite, durability,
/// readback validation, retirement and rebuilding resident epoch caches.
fn compact_once(dir: &Path) -> Result<()> {
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
fn churn_checkpoint(dir: &Path, percent: &str, rounds: &str) -> Result<()> {
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
fn churn_rewalk(dir: &Path, percent: &str, rounds: &str) -> Result<()> {
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
