//! Watches registered at policy reads. Parent watches also catch atomic saves
//! and creation of absent inputs; followed symlinks register their target too.
use std::os::fd::AsFd;

use super::*;

pub(super) fn unreliable(magic: u64) -> bool {
    // NFS, CIFS/SMB/SMB2, 9P and FUSE: remote or userspace writes need not
    // generate notifications on this client's watched inode.
    matches!(
        magic,
        0x6969 | 0xff534d42 | 0x517b | 0xfe534d42 | 0x01021997 | 0x65735546
    )
}
fn failure(s: &mut State, root: &Path, path: PathBuf) {
    let epoch = s.policy_epochs.get(root).copied().unwrap_or(0);
    s.gaps.insert(root.to_owned());
    s.policy_failed.insert((root.to_owned(), path), epoch);
}
impl Watch {
    /// Starts dependency retirement only for a complete root observation.
    pub(crate) fn begin_policy_root(&self, root: &Path) {
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let epoch = s.policy_epochs.entry(root.to_owned()).or_default();
        *epoch = epoch.wrapping_add(1);
        s.policy_refreshed.insert(root.to_owned());
        s.unreliable.remove(root);
    }
    /// Failed commands cannot later retire dependencies through a partial scan.
    pub fn abort_policy_roots(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .policy_refreshed
            .clear();
    }

    /// Registers one actual policy consultation, including a missing input.
    /// The root is refreshed through the production policy/retention seam.
    pub fn policy_path(&self, root: &Path, path: &Path) {
        let mut candidate = path.to_owned();
        loop {
            let Some(parent) = candidate.parent() else {
                self.gap(root);
                return;
            };
            match std::fs::File::open(parent) {
                Ok(fd) => {
                    if let Some(name) = candidate.file_name() {
                        self.policy_input(root, fd.as_fd(), name);
                    }
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    candidate = parent.to_owned();
                }
                Err(_) => {
                    let mut s = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let key = std::fs::canonicalize(parent)
                        .map(|p| p.join(candidate.file_name().unwrap_or_default()))
                        .unwrap_or_else(|_| path.to_owned());
                    failure(&mut s, root, key);
                    return;
                }
            }
        }
    }
    pub(crate) fn policy_input(&self, root: &Path, parent: BorrowedFd<'_>, name: &OsStr) {
        let proc = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
        let key = std::fs::read_link(&proc)
            .unwrap_or_else(|_| proc.clone())
            .join(name);
        let Ok(stat) = fstat(parent) else {
            self.gap(root);
            return;
        };
        let identity = (stat.st_dev, stat.st_ino);
        if rustix::fs::fstatfs(parent).map_or(true, |s| unreliable(s.f_type as u64)) {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .unreliable
                .insert(root.to_owned());
        }
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mask = WatchFlags::CREATE
            | WatchFlags::DELETE
            | WatchFlags::MOVED_FROM
            | WatchFlags::MOVED_TO
            | WatchFlags::ATTRIB
            | WatchFlags::MODIFY
            | WatchFlags::CLOSE_WRITE
            | WatchFlags::DELETE_SELF
            | WatchFlags::MOVE_SELF
            | WatchFlags::ONLYDIR
            | WatchFlags::MASK_ADD;
        let installed = s.identities.len();
        if installed >= self.config.watch_cap && !s.identities.contains_key(&identity) {
            failure(&mut s, root, key);
            return;
        }
        let result = s
            .identities
            .get(&identity)
            .copied()
            .map_or_else(|| inotify::add_watch(&self.fd, &proc, mask), Ok);
        match result {
            Ok(wd)
                if installed < self.config.watch_cap
                    || s.policies.contains_key(&wd)
                    || s.descriptors.contains_key(&wd) =>
            {
                if s.removed.remove(&wd) {
                    loss(&mut s, RefreshReason::Overflow);
                }
                s.identities.insert(identity, wd);
                let epoch = s.policy_epochs.get(root).copied().unwrap_or(0);
                s.policies
                    .entry(wd)
                    .or_default()
                    .insert((root.to_owned(), name.as_bytes().to_vec()), epoch);
                s.policy_failed.remove(&(root.to_owned(), key.clone()));
            }
            Ok(wd) => {
                let _ = inotify::remove_watch(&self.fd, wd);
                s.removed.insert(wd);
                failure(&mut s, root, key.clone());
            }
            Err(_) => {
                failure(&mut s, root, key.clone());
            }
        }
        drop(s);
        if !matches!(name.as_bytes(), b".git" | b".gitignore" | b"commondir")
            && std::fs::symlink_metadata(proc.join(name)).is_ok_and(|m| m.file_type().is_symlink())
        {
            match std::fs::canonicalize(proc.join(name)) {
                Ok(target) if target != key => self.policy_path(root, &target),
                _ => {
                    let mut s = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    failure(&mut s, root, key);
                }
            }
        }
    }
}
