//! One controller shared by the writer's job gate and catalog bulk allocation
//! admission. Signal sampling and the monotonic ratchet use injectable traits;
//! admitted publication is never cancelled by a subsequent pressure spike.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferret_catalog::bulk::{Blocked, Clock, Control, Kind, Limiter};
use ferret_catalog::Catalog;

use crate::config::Controller as Config;
use crate::politeness::{Idle, Sample, Signals};

#[derive(Clone, Debug)]
pub(crate) struct Status {
    pub sample: Sample,
    pub workers: usize,
    pub paused: Option<Blocked>,
    pub admission: Option<(Kind, Result<(), Blocked>)>,
    pub required_memory: u64,
    pub required_disk: u64,
    pub available_disk: Option<u64>,
}
#[derive(Debug)]
struct Machine {
    source: Box<dyn Signals>,
    status: Status,
    calm_since: Option<Duration>,
    resources: u64,
}
#[derive(Debug)]
pub(crate) struct Scheduler {
    config: Config,
    cpus: usize,
    index: PathBuf,
    clock: Arc<dyn Clock>,
    limiter: Arc<Limiter>,
    machine: Mutex<Machine>,
}
impl Scheduler {
    pub fn new(config: Config, cpus: usize, index: PathBuf, mut source: Box<dyn Signals>, clock: Arc<dyn Clock>) -> Self {
        let sample = source.sample();
        let limiter = Arc::new(Limiter::new(config.rate, clock.clone()));
        let scheduler = Self { config, cpus: cpus.max(1), index, clock, limiter,
            machine: Mutex::new(Machine { source, status: Status { sample, workers: 1, paused: None,
                admission: None, required_memory: 0, required_disk: 0, available_disk: None }, calm_since: None, resources: 0 }) };
        scheduler.sample();
        scheduler
    }
    pub fn sample(&self) {
        let mut m = self.machine.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let sample = m.source.sample();
        let paused = match (self.config.battery_pause, sample.battery, sample.io) {
            (true, Some(true), _) => Some(Blocked::Battery),
            (true, None, _) | (_, _, None) => Some(Blocked::Unavailable),
            (_, _, Some(io)) if io > 10.0 => Some(Blocked::IoPressure),
            _ => None,
        };
        let desired = if paused.is_some() || sample.cpu.is_none_or(|cpu| cpu > 20.0) {
            1
        } else {
            match sample.idle {
                Idle::Unknown => 1,
                Idle::Desktop(idle) if idle <= Duration::from_secs(30) => 1,
                Idle::Desktop(idle) if idle <= Duration::from_secs(300) => (self.cpus / 4).max(1),
                Idle::Headless | Idle::Desktop(_) => (self.cpus / 2).max(1),
            }
        }.min(self.config.concurrency.max(1));
        let now = self.clock.now();
        if desired <= m.status.workers {
            m.status.workers = desired;
            m.calm_since = None;
        } else {
            let since = *m.calm_since.get_or_insert(now);
            if now.saturating_sub(since) >= Duration::from_secs(10) {
                m.status.workers += 1;
                m.calm_since = Some(now);
            }
        }
        m.status.sample = sample;
        m.status.paused = paused;
    }
    pub fn status(&self) -> Status {
        self.machine.lock().unwrap_or_else(std::sync::PoisonError::into_inner).status.clone()
    }
    pub fn resources(&self, bytes: u64) {
        self.machine.lock().unwrap_or_else(std::sync::PoisonError::into_inner).resources = bytes;
    }
    pub fn rate_status(&self) -> ferret_catalog::bulk::RateStatus { self.limiter.status() }
}
fn scaled(bytes: u64, names: u32) -> u64 {
    // Linear at 10M; a floor covers fixed scratch on small catalogs. Ceil so
    // scaling never truncates a positive requirement to zero.
    bytes.saturating_mul(u64::from(names).max(1)).div_ceil(10_000_000)
}
impl Control for Scheduler {
    fn admit(&self, kind: Kind, view: &Catalog) -> Result<(), Blocked> {
        let disk = ferret_crawl::available_disk(&self.index).ok();
        let mut m = self.machine.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let memory = match kind { Kind::FullRewalk => self.config.full_memory, Kind::Checkpoint => self.config.checkpoint_memory };
        let required_memory = scaled(memory, view.name_count()).max(self.config.memory_floor).saturating_add(m.resources);
        let required_disk = scaled(self.config.disk, view.name_count()).max(self.config.disk.min(16 << 20));
        let result = if let Some(reason) = m.status.paused { Err(reason) }
            else if m.status.sample.memory.is_none_or(|n| n < required_memory) { Err(Blocked::Memory) }
            else if disk.is_none_or(|n| n < required_disk) { Err(Blocked::Disk) }
            else { Ok(()) };
        m.status.admission = Some((kind, result));
        m.status.required_memory = required_memory;
        m.status.required_disk = required_disk;
        m.status.available_disk = disk;
        result
    }
    fn limiter(&self) -> Arc<Limiter> { self.limiter.clone() }
}
