//! The D28 and D30 measurements over a walker dump.
//!
//! ```text
//! cargo run --release -p ferret-catalog --example names -- build <dump.tsv> <dir>
//! cargo run --release -p ferret-catalog --example names -- layout <dir> [runs]
//! cargo run --release -p ferret-catalog --example names -- open-bench <dir> <runs> [evict]
//! ```
//!
//! `build` reads a dump of `path TAB decision` lines, as `ferret-crawl`'s
//! `dump` example writes them, and commits it through a real [`Transaction`]:
//! `Descend` lines become directories, `Index` lines files with a distinct
//! synthetic hash each, `Catalog(Symlink)` symlinks and any other `Catalog(..)`
//! unindexed files. `Skip` lines are left out. Stats are synthetic; the names
//! are real.
//!
//! `layout` compares the name heap as stored (D28 A: raw, NUL-terminated,
//! sorted by parent then name) with the same names front-coded per directory
//! (D28 B: a LEB128 shared-prefix length against the previous sibling, then
//! the suffix and its NUL), and times a substring scan over each. The raw scan
//! runs over the whole heap at once; the front-coded scan must rebuild each
//! name before searching it. Both use the same hand-rolled search, a
//! first-byte `position` then a compare of the rest, and both count matching
//! names, not matches.
//!
//! `open-bench` times fresh processes opening the catalog: it re-runs this
//! binary as `open <dir>`, which reads the file and decodes it and prints both
//! times. With `evict` it drops the catalog file's pages first with
//! `dd iflag=nocache count=0` — per-file eviction, not a full `drop_caches`:
//! the binary, libc and the directory entries stay cached. With `names` it
//! instead times reading only the file's prefix through the name sections
//! (names, name heap, directory names, traversed, roots, strings), evicted
//! and warm: what a reader that loads sections on demand would pay for a
//! name-only query. That probe parses the section table itself, so it tracks
//! format version 1 only.

use std::collections::HashMap;
use std::error::Error;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use ferret_catalog::{Catalog, Content, DirToken, Hash, Stat, Transaction};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const SNIFFER: u32 = 1;

/// The needles `layout` times: rare, common and one byte.
const NEEDLES: [&[u8]; 3] = [b"ferret", b".js", b"q"];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let outcome = match words.as_slice() {
        ["build", dump, dir] => build(Path::new(dump), Path::new(dir)),
        ["layout", dir] => layout(Path::new(dir), 30),
        ["layout", dir, runs] => runs
            .parse()
            .map_err(Into::into)
            .and_then(|runs| layout(Path::new(dir), runs)),
        ["open", dir] => open(Path::new(dir)),
        ["open-names", dir] => open_names(Path::new(dir)),
        ["open-bench", dir, runs] => runs
            .parse()
            .map_err(Into::into)
            .and_then(|runs| open_bench(Path::new(dir), runs, false)),
        ["open-bench", dir, runs, "evict"] => runs
            .parse()
            .map_err(Into::into)
            .and_then(|runs| open_bench(Path::new(dir), runs, true)),
        ["open-bench", dir, runs, "names"] => runs
            .parse()
            .map_err(Into::into)
            .and_then(|runs| names_bench(Path::new(dir), runs)),
        _ => {
            eprintln!("usage: names build <dump.tsv> <dir> | layout <dir> [runs]");
            eprintln!("       | open-bench <dir> <runs> [evict | names]");
            return ExitCode::from(2);
        }
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("names: {error}");
            ExitCode::from(1)
        }
    }
}

fn stat(ino: u64, mode: u32) -> Stat {
    Stat {
        dev: 1,
        ino,
        size: 0,
        mtime_sec: 1_700_000_000,
        mtime_nsec: 0,
        ctime_sec: 1_700_000_000,
        ctime_nsec: 0,
        mode,
        uid: 1000,
        gid: 100,
    }
}

/// A distinct content hash per indexed file.
fn synthetic_hash(n: u64) -> Hash {
    let mut hash = [0; 16];
    hash[..8].copy_from_slice(&n.to_le_bytes());
    hash
}

/// Splits `a/b/c` into `(Some("a/b"), "c")` and `c` into `(None, "c")`.
fn split(path: &[u8]) -> (Option<&[u8]>, &[u8]) {
    match path.iter().rposition(|&b| b == b'/') {
        Some(slash) => (Some(&path[..slash]), &path[slash + 1..]),
        None => (None, path),
    }
}

fn build(dump: &Path, dir: &Path) -> Result<()> {
    let text = std::fs::read(dump)?;
    let mut dirs = Vec::new();
    let mut entries = Vec::new();
    for line in text.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        let tab = line
            .iter()
            .position(|&b| b == b'\t')
            .ok_or("a line without a tab")?;
        let (path, decision) = (&line[..tab], &line[tab + 1..]);
        match decision {
            b"Descend" => dirs.push(path),
            b"Skip" => {}
            _ => entries.push((path, decision)),
        }
    }
    // A parent's path is a prefix of its child's, so it sorts first.
    dirs.sort_unstable();

    let started = Instant::now();
    let mut txn = Transaction::begin(dir, SNIFFER)?;
    let mut batch = txn.batch();
    let mut ino = 1;
    let root = batch.root(b"/home/user", stat(ino, 0o040_755));
    let mut tokens: HashMap<&[u8], DirToken> = HashMap::new();
    let parent_of = |tokens: &HashMap<&[u8], DirToken>, parent: Option<&[u8]>| match parent {
        None => Ok(root),
        Some(parent) => tokens
            .get(parent)
            .copied()
            .ok_or_else(|| format!("no parent {}", String::from_utf8_lossy(parent))),
    };
    for path in &dirs {
        let (parent, name) = split(path);
        ino += 1;
        let token = batch.dir(parent_of(&tokens, parent)?, name, stat(ino, 0o040_755));
        tokens.insert(path, token);
    }
    let (mut indexed, mut unindexed, mut links) = (0, 0, 0);
    for (path, decision) in &entries {
        let (parent, name) = split(path);
        let parent = parent_of(&tokens, parent)?;
        ino += 1;
        match *decision {
            b"Index" => {
                indexed += 1;
                let content = Content::Hashed(synthetic_hash(ino));
                batch.file(parent, name, stat(ino, 0o100_644), content);
            }
            b"Catalog(Symlink)" => {
                links += 1;
                batch.symlink(parent, name, stat(ino, 0o120_777), b"target");
            }
            _ => {
                unindexed += 1;
                batch.file(parent, name, stat(ino, 0o100_644), Content::Unindexed);
            }
        }
    }
    let filled = started.elapsed();
    txn.add(batch);
    let catalog = txn.commit()?;
    let committed = started.elapsed();

    let size = std::fs::metadata(dir.join("catalog"))?.len();
    println!(
        "dirs {} (plus the root), indexed {indexed}, unindexed {unindexed}, symlinks {links}",
        dirs.len()
    );
    println!(
        "names {} inodes {} docs {}",
        catalog.name_count(),
        catalog.inode_count(),
        catalog.doc_count()
    );
    println!(
        "file {size} B; name heap {} B; batches filled in {} ms, committed at {} ms",
        catalog.name_heap().len(),
        filled.as_millis(),
        committed.as_millis()
    );
    Ok(())
}

fn push_leb128(out: &mut Vec<u8>, mut value: usize) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn read_leb128(bytes: &[u8], at: &mut usize) -> usize {
    let (mut value, mut shift) = (0, 0);
    loop {
        let byte = bytes[*at];
        *at += 1;
        value |= usize::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return value;
        }
        shift += 7;
    }
}

/// The catalog's names front-coded per directory: each name is its shared
/// prefix length with the previous sibling, then the rest and a NUL. The
/// first name in a directory shares nothing.
fn front_code(catalog: &Catalog) -> Vec<u8> {
    let mut out = Vec::with_capacity(catalog.name_heap().len());
    let mut previous: (Option<u32>, &[u8]) = (None, b"");
    for (id, name) in catalog.names() {
        let parent = catalog.name(id).parent.0;
        let shared = if previous.0 == Some(parent) {
            name.iter()
                .zip(previous.1)
                .take_while(|(a, b)| a == b)
                .count()
        } else {
            0
        };
        push_leb128(&mut out, shared);
        out.extend_from_slice(&name[shared..]);
        out.push(0);
        previous = (Some(parent), name);
    }
    out
}

/// The first occurrence of `needle` in `hay` at or after `from`: a
/// `position` on the first byte, then a compare of the rest.
fn find(hay: &[u8], needle: &[u8], mut from: usize) -> Option<usize> {
    let (&first, rest) = needle.split_first()?;
    while from + needle.len() <= hay.len() {
        let at = from
            + hay[from..=hay.len() - needle.len()]
                .iter()
                .position(|&b| b == first)?;
        if &hay[at + 1..at + needle.len()] == rest {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// Names containing `needle`, scanning the raw heap in one pass: a hit skips
/// to the end of its name, so each name counts once.
fn scan_raw(heap: &[u8], needle: &[u8]) -> usize {
    let (mut count, mut from) = (0, 0);
    while let Some(at) = find(heap, needle, from) {
        count += 1;
        from = match heap[at..].iter().position(|&b| b == 0) {
            Some(nul) => at + nul + 1,
            None => heap.len(),
        };
    }
    count
}

/// Names containing `needle`, rebuilding each front-coded name before
/// searching it.
fn scan_front_coded(coded: &[u8], needle: &[u8]) -> usize {
    let (mut count, mut at) = (0, 0);
    let mut name = Vec::with_capacity(256);
    while at < coded.len() {
        let shared = read_leb128(coded, &mut at);
        let len = coded[at..].iter().position(|&b| b == 0).unwrap_or(0);
        name.truncate(shared);
        name.extend_from_slice(&coded[at..at + len]);
        at += len + 1;
        count += usize::from(find(&name, needle, 0).is_some());
    }
    count
}

/// Median and minimum of `runs` timings of `f`, and its last result.
fn time(runs: usize, mut f: impl FnMut() -> usize) -> (Duration, Duration, usize) {
    let mut result = 0;
    let mut times: Vec<Duration> = (0..runs.max(1))
        .map(|_| {
            let started = Instant::now();
            result = std::hint::black_box(f());
            started.elapsed()
        })
        .collect();
    times.sort();
    (times[times.len() / 2], times[0], result)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn layout(dir: &Path, runs: usize) -> Result<()> {
    let catalog = Catalog::open(dir)?.ok_or("no catalog")?;
    let heap = catalog.name_heap();
    let coded = front_code(&catalog);
    let names = catalog.name_count() as usize;
    let size = std::fs::metadata(dir.join("catalog"))?.len() as usize;
    let saved = heap.len() - coded.len();
    println!("names {names}; snapshot {size} B");
    println!(
        "raw heap {} B ({:.2} B/name); front-coded {} B ({:.2} B/name)",
        heap.len(),
        heap.len() as f64 / names as f64,
        coded.len(),
        coded.len() as f64 / names as f64
    );
    println!(
        "front-coding saves {saved} B: {:.1}% of the heap, {:.1}% of the snapshot",
        100.0 * saved as f64 / heap.len() as f64,
        100.0 * saved as f64 / size as f64
    );
    println!("warm scans, {runs} runs each, median (min) ms:");
    for needle in NEEDLES {
        let (raw, raw_min, raw_hits) = time(runs, || scan_raw(heap, needle));
        let (fc, fc_min, fc_hits) = time(runs, || scan_front_coded(&coded, needle));
        if raw_hits != fc_hits {
            return Err(format!("layouts disagree: {raw_hits} vs {fc_hits}").into());
        }
        println!(
            "  {:>8} {raw_hits:>6} names: raw {:.2} ({:.2}), front-coded {:.2} ({:.2})",
            String::from_utf8_lossy(needle),
            ms(raw),
            ms(raw_min),
            ms(fc),
            ms(fc_min)
        );
    }
    Ok(())
}

/// The child side of `open-bench`: reads and decodes the catalog, then
/// prints `read_ns decode_ns`.
fn open(dir: &Path) -> Result<()> {
    let started = Instant::now();
    let bytes = std::fs::read(dir.join("catalog"))?;
    let read = started.elapsed();
    let catalog = Catalog::from_bytes(bytes)?;
    let decoded = started.elapsed() - read;
    std::hint::black_box(catalog.name_count());
    println!("{} {}", read.as_nanos(), decoded.as_nanos());
    Ok(())
}

/// The byte where format version 1's name sections end: the end of the
/// sixth entry (strings) in the section table after the 24-byte header.
const NAME_SECTIONS: usize = 6;

/// The child side of `open-bench .. names`: reads the header and table, then
/// the file up to the end of the name sections, and prints `read_ns 0`.
fn open_names(dir: &Path) -> Result<()> {
    use std::io::Read;
    let started = Instant::now();
    let mut file = std::fs::File::open(dir.join("catalog"))?;
    let mut bytes = vec![0; 24 + NAME_SECTIONS * 16];
    file.read_exact(&mut bytes)?;
    let entry = 24 + (NAME_SECTIONS - 1) * 16;
    let field = |at: usize| -> Result<usize> {
        let raw: [u8; 8] = bytes[at..at + 8].try_into()?;
        Ok(usize::try_from(u64::from_le_bytes(raw))?)
    };
    let end = field(entry)? + field(entry + 8)?;
    let have = bytes.len();
    bytes.resize(end, 0);
    file.read_exact(&mut bytes[have..])?;
    std::hint::black_box(&bytes);
    println!("{} 0", started.elapsed().as_nanos());
    Ok(())
}

fn names_bench(dir: &Path, runs: usize) -> Result<()> {
    let exe = std::env::current_exe()?;
    let file = dir.join("catalog");
    for cold in [true, false] {
        let mut read = Vec::new();
        for _ in 0..runs.max(1) {
            if cold {
                evict(&file)?;
            }
            let output = Command::new(&exe).arg("open-names").arg(dir).output()?;
            let text = String::from_utf8(output.stdout)?;
            let ns = text.split_whitespace().next().ok_or("no read time")?;
            read.push(Duration::from_nanos(ns.parse()?));
        }
        read.sort();
        println!(
            "name sections only, {}: read {:.2} ({:.2}) ms median (min)",
            if cold { "evicted" } else { "warm" },
            ms(read[read.len() / 2]),
            ms(read[0])
        );
    }
    Ok(())
}

fn evict(file: &Path) -> Result<()> {
    let status = Command::new("dd")
        .arg(format!("if={}", file.display()))
        .args(["iflag=nocache", "count=0", "status=none"])
        .status()?;
    if !status.success() {
        return Err("dd failed".into());
    }
    Ok(())
}

fn open_bench(dir: &Path, runs: usize, cold: bool) -> Result<()> {
    let exe = std::env::current_exe()?;
    let file = dir.join("catalog");
    let (mut wall, mut read, mut decode) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..runs.max(1) {
        if cold {
            evict(&file)?;
        }
        let started = Instant::now();
        let output = Command::new(&exe).arg("open").arg(dir).output()?;
        wall.push(started.elapsed());
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
        }
        let text = String::from_utf8(output.stdout)?;
        let mut fields = text.split_whitespace().map(str::parse::<u64>);
        read.push(Duration::from_nanos(fields.next().ok_or("no read time")??));
        decode.push(Duration::from_nanos(
            fields.next().ok_or("no decode time")??,
        ));
    }
    let label = if cold {
        "evicted (per-file, not drop_caches)"
    } else {
        "warm"
    };
    println!("{label}, {} fresh processes, median (min) ms:", wall.len());
    for (what, times) in [("read", read), ("decode", decode), ("process", wall)] {
        let mut times = times;
        times.sort();
        println!(
            "  {what:>7} {:.2} ({:.2})",
            ms(times[times.len() / 2]),
            ms(times[0])
        );
    }
    Ok(())
}
