//! One controller shared by the writer's job gate and catalog bulk allocation
//! admission. Signal sampling and the monotonic ratchet use injectable traits;
//! admitted publication is never cancelled by a subsequent pressure spike.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferret_catalog::Catalog;
use ferret_catalog::bulk::{Blocked, Clock, Control, Kind, Limiter};

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
    status: Status,
    calm_since: Option<Duration>,
    watch: Option<Arc<ferret_crawl::watch::Watch>>,
}
#[derive(Debug)]
pub(crate) struct Scheduler {
    source: Mutex<Box<dyn Signals>>,
    config: Config,
    cpus: usize,
    index: PathBuf,
    clock: Arc<dyn Clock>,
    limiter: Arc<Limiter>,
    machine: Mutex<Machine>,
}
impl Scheduler {
    pub fn new(
        config: Config,
        cpus: usize,
        index: PathBuf,
        mut source: Box<dyn Signals>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let sample = source.sample();
        let limiter = Arc::new(Limiter::new(config.rate, clock.clone()));
        let scheduler = Self {
            source: Mutex::new(source),
            config,
            cpus: cpus.max(1),
            index,
            clock,
            limiter,
            machine: Mutex::new(Machine {
                status: Status {
                    sample: sample.clone(),
                    workers: 1,
                    paused: None,
                    admission: None,
                    required_memory: 0,
                    required_disk: 0,
                    available_disk: None,
                },
                calm_since: None,
                watch: None,
            }),
        };
        scheduler.update(sample);
        scheduler
    }
    pub fn sample(&self) {
        // Proc/sys reads and an optional compositor probe never hold the state
        // mutex needed by query status or by a writer admission decision.
        let sample = self
            .source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sample();
        self.update(sample);
    }
    fn update(&self, sample: Sample) {
        let mut m = self
            .machine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let paused =
            (self.config.battery_pause && sample.battery == Some(true)).then_some(Blocked::Battery);
        let desired = if paused.is_some() || sample.cpu.is_none_or(|cpu| cpu > 20.0) {
            1
        } else {
            match sample.idle {
                Idle::Unknown => 1,
                Idle::Desktop(idle) if idle <= Duration::from_secs(30) => 1,
                Idle::Desktop(idle) if idle <= Duration::from_secs(300) => (self.cpus / 4).max(1),
                Idle::Headless | Idle::Desktop(_) => (self.cpus / 2).max(1),
            }
        }
        .min(self.config.concurrency.max(1));
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
    pub fn now(&self) -> Duration {
        self.clock.now()
    }
    pub fn status(&self) -> Status {
        self.machine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status
            .clone()
    }
    pub fn watch(&self, watch: Option<Arc<ferret_crawl::watch::Watch>>) {
        self.machine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .watch = watch;
    }
    pub fn rate_status(&self) -> ferret_catalog::bulk::RateStatus {
        self.limiter.status()
    }
    pub fn command_control(self: &Arc<Self>) -> Arc<dyn Control> {
        Arc::new(CommandControl(self.clone()))
    }
    pub fn configured_workers(&self) -> usize {
        self.config.concurrency.max(1)
    }
    fn admit_command(&self, kind: Kind, view: &Catalog) -> Result<(), Blocked> {
        self.admit_policy(kind, view, false)
    }
}
fn scaled(bytes: u64, names: u32) -> u64 {
    // Linear at 10M; a floor covers fixed scratch on small catalogs. Ceil so
    // scaling never truncates a positive requirement to zero.
    (u128::from(bytes) * u128::from(names).max(1))
        .div_ceil(10_000_000)
        .min(u128::from(u64::MAX)) as u64
}
impl Control for Scheduler {
    fn admit(&self, kind: Kind, view: &Catalog) -> Result<(), Blocked> {
        self.admit_policy(kind, view, true)
    }
    fn limiter(&self) -> Arc<Limiter> {
        self.limiter.clone()
    }
    fn workers(&self) -> usize {
        self.status().workers
    }
}
impl Scheduler {
    fn admit_policy(&self, kind: Kind, view: &Catalog, polite: bool) -> Result<(), Blocked> {
        let disk = ferret_crawl::available_disk(&self.index).ok();
        let watch = self
            .machine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .watch
            .clone();
        // Charge sparse state only at bulk boundaries; small bursts never scan
        // the complete descriptor/dependency map just to estimate its heap.
        let watch_bytes = watch.as_ref().map_or(0, |w| w.resource_bytes());
        let mut m = self
            .machine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let memory = match kind {
            Kind::FullRewalk => self.config.full_memory,
            Kind::Checkpoint => self.config.checkpoint_memory,
        };
        let required_memory = scaled(memory, view.name_count())
            .max(self.config.memory_floor)
            .saturating_add(self.config.additional_memory)
            .saturating_add(watch_bytes);
        let required_disk =
            scaled(self.config.disk, view.name_count()).max(self.config.disk.min(16 << 20));
        let result = if polite && let Some(reason) = m.status.paused {
            Err(reason)
        } else if m.status.sample.memory.is_none_or(|n| n < required_memory) {
            Err(Blocked::Memory)
        } else if disk.is_none_or(|n| n < required_disk) {
            Err(Blocked::Disk)
        } else {
            Ok(())
        };
        m.status.admission = Some((kind, result));
        m.status.required_memory = required_memory;
        m.status.required_disk = required_disk;
        m.status.available_disk = disk;
        result
    }
}
#[derive(Debug)]
struct CommandControl(Arc<Scheduler>);
impl Control for CommandControl {
    fn admit(&self, kind: Kind, view: &Catalog) -> Result<(), Blocked> {
        self.0.admit_command(kind, view)
    }
    fn limiter(&self) -> Arc<Limiter> {
        self.0.limiter()
    }
    fn paced(&self) -> bool {
        false
    }
    fn workers(&self) -> usize {
        self.0.configured_workers()
    }
}

#[cfg(test)]
mod tests;
