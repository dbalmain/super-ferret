//! Reading a catalogued document's bytes by DocId, for the content index's
//! follow pass (docs/S2.md § The content flow, § Coverage). `ferret-index`
//! never opens a file; its host hands it these bytes.
//!
//! Follow reads a document only if it is still the version the catalog recorded
//! (D21, D33): every directory on its path is opened `O_NOFOLLOW` and must
//! have the catalogued `(dev, ino)`, the file is opened `O_NOFOLLOW` beneath
//! its parent's descriptor and must match the catalog's stat, and a second
//! `fstat` after the read must still match. Anything else is a
//! [`ContentFault`], which the index records as unreadable and never retries.
//!
//! Query verification uses [`Documents::read_current_name`] instead: the same
//! checked directory walk, with the file bracketed against its current stat.
//! It returns that stat so the host can keep current path answers separate
//! from cacheable catalog-version content facts.
//!
//! [`Documents`] maps each DocId to one name by a single scan of the names,
//! built per pass. M5's daemon will want D30's lazy inverse instead; a full
//! scan is what M3's measurement and tests need.

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, TryLockError, Weak};
use std::time::Duration;

use ferret_catalog::bulk::Limiter;
use ferret_catalog::{Catalog, DocId, InoId, NameId, OpenError, Section, Target};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, fstat, open, openat, statat};

use crate::ContentFault;
use crate::observe::{BulkRead, Opened, Reader, bracket, bracket_search, catalog_stat};

/// Streams an explicit regular file or live-fallback file, using the same
/// current-version bracket as catalogued candidates. `None` imposes no policy
/// size cap. Zero reported size selects an EOF read so virtual-file content
/// is visible; nonzero sizes bound reads. Both retain the closing fstat against
/// the opening version, even after an early stop. The caller owns retry and
/// publication of buffered output.
pub fn stream_file_until(
    path: &std::path::Path,
    cap: Option<u64>,
    each: &mut dyn FnMut(&mut dyn Read, u64) -> io::Result<()>,
    cancelled: &dyn Fn() -> bool,
) -> Result<ferret_catalog::Stat, ContentFault> {
    stream_file_until_with_growth(path, cap, each, cancelled, false)
}

/// Search-only form of [`stream_file_until`]. The caller publishes the
/// opening-size prefix only after this function accepts its closing stat.
pub fn stream_search_file_until(
    path: &std::path::Path,
    cap: Option<u64>,
    each: &mut dyn FnMut(&mut dyn Read, u64) -> io::Result<()>,
    cancelled: &dyn Fn() -> bool,
) -> Result<ferret_catalog::Stat, ContentFault> {
    stream_file_until_with_growth(path, cap, each, cancelled, true)
}

fn stream_file_until_with_growth(
    path: &std::path::Path,
    cap: Option<u64>,
    each: &mut dyn FnMut(&mut dyn Read, u64) -> io::Result<()>,
    cancelled: &dyn Fn() -> bool,
    accept_growth: bool,
) -> Result<ferret_catalog::Stat, ContentFault> {
    if cancelled() {
        return Err(ContentFault::Cancelled);
    }
    let fd = open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| ContentFault::Open(error.into()))?;
    let (file, current) = regular_file(fd)?;
    if let Some(cap) = cap
        && current.size > cap
    {
        return Err(ContentFault::TooLarge {
            size: current.size,
            cap,
        });
    }
    checked_file(file, current, accept_growth, &mut |file, size| {
        let extent = if size == 0 { u64::MAX } else { size };
        each(&mut Read::take(file, extent), extent).map_err(|error| {
            if cancelled() {
                ContentFault::Cancelled
            } else {
                ContentFault::Read(error)
            }
        })
    })
}

/// Validate the open descriptor, rather than path metadata that may already
/// name another inode. Shared by sized transactions and direct virtual streams.
fn regular_file(fd: OwnedFd) -> Result<(File, ferret_catalog::Stat), ContentFault> {
    let stat = fstat(&fd).map_err(|error| ContentFault::Stat(error.into()))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(ContentFault::Changed);
    }
    Ok((File::from(fd), catalog_stat(&stat)))
}

fn checked_file(
    mut file: File,
    current: ferret_catalog::Stat,
    accept_growth: bool,
    pass: &mut dyn FnMut(&mut File, u64) -> Result<(), ContentFault>,
) -> Result<ferret_catalog::Stat, ContentFault> {
    pass(&mut file, current.size)?;
    if accept_growth {
        bracket_search(&file, &current)
    } else {
        bracket(&file, &current, ferret_catalog::Content::Binary)?;
        Ok(current)
    }
}

/// One catalog view's documents, each with one of its names.
pub struct Documents {
    /// `name[doc]` is the name to read `doc` through, plus one; 0 for none.
    names: Vec<u32>,
    /// With [`Documents::holding_directories`]: the last checked walk, root
    /// first, each directory with its descriptor, kept for the next path.
    /// Locked while this reader opens through it, and otherwise free for a
    /// reader out of descriptors to empty ([`Budget::relieve`]).
    held: Option<Arc<Mutex<Held>>>,
    /// Called once this reader has locked its held walk to open through it.
    #[cfg(test)]
    pause: Option<Arc<dyn Fn() + Send + Sync>>,
    limiter: Option<Arc<Limiter>>,
    config: ferret_policy::Config,
    /// Documents read whole and checked.
    pub files_read: u64,
    /// Their bytes.
    pub bytes_read: u64,
}

impl std::fmt::Debug for Documents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Documents")
            .field("documents", &self.names.iter().filter(|&&n| n != 0).count())
            .field("files_read", &self.files_read)
            .field("bytes_read", &self.bytes_read)
            .finish_non_exhaustive()
    }
}

impl Documents {
    /// Scans `catalog`'s names once, loading the sections a read needs.
    pub fn new(catalog: &Catalog) -> Result<Self, OpenError> {
        catalog.load(&Section::INODE)?;
        catalog.load(&[Section::Names, Section::Roots, Section::Doc, Section::Docs])?;
        let mut names = vec![0u32; catalog.next_doc().0 as usize];
        for (name, edge) in catalog.name_reader().runs_from(NameId(0)) {
            let Target::Inode(inode) = edge.target() else {
                continue;
            };
            if let Some(doc) = catalog.doc(inode)
                && let Some(slot) = names.get_mut(doc.0 as usize)
                && *slot == 0
                && catalog.is_live_name(name)
            {
                *slot = name.0 + 1;
            }
        }
        Ok(Self {
            names,
            held: None,
            #[cfg(test)]
            pause: None,
            limiter: None,
            config: ferret_policy::Config::default(),
            files_read: 0,
            bytes_read: 0,
        })
    }

    /// A reader with no DocId map, for a host that already knows which name
    /// to read a document through (a query verifying a row): it reads only
    /// through [`Documents::read_name`], and skips the scan of every name.
    pub fn by_name(catalog: &Catalog) -> Result<Self, OpenError> {
        catalog.load(&Section::INODE)?;
        catalog.load(&[Section::Names, Section::Roots, Section::Doc, Section::Docs])?;
        Ok(Self {
            names: Vec::new(),
            held: None,
            #[cfg(test)]
            pause: None,
            limiter: None,
            config: ferret_policy::Config::default(),
            files_read: 0,
            bytes_read: 0,
        })
    }

    /// Another reader by name with this one's policy and limiter, for a
    /// second thread: [`Documents::by_name`] without loading again. Counts
    /// start at zero, and it holds directories if this one does, from the
    /// same budget, holding none yet.
    pub fn fork(&self) -> Self {
        Self {
            names: Vec::new(),
            held: self.held.as_ref().map(|held| Held::new(lock(held).budget)),
            #[cfg(test)]
            pause: None,
            limiter: self.limiter.clone(),
            config: self.config,
            files_read: 0,
            bytes_read: 0,
        }
    }

    /// Uses the same size cap as a crawl configured with this policy. Product
    /// indexing and querying both use the default policy (8 MiB).
    pub fn with_config(mut self, config: ferret_policy::Config) -> Self {
        self.config = config;
        self
    }

    /// Keeps each checked directory walk's descriptors for later paths
    /// through the same directories: each path still checks every directory
    /// on it, by a stat of its path, instead of opening it again
    /// ([`hold_directory`]). For a reader that lives for one query: it holds
    /// a descriptor per level of the last path, as far as the process's
    /// budget for held descriptors allows ([`Budget`]), and opens the rest
    /// of a path without holding them.
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

    /// Paces every read through the shared byte limiter: background work.
    /// An explicit `ferret index` reads unpaced.
    pub fn with_limiter(mut self, limiter: Arc<Limiter>) -> Self {
        self.limiter = Some(limiter);
        self
    }

    /// Replaces `out` with `doc`'s bytes, if it is still the version
    /// `catalog` recorded.
    pub fn read(
        &mut self,
        catalog: &Catalog,
        doc: DocId,
        out: &mut Vec<u8>,
    ) -> Result<(), ContentFault> {
        out.clear();
        let name = match self.names.get(doc.0 as usize) {
            Some(&n) if n != 0 => NameId(n - 1),
            _ => return Err(ContentFault::Changed),
        };
        self.read_name(catalog, name, out)
    }

    /// Replaces `out` with the bytes of the file `name` names, if it is still
    /// the version `catalog` recorded.
    pub fn read_name(
        &mut self,
        catalog: &Catalog,
        name: NameId,
        out: &mut Vec<u8>,
    ) -> Result<(), ContentFault> {
        out.clear();
        let (parent, bytes) = catalog.name_reader().edge(name);
        let Target::Inode(inode) = catalog.name(name).target() else {
            return Err(ContentFault::Changed);
        };
        let recorded = catalog.inode(inode).stat;
        let name = std::ffi::OsStr::from_bytes(bytes);
        let mut file = self.in_directory(catalog, parent, &|| false, |dir| {
            match Reader::open(dir, name, &recorded) {
                Opened::Ready { file, .. } => Ok(file),
                Opened::Fault(fault) => Err(fault),
            }
        })?;
        read_all(
            &mut file,
            recorded.size,
            self.config.size_cap,
            self.limiter.as_deref(),
            out,
            &|| false,
        )?;
        bracket(&file, &recorded, ferret_catalog::Content::Binary)?;
        self.files_read += 1;
        self.bytes_read += out.len() as u64;
        Ok(())
    }

    /// Checks the file's version through its own fresh directory walk, with
    /// one no-follow stat and no content read.
    pub fn check_name(&mut self, catalog: &Catalog, name: NameId) -> Result<(), ContentFault> {
        let stat = self.stat_name(catalog, name)?;
        let recorded = catalog.inode(catalog.name(name).child).stat;
        recorded
            .same_version(&stat)
            .then_some(())
            .ok_or(ContentFault::Changed)
    }

    /// The current regular file's stat through a fresh checked directory walk.
    /// A changed file is observed; missing/replaced directory chains fail.
    pub fn stat_name(
        &mut self,
        catalog: &Catalog,
        name: NameId,
    ) -> Result<ferret_catalog::Stat, ContentFault> {
        let (parent, bytes) = catalog.name_reader().edge(name);
        let stat = self.in_directory(catalog, parent, &|| false, |dir| {
            statat(
                dir,
                std::ffi::OsStr::from_bytes(bytes),
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(|e| ContentFault::Stat(e.into()))
        })?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(ContentFault::Changed);
        }
        Ok(catalog_stat(&stat))
    }

    /// A stable current-byte read for a query's Maybe candidate, bracketed
    /// against the opened file's current stat rather than the catalog key.
    /// Returns that stat so the host can distinguish cacheable catalog bytes
    /// from a path-specific changed version. Follow still uses `read_name`.
    pub fn read_current_name(
        &mut self,
        catalog: &Catalog,
        name: NameId,
        out: &mut Vec<u8>,
    ) -> Result<ferret_catalog::Stat, ContentFault> {
        self.read_current_name_until(catalog, name, out, &|| false)
    }

    /// Like `read_current_name`, bounded by the policy size cap, with fallible
    /// allocation and host cancellation between 256 KiB read chunks.
    pub fn read_current_name_until(
        &mut self,
        catalog: &Catalog,
        name: NameId,
        out: &mut Vec<u8>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ferret_catalog::Stat, ContentFault> {
        self.read_current_name_with_growth(catalog, name, out, cancelled, false)
    }

    /// Search-only version of [`Documents::read_current_name_until`].
    pub fn read_search_name_until(
        &mut self,
        catalog: &Catalog,
        name: NameId,
        out: &mut Vec<u8>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ferret_catalog::Stat, ContentFault> {
        self.read_current_name_with_growth(catalog, name, out, cancelled, true)
    }

    fn read_current_name_with_growth(
        &mut self,
        catalog: &Catalog,
        name: NameId,
        out: &mut Vec<u8>,
        cancelled: &dyn Fn() -> bool,
        accept_growth: bool,
    ) -> Result<ferret_catalog::Stat, ContentFault> {
        out.clear();
        let (cap, limiter) = (self.config.size_cap, self.limiter.clone());
        let current = self.current_until(
            catalog,
            name,
            cancelled,
            accept_growth,
            &mut |file, size| {
                out.clear();
                read_all(file, size, cap, limiter.as_deref(), out, cancelled)
            },
        );
        if current.is_ok() {
            self.bytes_read += out.len() as u64;
        }
        current
    }

    /// [`Documents::read_current_name_until`], handing the bytes to `each`
    /// as a reader instead of collecting them: `each` reads as much as it
    /// needs, with the document's size, and should stop between reads once
    /// `cancelled` says so, failing. `each` is called once, and only for a
    /// regular file.
    pub fn stream_current_name_until(
        &mut self,
        catalog: &Catalog,
        name: NameId,
        each: &mut dyn FnMut(&mut dyn Read, u64) -> io::Result<()>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ferret_catalog::Stat, ContentFault> {
        self.stream_current_name_with_growth(catalog, name, each, cancelled, false)
    }

    /// Search-only version of [`Documents::stream_current_name_until`].
    pub fn stream_search_name_until(
        &mut self,
        catalog: &Catalog,
        name: NameId,
        each: &mut dyn FnMut(&mut dyn Read, u64) -> io::Result<()>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ferret_catalog::Stat, ContentFault> {
        self.stream_current_name_with_growth(catalog, name, each, cancelled, true)
    }

    fn stream_current_name_with_growth(
        &mut self,
        catalog: &Catalog,
        name: NameId,
        each: &mut dyn FnMut(&mut dyn Read, u64) -> io::Result<()>,
        cancelled: &dyn Fn() -> bool,
        accept_growth: bool,
    ) -> Result<ferret_catalog::Stat, ContentFault> {
        let (cap, limiter) = (self.config.size_cap, self.limiter.clone());
        self.current_until(
            catalog,
            name,
            cancelled,
            accept_growth,
            &mut |file, size| {
                if cancelled() {
                    return Err(ContentFault::Cancelled);
                }
                if size > cap {
                    return Err(ContentFault::TooLarge { size, cap });
                }
                let done = match limiter.as_deref() {
                    Some(limiter) => each(
                        &mut BulkRead {
                            file,
                            limiter: Some(limiter),
                            remaining: size,
                        },
                        size,
                    ),
                    None => each(&mut Read::take(file, size), size),
                };
                done.map_err(|e| match cancelled() {
                    true => ContentFault::Cancelled,
                    false => ContentFault::Read(e),
                })
            },
        )
    }

    /// The current version of the file `name` names, read by `pass` (given
    /// the file at its start and the size to read), bracketed by two stats
    /// of the open file: the first must show a regular file before `pass`
    /// sees a byte, and the second the same version as the first.
    ///
    /// The calls, for a file in a held directory: `openat`, `fstat`, the
    /// reads, `fstat`, `close`.
    fn current_until(
        &mut self,
        catalog: &Catalog,
        name: NameId,
        cancelled: &dyn Fn() -> bool,
        accept_growth: bool,
        pass: &mut dyn FnMut(&mut File, u64) -> Result<(), ContentFault>,
    ) -> Result<ferret_catalog::Stat, ContentFault> {
        if cancelled() {
            return Err(ContentFault::Cancelled);
        }
        let (parent, bytes) = catalog.name_reader().edge(name);
        let fd = self.in_directory(catalog, parent, cancelled, |dir| {
            openat(
                dir,
                std::ffi::OsStr::from_bytes(bytes),
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| ContentFault::Open(e.into()))
        })?;
        let (file, current) = regular_file(fd)?;
        let current = checked_file(file, current, accept_growth, pass)?;
        self.files_read += 1;
        Ok(current)
    }

    /// `open` given a checked descriptor for `dir`, re-walked from the
    /// root for each path. A retained descriptor alone cannot establish that
    /// its original path still names it after a rename: a reader [holding
    /// directories](Documents::holding_directories) checks each path again
    /// before reusing one.
    ///
    /// An open that finds the process out of descriptors (`EMFILE`,
    /// `ENFILE`) is tried again once every held descriptor in the process
    /// is closed ([`Budget::relieve`]), waiting as long as a reader busy
    /// opening takes to close its own, unless `cancelled` says to stop
    /// first. So a reader fails for want of
    /// descriptors only where an unheld walk on every thread would: whether
    /// a path reads does not depend on how many readers hold directories.
    fn in_directory<T>(
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
                Some((_, fd)) => open(fd.as_fd()),
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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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
struct Held {
    fds: Vec<(InoId, OwnedFd)>,
    budget: &'static Budget,
}

impl Held {
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
    let path = catalog
        .roots()
        .find(|&(id, _)| id == root)
        .map(|(_, path)| path)
        .ok_or(ContentFault::Changed)?;
    let path = std::ffi::OsStr::from_bytes(path);
    let same = |stat: &rustix::fs::Stat, id: InoId| {
        FileType::from_raw_mode(stat.st_mode) == FileType::Directory
            && (stat.st_dev, stat.st_ino) == catalog.identity(id)
    };
    // How much of the held chain is `dir`'s and still at its path.
    let mut kept = 0;
    if held.fds.first().is_some_and(|&(id, _)| id == root) {
        // The root's own path may pass through symlinks: the user's.
        let stat = rustix::fs::stat(path).map_err(|e| ContentFault::Stat(e.into()))?;
        if same(&stat, root) {
            kept = 1;
            for (&(id, bytes), (parent, (held_id, _))) in
                chain.iter().rev().zip(held.fds.iter().zip(&held.fds[1..]))
            {
                if id != *held_id {
                    break;
                }
                let stat = statat(
                    &parent.1,
                    std::ffi::OsStr::from_bytes(bytes),
                    AtFlags::SYMLINK_NOFOLLOW,
                )
                .map_err(|e| ContentFault::Stat(e.into()))?;
                if !same(&stat, id) {
                    return Err(ContentFault::Changed);
                }
                kept += 1;
            }
        }
    }
    held.truncate(kept);
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    // Past the budget, the walk goes on through this one descriptor.
    let mut unheld = None;
    if held.fds.is_empty() {
        // The root's own path is the user's; symlinks above it are theirs
        // to keep. Everything below it must not follow one.
        let fd = open(path, flags, Mode::empty()).map_err(|e| ContentFault::Open(e.into()))?;
        check_directory(&fd, catalog, root)?;
        match held.budget.take() {
            true => held.fds.push((root, fd)),
            false => unheld = Some(fd),
        }
    }
    let done = held.fds.len().saturating_sub(1);
    for &(id, bytes) in chain.iter().rev().skip(done) {
        let parent = match (&unheld, held.fds.last()) {
            (Some(fd), _) | (None, Some((_, fd))) => fd,
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
        match unheld.is_none() && held.budget.take() {
            true => held.fds.push((id, fd)),
            false => unheld = Some(fd),
        }
    }
    Ok(unheld)
}

fn check_directory(fd: &OwnedFd, catalog: &Catalog, id: InoId) -> Result<(), ContentFault> {
    let stat = fstat(fd).map_err(|e| ContentFault::Stat(e.into()))?;
    let same = FileType::from_raw_mode(stat.st_mode) == FileType::Directory
        && (stat.st_dev, stat.st_ino) == catalog.identity(id);
    same.then_some(()).ok_or(ContentFault::Changed)
}

/// Reads `size` bytes, the size the open file was checked at, through the
/// limiter if any. The bracket afterwards catches growth or shrinkage.
fn read_all(
    file: &mut File,
    size: u64,
    cap: u64,
    limiter: Option<&Limiter>,
    out: &mut Vec<u8>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), ContentFault> {
    if cancelled() {
        return Err(ContentFault::Cancelled);
    }
    if size > cap {
        return Err(ContentFault::TooLarge { size, cap });
    }
    let size_usize = usize::try_from(size).map_err(|_| ContentFault::TooLarge { size, cap })?;
    out.try_reserve_exact(size_usize)
        .map_err(ContentFault::Allocation)?;
    let mut remaining = size;
    loop {
        if cancelled() {
            return Err(ContentFault::Cancelled);
        }
        if remaining == 0 {
            return Ok(());
        }
        // A chunk at a time, for cancellation, and appended without zeroing
        // first: `File` reads into the spare capacity as it is.
        let want = remaining.min(256 << 10);
        let mut chunk = Read::take(&mut *file, want);
        let read = match limiter {
            Some(limiter) => limiter.transfer(want as usize, || chunk.read_to_end(out)),
            None => chunk.read_to_end(out),
        };
        match read {
            Ok(0) => return Ok(()),
            Ok(n) => remaining -= n as u64,
            Err(e) => return Err(ContentFault::Read(e)),
        }
    }
}
