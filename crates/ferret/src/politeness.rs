//! Cheap procfs/sysfs signals; missing values remain explicit. The scheduler
//! consumes this trait for production sampling and injected test transitions.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Idle {
    Unknown,
    Headless,
    Desktop(Duration),
}
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
    fn default() -> Self {
        Self {
            proc: "/proc".into(),
            power: "/sys/class/power_supply".into(),
            probe: None,
        }
    }
}
impl Linux {
    pub fn from_env() -> Self {
        Self {
            proc: std::env::var_os("FERRET_SIGNAL_PROC")
                .map_or_else(|| "/proc".into(), PathBuf::from),
            power: std::env::var_os("FERRET_SIGNAL_POWER")
                .map_or_else(|| "/sys/class/power_supply".into(), PathBuf::from),
            probe: None,
        }
    }
}
fn psi(text: &str) -> Option<f64> {
    let line = text.lines().find(|line| line.starts_with("some "))?;
    let value: f64 = line
        .split_whitespace()
        .find_map(|part| part.strip_prefix("avg10="))?
        .parse()
        .ok()?;
    (value.is_finite() && (0.0..=100.0).contains(&value)).then_some(value)
}
fn battery(path: &Path) -> Option<bool> {
    let mut battery = false;
    for entry in fs::read_dir(path).ok()? {
        let path = entry.ok()?.path();
        if fs::read_to_string(path.join("type")).ok()?.trim() == "Battery" {
            match fs::read_to_string(path.join("status")).ok()?.trim() {
                "Discharging" => battery = true,
                "Charging" | "Full" | "Not charging" => {}
                _ => return None,
            }
        }
    }
    Some(battery)
}
fn session_idle() -> Idle {
    // DISPLAY absence alone cannot prove there is no interactive tty session.
    if ["DISPLAY", "WAYLAND_DISPLAY", "XDG_SESSION_TYPE"]
        .iter()
        .any(|key| std::env::var_os(key).is_some())
    {
        return Idle::Unknown;
    }
    let Ok(sessions) = fs::read_dir("/run/systemd/sessions") else {
        return Idle::Unknown;
    };
    for session in sessions {
        let Some(text) = session
            .ok()
            .and_then(|entry| fs::read_to_string(entry.path()).ok())
        else {
            return Idle::Unknown;
        };
        if text
            .lines()
            .any(|line| matches!(line, "TYPE=tty" | "TYPE=x11" | "TYPE=wayland"))
        {
            return Idle::Unknown;
        }
    }
    Idle::Headless
}
impl Signals for Linux {
    fn sample(&mut self) -> Sample {
        let pressure = |name: &str| {
            fs::read_to_string(self.proc.join("pressure").join(name))
                .ok()
                .and_then(|text| psi(&text))
        };
        let memory = fs::read_to_string(self.proc.join("meminfo"))
            .ok()
            .and_then(|text| {
                text.lines().find_map(|line| {
                    line.strip_prefix("MemAvailable:")
                        .and_then(|s| s.split_whitespace().next()?.parse::<u64>().ok())
                        .map(|n| n.saturating_mul(1024))
                })
            });
        let load = fs::read_to_string(self.proc.join("loadavg"))
            .ok()
            .and_then(|s| s.split_whitespace().next()?.parse().ok());
        Sample {
            cpu: pressure("cpu"),
            io: pressure("io"),
            battery: battery(&self.power),
            load,
            memory,
            idle: self
                .probe
                .as_mut()
                .and_then(|p| p.idle())
                .map_or_else(session_idle, Idle::Desktop),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn psi_reads_some_avg10_and_rejects_missing_or_invalid_values() {
        assert_eq!(
            psi("some avg10=12.50 avg60=2.0 total=1\nfull avg10=99.0"),
            Some(12.5)
        );
        for text in [
            "full avg10=1.0",
            "some avg60=1.0",
            "some avg10=NaN",
            "some avg10=-1",
            "some avg10=101",
        ] {
            assert_eq!(psi(text), None);
        }
    }
    #[test]
    fn real_signal_source_reports_missing_files_and_battery_transitions() {
        let base = std::env::temp_dir().join(format!("ferret-signals-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("power/BAT0")).unwrap_or_else(|e| panic!("fixture: {e}"));
        fs::create_dir_all(base.join("proc/pressure")).unwrap_or_else(|e| panic!("fixture: {e}"));
        let mut source = Linux {
            proc: base.join("proc"),
            power: base.join("power"),
            probe: None,
        };
        let missing = source.sample();
        assert_eq!(missing.cpu, None);
        assert_eq!(missing.io, None);
        assert_eq!(missing.battery, None);
        fs::write(base.join("power/BAT0/type"), "Battery\n")
            .unwrap_or_else(|e| panic!("fixture: {e}"));
        for (state, expected) in [
            ("Discharging", Some(true)),
            ("Charging", Some(false)),
            ("Unknown", None),
        ] {
            fs::write(base.join("power/BAT0/status"), state)
                .unwrap_or_else(|e| panic!("fixture: {e}"));
            assert_eq!(source.sample().battery, expected);
        }
        fs::write(base.join("proc/pressure/cpu"), "some avg10=3.0 avg60=1.0")
            .unwrap_or_else(|e| panic!("fixture: {e}"));
        fs::write(base.join("proc/pressure/io"), "some avg10=11.0 avg60=0.0")
            .unwrap_or_else(|e| panic!("fixture: {e}"));
        fs::write(base.join("proc/meminfo"), "MemAvailable: 1234 kB\n")
            .unwrap_or_else(|e| panic!("fixture: {e}"));
        fs::write(base.join("proc/loadavg"), "1.25 2.0 3.0 1/100 4")
            .unwrap_or_else(|e| panic!("fixture: {e}"));
        let sample = source.sample();
        assert_eq!(sample.cpu, Some(3.0));
        assert_eq!(sample.io, Some(11.0));
        assert_eq!(sample.memory, Some(1234 * 1024));
        assert_eq!(sample.load, Some(1.25));
        fs::remove_dir_all(base).unwrap_or_else(|e| panic!("cleanup: {e}"));
    }
}
