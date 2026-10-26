//! Watches registered at policy reads. Parent watches also catch atomic saves
//! and creation of absent inputs; followed symlinks register their target too.
use std::os::fd::AsFd;

use super::*;

/// Linux's `MAXSYMLINKS`: past this many links, path resolution fails with
/// `ELOOP`. Following a policy input's link chain for ancestor watches stops
/// here, so a self-referencing `.ferretignore` cannot recurse without end.
const MAX_HOPS: u32 = 40;

pub(super) fn unreliable(magic: u64) -> bool {
    // NFS, CIFS/SMB/SMB2, 9P and FUSE: remote or userspace writes need not
    // generate notifications on this client's watched inode.
    matches!(
        magic,
        0x6969 | 0xff534d42 | 0x517b | 0xfe534d42 | 0x01021997 | 0x65735546
    )
}
/// Opens a parent as a directory, never blocking: a FIFO at the pathname
/// fails `ENOTDIR` instead of waiting for a writer. Symlinks are followed,
/// since policy paths may legitimately pass through them.
fn open_parent(parent: &Path) -> Result<OwnedFd, Errno> {
    rustix::fs::open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

/// The crawl consults a few names in every directory; those cost no
/// allocation. A followed symlink's ancestors can have any name.
const NAMES: [&[u8]; 8] = [
    b".ferretignore",
    b".git",
    b".gitignore",
    b".ignore",
    b".rgignore",
    b"info",
    b"exclude",
    b"commondir",
];
#[derive(Debug)]
pub(super) enum Name {
    Static(&'static [u8]),
    Owned(Box<[u8]>),
}
impl Name {
    fn new(name: &[u8]) -> Self {
        NAMES
            .iter()
            .find(|n| **n == name)
            .map_or_else(|| Self::Owned(name.into()), |n| Self::Static(n))
    }
    pub(super) fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Static(name) => name,
            Self::Owned(name) => name,
        }
    }
    pub(super) fn owned_len(&self) -> usize {
        match self {
            Self::Static(_) => 0,
            Self::Owned(name) => name.len(),
        }
    }
}
/// One policy read registered at a watch. A watched directory typically holds
/// two or three, all for one root, so the root is shared rather than copied.
#[derive(Debug)]
pub(super) struct Input {
    pub(super) root: Arc<Path>,
    pub(super) name: Name,
    pub(super) epoch: u64,
}
fn failure(s: &mut State, root: &Path, key: impl FnOnce() -> PathBuf) {
    fail(s, root, Failed::Policy(key));
}
/// The path a policy input is recorded under when its watch fails.
fn input_key(proc: &Path, name: &OsStr) -> PathBuf {
    std::fs::read_link(proc)
        .unwrap_or_else(|_| proc.to_owned())
        .join(name)
}
/// Canonicalizes `path`'s longest existing prefix and appends the missing
/// rest, so a dangling link's target is watched where it would appear. Only a
/// missing component is tolerated: a loop or a denial is still an error.
fn canonical_or_missing(path: &Path) -> std::io::Result<PathBuf> {
    let mut missing = Vec::new();
    let mut prefix = path;
    loop {
        match std::fs::canonicalize(prefix) {
            Ok(base) => return Ok(missing.iter().rev().fold(base, |p, name| p.join(name))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let (Some(parent), Some(name)) = (prefix.parent(), prefix.file_name()) else {
                    return Err(error);
                };
                missing.push(name);
                prefix = parent;
            }
            Err(error) => return Err(error),
        }
    }
}
/// The shared root and its current epoch. The root is interned on first use.
fn root_epoch(s: &mut State, root: &Path) -> (Arc<Path>, u64) {
    if let Some((root, &epoch)) = s.policy_epochs.get_key_value(root) {
        return (root.clone(), epoch);
    }
    let shared: Arc<Path> = Arc::from(root);
    s.policy_epochs.insert(shared.clone(), 0);
    (shared, 0)
}
impl Watch {
    /// Starts dependency retirement only for a complete root observation.
    pub(crate) fn begin_policy_root(&self, root: &Path) {
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (_, epoch) = root_epoch(&mut s, root);
        if let Some(current) = s.policy_epochs.get_mut(root) {
            *current = epoch.wrapping_add(1);
        }
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
        self.path_input(root, path, 0);
    }
    /// `hops` counts the symlinks followed to reach `path`; see [`MAX_HOPS`].
    fn path_input(&self, root: &Path, path: &Path, hops: u32) {
        let mut candidate = path.to_owned();
        loop {
            let Some(parent) = candidate.parent() else {
                self.gap(root);
                return;
            };
            match open_parent(parent) {
                Ok(fd) => {
                    if let Some(name) = candidate.file_name() {
                        self.input(root, fd.as_fd(), name, hops);
                        self.ancestors(root, path, hops);
                    }
                    return;
                }
                // A parent that is not a directory (a FIFO, a file) cannot
                // hold the input either; watching where it would appear
                // notices when a directory replaces it.
                Err(Errno::NOENT | Errno::NOTDIR) => {
                    candidate = parent.to_owned();
                }
                Err(_) => {
                    failure(
                        &mut self
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner),
                        root,
                        || {
                            std::fs::canonicalize(parent)
                                .map(|p| p.join(candidate.file_name().unwrap_or_default()))
                                .unwrap_or_else(|_| path.to_owned())
                        },
                    );
                    return;
                }
            }
        }
    }
    /// Changing an ancestor symlink changes the policy namespace even when
    /// the followed file and its watched physical parent do not move. `hops`
    /// counts the symlinks followed to reach `path`; see [`MAX_HOPS`].
    fn ancestors(&self, root: &Path, path: &Path, hops: u32) {
        let mut prefix = PathBuf::new();
        for component in path.components() {
            prefix.push(component);
            // Proc-fd paths already anchor held handles. They are not mutable
            // namespace dependencies and must not install process watches.
            if prefix.starts_with("/proc") {
                return;
            }
            if !std::fs::symlink_metadata(&prefix).is_ok_and(|m| m.file_type().is_symlink()) {
                continue;
            }
            if let Some(parent) = prefix.parent() {
                match open_parent(parent) {
                    Ok(fd) => self.input(
                        root,
                        fd.as_fd(),
                        prefix.file_name().unwrap_or_default(),
                        hops,
                    ),
                    Err(_) => failure(
                        &mut self
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner),
                        root,
                        || prefix.clone(),
                    ),
                }
            }
        }
    }
    pub(crate) fn policy_input(&self, root: &Path, parent: BorrowedFd<'_>, name: &OsStr) {
        self.inputs(root, parent, &[name], 0);
    }
    /// [`Watch::policy_input`] for several names in one parent: the parent's
    /// identity and filesystem are read once for all of them, where a
    /// directory's ignore inputs cost an `fstat` and `fstatfs` each.
    pub(crate) fn policy_inputs(&self, root: &Path, parent: BorrowedFd<'_>, names: &[&OsStr]) {
        self.inputs(root, parent, names, 0);
    }
    fn input(&self, root: &Path, parent: BorrowedFd<'_>, name: &OsStr, hops: u32) {
        self.inputs(root, parent, &[name], hops);
    }
    fn inputs(&self, root: &Path, parent: BorrowedFd<'_>, names: &[&OsStr], hops: u32) {
        let proc = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
        let Ok(stat) = fstat(parent) else {
            for _ in names {
                self.gap(root);
            }
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
        for name in names {
            self.input_of(root, &proc, identity, name, hops);
        }
    }
    /// One input of a parent whose `identity` was just read through `proc`.
    fn input_of(&self, root: &Path, proc: &Path, identity: (u64, u64), name: &OsStr, hops: u32) {
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
            failure(&mut s, root, || input_key(&proc, name));
            return;
        }
        let result = s
            .identities
            .get(&identity)
            .copied()
            .map_or_else(|| inotify::add_watch(&self.fd, proc, mask), Ok);
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
                let (shared, epoch) = root_epoch(&mut s, root);
                let inputs = s
                    .policies
                    .entry(wd)
                    .or_insert_with(|| Vec::with_capacity(3));
                match inputs
                    .iter_mut()
                    .find(|i| *i.root == *root && i.name.as_bytes() == name.as_bytes())
                {
                    Some(input) => input.epoch = epoch,
                    None => inputs.push(Input {
                        root: shared,
                        name: Name::new(name.as_bytes()),
                        epoch,
                    }),
                }
                if let Some(record) = s.failures.get_mut(root)
                    && record.exact() > 0
                {
                    record.repair_policy(&input_key(&proc, name));
                }
            }
            Ok(wd) => {
                let _ = inotify::remove_watch(&self.fd, wd);
                s.removed.insert(wd);
                failure(&mut s, root, || input_key(&proc, name));
            }
            Err(_) => failure(&mut s, root, || input_key(&proc, name)),
        }
        drop(s);
        if !matches!(name.as_bytes(), b".git" | b".gitignore" | b"commondir")
            && std::fs::symlink_metadata(proc.join(name)).is_ok_and(|m| m.file_type().is_symlink())
        {
            // Only a followed symlink needs the input's own path.
            let key = input_key(&proc, name);
            match std::fs::read_link(proc.join(name)) {
                // A link chain longer than the kernel follows, a cycle
                // included, cannot resolve: the input is a failure, as when
                // canonicalizing it fails, not a deeper walk.
                Ok(_) if hops >= MAX_HOPS => failure(
                    &mut self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                    root,
                    || key,
                ),
                Ok(target) => {
                    let target = if target.is_absolute() {
                        target
                    } else {
                        key.parent().unwrap_or(Path::new("/")).join(target)
                    };
                    // Keep the link spelling for ancestor watches;
                    // canonicalizing first would lose a
                    // symlinked parent that can be retargeted.
                    self.ancestors(root, &target, hops + 1);
                    // A dangling link is an absent input, not a failure: its
                    // target can be missing for a moment while a link on the
                    // way is replaced, and watching where it would appear sees
                    // it come back.
                    match canonical_or_missing(&target) {
                        Ok(target) if target != key => self.path_input(root, &target, hops + 1),
                        _ => failure(
                            &mut self
                                .state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                            root,
                            || key,
                        ),
                    }
                }
                _ => failure(
                    &mut self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                    root,
                    || key,
                ),
            }
        }
    }
}
