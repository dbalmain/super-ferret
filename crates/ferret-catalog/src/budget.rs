//! Published log budgets. Distinct row sets describe the final effective state:
//! a deleted base row is dead, rather than both dirty and dead.
use std::collections::BTreeSet;

use crate::OpenError;
use crate::log::{ChangeSet, Family, Published, Record};

/// Limits which request an idle-boundary checkpoint. Fractions use the live
/// base counts, never allocation high-water marks. Readers impose no such caps.
#[derive(Clone, Copy, Debug)]
pub struct CompactionLimits {
    pub log_bytes: u64,
    pub records: u64,
    pub dirty_percent: u32,
    pub dead_percent: u32,
}
impl Default for CompactionLimits {
    fn default() -> Self {
        Self {
            log_bytes: 64 * 1024 * 1024,
            records: 500_000,
            dirty_percent: 1,
            dead_percent: 5,
        }
    }
}

/// Current usage, also available to `ferret stats` without taking a writer
/// lock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BudgetUsage {
    pub log_bytes: u64,
    pub records: u64,
    pub transactions: u64,
    pub base_inodes: u32,
    pub base_names: u32,
    pub dirty_inodes: u32,
    pub dirty_names: u32,
    pub dead_inodes: u32,
    pub dead_names: u32,
}
impl BudgetUsage {
    pub fn reached(self, limits: CompactionLimits) -> bool {
        let fraction = |n: u32, base: u32, percent: u32| {
            n > 0 && u64::from(n) * 100 >= u64::from(base) * u64::from(percent)
        };
        self.log_bytes >= limits.log_bytes
            || self.records >= limits.records
            || fraction(self.dirty_inodes, self.base_inodes, limits.dirty_percent)
            || fraction(self.dirty_names, self.base_names, limits.dirty_percent)
            || fraction(self.dead_inodes, self.base_inodes, limits.dead_percent)
            || fraction(self.dead_names, self.base_names, limits.dead_percent)
    }
}

#[derive(Clone)]
pub(crate) struct Budget {
    pub(crate) usage: BudgetUsage,
    dirty_inodes: BTreeSet<u32>,
    dirty_names: BTreeSet<u32>,
    dead_inodes: BTreeSet<u32>,
    dead_names: BTreeSet<u32>,
}
impl Budget {
    pub(crate) fn empty(inodes: u32, names: u32) -> Self {
        Self {
            usage: BudgetUsage {
                log_bytes: crate::log::HEADER,
                base_inodes: inodes,
                base_names: names,
                ..BudgetUsage::default()
            },
            dirty_inodes: BTreeSet::new(),
            dirty_names: BTreeSet::new(),
            dead_inodes: BTreeSet::new(),
            dead_names: BTreeSet::new(),
        }
    }
    pub(crate) fn open(published: &Published) -> Result<Self, OpenError> {
        published.log().load(Family::Namespace)?;
        published.log().load(Family::Inodes)?;
        let base = published.checkpoint();
        let mut budget = Self::empty(base.inode_count(), base.name_count());
        // Last replacement wins across transactions. Families have disjoint
        // row kinds, so their independent replay order is sufficient here.
        for family in [Family::Namespace, Family::Inodes] {
            for (_, record) in published.log().records(family) {
                budget.apply(record);
            }
        }
        budget.usage.records = published.log().record_count();
        budget.usage.log_bytes = published.log().committed_end();
        budget.usage.transactions = published.log().transaction_count() as u64;
        budget.update_counts();
        Ok(budget)
    }
    fn apply(&mut self, record: &Record) {
        match *record {
            Record::NamePut { id, .. } => {
                self.dirty_names.insert(id);
                self.dead_names.remove(&id);
            }
            Record::NameDelete { id } => {
                self.dirty_names.remove(&id);
                self.dead_names.insert(id);
            }
            Record::LifePut { id, .. }
            | Record::DirPut { id, .. }
            | Record::InodePut { id, .. }
                if !self.dead_inodes.contains(&id) =>
            {
                // The namespace lifecycle tombstone owns inode liveness;
                // replaying an older stat replacement must not revive it.
                self.dirty_inodes.insert(id);
            }
            Record::InodeDelete { id } => {
                self.dirty_inodes.remove(&id);
                self.dead_inodes.insert(id);
            }
            _ => {}
        }
    }
    fn update_counts(&mut self) {
        self.usage.dirty_inodes = self.dirty_inodes.len() as u32;
        self.usage.dirty_names = self.dirty_names.len() as u32;
        self.usage.dead_inodes = self.dead_inodes.range(..self.usage.base_inodes).count() as u32;
        self.usage.dead_names = self.dead_names.range(..self.usage.base_names).count() as u32;
    }
    pub(crate) fn project(&self, changes: &ChangeSet, bytes: u64) -> Self {
        let mut next = self.clone();
        for record in &changes.records {
            next.apply(record);
        }
        next.usage.log_bytes += bytes;
        next.usage.records += changes.records.len() as u64;
        next.usage.transactions += u64::from(!changes.records.is_empty());
        next.update_counts();
        next
    }
}
impl Published {
    pub fn budget_usage(&self) -> Result<BudgetUsage, OpenError> {
        Ok(Budget::open(self)?.usage)
    }
}
