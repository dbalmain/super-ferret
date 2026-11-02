//! Directory descriptors a reader holds between paths, so a path that
//! shares its parents with the last one reopens only what differs; and the
//! process-wide budget that bounds them.

use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, OnceLock, PoisonError, TryLockError, Weak};
use std::time::Duration;

use ferret_catalog::InoId;
use rustix::fs::open;

use super::*;
use crate::lock;

impl Documents {
    /// Keeps each checked directory walk's descriptors for later paths
    /// through the same directories: each path still checks every directory
    /// on it, by a stat of its path, instead of opening it again
    /// ([`hold_directory`]). For a reader that lives for one query or one
    /// follow pass: it holds a descriptor per level of the last path, as far
    /// as the process's budget for held descriptors allows ([`Budget`]), and
    /// opens the rest of a path without holding them. A directory's read
    /// permission is checked when its descriptor is opened, so one that loses
    /// it while held still serves later paths through it; the next crawl
    /// finds it unlistable and retires its children.
    pub fn holding_directories(self) -> Self {
        self.holding_within(Budget::process())
    }

    /// [`Documents::holding_directories`] within `budget`.
    fn holding_within(mut self, budget: &'static Budget) -> Self {
        self.held = Some(Held::new(budget));
        self
    }

    /// [`Documents::holding_directories`] within a budget of its own, shared
    /// with its forks, of `fds` descriptors.
    #[cfg(test)]
    pub(crate) fn holding_at_most(self, fds: usize) -> Self {
        self.holding_within(Box::leak(Box::new(Budget::new(fds))))
    }

    /// Descriptors this reader holds, and those its budget has left.
    #[cfg(test)]
    pub(crate) fn held(&self) -> (usize, usize) {
        self.held.as_ref().map_or((0, 0), |held| {
            let held = lock(held);
            (held.fds.len(), held.budget.left.load(Ordering::Relaxed))
        })
    }

    /// Calls `pause` each time this reader has locked its held walk to open
    /// through it, before anything else.
    #[cfg(test)]
    pub(crate) fn pausing(mut self, pause: impl Fn() + Send + Sync + 'static) -> Self {
        self.pause = Some(Arc::new(pause));
        self
    }

    /// Whether the process's readers have run out of descriptors.
    #[cfg(test)]
    pub(crate) fn pressured() -> bool {
        Budget::process().pressured.load(Ordering::Acquire)
    }

    /// `open` given a checked descriptor for `dir`, re-walked from the
    /// root for each path. A retained descriptor alone cannot establish that
    /// its original path still names it after a rename: a reader [holding
    /// directories](Documents::holding_directories) checks each path again
    /// before reusing one. The callback may read through the directory;
    /// pressure relief waits for that read as it does for an open.
    ///
    /// An open that finds the process out of descriptors (`EMFILE`,
    /// `ENFILE`) is tried again once every held descriptor in the process
    /// is closed ([`Budget::relieve`]), waiting as long as a reader busy
    /// opening takes to close its own, unless `cancelled` says to stop
    /// first. So a reader fails for want of
    /// descriptors only where an unheld walk on every thread would: whether
    /// a path reads does not depend on how many readers hold directories.
    pub(super) fn in_directory<T>(
        &mut self,
        catalog: &Catalog,
        dir: InoId,
        cancelled: &dyn Fn() -> bool,
        mut open: impl FnMut(BorrowedFd<'_>) -> Result<T, ContentFault>,
    ) -> Result<T, ContentFault> {
        let mut last = false;
        loop {
            let opened = self.once_in_directory(catalog, dir, &mut open);
            let budget = self.held.as_ref().map(|held| lock(held).budget);
            let budget = budget.or_else(|| PROCESS.get());
            let Some(budget) = budget.filter(|_| out_of_descriptors(&opened)) else {
                return opened;
            };
            // With nothing held anywhere, this is as far as an unheld walk
            // on every thread gets.
            if last {
                return opened;
            }
            // Readers busy opening close theirs as they finish, however
            // long that takes: a slow open elsewhere must not make this
            // file unreadable.
            budget.relieve();
            if !budget.wait_unheld(cancelled) {
                return Err(ContentFault::Cancelled);
            }
            last = true;
        }
    }

    fn once_in_directory<T>(
        &mut self,
        catalog: &Catalog,
        dir: InoId,
        open: &mut impl FnMut(BorrowedFd<'_>) -> Result<T, ContentFault>,
    ) -> Result<T, ContentFault> {
        let Some(held) = &self.held else {
            return open(open_directory(catalog, dir)?.as_fd());
        };
        let mut held = lock(held);
        #[cfg(test)]
        if let Some(pause) = &self.pause {
            pause();
        }
        if held.budget.pressured.load(Ordering::Acquire) {
            held.truncate(0);
        }
        let opened = match hold_directory(&mut held, catalog, dir) {
            Ok(Some(fd)) => open(fd.as_fd()),
            Ok(None) => match held.fds.last() {
                Some(level) => open(level.fd.as_fd()),
                None => Err(ContentFault::Changed),
            },
            Err(fault) => Err(fault),
        };
        // A reader that ran out while this one opened may be waiting for
        // these.
        if held.budget.pressured.load(Ordering::Acquire) {
            held.truncate(0);
        }
        opened
    }
}

/// Whether `opened` failed for want of a descriptor.
fn out_of_descriptors<T>(opened: &Result<T, ContentFault>) -> bool {
    let Err(ContentFault::Open(error)) = opened else {
        return false;
    };
    let errno = error.raw_os_error();
    errno == Some(rustix::io::Errno::MFILE.raw_os_error())
        || errno == Some(rustix::io::Errno::NFILE.raw_os_error())
}

static PROCESS: OnceLock<Budget> = OnceLock::new();

/// Directory descriptors the process's readers may hold between paths: a
/// quarter of its open-file limit, at most [`Budget::MOST`]. Without one,
/// every reader of a pool holds its whole last path, and a deep tree on
/// many threads runs out of descriptors where one thread does not, which
/// would make the rows depend on the thread count. A reader past the budget
/// opens the rest of a path as an unheld walk does, two at a time.
///
/// Holding stops for good, process-wide, the first time any reader runs
/// out of descriptors ([`Budget::relieve`]): from then on every reader
/// walks unheld, as without a budget. That costs a deep tree's reads their
/// saved opens, and buys a bound that holds whatever else the process has
/// open, since the quarter alone reserves nothing for the descriptors
/// other code holds.
#[derive(Debug)]
pub(crate) struct Budget {
    /// Descriptors still to be held.
    left: AtomicUsize,
    capacity: usize,
    /// Set by the first reader out of descriptors.
    pressured: AtomicBool,
    /// Every reader holding from this budget, to empty under pressure.
    readers: Mutex<Vec<Weak<Mutex<Held>>>>,
    /// Signalled under pressure as held descriptors close.
    quiet: Mutex<()>,
    freed: Condvar,
}

impl Budget {
    const MOST: usize = 1024;

    const fn new(capacity: usize) -> Self {
        Self {
            left: AtomicUsize::new(capacity),
            capacity,
            pressured: AtomicBool::new(false),
            readers: Mutex::new(Vec::new()),
            quiet: Mutex::new(()),
            freed: Condvar::new(),
        }
    }

    fn process() -> &'static Budget {
        PROCESS.get_or_init(|| {
            let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile).current;
            let quarter = limit.map_or(Self::MOST as u64, |limit| limit / 4);
            Budget::new(quarter.min(Self::MOST as u64) as usize)
        })
    }

    fn take(&self) -> bool {
        !self.pressured.load(Ordering::Acquire)
            && self
                .left
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
    }

    fn give(&self, fds: usize) {
        self.left.fetch_add(fds, Ordering::AcqRel);
        if fds > 0 && self.pressured.load(Ordering::Acquire) {
            // Taken so a waiter between its check and its wait still hears.
            let _quiet = lock(&self.quiet);
            self.freed.notify_all();
        }
    }

    fn holding(&self) -> usize {
        self.capacity - self.left.load(Ordering::Acquire)
    }

    /// Sleeps until no reader holds a descriptor from this budget: false if
    /// `cancelled` first, which is asked every 50 ms.
    fn wait_unheld(&self, cancelled: &dyn Fn() -> bool) -> bool {
        let mut quiet = lock(&self.quiet);
        while self.holding() > 0 {
            if cancelled() {
                return false;
            }
            quiet = self
                .freed
                .wait_timeout(quiet, Duration::from_millis(50))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Stops all holding and closes what every idle reader holds; a reader
    /// busy opening closes its own when it finishes.
    fn relieve(&self) {
        self.pressured.store(true, Ordering::Release);
        lock(&self.readers).retain(|reader| {
            let Some(reader) = reader.upgrade() else {
                return false;
            };
            match reader.try_lock() {
                Ok(mut held) => held.truncate(0),
                Err(TryLockError::Poisoned(held)) => held.into_inner().truncate(0),
                Err(TryLockError::WouldBlock) => {}
            }
            true
        });
    }
}

/// None to hold: an unheld walk.
static NO_BUDGET: Budget = Budget::new(0);

/// A reader's held walk, root first, each descriptor charged to `budget`
/// until it is closed.
#[derive(Debug)]
pub(super) struct Held {
    fds: Vec<Level>,
    budget: &'static Budget,
}

/// One held directory: what its next check needs without the catalog.
#[derive(Debug)]
struct Level {
    id: InoId,
    fd: OwnedFd,
    /// Its name in the level above; empty for the root.
    name: Box<[u8]>,
    /// The catalog's `(dev, ino)` for `id`, which it was checked against.
    identity: (u64, u64),
}

impl Held {
    /// Another walk from `held`'s budget, holding nothing yet: a forked
    /// reader's.
    pub(super) fn sibling(held: &Mutex<Self>) -> Arc<Mutex<Self>> {
        Self::new(lock(held).budget)
    }

    /// An empty walk, registered with `budget` for [`Budget::relieve`].
    fn new(budget: &'static Budget) -> Arc<Mutex<Self>> {
        let held = Arc::new(Mutex::new(Self {
            fds: Vec::new(),
            budget,
        }));
        let mut readers = lock(&budget.readers);
        readers.retain(|reader| reader.strong_count() > 0);
        readers.push(Arc::downgrade(&held));
        held
    }

    /// Closes all but the first `len` descriptors, then credits them back.
    fn truncate(&mut self, len: usize) {
        let closed = self.fds.len().saturating_sub(len);
        self.fds.truncate(len);
        self.budget.give(closed);
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        // Closed before credited, so the budget never counts more than are
        // open.
        self.truncate(0);
    }
}

/// Opens `dir` from its root, one `O_NOFOLLOW` component at a time, each
/// checked against the catalog's `(dev, ino)`, with at most two open.
fn open_directory(catalog: &Catalog, dir: InoId) -> Result<OwnedFd, ContentFault> {
    let mut held = Held {
        fds: Vec::new(),
        budget: &NO_BUDGET,
    };
    hold_directory(&mut held, catalog, dir)?.ok_or(ContentFault::Changed)
}

/// [`open_directory`], reusing the descriptors `held` has for the start of
/// `dir`'s chain and leaving as much of the chain in it, root first, as its
/// budget allows. `None` when the whole chain is held, its last descriptor
/// being `dir`'s; else `dir`'s descriptor, opened from the last held one
/// (or the root) without holding those between.
///
/// A held descriptor is reused only once its path is checked again: the
/// root's path by a `stat`, each directory below by a no-follow `fstatat`
/// in its held parent, each against the catalog's `(dev, ino)` that the
/// descriptor was checked against when opened. So, as on a fresh walk, the
/// directory read through is the one its path named, at a moment between
/// the walk's start and the file's open; a renamed or replaced directory
/// fails the same way. One call per level instead of three (open, fstat,
/// close).
fn hold_directory(
    held: &mut Held,
    catalog: &Catalog,
    dir: InoId,
) -> Result<Option<OwnedFd>, ContentFault> {
    // A held `dir`'s chain is the held walk through it: the same checks,
    // from what each level kept rather than from the catalog, which is the
    // one view this reader's documents, and so its held ids, come from. Most
    // files a reader opens sit in the directory its last one did.
    if let Some(at) = held.fds.iter().position(|level| level.id == dir)
        && held.recheck(at, root_path(catalog, held.fds[0].id)?)?
    {
        held.truncate(at + 1);
        return Ok(None);
    }
    let names = catalog.name_reader();
    // `dir` and its ancestors below the root, nearest first.
    let mut chain = Vec::new();
    let mut at = dir;
    while let Some(name) = catalog.dir_name(at) {
        let (parent, bytes) = names.edge(name);
        chain.push((at, bytes));
        at = parent;
    }
    let root = at;
    let path = root_path(catalog, root)?;
    let kept = held.kept(catalog, root, path, &chain)?;
    held.truncate(kept);
    held.open_chain(catalog, root, path, &chain)
}

/// The path of root `root`.
fn root_path(catalog: &Catalog, root: InoId) -> Result<&std::ffi::OsStr, ContentFault> {
    catalog
        .roots()
        .find(|&(id, _)| id == root)
        .map(|(_, path)| std::ffi::OsStr::from_bytes(path))
        .ok_or(ContentFault::Changed)
}

/// Whether `stat` is of a directory with `identity`.
fn is_directory(stat: &rustix::fs::Stat, identity: (u64, u64)) -> bool {
    FileType::from_raw_mode(stat.st_mode) == FileType::Directory
        && (stat.st_dev, stat.st_ino) == identity
}

/// Stats `name` in `parent`, not following a symlink.
fn stat_child(parent: &OwnedFd, name: &[u8]) -> Result<rustix::fs::Stat, ContentFault> {
    statat(
        parent,
        std::ffi::OsStr::from_bytes(name),
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .map_err(|e| ContentFault::Stat(e.into()))
}

impl Held {
    /// Checks the held chain through level `at` again, from its root at
    /// `path`. False when the root is no longer the held directory there.
    fn recheck(&self, at: usize, path: &std::ffi::OsStr) -> Result<bool, ContentFault> {
        // The root's own path may pass through symlinks: the user's.
        let stat = rustix::fs::stat(path).map_err(|e| ContentFault::Stat(e.into()))?;
        if !is_directory(&stat, self.fds[0].identity) {
            return Ok(false);
        }
        for pair in self.fds[..=at].windows(2) {
            if !is_directory(&stat_child(&pair[0].fd, &pair[1].name)?, pair[1].identity) {
                return Err(ContentFault::Changed);
            }
        }
        Ok(true)
    }

    /// How much of the held chain is `chain`'s, under `root` at `path`, and
    /// still at its path.
    fn kept(
        &self,
        catalog: &Catalog,
        root: InoId,
        path: &std::ffi::OsStr,
        chain: &[(InoId, &[u8])],
    ) -> Result<usize, ContentFault> {
        if self.fds.first().is_none_or(|level| level.id != root) {
            return Ok(0);
        }
        let stat = rustix::fs::stat(path).map_err(|e| ContentFault::Stat(e.into()))?;
        if !is_directory(&stat, catalog.identity(root)) {
            return Ok(0);
        }
        let mut kept = 1;
        for (&(id, bytes), pair) in chain.iter().rev().zip(self.fds.windows(2)) {
            if id != pair[1].id {
                break;
            }
            if !is_directory(&stat_child(&pair[0].fd, bytes)?, catalog.identity(id)) {
                return Err(ContentFault::Changed);
            }
            kept += 1;
        }
        Ok(kept)
    }

    /// Opens the rest of `chain` below what is held, holding each level
    /// while the budget allows. Past it, the walk goes on through one
    /// descriptor, which is returned.
    fn open_chain(
        &mut self,
        catalog: &Catalog,
        root: InoId,
        path: &std::ffi::OsStr,
        chain: &[(InoId, &[u8])],
    ) -> Result<Option<OwnedFd>, ContentFault> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let mut unheld = None;
        if self.fds.is_empty() {
            // The root's own path is the user's; symlinks above it are
            // theirs to keep. Everything below it must not follow one.
            let fd = open(path, flags, Mode::empty()).map_err(|e| ContentFault::Open(e.into()))?;
            check_directory(&fd, catalog, root)?;
            match self.budget.take() {
                true => self.fds.push(Level {
                    id: root,
                    fd,
                    name: Box::default(),
                    identity: catalog.identity(root),
                }),
                false => unheld = Some(fd),
            }
        }
        let done = self.fds.len().saturating_sub(1);
        for &(id, bytes) in chain.iter().rev().skip(done) {
            let parent = match (&unheld, self.fds.last()) {
                (Some(fd), _) | (None, Some(Level { fd, .. })) => fd,
                (None, None) => return Err(ContentFault::Changed),
            };
            let fd = openat(
                parent,
                std::ffi::OsStr::from_bytes(bytes),
                flags | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map_err(|e| ContentFault::Open(e.into()))?;
            check_directory(&fd, catalog, id)?;
            match unheld.is_none() && self.budget.take() {
                true => self.fds.push(Level {
                    id,
                    fd,
                    name: bytes.into(),
                    identity: catalog.identity(id),
                }),
                false => unheld = Some(fd),
            }
        }
        Ok(unheld)
    }
}

fn check_directory(fd: &OwnedFd, catalog: &Catalog, id: InoId) -> Result<(), ContentFault> {
    let stat = fstat(fd).map_err(|e| ContentFault::Stat(e.into()))?;
    let same = FileType::from_raw_mode(stat.st_mode) == FileType::Directory
        && (stat.st_dev, stat.st_ino) == catalog.identity(id);
    same.then_some(()).ok_or(ContentFault::Changed)
}
