//! Arm 1: a full scan with ripgrep, flagged to the oracle's semantics.
//! The oracle runs the `regex` crate over whole files with Unicode on,
//! `multi_line`, and every byte searched. So rg searches every file with
//! no ignore rules or hidden-file skipping (`--no-ignore --hidden`),
//! treats binary files as text (`-a`), never transcodes a UTF-16 BOM
//! (`-E none`), and lists files (`-l`), stopping at a file's first match
//! as `is_match` does. Case follows the population's flag. No seed
//! pattern can match a newline, so line-oriented search finds the same
//! files as a whole-file match; the file-set check confirms it per query.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub(super) struct Scan {
    pub(super) docs: Vec<u32>,
    pub(super) wall: Duration,
    pub(super) cpu: f64,
    /// `rchar`: bytes rg asked the kernel for, cache hits included.
    pub(super) rchar: u64,
    /// `read_bytes`: bytes the kernel fetched from the device.
    pub(super) device: u64,
}

/// Reaped children's I/O lands in the parent's `/proc/self/io`, and their
/// CPU in `cutime`/`cstime`; deltas around one spawn are that child's.
fn counters() -> crate::Result<(u64, u64, f64)> {
    let io = fs::read_to_string("/proc/self/io")?;
    let field = |name: &str| -> crate::Result<u64> {
        Ok(io
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .ok_or_else(|| format!("no {name} in /proc/self/io"))?
            .trim()
            .parse()?)
    };
    let stat = fs::read_to_string("/proc/self/stat")?;
    let fields: Vec<&str> = stat
        .rsplit_once(") ")
        .map_or(Vec::new(), |(_, rest)| rest.split(' ').collect());
    let tick = |i: usize| -> crate::Result<u64> {
        Ok(fields.get(i).ok_or("short /proc/self/stat")?.parse()?)
    };
    Ok((
        field("rchar:")?,
        field("read_bytes:")?,
        (tick(13)? + tick(14)?) as f64 / 100.0,
    ))
}

pub(super) fn scan(
    pattern: &str,
    case_insensitive: bool,
    corpus: &Path,
    threads: Option<usize>,
) -> crate::Result<Scan> {
    let mut command = Command::new("rg");
    command.args([
        "--no-config",
        "--no-ignore",
        "--hidden",
        "-a",
        "-E",
        "none",
        "-l",
        "--no-messages",
        "-g",
        "!population",
    ]);
    if case_insensitive {
        command.arg("-i");
    } else {
        command.arg("-s");
    }
    if let Some(threads) = threads {
        command.arg(format!("-j{threads}"));
    }
    command
        .arg("-e")
        .arg(pattern)
        .arg(corpus)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit());
    let before = counters()?;
    let start = Instant::now();
    let output = command.output()?;
    let wall = start.elapsed();
    let after = counters()?;
    // rg exits 1 when nothing matched, 2 on an error.
    if !matches!(output.status.code(), Some(0 | 1)) {
        return Err(format!("rg failed: {}", output.status).into());
    }
    let mut docs = Vec::new();
    for line in String::from_utf8(output.stdout)?.lines() {
        let name = Path::new(line)
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("rg listed an unnamed path")?;
        docs.push(name.parse::<u32>()?);
    }
    docs.sort_unstable();
    Ok(Scan {
        docs,
        wall,
        cpu: after.2 - before.2,
        rchar: after.0 - before.0,
        device: after.1 - before.1,
    })
}
