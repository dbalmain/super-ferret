//! Process measurements and the disk floor.

use std::fs;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

/// Seconds since the first call.
pub fn wall() -> f64 {
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// User plus system CPU seconds of this process, all threads.
pub fn cpu_seconds() -> f64 {
    let stat = fs::read_to_string("/proc/self/stat").expect("stat");
    let after = &stat[stat.rfind(')').expect("comm") + 2..];
    let fields: Vec<&str> = after.split(' ').collect();
    // utime and stime are fields 14 and 15; `after` starts at field 3.
    let ticks: u64 = fields[11].parse::<u64>().expect("utime") + fields[12].parse::<u64>().expect("stime");
    ticks as f64 / 100.0
}

/// Peak resident set, kB.
pub fn peak_rss_kb() -> u64 {
    status_kb("VmHWM:")
}

fn status_kb(key: &str) -> u64 {
    let status = fs::read_to_string("/proc/self/status").expect("status");
    status
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .unwrap_or(0)
}

/// Bytes this process has caused to be read from storage.
pub fn device_read_bytes() -> u64 {
    let io = fs::read_to_string("/proc/self/io").expect("io");
    io.lines()
        .find_map(|l| l.strip_prefix("read_bytes: "))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Refuses below the brief's floor of 8 GB free.
pub fn check_disk(path: &Path) {
    let st = rustix::fs::statvfs(path).expect("statvfs");
    let free = st.f_bavail * st.f_frsize;
    assert!(free >= 8_000_000_000, "disk floor: {free} bytes free under {}", path.display());
}

pub fn median_p90(mut v: Vec<f64>) -> (f64, f64) {
    if v.is_empty() {
        return (f64::NAN, f64::NAN);
    }
    v.sort_by(|a, b| a.partial_cmp(b).expect("nan"));
    // The screens harness's rule: the lower median, and ceil(0.9 n).
    (v[(v.len() - 1) / 2], v[(v.len() * 9).div_ceil(10) - 1])
}
