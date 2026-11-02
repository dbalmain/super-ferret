//! Watches that could not be installed: kept by key up to a bound, then
//! only counted, per root and per complete-observation epoch.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::*;

/// Failures kept by key for one root, directories and policy inputs together.
/// A scoped refresh that later installs one of these clears it; past the
/// bound, and for a loss with no key, only the next complete observation does.
/// Counted failures are attempts, not distinct watches, so a status count past
/// the bound can repeat a directory.
pub(super) const EXACT_FAILURES: usize = 1024;
/// Watches that could not be installed for one root since its latest complete
/// observation began. The first few are kept by key. The rest are only counted:
/// a failure makes the root polled, and a poll observes the whole root. Over
/// the watch cap that is millions of directories (D116).
#[derive(Debug, Default)]
pub(super) struct Failures {
    pub(super) epoch: u64,
    directories: BTreeSet<(u64, u64)>,
    policies: BTreeSet<PathBuf>,
    counted: usize,
    /// Failures from before `epoch` began. The complete observation that
    /// started it may still fail, so only its success retires them.
    pub(super) earlier: Option<Box<Failures>>,
}
impl Failures {
    pub(super) fn exact(&self) -> usize {
        self.directories.len()
            + self.policies.len()
            + self.earlier.as_ref().map_or(0, |e| e.exact())
    }
    pub(super) fn len(&self) -> usize {
        self.exact() + self.counted + self.earlier.as_ref().map_or(0, |e| e.counted)
    }
    /// The failures to report. A complete observation in progress repeats
    /// the earlier epoch's counted attempts, so the two counts overlap.
    pub(super) fn reported(&self) -> usize {
        self.exact()
            + self
                .counted
                .max(self.earlier.as_ref().map_or(0, |e| e.counted))
    }
    pub(super) fn repair_directory(&mut self, identity: &(u64, u64)) {
        self.directories.remove(identity);
        if let Some(earlier) = &mut self.earlier {
            earlier.directories.remove(identity);
        }
    }
    pub(super) fn repair_policy(&mut self, key: &Path) {
        self.policies.remove(key);
        if let Some(earlier) = &mut self.earlier {
            earlier.policies.remove(key);
        }
    }
    /// Starts `epoch`'s failures, keeping every older one as `earlier`.
    fn advance(&mut self, epoch: u64) {
        let mut now = std::mem::replace(
            self,
            Failures {
                epoch,
                ..Failures::default()
            },
        );
        let mut earlier = now.earlier.take().map_or_else(Failures::default, |e| *e);
        earlier.directories.append(&mut now.directories);
        earlier.policies.append(&mut now.policies);
        earlier.counted += now.counted;
        if earlier.len() > 0 {
            self.earlier = Some(Box::new(earlier));
        }
    }
}
pub(super) enum Failed<K> {
    Directory((u64, u64)),
    /// The input's path, computed only while it can still be kept.
    Policy(K),
    /// Coverage lost with nothing to key it by. Only a complete observation
    /// of the root retires it.
    Lost,
}

/// Records a failed watch. Past the exact bound it keeps nothing new, and it
/// computes a policy key only while one can be kept or an earlier epoch holds
/// policy keys it may repeat.
pub(super) fn fail<K: FnOnce() -> PathBuf>(s: &mut State, root: &Path, failed: Failed<K>) {
    let epoch = s.policy_epochs.get(root).copied().unwrap_or(0);
    if !s.gaps.contains(root) {
        s.gaps.insert(root.to_owned());
    }
    if !s.failures.contains_key(root) {
        s.failures.insert(root.to_owned(), Failures::default());
    }
    let Some(record) = s.failures.get_mut(root) else {
        return;
    };
    if record.epoch != epoch {
        record.advance(epoch);
    }
    // A key this record already holds is held once. Past the bound a policy
    // key is computed only to match an earlier epoch's, so a repeat of one
    // counted here is counted again; that only keeps the root polled.
    match failed {
        Failed::Lost => record.counted += 1,
        Failed::Directory(identity) => {
            let repeated = record
                .earlier
                .as_mut()
                .is_some_and(|e| e.directories.remove(&identity));
            if repeated || record.directories.contains(&identity) || record.exact() < EXACT_FAILURES
            {
                record.directories.insert(identity);
            } else {
                record.counted += 1;
            }
        }
        Failed::Policy(key) => {
            let earlier = record
                .earlier
                .as_ref()
                .is_some_and(|e| !e.policies.is_empty());
            if !earlier && record.exact() >= EXACT_FAILURES {
                record.counted += 1;
                return;
            }
            let key = key();
            let repeated = record
                .earlier
                .as_mut()
                .is_some_and(|e| e.policies.remove(&key));
            if repeated || record.policies.contains(&key) || record.exact() < EXACT_FAILURES {
                record.policies.insert(key);
            } else {
                record.counted += 1;
            }
        }
    }
}
