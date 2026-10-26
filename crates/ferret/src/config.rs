//! Daemon controller configuration. Environment overrides retain the existing
//! CLI configuration model; byte reserves are additional available headroom.

#[derive(Clone, Debug)]
pub(crate) struct Controller {
    pub concurrency: usize,
    pub battery_pause: bool,
    pub full_memory: u64,
    pub checkpoint_memory: u64,
    pub disk: u64,
    pub memory_floor: u64,
    pub rate: u64,
    pub additional_memory: u64,
}
impl Default for Controller {
    fn default() -> Self {
        Self {
            concurrency: ferret_crawl::default_workers(),
            battery_pause: true,
            full_memory: 3 << 30,
            checkpoint_memory: 700_000_000,
            disk: 700_000_000,
            memory_floor: 64 << 20,
            rate: 32 << 20,
            additional_memory: 0,
        }
    }
}
impl Controller {
    pub fn from_env() -> Self {
        let mut config = Self::default();
        let value = |name: &str, default| {
            std::env::var(name)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(default)
        };
        config.concurrency =
            value("FERRET_INDEX_WORKERS", config.concurrency as u64).max(1) as usize;
        config.full_memory = value("FERRET_FULL_MEMORY_BYTES", config.full_memory);
        config.checkpoint_memory =
            value("FERRET_CHECKPOINT_MEMORY_BYTES", config.checkpoint_memory);
        config.disk = value("FERRET_CHECKPOINT_DISK_BYTES", config.disk);
        config.memory_floor = value("FERRET_MEMORY_FLOOR_BYTES", config.memory_floor);
        config.additional_memory =
            value("FERRET_ADDITIONAL_MEMORY_BYTES", config.additional_memory);
        config.rate = value("FERRET_BULK_BYTES_PER_SECOND", config.rate);
        config.battery_pause = value("FERRET_BATTERY_PAUSE", u64::from(config.battery_pause)) != 0;
        config
    }
}

#[cfg(test)]
mod tests {
    use super::Controller;

    /// Names the case a child copy of this test binary checks. Setting the
    /// environment in-process is unsafe and would race other tests, so each
    /// case runs in its own process with only that case's values.
    const MARKER: &str = "FERRET_TEST_CONTROLLER_CASE";
    const TEST: &str = "config::tests::controller_overrides_are_read_from_the_environment";
    const BYTES: [&str; 5] = [
        "FERRET_FULL_MEMORY_BYTES",
        "FERRET_CHECKPOINT_MEMORY_BYTES",
        "FERRET_CHECKPOINT_DISK_BYTES",
        "FERRET_MEMORY_FLOOR_BYTES",
        "FERRET_ADDITIONAL_MEMORY_BYTES",
    ];

    fn bytes(c: &Controller) -> [u64; 5] {
        [
            c.full_memory,
            c.checkpoint_memory,
            c.disk,
            c.memory_floor,
            c.additional_memory,
        ]
    }

    /// Each override the daemon tests leave unset, through the real
    /// `Controller::from_env`. Byte sizes parse as `u64`, and anything else
    /// keeps the default. `FERRET_BATTERY_PAUSE` parses the same way, then
    /// `0` turns the pause off; a value that does not parse keeps it on.
    #[test]
    fn controller_overrides_are_read_from_the_environment() {
        match std::env::var(MARKER).as_deref() {
            Err(_) => {
                let cases: [(&str, [&str; 5], &str); 2] = [
                    ("set", ["1001", "1002", "1003", "1004", "1005"], "0"),
                    ("malformed", ["3G", "-1", "", "1e6", " 5"], "off"),
                ];
                for (case, values, battery) in cases {
                    let output = std::process::Command::new(std::env::current_exe().unwrap())
                        .args(["--exact", TEST, "--nocapture"])
                        .envs(BYTES.iter().zip(values))
                        .env("FERRET_BATTERY_PAUSE", battery)
                        .env(MARKER, case)
                        .output()
                        .unwrap();
                    assert!(output.status.success(), "{case}: {output:?}");
                    // A filter that matched nothing would also exit 0.
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    assert!(stdout.contains("1 passed"), "{case}: {stdout}");
                }
            }
            Ok("set") => {
                let c = Controller::from_env();
                assert_eq!(bytes(&c), [1001, 1002, 1003, 1004, 1005]);
                assert!(!c.battery_pause);
            }
            Ok("malformed") => {
                let c = Controller::from_env();
                assert_eq!(bytes(&c), bytes(&Controller::default()));
                assert!(c.battery_pause);
            }
            Ok(other) => panic!("unknown case {other}"),
        }
    }
}
