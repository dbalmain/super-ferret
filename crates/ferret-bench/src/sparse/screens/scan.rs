//! The `pure-scan` command: a pure scan, measured against `rg` (docs/SCAN.md).
//!
//! Every live readable document is verified through the product's path,
//! [`Query::run_content_parallel`], with no content index: every document
//! is uncovered, so every one is a Maybe candidate and is read and matched.
//! The bound is lifted as `ferret search --scan` lifts it. The reader is
//! the bench's, as in the `product` arm: one `open` and a read of the
//! frozen snapshot file. `e2e` runs the real binary instead, with its
//! crawl reader, on a catalog rooted at the corpus.
//!
//! The seeds and their matches come from a screens oracle cache, so no
//! index is needed: 250 regexes, 50 per class, in [`CLASSES`] order.
//!
//! Arms, in the order given:
//!
//! - `rg`: `rg -l` at its default threads (12: cores capped at 12), cold and
//!   warm, then warm at `-j32`; its file set must equal the oracle's.
//! - `rg-lines`: rg's default output (every matching line) into /dev/null, cold
//!   and warm, then warm into a drained pipe.
//! - `ours`: cold at the cold thread counts, warm at every count; matches must
//!   equal the oracle's, and every readable document must be read.
//! - `e2e`: `ferret search --scan` as a process on the first
//!   `SCAN_E2E_PER_CLASS` (default 5) queries of each class, cold and warm,
//!   with `rg -l` beside it; both must list the oracle's files.
//!
//! Cold evicts every readable corpus file (and for `e2e`, the catalog)
//! with `posix_fadvise(DONTNEED)`; warm is an immediate repeat.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Read as _;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use ferret_catalog::{NameId, Target};
use ferret_index::{DocSet, Pinned};
use ferret_query::{Content, DocNames, NameIndex, Query, ReadRequest, ReadVersion, Workers};

use super::super::latency::ns;
use super::super::stats;
use super::CLASSES;
use super::rg::{self, Sink, counters};
use crate::content_snapshot::Snapshot;

const PER_CLASS: usize = 50;

/// Warm runs at each count, cold at every core and at rg's default of 12;
/// replaced, comma-separated, by `SCAN_THREADS` / `SCAN_COLD`. 0 means every
/// core (the product's default is every core up to 16, D77).
const THREADS: [usize; 5] = [1, 8, 12, 16, 32];
const COLD: [usize; 2] = [0, 12];

/// The brief asks for an 8 GB floor; check_disk's own is 5.
const DISK_FLOOR_EXTRA: u64 = 3_000_000_000;

struct Seed {
    class: &'static str,
    /// As the population wrote it; `(?i)` is prefixed for case-insensitive.
    pattern: String,
    oracle: Vec<u32>,
}

struct Population {
    catalog: ferret_catalog::Catalog,
    snapshot: Snapshot,
    live: DocSet,
    readable: Vec<u32>,
    bytes: u64,
    /// Each readable document's size, by DocId, for a streamed read.
    sizes: Vec<u64>,
    seeds: Vec<Seed>,
    case_insensitive: bool,
}

/// Undoes `str::escape_default`, which the oracle cache wrote patterns in.
fn unescape(text: &str) -> crate::Result<String> {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next().ok_or("trailing backslash")? {
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'n' => out.push('\n'),
            'u' => {
                let rest: String = chars.by_ref().take_while(|&c| c != '}').collect();
                let hex = rest.strip_prefix('{').ok_or("bad \\u escape")?;
                out.push(char::from_u32(u32::from_str_radix(hex, 16)?).ok_or("bad char")?);
            }
            other => out.push(other),
        }
    }
    if out.escape_default().to_string() != text {
        return Err("pattern does not round-trip".into());
    }
    Ok(out)
}

fn load(
    catalog_dir: &Path,
    corpus: &Path,
    cache: &Path,
    case_insensitive: bool,
) -> crate::Result<Population> {
    let catalog = crate::open_catalog(catalog_dir)?;
    catalog.load_all()?;
    let catalog = catalog.into_resident()?;
    let snapshot = Snapshot::open(corpus, &catalog)?;
    let live = DocSet::new(catalog.next_doc().0, catalog.docs().map(|(doc, _)| doc.0));
    let readable: Vec<u32> = live
        .range(0, live.bound())
        .filter(|&d| snapshot.contains(d))
        .collect();
    let mut bytes = 0;
    let mut sizes = vec![0; catalog.next_doc().0 as usize];
    for &doc in &readable {
        let size = fs::metadata(snapshot.path(doc))?.len();
        sizes[doc as usize] = size;
        bytes += size;
    }
    let text = fs::read_to_string(cache)?;
    let mut lines = text.lines();
    if lines.next() != Some(&format!("case_insensitive={case_insensitive}")) {
        return Err("oracle cache flags differ".into());
    }
    let mut seeds = Vec::new();
    for (i, line) in lines.enumerate() {
        let (pattern, docs) = line.rsplit_once('\t').ok_or("oracle cache line")?;
        let oracle: Vec<u32> = docs
            .split(',')
            .filter(|d| !d.is_empty())
            .map(str::parse)
            .collect::<Result<_, _>>()?;
        if oracle.iter().any(|&d| !snapshot.contains(d)) {
            return Err("oracle names a document the corpus lacks".into());
        }
        seeds.push(Seed {
            class: CLASSES.get(i / PER_CLASS).ok_or("oracle cache too long")?,
            pattern: unescape(pattern)?,
            oracle,
        });
    }
    if seeds.len() != CLASSES.len() * PER_CLASS {
        return Err("oracle cache too short".into());
    }
    // `SCAN_PER_CLASS` keeps the first n of each class, for a smoke run.
    if let Ok(n) = std::env::var("SCAN_PER_CLASS") {
        let n: usize = n.parse()?;
        let mut i = 0;
        seeds.retain(|_| {
            i += 1;
            (i - 1) % PER_CLASS < n
        });
    }
    println!(
        "population case_insensitive={case_insensitive} live={} readable={} bytes={bytes} queries={}",
        live.len(),
        readable.len(),
        seeds.len()
    );
    Ok(Population {
        catalog,
        snapshot,
        live,
        readable,
        bytes,
        sizes,
        seeds,
        case_insensitive,
    })
}

fn evict(paths: impl IntoIterator<Item = PathBuf>) -> crate::Result<()> {
    for path in paths {
        let file = File::open(path)?;
        rustix::fs::fadvise(&file, 0, None, rustix::fs::Advice::DontNeed)?;
    }
    Ok(())
}

fn evict_corpus(pop: &Population) -> crate::Result<()> {
    evict(pop.readable.iter().map(|&d| pop.snapshot.path(d)))
}

fn evict_tree(dir: &Path) -> crate::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            evict_tree(&path)?;
        } else if path.is_file() {
            evict([path])?;
        }
    }
    Ok(())
}

fn check_disk() -> crate::Result<()> {
    crate::trigram::check_disk_for_bytes(DISK_FLOOR_EXTRA)
}

/// One run of the product's scan.
#[derive(Clone, Copy, Default)]
struct Run {
    wall: Duration,
    /// Planning and candidate rows, until the first document read.
    before_reads: Duration,
    /// Summed busy time of the verifying threads (reader plus matcher).
    busy: Duration,
    /// Summed time inside the reader's `open` and read.
    read: Duration,
    /// Summed time in the regex engine.
    regex: Duration,
    /// Process CPU, every thread.
    cpu: f64,
    /// The calling thread's CPU: rows, the window, emitting.
    caller: Duration,
    device: u64,
}

/// What a reader saw, summed over every reader of a run.
#[derive(Default)]
struct Reads {
    docs: u64,
    bytes: u64,
    time: Duration,
}

/// A reader's own count, added to the run's when the reader is dropped,
/// so the readers share nothing per document.
struct Tally<'a> {
    reads: Reads,
    into: &'a Mutex<Reads>,
}

impl Tally<'_> {
    /// A method, so the reader's closure captures the whole tally (and its
    /// `Drop`) rather than copies of its fields.
    fn add(&mut self, bytes: u64, time: Duration) {
        self.reads.docs += 1;
        self.reads.bytes += bytes;
        self.reads.time += time;
    }
}

/// A streamed document, timing its reads.
struct Timed {
    file: std::io::Take<File>,
    time: Duration,
}

impl std::io::Read for Timed {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let start = Instant::now();
        let read = self.file.read(buf);
        self.time += start.elapsed();
        read
    }
}

impl Drop for Tally<'_> {
    fn drop(&mut self) {
        let mut total = self.into.lock().unwrap_or_else(PoisonError::into_inner);
        total.docs += self.reads.docs;
        total.bytes += self.reads.bytes;
        total.time += self.reads.time;
    }
}

struct Session<'p> {
    pop: &'p Population,
    names: NameIndex,
    docs: DocNames,
}

impl Session<'_> {
    fn query(&self, seed: &Seed) -> crate::Result<Query> {
        let inline = if self.pop.case_insensitive {
            "(?i)"
        } else {
            ""
        };
        Ok(Query::from_args(
            &[format!("grep:{inline}{}", seed.pattern)],
            SystemTime::now(),
        )?)
    }

    fn once(&self, query: &Query, seed: &Seed, threads: usize) -> crate::Result<Run> {
        let pop = self.pop;
        let catalog = &pop.catalog;
        let pinned = Pinned::new(None, &pop.live);
        let content = Content {
            pinned: &pinned,
            docs: &self.docs,
            bound: None,
        };
        let first = OnceLock::new();
        let reads = Mutex::new(Reads::default());
        let (first, reads) = (&first, &reads);
        let fork = || -> Box<ferret_query::Reader<'_>> {
            let mut tally = Tally {
                reads: Reads::default(),
                into: reads,
            };
            Box::new(move |name: NameId, request: ReadRequest<'_>| {
                let doc = match catalog.name(name).target() {
                    Target::Inode(inode) => catalog.doc(inode)?.0,
                    _ => return None,
                };
                let path = pop.snapshot.path(doc);
                match request {
                    ReadRequest::Stat => fs::metadata(path).ok().map(|_| ReadVersion::Catalogued),
                    ReadRequest::Bytes(out, _) => {
                        let start = Instant::now();
                        first.get_or_init(|| start);
                        out.clear();
                        File::open(path).ok()?.read_to_end(out).ok()?;
                        tally.add(out.len() as u64, start.elapsed());
                        Some(ReadVersion::Catalogued)
                    }
                    // Streamed as the product streams: the consumer reads
                    // what it needs into its own buffer. "read ms" is the
                    // open and the reads, not the matching between them;
                    // the bytes counted are the size offered, since the
                    // consumer stops once every atom has matched.
                    ReadRequest::Stream(each, _) => {
                        let start = Instant::now();
                        first.get_or_init(|| start);
                        let size = pop.sizes[doc as usize];
                        let file = File::open(path).ok()?;
                        let mut timed = Timed {
                            file: file.take(size),
                            time: start.elapsed(),
                        };
                        each(&mut timed, size).ok()?;
                        tally.add(size, timed.time);
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
            &self.names,
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
        if matched != seed.oracle {
            return Err(format!(
                "scan matches differ from the oracle ({} at {threads} threads: {} against {})",
                seed.class,
                matched.len(),
                seed.oracle.len()
            )
            .into());
        }
        let reads = std::mem::take(&mut *reads.lock().unwrap_or_else(PoisonError::into_inner));
        if reads.docs != pop.readable.len() as u64 || reads.bytes != pop.bytes {
            return Err(format!(
                "scan read {} documents, {} bytes; the corpus holds {} and {}",
                reads.docs,
                reads.bytes,
                pop.readable.len(),
                pop.bytes
            )
            .into());
        }
        Ok(Run {
            wall,
            before_reads: first.get().map_or(wall, |&at| at - start),
            busy: report.verification_time,
            read: reads.time,
            regex: report.regex_time,
            cpu: after.self_cpu - before.self_cpu,
            caller,
            device: after.device - before.device,
        })
    }
}

/// This thread's on-CPU time.
fn caller_cpu() -> crate::Result<Duration> {
    let stat = fs::read_to_string("/proc/thread-self/schedstat")?;
    let ns = stat
        .split_whitespace()
        .next()
        .ok_or("empty schedstat")?
        .parse()?;
    Ok(Duration::from_nanos(ns))
}

fn counts(name: &str, default: &[usize]) -> crate::Result<Vec<usize>> {
    let all = std::thread::available_parallelism().map_or(1, |n| n.get());
    let list: Vec<usize> = match std::env::var(name) {
        Err(_) => default.to_vec(),
        Ok(list) => list
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::parse)
            .collect::<Result<_, _>>()?,
    };
    let mut out: Vec<usize> = Vec::new();
    for threads in list.into_iter().map(|t| if t == 0 { all } else { t }) {
        if !out.contains(&threads) {
            out.push(threads);
        }
    }
    Ok(out)
}

/// One timed thing for the tables: a label, the thread count, and runs.
struct Cell {
    arm: String,
    cache: &'static str,
    threads: usize,
    class: &'static str,
    wall: Duration,
    cpu: f64,
    device: u64,
    /// Arm-specific extras, in nanoseconds or bytes.
    extra: Vec<(&'static str, u64)>,
}

fn rg_cell(arm: &str, cache: &'static str, class: &'static str, scan: &rg::Scan) -> Cell {
    Cell {
        arm: arm.to_owned(),
        cache,
        threads: 0,
        class,
        wall: scan.wall,
        cpu: scan.cpu,
        device: scan.device,
        extra: Vec::new(),
    }
}

fn printed_cell(arm: &str, cache: &'static str, class: &'static str, run: &rg::Printed) -> Cell {
    Cell {
        arm: arm.to_owned(),
        cache,
        threads: 0,
        class,
        wall: run.wall,
        cpu: run.cpu,
        device: run.device,
        extra: vec![("printed B", run.printed)],
    }
}

fn ours_cell(cache: &'static str, threads: usize, class: &'static str, run: &Run) -> Cell {
    Cell {
        arm: "ours".to_owned(),
        cache,
        threads,
        class,
        wall: run.wall,
        cpu: run.cpu,
        device: run.device,
        extra: vec![
            ("before reads ms", ns(run.before_reads)),
            ("busy ms", ns(run.busy)),
            ("read ms", ns(run.read)),
            ("regex ms", ns(run.regex)),
            ("caller cpu ms", ns(run.caller)),
        ],
    }
}

/// `ferret search --scan` on the corpus catalog, as a process.
struct Product {
    binary: PathBuf,
    index: PathBuf,
    xdg: PathBuf,
}

impl Product {
    fn from_env() -> crate::Result<Self> {
        let var = |name: &str| -> crate::Result<PathBuf> {
            Ok(std::env::var_os(name)
                .ok_or_else(|| format!("e2e needs {name}"))?
                .into())
        };
        Ok(Self {
            binary: var("SCAN_FERRET")?,
            index: var("SCAN_FERRET_INDEX")?,
            xdg: var("SCAN_XDG")?,
        })
    }

    /// Runs one search, returning the listed corpus documents.
    fn search(&self, pop: &Population, seed: &Seed) -> crate::Result<(Vec<u32>, Cell)> {
        let inline = if pop.case_insensitive { "(?i)" } else { "" };
        let mut command = Command::new(&self.binary);
        command
            .arg("--index")
            .arg(&self.index)
            .args(["search", "--scan", "--"])
            .arg(format!("grep:{inline}{}", seed.pattern))
            .stdin(Stdio::null())
            .stderr(Stdio::inherit());
        for (var, dir) in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_RUNTIME_DIR", "runtime"),
        ] {
            command.env(var, self.xdg.join(dir));
        }
        // Without it, search starts ferretd, which follows the catalog and
        // builds the content index this scan must not have.
        command.env("FERRET_NO_DAEMON", "1");
        command.env_remove("FERRET_SEARCH_THREADS");
        command.env_remove("FERRET_INDEX");
        let before = counters()?;
        let start = Instant::now();
        let output = command.output()?;
        let wall = start.elapsed();
        let after = counters()?;
        // 0 printed a row, 1 matched nothing.
        if !matches!(output.status.code(), Some(0 | 1)) {
            return Err(format!("ferret search failed: {}", output.status).into());
        }
        let docs = listed(&output.stdout)?;
        Ok((
            docs,
            Cell {
                arm: "e2e ferret".to_owned(),
                cache: "",
                threads: 0,
                class: seed.class,
                wall,
                cpu: after.children_cpu - before.children_cpu,
                device: after.device - before.device,
                extra: vec![("stdout B", output.stdout.len() as u64)],
            },
        ))
    }
}

/// Document ids from a list of corpus paths, sorted.
fn listed(stdout: &[u8]) -> crate::Result<Vec<u32>> {
    let mut docs = Vec::new();
    for line in std::str::from_utf8(stdout)?.lines() {
        let name = Path::new(line)
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("an unnamed path")?;
        docs.push(name.parse::<u32>()?);
    }
    docs.sort_unstable();
    Ok(docs)
}

const ARMS: [&str; 4] = ["rg", "rg-lines", "ours", "e2e"];

pub(crate) fn run(args: &[String]) -> crate::Result<()> {
    let [catalog, corpus, cache, case, scratch, arms] = args else {
        return Err(
            "pure-scan <catalog> <corpus> <oracle-cache> <ci|cs> <scratch> <rg,rg-lines,ours,e2e>"
                .into(),
        );
    };
    let arms: Vec<&str> = arms.split(',').collect();
    if let Some(bad) = arms.iter().find(|a| !ARMS.contains(a)) {
        return Err(format!("unknown scan arm {bad}").into());
    }
    let case_insensitive = match case.as_str() {
        "ci" => true,
        "cs" => false,
        _ => return Err("case must be ci or cs".into()),
    };
    let (corpus, scratch) = (Path::new(corpus), Path::new(scratch));
    fs::create_dir_all(scratch)?;
    check_disk()?;
    let pop = load(
        Path::new(catalog),
        corpus,
        Path::new(cache),
        case_insensitive,
    )?;
    let warm_threads = counts("SCAN_THREADS", &THREADS)?;
    let cold_threads = counts("SCAN_COLD", &COLD)?;
    let mut cells = Vec::new();
    let mut private = String::new();
    for arm in &arms {
        match *arm {
            "rg" => rg_arm(&pop, corpus, &mut cells, &mut private)?,
            "rg-lines" => rg_lines_arm(&pop, corpus, &mut cells, &mut private)?,
            "ours" => ours_arm(&pop, &warm_threads, &cold_threads, &mut cells, &mut private)?,
            "e2e" => e2e_arm(&pop, corpus, &mut cells, &mut private)?,
            _ => unreachable!("arms are checked"),
        }
    }
    report(&cells);
    let path = scratch.join(format!(
        "scan-rows-{case}-{}-{}.tsv",
        arms.join("+"),
        std::process::id()
    ));
    fs::write(&path, private)?;
    println!("private_rows written");
    Ok(())
}

fn rg_arm(
    pop: &Population,
    corpus: &Path,
    cells: &mut Vec<Cell>,
    private: &mut String,
) -> crate::Result<()> {
    for (q, seed) in pop.seeds.iter().enumerate() {
        check_disk()?;
        evict_corpus(pop)?;
        let cold = rg::scan(&seed.pattern, pop.case_insensitive, corpus, None)?;
        let warm = rg::scan(&seed.pattern, pop.case_insensitive, corpus, None)?;
        let j32 = rg::scan(&seed.pattern, pop.case_insensitive, corpus, Some(32))?;
        if cold.docs != seed.oracle || warm.docs != seed.oracle || j32.docs != seed.oracle {
            return Err(format!("rg -l file set differs from the oracle: query {q}").into());
        }
        let _ = writeln!(
            private,
            "rg\t{q}\t{}\t{}\t{}\t{}",
            seed.class,
            ns(cold.wall),
            ns(warm.wall),
            ns(j32.wall)
        );
        println!(
            "progress arm=rg query={} class={} cold_ms={:.1} warm_ms={:.1} warm_j32_ms={:.1}",
            q + 1,
            seed.class,
            cold.wall.as_secs_f64() * 1e3,
            warm.wall.as_secs_f64() * 1e3,
            j32.wall.as_secs_f64() * 1e3
        );
        cells.push(rg_cell("rg -l", "cold", seed.class, &cold));
        cells.push(rg_cell("rg -l", "warm", seed.class, &warm));
        let mut j32 = rg_cell("rg -l", "warm", seed.class, &j32);
        j32.threads = 32;
        cells.push(j32);
    }
    println!(
        "correctness arm=rg queries={} file_set_equals_oracle=all",
        pop.seeds.len()
    );
    Ok(())
}

fn rg_lines_arm(
    pop: &Population,
    corpus: &Path,
    cells: &mut Vec<Cell>,
    private: &mut String,
) -> crate::Result<()> {
    for (q, seed) in pop.seeds.iter().enumerate() {
        check_disk()?;
        evict_corpus(pop)?;
        let ci = pop.case_insensitive;
        let cold = rg::lines(&seed.pattern, ci, corpus, None, Sink::DevNull)?;
        let warm = rg::lines(&seed.pattern, ci, corpus, None, Sink::DevNull)?;
        let pipe = rg::lines(&seed.pattern, ci, corpus, None, Sink::Pipe)?;
        let _ = writeln!(
            private,
            "rg-lines\t{q}\t{}\t{}\t{}\t{}\t{}",
            seed.class,
            ns(cold.wall),
            ns(warm.wall),
            ns(pipe.wall),
            pipe.printed
        );
        println!(
            "progress arm=rg-lines query={} class={} cold_ms={:.1} warm_ms={:.1} pipe_ms={:.1}",
            q + 1,
            seed.class,
            cold.wall.as_secs_f64() * 1e3,
            warm.wall.as_secs_f64() * 1e3,
            pipe.wall.as_secs_f64() * 1e3
        );
        cells.push(printed_cell(
            "rg lines >/dev/null",
            "cold",
            seed.class,
            &cold,
        ));
        cells.push(printed_cell(
            "rg lines >/dev/null",
            "warm",
            seed.class,
            &warm,
        ));
        cells.push(printed_cell("rg lines | pipe", "warm", seed.class, &pipe));
    }
    Ok(())
}

fn ours_arm(
    pop: &Population,
    warm_threads: &[usize],
    cold_threads: &[usize],
    cells: &mut Vec<Cell>,
    private: &mut String,
) -> crate::Result<()> {
    let session = Session {
        pop,
        names: NameIndex::new(&pop.catalog),
        docs: DocNames::new(&pop.catalog)?,
    };
    for (q, seed) in pop.seeds.iter().enumerate() {
        check_disk()?;
        let query = session.query(seed)?;
        let mut line = Vec::new();
        for &threads in cold_threads {
            evict_corpus(pop)?;
            let run = session.once(&query, seed, threads)?;
            line.push(format!("cold{threads}={:.1}", run.wall.as_secs_f64() * 1e3));
            cells.push(ours_cell("cold", threads, seed.class, &run));
        }
        for &threads in warm_threads {
            let run = session.once(&query, seed, threads)?;
            line.push(format!("warm{threads}={:.1}", run.wall.as_secs_f64() * 1e3));
            cells.push(ours_cell("warm", threads, seed.class, &run));
        }
        let _ = writeln!(private, "ours\t{q}\t{}\t{}", seed.class, line.join(","));
        println!(
            "progress arm=ours query={} class={} {}",
            q + 1,
            seed.class,
            line.join(" ")
        );
    }
    println!(
        "correctness arm=ours queries={} matches_equal_oracle=all read_every_document=all",
        pop.seeds.len()
    );
    Ok(())
}

fn e2e_arm(
    pop: &Population,
    corpus: &Path,
    cells: &mut Vec<Cell>,
    private: &mut String,
) -> crate::Result<()> {
    let product = Product::from_env()?;
    let per_class: usize = std::env::var("SCAN_E2E_PER_CLASS")
        .ok()
        .map(|n| n.parse())
        .transpose()?
        .unwrap_or(5);
    for (q, seed) in pop.seeds.iter().enumerate() {
        if pop.seeds[..q]
            .iter()
            .filter(|s| s.class == seed.class)
            .count()
            >= per_class
        {
            continue;
        }
        check_disk()?;
        let mut line = Vec::new();
        for cache in ["cold", "warm"] {
            if cache == "cold" {
                evict_corpus(pop)?;
                evict_tree(&product.index)?;
            }
            let (docs, mut cell) = product.search(pop, seed)?;
            if docs != seed.oracle {
                return Err(format!(
                    "ferret search --scan differs from the oracle: query {q} ({} against {})",
                    docs.len(),
                    seed.oracle.len()
                )
                .into());
            }
            cell.cache = cache;
            line.push(format!(
                "ferret_{cache}={:.1}",
                cell.wall.as_secs_f64() * 1e3
            ));
            cells.push(cell);
            if cache == "cold" {
                evict_corpus(pop)?;
            }
            let scan = rg::scan(&seed.pattern, pop.case_insensitive, corpus, None)?;
            if scan.docs != seed.oracle {
                return Err(format!("rg -l file set differs from the oracle: query {q}").into());
            }
            line.push(format!("rg_{cache}={:.1}", scan.wall.as_secs_f64() * 1e3));
            cells.push(rg_cell("e2e rg -l", cache, seed.class, &scan));
        }
        let _ = writeln!(private, "e2e\t{q}\t{}\t{}", seed.class, line.join(","));
        println!(
            "progress arm=e2e query={} class={} {}",
            q + 1,
            seed.class,
            line.join(" ")
        );
    }
    println!("correctness arm=e2e file_sets_equal_oracle=all");
    Ok(())
}

fn ms(values: Vec<u64>) -> String {
    let (median, p90, _) = stats(values);
    format!("{:.1}/{:.1}", median as f64 / 1e6, p90 as f64 / 1e6)
}

/// Median/p90 per arm, class, cache and thread count, in first-seen order.
fn report(cells: &[Cell]) {
    let mut keys: Vec<(&str, &str, usize)> = Vec::new();
    for cell in cells {
        let key = (cell.arm.as_str(), cell.cache, cell.threads);
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    println!(
        "| arm | cache | threads | class | queries | wall ms median/p90 | cpu ms median/p90 | device MB median | extras median/p90 |"
    );
    for (arm, cache, threads) in keys {
        for class in CLASSES {
            let picked: Vec<&Cell> = cells
                .iter()
                .filter(|c| {
                    c.arm == arm && c.cache == cache && c.threads == threads && c.class == class
                })
                .collect();
            if picked.is_empty() {
                continue;
            }
            let extras: Vec<String> = picked[0]
                .extra
                .iter()
                .enumerate()
                .map(|(i, &(name, _))| {
                    let values: Vec<u64> = picked.iter().map(|c| c.extra[i].1).collect();
                    if name.ends_with(" ms") {
                        format!("{name} {}", ms(values))
                    } else {
                        let (m, p, _) = stats(values);
                        format!("{name} {m}/{p}")
                    }
                })
                .collect();
            println!(
                "| {arm} | {cache} | {} | {class} | {} | {} | {} | {:.0} | {} |",
                if threads == 0 {
                    "default".to_owned()
                } else {
                    threads.to_string()
                },
                picked.len(),
                ms(picked.iter().map(|c| ns(c.wall)).collect()),
                ms(picked.iter().map(|c| (c.cpu * 1e9) as u64).collect()),
                stats(picked.iter().map(|c| c.device).collect()).0 as f64 / 1e6,
                extras.join("; "),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::unescape;

    #[test]
    fn cache_patterns_unescape_to_what_was_escaped() {
        for pattern in [r"defi\w+acro", "(a|b)", r"[A-Z_]+x", "tab\there", "é\u{1}"] {
            let escaped = pattern.escape_default().to_string();
            assert_eq!(unescape(&escaped).unwrap(), pattern);
        }
        assert!(unescape(r"\").is_err());
        // Not what escape_default writes for this text.
        assert!(unescape(r"\q").is_err());
    }
}
