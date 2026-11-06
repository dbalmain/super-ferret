//! Scratch-tree engine measurements using public `Engine` and `Documents`
//! APIs. DESIGN.md permits these dependencies in ferret-bench; no production
//! visibility is widened. The private `content_faults` measurement stays an
//! ignored crawl test, explicitly excluded from the extended gate.
//!
//! `verify-100k` measures three checked-read passes over 100,000 documents,
//! then forty eight-file follow refreshes. It prints VERIFY_100K and
//! FOLLOW_100K with the original counters and units.
//!
//! `steady-content-follow` measures forty eight-file follow refreshes over
//! 512 files of 64 KiB, after real refresh has warmed each edited document.
//! It prints M5_FOLLOW: bytes, I/O counters, CPU/wall ns, resident view bytes,
//! and the RSS cost of 128 pins. Both commands own and remove their scratch
//! trees; neither connects to a daemon. Run in release, one at a time.
//!
//! `--smoke` runs one refresh on a small tree (8 verification files or 16
//! steady-follow files), exercising all counters without a full measurement.
//! Its numbers are smoke output, not comparable measurement results.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use ferret::engine::{Engine, QuerySession};
use ferret_catalog::{CompactionLimits, WriterSession};
use ferret_crawl::{IndexOptions, Refresh, RefreshReason, RefreshRequest, RefreshScope, index};
use ferret_index::Budget;

use crate::Result;

struct Tree {
    base: PathBuf,
    workers: usize,
}

impl Tree {
    fn new(workers: usize) -> Result<Self> {
        let base =
            std::env::temp_dir().join(format!("ferret-bench-content-{}", std::process::id()));
        fs::create_dir(&base)?;
        let tree = Self { base, workers };
        fs::create_dir(tree.root())?;
        Ok(tree)
    }

    fn root(&self) -> PathBuf {
        self.base.join("tree")
    }
    fn index(&self) -> PathBuf {
        self.base.join("index")
    }
    fn options(&self) -> IndexOptions {
        IndexOptions {
            workers: self.workers,
            ..IndexOptions::default()
        }
    }

    fn write(&self, relative: &str, bytes: &[u8]) -> Result<()> {
        let path = self.root().join(relative);
        fs::create_dir_all(path.parent().ok_or("missing parent")?)?;
        fs::write(path, bytes)?;
        Ok(())
    }

    fn crawl(&self) -> Result<()> {
        index(&self.index(), &[self.root()], Refresh::All, &self.options())?;
        Ok(())
    }

    fn writer(&self) -> Result<Engine> {
        let mut writer = WriterSession::open(&self.index())?;
        writer.set_compaction_limits(CompactionLimits {
            log_bytes: u64::MAX,
            records: u64::MAX,
            dirty_percent: 100,
            dead_percent: 100,
        });
        Ok(Engine::from_writer(writer))
    }

    fn refresh(&self, engine: &Engine) -> Result<()> {
        engine.refresh(
            RefreshRequest {
                expected_generation: engine.generation(),
                scopes: vec![RefreshScope::Root(self.root())],
                rename_hints: Vec::new(),
                reason: RefreshReason::Burst,
            },
            &self.options(),
        )?;
        Ok(())
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn cpu() -> Result<u64> {
    Ok(fs::read_to_string("/proc/thread-self/schedstat")?
        .split_whitespace()
        .next()
        .ok_or("missing schedstat CPU counter")?
        .parse()?)
}

fn counter(path: &str, key: &str) -> Result<u64> {
    let contents = fs::read_to_string(path)?;
    Ok(contents
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .and_then(|value| value.split_whitespace().next())
        .ok_or_else(|| format!("missing {key} in {path}"))?
        .parse()?)
}

/// Checked verification and follow on a 100k-file tree; see module docs.
pub(crate) fn verify(smoke: bool) -> Result<()> {
    if smoke {
        eprintln!("SMOKE: reduced verification tree and one refresh");
    }
    let tree = Tree::new(2)?;
    let (dirs, files, rounds, passes) = if smoke {
        (1, 8, 1, 1)
    } else {
        (100, 1000, 40, 3)
    };
    for dir in 0..dirs {
        for file in 0..files {
            tree.write(
                &format!("d{dir}/inner/f{file}.txt"),
                format!("alpha beta unique{dir}file{file}\n").as_bytes(),
            )?;
        }
    }
    tree.crawl()?;
    let engine = tree.writer()?;
    let pin = engine.pin();
    let catalog = pin.catalog();
    let names: Vec<_> = catalog
        .name_reader()
        .runs_from(ferret_catalog::NameId(0))
        .filter_map(|(name, edge)| match edge.target() {
            ferret_catalog::Target::Inode(inode) if catalog.doc(inode).is_some() => Some(name),
            _ => None,
        })
        .collect();
    assert_eq!(names.len(), dirs * files);
    let text = ferret_verify::Text::new(b"alpha beta", false).ok_or("empty verification text")?;
    for pass in 0..passes {
        let mut documents = ferret_crawl::Documents::by_name(catalog)?;
        let mut matcher = ferret_verify::TextMatcher::new();
        let mut bytes = Vec::new();
        let cpu_before = cpu()?;
        let started = std::time::Instant::now();
        for &name in &names {
            documents
                .read_name(catalog, name, &mut bytes)
                .map_err(|fault| format!("verification read for {name:?}: {fault:?}"))?;
            assert!(matcher.is_match(&text, &bytes));
        }
        let wall = started.elapsed();
        let cpu_ns = cpu()? - cpu_before;
        println!(
            "VERIFY_100K pass={pass} files={} bytes={} cpu_ns={cpu_ns} wall_ns={} files_per_second={:.0}",
            documents.files_read,
            documents.bytes_read,
            wall.as_nanos(),
            names.len() as f64 / wall.as_secs_f64()
        );
    }

    engine.attach_content(&tree.index())?;
    while engine.follow_content(&Budget::unbounded(), None)?.remaining > 0 {}
    let (mut follow_cpu, mut follow_wall, mut changed) = (0u64, 0u128, 0u64);
    for round in 1..=rounds {
        for file in 0..8 {
            let mut bytes = format!("document{file} revision{round} alpha beta ").into_bytes();
            bytes.extend_from_slice(&b"alpha beta requestHandler ".repeat(3000));
            bytes.truncate(64 << 10);
            tree.write(&format!("d0/inner/f{file}.txt"), &bytes)?;
        }
        tree.refresh(&engine)?;
        let before_cpu = cpu()?;
        let started = std::time::Instant::now();
        loop {
            let followed = engine.follow_content(&Budget::unbounded(), None)?;
            changed += followed.bytes;
            if followed.remaining == 0 {
                break;
            }
        }
        follow_wall += started.elapsed().as_nanos();
        follow_cpu += cpu()? - before_cpu;
    }
    assert_eq!(changed, rounds * 8 * (64 << 10));
    println!(
        "FOLLOW_100K refreshes={rounds} changed_bytes={changed} cpu_ns={follow_cpu} wall_ns={follow_wall}"
    );
    Ok(())
}

/// Steady follow after production refresh; see module docs.
pub(crate) fn follow(smoke: bool) -> Result<()> {
    if smoke {
        eprintln!("SMOKE: reduced follow tree and one refresh");
    }
    let tree = Tree::new(4)?;
    tree.write("a.txt", b"alpha")?;
    tree.write("sub/b.txt", b"beta")?;
    tree.crawl()?;
    let (files, rounds) = if smoke { (16, 1) } else { (512, 40) };
    let document = |file, round| {
        let head = format!("document{file} revision{round} common requestHandler ");
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(&b"common requestHandler alpha beta ".repeat(2048));
        bytes.truncate(64 << 10);
        bytes
    };
    for file in 0..files {
        fs::write(
            tree.root().join(format!("measure-{file}.txt")),
            document(file, 0),
        )?;
    }
    let engine = tree.writer()?;
    tree.refresh(&engine)?;
    engine.attach_content(&tree.index())?;
    while engine.follow_content(&Budget::unbounded(), None)?.remaining > 0 {}
    while engine.merge_content(&Budget::unbounded())?.is_some() {}
    let (mut nanos, mut chars, mut device, mut changed, mut wall) =
        (0u64, 0u64, 0u64, 0u64, Duration::ZERO);
    for round in 1..=rounds {
        for offset in 0..8 {
            let file = (round * 8 + offset) % files;
            fs::write(
                tree.root().join(format!("measure-{file}.txt")),
                document(file, round),
            )?;
        }
        tree.refresh(&engine)?;
        let before_chars = counter("/proc/self/io", "rchar:")?;
        let before_device = counter("/proc/self/io", "read_bytes:")?;
        let before_cpu = cpu()?;
        let started = std::time::Instant::now();
        loop {
            let followed = engine.follow_content(&Budget::unbounded(), None)?;
            changed += followed.bytes;
            if followed.remaining == 0 {
                break;
            }
        }
        wall += started.elapsed();
        nanos += cpu()? - before_cpu;
        chars += counter("/proc/self/io", "rchar:")? - before_chars;
        device += counter("/proc/self/io", "read_bytes:")? - before_device;
        while engine.merge_content(&Budget::unbounded())?.is_some() {}
    }
    let before_rss = counter("/proc/self/status", "VmRSS:")?;
    let pins: Vec<_> = (0..128).map(|_| engine.pin()).collect();
    let after_rss = counter("/proc/self/status", "VmRSS:")?;
    println!(
        "M5_FOLLOW refreshes={rounds} changed_bytes={changed} rchar={chars} read_bytes={device} cpu_ns={nanos} wall_ns={} view_bytes={} pin_count={} pin_size={} rss_delta_kib={}",
        wall.as_nanos(),
        pins[0]
            .content()
            .ok_or("missing content view")?
            .resident_bytes(),
        pins.len(),
        std::mem::size_of::<QuerySession>(),
        after_rss.saturating_sub(before_rss)
    );
    assert_eq!(changed, rounds * 8 * (64 << 10));
    Ok(())
}
