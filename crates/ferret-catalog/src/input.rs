//! Unpublished crawl input guard, independent of published log budgets.
use std::sync::Mutex;

/// Provisional ceilings for changed observations and reconciliation scratch.
/// Base directory graphs and equal-row seen bits are resident-size storage;
/// changed rows, records, and their owned names/targets are charged here.
#[derive(Clone, Copy, Debug)]
pub struct InputLimits {
    pub records: usize,
    pub owned_bytes: usize,
}
impl Default for InputLimits {
    fn default() -> Self {
        Self {
            records: 500_000,
            owned_bytes: 64 * 1024 * 1024,
        }
    }
}
/// High-water accounting for an unpublished attempt. Refused allocations are
/// excluded; exceeded means the attempt must be discarded without publication.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputUsage {
    pub records: usize,
    pub owned_bytes: usize,
    pub exceeded: bool,
}
/// Shared across workers and the final reconciler. Charging precedes ownership.
/// Counts are conservative: intermediate rows are charged even if later merged.
pub struct InputBudget {
    limits: InputLimits,
    usage: Mutex<InputUsage>,
    exceeded: std::sync::atomic::AtomicBool,
}
impl InputBudget {
    pub fn new(limits: InputLimits) -> Self {
        Self {
            limits,
            usage: Mutex::new(InputUsage::default()),
            exceeded: std::sync::atomic::AtomicBool::new(false),
        }
    }
    pub fn exceeded(&self) -> bool {
        self.exceeded.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn usage(&self) -> InputUsage {
        *self
            .usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub fn charge(&self, records: usize, bytes: usize) -> Result<(), crate::log::Error> {
        let mut usage = self
            .usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if usage.exceeded
            || records > self.limits.records.saturating_sub(usage.records)
            || bytes > self.limits.owned_bytes.saturating_sub(usage.owned_bytes)
        {
            usage.exceeded = true;
            self.exceeded
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return Err(crate::log::Error::InputLimit(*usage));
        }
        usage.records += records;
        usage.owned_bytes += bytes;
        Ok(())
    }
}
