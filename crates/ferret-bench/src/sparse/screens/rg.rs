//! Arm 1: a full scan with ripgrep, flagged to the oracle's semantics.
//! The oracle runs the `regex` crate over whole files with Unicode on,
//! `multi_line`, and every byte searched. So rg searches every file with
//! no ignore rules or hidden-file skipping (`--no-ignore --hidden`),
//! treats binary files as text (`-a`), never transcodes a UTF-16 BOM
//! (`-E none`), and lists files (`-l`), stopping at a file's first match
//! as `is_match` does; [`lines`] prints
//! every matching line instead, rg's default. Case follows the population's
//! flag. No seed pattern can match a newline, so line-oriented search finds the
//! same files as a whole-file match; the file-set check confirms it per query.

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

/// This process's I/O and CPU, threads included. Reaped children's I/O
/// lands in `/proc/self/io` too, and their CPU in `cutime`/`cstime`; deltas
/// around one spawn are that child's.
pub(super) struct Counters {
    pub(super) rchar: u64,
    pub(super) device: u64,
    pub(super) children_cpu: f64,
    pub(super) self_cpu: f64,
}

pub(super) fn counters() -> crate::Result<Counters> {
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
    // Fields after the command, from `state` (field 3) on.
    let fields: Vec<&str> = stat
        .rsplit_once(") ")
        .map_or(Vec::new(), |(_, rest)| rest.split(' ').collect());
    let tick = |i: usize| -> crate::Result<u64> {
        Ok(fields.get(i).ok_or("short /proc/self/stat")?.parse()?)
    };
    Ok(Counters {
        rchar: field("rchar:")?,
        device: field("read_bytes:")?,
        self_cpu: (tick(11)? + tick(12)?) as f64 / 100.0,
        children_cpu: (tick(13)? + tick(14)?) as f64 / 100.0,
    })
}

/// rg flagged to the oracle's semantics, listing files with `-l` or
/// printing every matching line.
fn command(
    pattern: &str,
    case_insensitive: bool,
    corpus: &Path,
    threads: Option<usize>,
    list: bool,
) -> Command {
    let mut command = Command::new("rg");
    command.args([
        "--no-config",
        "--no-ignore",
        "--hidden",
        "-a",
        "-E",
        "none",
        "--no-messages",
        "-g",
        "!population",
    ]);
    if list {
        command.arg("-l");
    }
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
    command
}

/// Where `lines` sends rg's output.
#[derive(Clone, Copy)]
pub(super) enum Sink {
    DevNull,
    /// A pipe this process drains, counting the bytes.
    Pipe,
}

/// One run of rg printing lines.
pub(super) struct Printed {
    pub(super) wall: Duration,
    pub(super) cpu: f64,
    pub(super) device: u64,
    /// Bytes printed into a pipe; zero for /dev/null.
    pub(super) printed: u64,
}

/// rg printing every matching line (its default output) into `sink`.
pub(super) fn lines(
    pattern: &str,
    case_insensitive: bool,
    corpus: &Path,
    threads: Option<usize>,
    sink: Sink,
) -> crate::Result<Printed> {
    let mut command = command(pattern, case_insensitive, corpus, threads, false);
    command.stdout(match sink {
        Sink::DevNull => Stdio::from(fs::File::create("/dev/null")?),
        Sink::Pipe => Stdio::piped(),
    });
    let before = counters()?;
    let start = Instant::now();
    let mut child = command.spawn()?;
    let mut printed = 0;
    if let Some(mut out) = child.stdout.take() {
        printed = std::io::copy(&mut out, &mut std::io::sink())?;
    }
    let status = child.wait()?;
    let wall = start.elapsed();
    let after = counters()?;
    if !matches!(status.code(), Some(0 | 1)) {
        return Err(format!("rg failed: {status}").into());
    }
    Ok(Printed {
        wall,
        cpu: after.children_cpu - before.children_cpu,
        device: after.device - before.device,
        printed,
    })
}

pub(super) fn scan(
    pattern: &str,
    case_insensitive: bool,
    corpus: &Path,
    threads: Option<usize>,
) -> crate::Result<Scan> {
    let mut command = command(pattern, case_insensitive, corpus, threads, true);
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
        cpu: after.children_cpu - before.children_cpu,
        rchar: after.rchar - before.rchar,
        device: after.device - before.device,
    })
}
