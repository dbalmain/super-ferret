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
}
impl Default for Controller {
    fn default() -> Self {
        Self { concurrency: ferret_crawl::default_workers(), battery_pause: true,
            full_memory: 3 << 30, checkpoint_memory: 700_000_000,
            disk: 700_000_000, memory_floor: 64 << 20, rate: 32 << 20 }
    }
}
impl Controller {
    pub fn from_env() -> Self {
        let mut config = Self::default();
        let value = |name: &str, default| std::env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(default);
        config.concurrency = value("FERRET_INDEX_WORKERS", config.concurrency as u64).max(1) as usize;
        config.full_memory = value("FERRET_FULL_MEMORY_BYTES", config.full_memory);
        config.checkpoint_memory = value("FERRET_CHECKPOINT_MEMORY_BYTES", config.checkpoint_memory);
        config.disk = value("FERRET_CHECKPOINT_DISK_BYTES", config.disk);
        config.memory_floor = value("FERRET_MEMORY_FLOOR_BYTES", config.memory_floor);
        config.rate = value("FERRET_BULK_BYTES_PER_SECOND", config.rate);
        config.battery_pause = value("FERRET_BATTERY_PAUSE", u64::from(config.battery_pause)) != 0;
        config
    }
}
