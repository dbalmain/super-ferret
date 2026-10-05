//! Cheap procfs/sysfs signals; missing values remain explicit. The scheduler
//! consumes this trait for production sampling and injected test transitions.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Idle { Unknown, Headless, Desktop(Duration) }
#[derive(Clone, Debug)]
pub(crate) struct Sample {
    pub cpu: Option<f64>,
    pub io: Option<f64>,
    pub battery: Option<bool>,
    pub load: Option<f64>,
    pub memory: Option<u64>,
    pub idle: Idle,
}
pub(crate) trait Signals: Send + std::fmt::Debug {
    fn sample(&mut self) -> Sample;
}
/// A host may supply a compositor probe without making it a core dependency.
pub(crate) trait IdleProbe: Send + std::fmt::Debug {
    fn idle(&mut self) -> Option<Duration>;
}
#[derive(Debug)]
pub(crate) struct Linux {
    proc: PathBuf,
    power: PathBuf,
    probe: Option<Box<dyn IdleProbe>>,
}
impl Default for Linux {
    fn default() -> Self { Self { proc: "/proc".into(), power: "/sys/class/power_supply".into(), probe: None } }
}
fn psi(text: &str) -> Option<f64> {
    let line = text.lines().find(|line| line.starts_with("some "))?;
    let value: f64 = line.split_whitespace().find_map(|part| part.strip_prefix("avg10="))?.parse().ok()?;
    (value.is_finite() && (0.0..=100.0).contains(&value)).then_some(value)
}
fn battery(path: &Path) -> Option<bool> {
    let mut battery = false;
    for entry in fs::read_dir(path).ok()? {
        let path = entry.ok()?.path();
        if fs::read_to_string(path.join("type")).ok()?.trim() == "Battery" {
            match fs::read_to_string(path.join("status")).ok()?.trim() {
                "Discharging" => battery = true,
                "Charging" | "Full" | "Not charging" => {},
                _ => return None,
            }
        }
    }
    Some(battery)
}
fn session_idle() -> Idle {
    // DISPLAY absence alone cannot prove there is no interactive tty session.
    if ["DISPLAY", "WAYLAND_DISPLAY", "XDG_SESSION_TYPE"].iter().any(|key| std::env::var_os(key).is_some()) {
        return Idle::Unknown;
    }
    let Ok(sessions) = fs::read_dir("/run/systemd/sessions") else { return Idle::Unknown; };
    for session in sessions {
        let Some(text) = session.ok().and_then(|entry| fs::read_to_string(entry.path()).ok()) else { return Idle::Unknown; };
        if text.lines().any(|line| matches!(line, "TYPE=tty" | "TYPE=x11" | "TYPE=wayland")) {
            return Idle::Unknown;
        }
    }
    Idle::Headless
}
impl Signals for Linux {
    fn sample(&mut self) -> Sample {
        let pressure = |name: &str| fs::read_to_string(self.proc.join("pressure").join(name)).ok().and_then(|text| psi(&text));
        let memory = fs::read_to_string(self.proc.join("meminfo")).ok().and_then(|text| {
            text.lines().find_map(|line| line.strip_prefix("MemAvailable:").and_then(|s| s.split_whitespace().next()?.parse::<u64>().ok()).map(|n| n.saturating_mul(1024)))
        });
        let load = fs::read_to_string(self.proc.join("loadavg")).ok().and_then(|s| s.split_whitespace().next()?.parse().ok());
        Sample { cpu: pressure("cpu"), io: pressure("io"), battery: battery(&self.power), load, memory,
            idle: self.probe.as_mut().and_then(|p| p.idle()).map_or_else(session_idle, Idle::Desktop) }
    }
}
