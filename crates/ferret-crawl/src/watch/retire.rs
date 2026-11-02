//! Retiring watches once the catalog has observed their directories: policy
//! inputs, failures and aliases whose root is gone, then each descriptor
//! whose directories no longer resolve.

use super::*;

impl Watch {
    /// Retire watches only after checked catalog observation. Known removals
    /// have an expected IGNORED; every unknown descriptor lifetime is loss.
    pub fn reconcile(&self, view: &Catalog) {
        let roots: BTreeSet<_> = view
            .roots()
            .map(|(_, p)| PathBuf::from(OsStr::from_bytes(p)))
            .collect();
        let protected = view
            .dir_ids()
            .filter(|&id| view.retained_at(id).is_some())
            .filter_map(|id| crate::refresh::containing_root(view, id).ok())
            .collect::<BTreeSet<_>>();
        let snapshot = self.retire_policies(&mut lock(&self.state), view, &roots, &protected);
        // Resolution may traverse a large watch set. Kernel intake must not
        // wait on its catalog/path work or on a sweep's removal syscalls.
        for (wd, directories) in snapshot {
            self.retire_descriptor(view, wd, directories);
        }
    }
    /// Drops policy inputs, failures and aliases whose root is gone or whose
    /// refreshed policy no longer reads them, then rebuilds the gaps. Returns
    /// the descriptors for an unlocked resolution pass.
    fn retire_policies(
        &self,
        s: &mut State,
        view: &Catalog,
        roots: &BTreeSet<PathBuf>,
        protected: &BTreeSet<PathBuf>,
    ) -> Vec<(i32, Vec<Arc<Directory>>)> {
        s.file_aliases.retain(|_, aliases| {
            aliases.names.retain(|_, owners| {
                owners.retain(|r| roots.contains(r));
                !owners.is_empty()
            });
            !aliases.names.is_empty()
        });
        let refreshed = std::mem::take(&mut s.policy_refreshed);
        let epochs = s.policy_epochs.clone();
        let keep = |r: &Path, epoch: u64| {
            roots.contains(r)
                && (!refreshed.contains(r)
                    || protected.contains(r)
                    || epochs.get(r).is_some_and(|current| *current == epoch))
        };
        s.policy_epochs.retain(|r, _| roots.contains(&**r));
        s.failures.retain(|r, f| {
            if refreshed.contains(r) && !protected.contains(r) {
                f.earlier = None;
            }
            f.len() > 0 && keep(r, f.epoch)
        });
        let mut retired = Vec::new();
        for (&wd, inputs) in &mut s.policies {
            inputs.retain(|i| keep(&i.root, i.epoch));
            if inputs.is_empty() {
                retired.push(wd);
            }
        }
        for wd in retired {
            s.policies.remove(&wd);
            if !s.descriptors.contains_key(&wd) {
                s.identities.retain(|_, value| *value != wd);
                if inotify::remove_watch(&self.fd, wd).is_ok() {
                    s.removed.insert(wd);
                }
            }
        }
        s.unreliable
            .retain(|root| view.roots().any(|(_, p)| p == root.as_os_str().as_bytes()));
        // Failures are per root, so a root leaves the gaps once its own
        // record is gone, whatever another root still holds.
        s.gaps = s.unreliable.clone();
        let failed = s.failures.keys().cloned().collect::<Vec<_>>();
        s.gaps.extend(failed);
        s.descriptors
            .iter()
            .map(|(&wd, d)| (wd, d.clone()))
            .collect::<Vec<_>>()
    }
    /// Keeps the directories of watch `wd` that still resolve in `view`,
    /// removing the watch when none do, unless it changed since `directories`
    /// was taken.
    fn retire_descriptor(&self, view: &Catalog, wd: i32, directories: Vec<Arc<Directory>>) {
        let kept = directories
            .iter()
            .filter(|d| d.resolve(view).is_some())
            .cloned()
            .collect::<Vec<_>>();
        let mut s = lock(&self.state);
        if s.descriptors.get(&wd).is_none_or(|ds| {
            ds.len() != directories.len()
                || ds.iter().zip(&directories).any(|(a, b)| !Arc::ptr_eq(a, b))
        }) {
            return;
        }
        if !kept.is_empty() {
            s.descriptors.insert(wd, kept);
            return;
        }
        s.descriptors.remove(&wd);
        if !s.policies.contains_key(&wd) {
            s.identities.retain(|_, value| *value != wd);
        }
        if !s.policies.contains_key(&wd) && inotify::remove_watch(&self.fd, wd).is_ok() {
            s.removed.insert(wd);
        }
    }
}
