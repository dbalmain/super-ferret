//! Arming a watch on a directory the crawl observed, through its own handle
//! before it is listed, and recording it under its parent.

use rustix::fs::openat;

use super::*;

impl Watch {
    /// Arms the observed handle before enumeration. A failure is coverage loss,
    /// never a crawl deletion. Parents are armed before their child is listed.
    pub(crate) fn arm(&self, root: &Path, relative: &Path, fd: BorrowedFd<'_>) {
        let Ok(stat) = fstat(fd) else {
            self.gap(root);
            return;
        };
        self.arm_at(root, relative, fd, (stat.st_dev, stat.st_ino));
    }
    /// [`Watch::arm`] for a handle whose `(dev, ino)` its caller has just read.
    fn arm_at(&self, root: &Path, relative: &Path, fd: BorrowedFd<'_>, identity: (u64, u64)) {
        if relative.as_os_str().is_empty() {
            self.policy_path(root, root);
        }
        if rustix::fs::fstatfs(fd).map_or(true, |s| policy::unreliable(s.f_type as u64)) {
            lock(&self.state).unreliable.insert(root.to_owned());
        }
        let parent_identity = if relative.as_os_str().is_empty() {
            None
        } else {
            Some(parent_identity(fd))
        };
        let mut state = lock(&self.state);
        let path = root.join(relative);
        let parent = parent_identity.and_then(|identity| {
            parent_directory(&state, identity.unwrap_or_default(), &path, root)
        });
        if !relative.as_os_str().is_empty() && parent.is_none() {
            fail(
                &mut state,
                root,
                Failed::<fn() -> PathBuf>::Directory(identity),
            );
            return;
        }
        let basename = relative.file_name().map_or(&b""[..], OsStr::as_bytes);
        if watched(&state, identity, &path) {
            return;
        }
        if state.identities.len() >= self.config.watch_cap
            && !state.identities.contains_key(&identity)
        {
            fail(
                &mut state,
                root,
                Failed::<fn() -> PathBuf>::Directory(identity),
            );
            return;
        }
        // /proc/self/fd follows the already observed handle, including after an
        // ancestor rename. ONLYDIR rejects a non-directory without a path race.
        let path = format!("/proc/self/fd/{}", fd.as_raw_fd());
        // UNMOUNT and IGNORED are automatically reported by inotify; only
        // subscriptions with user-selectable mask bits belong here.
        let mask = WatchFlags::CREATE
            | WatchFlags::DELETE
            | WatchFlags::MOVED_FROM
            | WatchFlags::MOVED_TO
            | WatchFlags::ATTRIB
            | WatchFlags::MODIFY
            | WatchFlags::CLOSE_WRITE
            | WatchFlags::MOVE_SELF
            | WatchFlags::DELETE_SELF
            | WatchFlags::ONLYDIR
            | WatchFlags::MASK_ADD;
        match inotify::add_watch(&self.fd, path.as_str(), mask) {
            Ok(wd) => add_directory(&mut state, wd, identity, parent, basename, root),
            Err(_) => fail(
                &mut state,
                root,
                Failed::<fn() -> PathBuf>::Directory(identity),
            ),
        }
    }
    /// Attempt a watch even when the later readable directory open will be
    /// denied. O_PATH and the observed parent avoid rebuilding a live pathname.
    pub(crate) fn arm_entry(
        &self,
        root: &Path,
        entry: &crate::Decided<'_, ferret_catalog::DirToken>,
    ) {
        let Some(stat) = entry.stat else {
            return;
        };
        match openat(
            entry.parent_fd,
            entry.name,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => {
                if fstat(&fd).is_ok_and(|s| (s.st_dev, s.st_ino) == (stat.dev, stat.ino)) {
                    let identity = (stat.dev, stat.ino);
                    self.arm_at(root, entry.path, std::os::fd::AsFd::as_fd(&fd), identity);
                } else {
                    self.gap(root);
                    self.backstop(RefreshReason::Overflow);
                }
            }
            Err(_) => {
                self.gap(root);
            }
        }
    }
}

/// The `(dev, ino)` of `fd`'s parent, if it can be read.
fn parent_identity(fd: BorrowedFd<'_>) -> Option<(u64, u64)> {
    openat(
        fd,
        "..",
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .and_then(fstat)
    .ok()
    .map(|s| (s.st_dev, s.st_ino))
}

/// The watched directory under `root` with `identity` that is `path`'s
/// parent.
fn parent_directory(
    s: &State,
    identity: (u64, u64),
    path: &Path,
    root: &Path,
) -> Option<Arc<Directory>> {
    s.identities
        .get(&identity)
        .and_then(|wd| s.descriptors.get(wd))
        .and_then(|ds| {
            ds.iter()
                .find(|d| Some(d.path().as_path()) == path.parent() && d.root.as_path() == root)
        })
        .cloned()
}

/// Records watch `wd` on the directory with `identity`, named `basename`
/// in `parent`, or a root when it has none.
fn add_directory(
    s: &mut State,
    wd: i32,
    identity: (u64, u64),
    parent: Option<Arc<Directory>>,
    basename: &[u8],
    root: &Path,
) {
    if s.removed.remove(&wd) {
        loss(s, RefreshReason::Overflow);
    }
    if s.descriptors
        .get(&wd)
        .is_some_and(|ds| ds.iter().any(|d| d.identity != identity))
    {
        s.descriptors.remove(&wd);
        s.identities.retain(|_, value| *value != wd);
        loss(s, RefreshReason::Overflow);
    }
    let shared = parent
        .as_ref()
        .map_or_else(|| Arc::new(root.to_owned()), |p| p.root.clone());
    s.descriptors
        .entry(wd)
        .or_default()
        .push(Arc::new(Directory {
            identity,
            parent,
            basename: basename.to_vec(),
            root: shared,
        }));
    s.identities.insert(identity, wd);
    if let Some(record) = s.failures.get_mut(root) {
        record.repair_directory(&identity);
    }
}
