//! One root, walked by directory handles, with one [`DirRules`] per directory.
//!
//! Seam: [`ferret_policy`] decides what to do with each entry. This module
//! reads the tree and reports. It does not touch the catalog.
//!
//! The root is opened by the path the caller gave, following a symlink there
//! (that path is the user's), with `O_DIRECTORY`. Every later open, stat,
//! `readlink` and ignore-file read is relative to a directory descriptor.
//! Each worker keeps parent continuations in a local LIFO stack. When another
//! worker is idle it shares the oldest continuation through a mutex and
//! `Condvar`. Jobs in local stacks and the shared queue hold at most 128
//! directory descriptors combined. Beyond that, a continuation closes its
//! descriptor and reopens by checked `openat` steps from the root when resumed.
//! Each worker holds one active job and at most three extra descriptors while
//! opening a child and reading `.git/info/exclude`. Thus a parallel walk with N
//! workers holds at most 128 + 4N descriptors, independent of depth and width.
//! The git directory closes as soon as `info` opens, and `info` closes as soon
//! as `exclude` opens, so five worker descriptors never overlap.
//! `EMFILE` opening a child is an [`Event::Io`] and the walk continues. There
//! is no path-based fallback below the root.
//! Listings store names in one byte buffer with offsets, and a worker reuses
//! one root-relative byte path, restoring its length after each entry.
//!
//! A catalogued symlink is one observation: `openat` with `O_PATH |
//! O_NOFOLLOW`, then `fstat` and `readlinkat` on that descriptor (an empty
//! path, which on Linux reads the link the descriptor refers to). The stat
//! in the event and the stored target cannot disagree.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;

use ferret_policy::{Config, Decision, DirRules, Entry, IgnoreFiles, PatternError};
use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, RawDir, fstat, open as open_path, openat, readlinkat, statat,
};
use rustix::io::Errno;

/// An ignore file larger than this is an [`Event::Io`] and counts as absent.
/// One mebibyte is past any ignore file this tree has seen; the bound is what
/// stops a hostile or accidental giant from being pulled into the walk.
const MAX_IGNORE_BYTES: u64 = 1 << 20;

/// `lstat` fields for an entry the walk did not skip.
///
/// Times are Unix `st_mtim` and `st_ctim` (seconds plus nanoseconds). `mode`
/// is `st_mode`, type bits included. `uid` and `gid` are `st_uid` and
/// `st_gid`: the catalog's inode row stores them, and they come from the same
/// `lstat` as the rest.
///
/// `link_target` is the raw `readlink` bytes of a catalogued symlink, not
/// resolved and not re-encoded (D18). It is `Some` exactly then. A catalogued
/// symlink is not emitted until `readlink` succeeds; a failure is
/// [`Event::Io`] and no [`Decided`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat<'a> {
    /// `st_size`. For a symlink, the length of the target path.
    pub size: u64,
    /// `st_mtim` seconds.
    pub mtime_sec: i64,
    /// `st_mtim` nanoseconds.
    pub mtime_nsec: i64,
    /// `st_ctim` seconds.
    pub ctime_sec: i64,
    /// `st_ctim` nanoseconds.
    pub ctime_nsec: i64,
    /// `st_dev`.
    pub dev: u64,
    /// `st_ino`.
    pub ino: u64,
    /// `st_mode`.
    pub mode: u32,
    /// `st_uid`.
    pub uid: u32,
    /// `st_gid`.
    pub gid: u32,
    /// `st_nlink`: for a directory, 2 plus its subdirectories on most
    /// filesystems, but whatever the filesystem reports.
    pub nlink: u64,
    /// Raw `readlink` text when this entry is a catalogued symlink.
    pub link_target: Option<&'a OsStr>,
}

/// One entry [`DirRules::decide`] classified.
///
/// `path` is relative to the walk's root and borrows the walker's path
/// buffer. It, `name` and `parent_fd` are valid only for the callback that
/// receives them. `stat` is absent for [`Decision::Skip`] and present for every
/// other decision. If `lstat` fails, or `readlink` fails for a catalogued
/// symlink, the walk emits [`Event::Io`] and does not emit `Decided`.
#[derive(Clone, Copy, Debug)]
pub struct Decided<'a, D> {
    /// The token of the directory that holds this entry.
    pub parent: D,
    /// That directory, open. `openat(parent_fd, name, O_NOFOLLOW)` reaches the
    /// entry without resolving a path, and a `(dev, ino)` check against
    /// `stat` proves it is the inode this event describes (D33).
    pub parent_fd: BorrowedFd<'a>,
    /// The entry's name in `parent`: the last component of `path`.
    pub name: &'a OsStr,
    /// Root-relative path of the entry.
    pub path: &'a Path,
    /// What the policy said to do with `path`.
    pub decision: Decision,
    /// File type, including for ignored names without stat data.
    pub kind: ferret_catalog::Kind,
    /// `lstat` of `path`. Present for every decision other than
    /// [`Decision::Skip`].
    pub stat: Option<Stat<'a>>,
}

/// How a work tree found at or below the root is attached to its repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkTreeKind {
    /// A `.git` directory, or a `.git` file whose gitdir has no `commondir`
    /// outside any work tree the walk has seen (`git init
    /// --separate-git-dir`).
    Main,
    /// A `.git` file whose gitdir names a `commondir` (`git worktree add`).
    Linked,
    /// A `.git` file whose gitdir has no `commondir`, in a directory already
    /// inside a work tree the walk has seen. That is a submodule's layout; a
    /// separate-git-dir repository nested in another work tree reads the
    /// same and is reported the same.
    Submodule,
}

/// A work tree whose top is the directory being entered (D23, D33).
///
/// Only work trees at or below the root are reported: nothing above a root is
/// read (D22). A symlinked `.git`, a `.git` file that names no readable gitdir,
/// or a gitdir whose `commondir` cannot be read or is empty still makes the
/// directory a work tree for ignore rules, and reports no `WorkTree`. Each of
/// those except the symlink is also an [`IoOp::ProbeGit`] fault on `.git`, a
/// missing gitdir included: the work tree's exclude rules and record are
/// unknown, not absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkTree<'a> {
    /// Main, linked or submodule.
    pub kind: WorkTreeKind,
    /// The common directory's path: the root as the caller gave it (made
    /// absolute), joined with the relative path and `.git` for a `.git`
    /// directory, or the gitdir and `commondir` text for a `.git` file, with
    /// `.` and `..` resolved lexically. A display name for the repository.
    pub common_dir: &'a Path,
    /// `(st_dev, st_ino)` of the common directory, from the descriptor the
    /// walk opened. Two work trees of one repository agree on this even when
    /// their paths to it are spelled differently; it is the repository's
    /// identity within a run.
    pub common_id: (u64, u64),
}

/// The operation that failed, for [`Event::Io`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoOp {
    /// Reading a directory's entries (`getdents`). Entries read before the
    /// error are still walked. `NotFound` here is uncertain coverage, not a
    /// vanished path (D26 A′): the opened directory could not be listed, and
    /// its path may since name a replacement.
    List,
    /// Opening a directory, or checking that what opened is the inode that was
    /// statted.
    OpenDir,
    /// Reopening a spilled continuation from the root, one component at a
    /// time; this includes the check that the directory is still the same
    /// inode.
    Reopen,
    /// `lstat` of an entry (`statat`, or `fstat` of its `O_PATH` descriptor).
    Lstat,
    /// `readlink` of a catalogued symlink.
    Readlink,
    /// Opening or reading `.ferretignore`, `.gitignore` or `info/exclude`.
    ReadIgnore,
    /// Classifying `.git`, reading a `.git` file, or opening the gitdir or its
    /// `commondir`.
    ProbeGit,
}

/// Which directory or entry an [`Event::Io`] is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultContext<'a, D> {
    /// The root itself: opening it, statting it or listing it. This can come
    /// before [`EventVisitor::root`] gave the root a token.
    Root,
    /// A directory below the root that already has its token: listing it, or
    /// reopening it as a spilled continuation.
    Dir(D),
    /// An entry of `parent` with no token of its own: a file's `lstat` or
    /// `readlink`, a named ignore file, the `.git` probe (`name` is `.git`
    /// for faults on the gitdir, `commondir` and `info/exclude` too), or a
    /// directory whose open failed.
    Child {
        /// The token of the directory holding the entry.
        parent: D,
        /// The entry's name in `parent`.
        name: &'a OsStr,
    },
}

/// What the walk reports, in the order it happens.
///
/// A directory is reported before its children in [`walk`]. Its siblings come
/// out in directory order, which is not sorted. [`walk_parallel`] gives no
/// cross-worker event order, but a directory's `Decided`, then its `Entered`,
/// then its children's events are always in that order. Paths borrow a
/// worker's buffer and must be copied to be kept. `D` is the visitor's
/// directory token ([`EventVisitor::Dir`]).
#[derive(Debug)]
pub enum Event<'a, D> {
    /// An entry `decide` classified.
    Decided(Decided<'a, D>),
    /// The directory `dir` is open and listed and its children follow. For a
    /// descended directory its ignore files have been read and `.git` probed;
    /// a traversed directory reads neither and always has `work_tree: None`.
    Entered {
        /// The directory's token.
        dir: D,
        /// The work tree whose top this directory is, if one starts here.
        work_tree: Option<WorkTree<'a>>,
        /// Every entry `getdents` returned, minus `.` and `..`, counted
        /// before any policy drops one, so a directory whose children are all
        /// ignored still counts them (`find -empty`, D47). `None` when the
        /// listing failed partway, which leaves the count uncertain.
        entries: Option<u32>,
    },
    /// A directory at a [`Boundary`] of [`WalkOptions::boundaries`]. It is not
    /// classified, not opened and not descended: another root owns it (D34).
    Boundary {
        /// The token of the directory holding it.
        parent: D,
        /// Its name in `parent`.
        name: &'a OsStr,
        /// Its root-relative path.
        path: &'a Path,
    },
    /// An open, stat, listing, `readlink` or ignore-file read failed.
    ///
    /// `path` is root-relative. It is empty when the root itself cannot be
    /// opened or listed, and it is the directory being listed when `getdents`
    /// yields an error with no name. The walk continues with the next entry.
    Io {
        /// Root-relative path the operation was about.
        path: &'a Path,
        /// What was being done.
        op: IoOp,
        /// Which directory or entry it was done to.
        context: FaultContext<'a, D>,
        /// The OS error.
        error: io::Error,
    },
    /// A pattern [`DirRules::root`] or [`DirRules::enter`] dropped. The rest
    /// of that ignore file still applies, and the walk continues.
    Pattern(PatternError),
}

/// A directory the walk stops at because another root owns it (D34).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Boundary {
    /// Root-relative path of the directory, compared by components.
    pub path: PathBuf,
    /// `(st_dev, st_ino)` the directory must have. When given, a directory at
    /// `path` with a different identity is not this boundary and is walked
    /// normally. A directory with this identity at another path (a bind
    /// mount, say) is walked too: the match is by path.
    pub id: Option<(u64, u64)>,
}

/// How [`walk_parallel`] runs, apart from what the policy decides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalkOptions {
    /// Worker threads. Zero means one.
    pub workers: usize,
    /// Directories to stop at, reported as [`Event::Boundary`].
    pub boundaries: Vec<Boundary>,
}

impl Default for WalkOptions {
    /// [`default_workers`] and no boundaries.
    fn default() -> Self {
        Self {
            workers: default_workers(),
            boundaries: Vec::new(),
        }
    }
}

/// Walks one configured root on the calling thread, with no boundaries.
///
/// `global` is the text of the global ignore file, or `None` when there is no
/// such file (nothing is excluded by default). `visit` is called for every
/// entry and every fault as the walk goes, so a catalog can record about a
/// million entries without this function retaining them. A callback is the
/// shape that allows that: the root-relative path is one reused buffer, and a
/// lending iterator is not expressible in stable Rust. Its directory token is
/// `()`, and it enters every directory the policy enters; a visitor that needs
/// tokens or boundaries uses [`walk_parallel`] with one worker.
///
/// The root directory itself is not a [`Decided`] event. It is where the walk
/// starts, not an entry `decide` sees; [`EventVisitor::root`] receives its
/// stat. Faults that belong to the root use an empty path.
///
/// Ignore files are read when the walk enters a directory
/// ([`Decision::Descend`]). A directory reached only so a `.ferretignore` `!`
/// pattern can re-include beneath it ([`Decision::Traverse`]) is listed, and
/// its ignore files are not read (D13). `.gitignore` is read only inside a
/// work tree, where its rules can apply. A configured root is self-contained:
/// `.gitignore` and `info/exclude` above it do not apply (D22). A `.gitignore`
/// applies at the root only when the root itself starts a work tree.
/// `.git/info/exclude` applies when a work tree starts at or below the root.
/// A `.git` file's first line (`gitdir: <path>`, relative to that directory)
/// points to the gitdir. Exclude comes from that gitdir or its `commondir`,
/// when present, as in a linked work tree. Nothing is opened through a
/// symlinked `.git`.
///
/// `.ferretignore`, `.gitignore` and `info/exclude` are opened without blocking
/// (`O_NONBLOCK`) and read only when that open file is a regular file of at
/// most 1 MiB. A larger file is an [`Event::Io`] and is then treated as
/// absent. A missing file, or an opened inode that is not a regular file, is
/// absent and not a fault: a FIFO or directory of one of these names must not
/// stall the walk. A name that cannot be opened at all (a socket, say) is a
/// fault. A regular file that exists but cannot be read is a fault, and is
/// then treated as absent. Bytes that are not UTF-8 are converted lossily. A
/// symlinked `.gitignore` is absent in descended directories, as git
/// disregards it; a symlinked `.ferretignore` or `exclude` is followed; `info`
/// is not, and neither is `.git`.
///
/// The listing is a snapshot. An ignore file created after the directory was
/// listed is not seen until the next crawl, so it does not apply to the
/// siblings listed with it. A listed name that is gone by the time it is
/// opened is absent.
///
/// File types come from the directory entry (`d_type`, or `statat` with
/// `AT_SYMLINK_NOFOLLOW` when the filesystem leaves the type unknown).
/// `d_type` does not follow symlinks. Once a stat has run, its type is the
/// truth and the entry is classified again when the listing disagreed: the
/// catalog stores that inode, so the decision should describe it. A name
/// skipped from `d_type` alone is not statted. Regular files are statted so
/// `decide` can see the size. Anything that is not skipped is statted for the
/// catalog fields. A catalogued symlink is opened `O_PATH | O_NOFOLLOW` and
/// both the stat and the target come from that descriptor. If the stat or
/// `readlink` fails, the walk emits [`Event::Io`] and no [`Event::Decided`].
///
/// A directory that is descended or traversed is opened with `O_DIRECTORY |
/// O_NOFOLLOW` after its `Decided` event and `fstat`ed. A different `(dev,
/// ino)` from the stat `decide` was given, or a symlink where the directory
/// was, is an [`Event::Io`] and the directory is not listed.
///
/// Mount points are crossed: the walk does not compare `st_dev` with the
/// root. See the module docs for the descriptor bound.
pub fn walk(root: &Path, global: Option<&str>, config: Config, visit: impl FnMut(Event<'_, ()>)) {
    let boundaries = BoundaryIndex::default();
    let mut walker = Walker::new(root, &boundaries, visit);
    let Some(root_job) = walker.root_job(root, global, config) else {
        return;
    };
    run_worker(walker, &Shared::new(root_job));
}

/// A worker-local visitor. The returned visitors retain their accumulated
/// state, so callers can merge it after the walk without locking per entry.
///
/// # Directory lifecycle
///
/// Each directory the walk enters is named by a token of the visitor's
/// choosing, [`EventVisitor::Dir`] (`Copy + Send`; D29). The walk stores the
/// token in its per-directory job and hands it back with every event about
/// that directory's entries, so a visitor can attach a child to its parent
/// without a path map, whichever worker reports the child.
///
/// ```text
/// root:   open, fstat -> root(stat) = token
///         list, read ignore files, probe .git -> Entered { dir: token, work_tree }
/// child:  Decided { parent, .. }  (before any open)
///           returns Some(token) for Descend / Traverse; None prunes it
///         Descend:  open, fstat, list, ignore files, .git -> Entered { dir, work_tree }
///         Traverse: open, fstat, list                     -> Entered { dir, work_tree: None }
///         its children: Decided { parent: token, .. }, Io, Boundary, ...
/// ```
///
/// - A directory's [`Decided`] fires before it is opened, as for every entry.
///   The value the visitor returns is that directory's token. For a directory
///   the policy would enter ([`Decision::Descend`] or [`Decision::Traverse`])
///   `None` prunes it: it is not opened and nothing below it is reported. For
///   every other event the return value is ignored.
/// - [`Event::Entered`] fires once the directory is open, listed, and, for
///   `Descend`, its ignore files are read and `.git` probed. A directory that
///   has a token but never gets `Entered` was not listed: its coverage is
///   uncertain, and an [`Event::Io`] says why.
/// - The root gets its token from [`EventVisitor::root`], called after the root
///   is opened and statted and before it is listed; then `Entered`.
/// - Every child event carries its parent's token: [`Decided::parent`],
///   [`Event::Boundary`], and [`Event::Io`]'s [`FaultContext`]. Tokens travel
///   with the job across workers, so a child can be reported on a different
///   worker from the one that minted its parent's token.
pub trait EventVisitor {
    /// A directory's token: small, chosen by the visitor, carried by the walk.
    type Dir: Copy + Send;

    /// The root, opened and statted; returns its token. Called once per walk,
    /// on the first visitor, before any other event except a root fault.
    fn root(&mut self, stat: Stat<'_>) -> Self::Dir;

    /// Receives one event; borrowed paths are valid only during this call.
    /// Returns the token for a directory's [`Event::Decided`] (see the
    /// lifecycle above); ignored otherwise.
    fn visit(&mut self, event: Event<'_, Self::Dir>) -> Option<Self::Dir>;

    /// Observed directory handle, before its complete listing. A watch host
    /// arms here so notifications during observation survive into its next run.
    /// Policy input consulted relative to the held parent, before reading.
    /// A path-valued policy dependency outside the observed directory handle.
    fn policy_path(&mut self, _path: &Path) {}

    /// Requires a separate thread even for one worker, so lowering an index
    /// worker's priority cannot affect the caller's query or intake work.
    fn dedicated_worker(&self) -> bool {
        false
    }

    /// Called on the executing worker, including a single-worker walk.
    fn worker_started(&mut self) {}

    fn policy_input(&mut self, _parent: BorrowedFd<'_>, _name: &OsStr) {}

    fn observing(&mut self, _fd: BorrowedFd<'_>, _path: &Path) {}

    /// Selects work before child stat/open. A false result deliberately keeps
    /// this untouched scope; it is not an ignored or vanished observation.
    fn consider(&mut self, _parent: Self::Dir, _name: &OsStr, _path: &Path) -> bool {
        true
    }
}

/// A closure sees every event, has no tokens (`()`), and enters every
/// directory the policy enters.
impl<F: FnMut(Event<'_, ()>)> EventVisitor for F {
    type Dir = ();

    fn root(&mut self, _stat: Stat<'_>) {}

    fn visit(&mut self, event: Event<'_, ()>) -> Option<()> {
        self(event);
        Some(())
    }
}

/// Most workers [`default_workers`] picks. Warm on a 16-core machine, 16
/// workers walked `$HOME` in 0.097 s against 0.154 s for 8 and 0.090 s for 32,
/// and cold in 2.50 s against 3.08 s and 2.60 s; 32 cost 39% more CPU than 16
/// for no gain (D24).
const MAX_DEFAULT_WORKERS: usize = 16;

/// The worker count to use when the user sets none: the machine's available
/// parallelism, capped at 16 (D24).
pub fn default_workers() -> usize {
    std::thread::available_parallelism().map_or(1, |threads| threads.get().min(MAX_DEFAULT_WORKERS))
}

/// Walks with `options.workers` worker-local visitors and returns them for
/// merging. Event order is unspecified across workers. When the root fails
/// before threads start, only its fault visitor is returned. Directories in
/// `options.boundaries` are reported as [`Event::Boundary`] and not entered.
/// See [`walk`] for what is read and when, and [`EventVisitor`] for the
/// directory lifecycle.
pub fn walk_parallel<V: EventVisitor + Send>(
    root: &Path,
    global: Option<&str>,
    config: Config,
    options: &WalkOptions,
    make_visitor: impl Fn() -> V + Sync,
) -> Vec<V> {
    let count = options.workers.max(1);
    let index = BoundaryIndex::new(&options.boundaries);
    let boundaries = &index;
    let mut first = Walker::new(root, boundaries, make_visitor());
    let Some(root_job) = first.root_job(root, global, config) else {
        return vec![first.visit];
    };
    let root_id = first.root_id;
    let shared = Shared::new(root_job);
    if count == 1 && !first.visit.dedicated_worker() {
        return vec![run_worker(first, &shared)];
    }
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(count);
        handles.push(scope.spawn(|| run_guarded(first, &shared)));
        for _ in 1..count {
            let visitor = make_visitor();
            let mut walker = Walker::new(root, boundaries, visitor);
            walker.root_id = root_id;
            handles.push(scope.spawn(|| run_guarded(walker, &shared)));
        }
        handles
            .into_iter()
            .map(|handle| match handle.join() {
                Ok(visitor) => visitor,
                Err(panic) => std::panic::resume_unwind(panic),
            })
            .collect()
    })
}

const MAX_OPEN_JOBS: usize = 128;

struct Shared<D> {
    jobs: Mutex<Vec<Job<D>>>,
    ready: Condvar,
    idle: AtomicUsize,
    outstanding: AtomicUsize,
    open_jobs: AtomicUsize,
    cancelled: AtomicBool,
}

impl<D> Shared<D> {
    fn new(root: Job<D>) -> Self {
        Self {
            jobs: Mutex::new(vec![root]),
            ready: Condvar::new(),
            idle: AtomicUsize::new(0),
            outstanding: AtomicUsize::new(1),
            open_jobs: AtomicUsize::new(1),
            cancelled: AtomicBool::new(false),
        }
    }

    fn take(&self) -> Option<Job<D>> {
        let mut jobs = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return None;
            }
            if let Some(job) = jobs.pop() {
                self.release_open(&job);
                return Some(job);
            }
            if self.outstanding.load(Ordering::Acquire) == 0 {
                return None;
            }
            // Mark idle while holding the queue lock, before wait releases it.
            self.idle.fetch_add(1, Ordering::SeqCst);
            while jobs.is_empty()
                && !self.cancelled.load(Ordering::Acquire)
                && self.outstanding.load(Ordering::Acquire) != 0
            {
                jobs = self
                    .ready
                    .wait(jobs)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            self.idle.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn reserve_or_spill(&self, parent: &mut Job<D>) {
        let reserved = self
            .open_jobs
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |open| {
                (open < MAX_OPEN_JOBS).then_some(open + 1)
            });
        if reserved.is_err() {
            parent.dir.take();
        }
    }

    fn release_open(&self, job: &Job<D>) {
        if job.dir.is_some() {
            self.open_jobs.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn share_oldest(&self, local: &mut VecDeque<Job<D>>) {
        if local.is_empty() || self.idle.load(Ordering::SeqCst) == 0 {
            return;
        }
        let mut jobs = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Nearest the root is likely to expose the most independent work.
        let Some(oldest) = local.pop_front() else {
            return;
        };
        jobs.push(oldest);
        if self.idle.load(Ordering::SeqCst) != 0 {
            self.ready.notify_one();
        }
    }

    fn finish_one(&self) {
        if self.outstanding.fetch_sub(1, Ordering::AcqRel) == 1 {
            // Synchronize with a worker about to wait, or it can miss this
            // wake.
            let _jobs = self
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.ready.notify_all();
        }
    }
}

fn run_guarded<V: EventVisitor>(walker: Walker<'_, V>, shared: &Shared<V::Dir>) -> V {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_worker(walker, shared))) {
        Ok(visitor) => visitor,
        Err(panic) => {
            shared.cancelled.store(true, Ordering::Release);
            let _jobs = shared
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            shared.ready.notify_all();
            std::panic::resume_unwind(panic);
        }
    }
}

fn run_worker<V: EventVisitor>(mut walker: Walker<'_, V>, shared: &Shared<V::Dir>) -> V {
    walker.visit.worker_started();
    let mut local = VecDeque::new();
    let Some(mut current) = shared.take() else {
        return walker.visit;
    };
    loop {
        if shared.cancelled.load(Ordering::Acquire) {
            return walker.visit;
        }
        match walker.process(current) {
            Some((mut parent, child)) => {
                if parent.next != parent.children.entries.len() {
                    shared.outstanding.fetch_add(1, Ordering::AcqRel);
                    shared.reserve_or_spill(&mut parent);
                    local.push_back(parent);
                    shared.share_oldest(&mut local);
                }
                current = child;
            }
            None => {
                shared.finish_one();
                shared.share_oldest(&mut local);
                if let Some(job) = local.pop_back() {
                    shared.release_open(&job);
                    current = job;
                } else if let Some(job) = shared.take() {
                    current = job;
                } else {
                    return walker.visit;
                }
            }
        }
    }
}

impl<V: EventVisitor> Walker<'_, V> {
    fn root_job(
        &mut self,
        root: &Path,
        global: Option<&str>,
        config: Config,
    ) -> Option<Job<V::Dir>> {
        #[cfg(test)]
        if let Some((op, error)) = inject(root, IoPoint::Root, Path::new("")) {
            self.fail(op, FaultContext::Root, error);
            return None;
        }
        let fd = match open_path(root, root_dir_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(error) => {
                if error == Errno::ACCESS {
                    // A root denied at open still has a catalog row, just as
                    // a denied child does. Follow the user's root symlink.
                    let observed = match open_path(
                        root,
                        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                        Mode::empty(),
                    ) {
                        Ok(fd) => {
                            fstat(&fd).inspect(|_| self.visit.observing(fd.as_fd(), Path::new("")))
                        }
                        Err(_) => statat(rustix::fs::CWD, root, AtFlags::empty()),
                    };
                    match observed {
                        Ok(stat) if file_type(&stat) == FileType::Directory => {
                            self.visit.root(public_stat(&stat, None));
                        }
                        Ok(_) => {}
                        Err(error) => {
                            self.fail(IoOp::Lstat, FaultContext::Root, io::Error::from(error))
                        }
                    }
                }
                self.fail(IoOp::OpenDir, FaultContext::Root, io::Error::from(error));
                return None;
            }
        };
        let root_stat = match fstat(&fd) {
            Ok(stat) => stat,
            Err(error) => {
                self.fail(IoOp::OpenDir, FaultContext::Root, io::Error::from(error));
                return None;
            }
        };
        self.root_id = Some((root_stat.st_dev, root_stat.st_ino));
        let token = self.visit.root(public_stat(&root_stat, None));
        let children = self.list(fd.as_fd(), FaultContext::Root)?;
        let loaded = self.load_ignores(fd.as_fd(), &children, false, token);
        let (rules, errors) = DirRules::root(root, global, loaded.files(), config);
        self.patterns(errors);
        self.entered(token, loaded.work_tree.as_ref(), children.count());
        Some(Job::new(
            fd,
            rules,
            children,
            self.rel.clone(),
            (root_stat.st_dev, root_stat.st_ino),
            token,
        ))
    }
}

fn root_dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC
}

fn child_dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn nofollow_file_flags() -> OFlags {
    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// `.ferretignore` and `info/exclude` follow a final symlink, as git does for
/// `exclude`.
fn ignore_flags() -> OFlags {
    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC
}

/// Git disregards a `.gitignore` that is a symlink (it warns "unable to
/// access"), so it is opened `O_NOFOLLOW` and a symlink reads as absent.
fn gitignore_flags() -> OFlags {
    ignore_flags() | OFlags::NOFOLLOW
}

fn link_flags() -> OFlags {
    OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn bytes_path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

const DOT_GIT: &str = ".git";

// ── walk state ──

struct Walker<'b, F> {
    root: PathBuf,
    root_id: Option<(u64, u64)>,
    boundaries: &'b BoundaryIndex,
    /// Root-relative path bytes. Empty at the root. Reused for every entry.
    rel: Vec<u8>,
    /// The `getdents` buffer, reused for every listing: empty, with capacity
    /// [`DENTS_BYTES`].
    dents: Vec<u8>,
    visit: F,
}

/// A worker's `getdents` buffer. 32 KiB holds several hundred entries of
/// typical name length, so most directories list in one call.
const DENTS_BYTES: usize = 32 << 10;

/// The directory whose entries are being considered.
struct Here<'a, D> {
    fd: BorrowedFd<'a>,
    rules: &'a DirRules,
    token: D,
}

struct Child {
    start: usize,
    end: usize,
    kind: FileType,
}

struct Children {
    names: Vec<u8>,
    entries: Vec<Child>,
    /// The listing reached its end. False after an error partway, when
    /// `entries` holds only what was read before it.
    complete: bool,
}

impl Children {
    /// The raw entry count for [`Event::Entered`]: `None` if the listing is
    /// incomplete, and saturated to fit a `u32`.
    fn count(&self) -> Option<u32> {
        self.complete
            .then(|| u32::try_from(self.entries.len()).unwrap_or(u32::MAX))
    }

    fn name(&self, child: &Child) -> &OsStr {
        OsStr::from_bytes(&self.names[child.start..child.end])
    }

    fn contains(&self, name: &str) -> bool {
        self.entries
            .iter()
            .any(|child| self.name(child).as_bytes() == name.as_bytes())
    }
}

struct Job<D> {
    dir: Option<OwnedFd>,
    rules: DirRules,
    children: Children,
    next: usize,
    rel: Vec<u8>,
    id: (u64, u64),
    token: D,
}

impl<D> Job<D> {
    fn new(
        dir: OwnedFd,
        rules: DirRules,
        children: Children,
        rel: Vec<u8>,
        id: (u64, u64),
        token: D,
    ) -> Self {
        Self {
            dir: Some(dir),
            rules,
            children,
            next: 0,
            rel,
            id,
            token,
        }
    }
}

/// [`WalkOptions::boundaries`] keyed by root-relative path bytes, so a
/// directory's check is one hash lookup however many boundaries there are.
/// Paths are joined from their normal components with `/`, the form of the
/// walker's own path buffer; one with a root, `..` or a prefix is not
/// root-relative and can never match, so it is left out.
#[derive(Default)]
struct BoundaryIndex {
    by_path: std::collections::HashMap<Vec<u8>, Vec<Option<(u64, u64)>>>,
}

impl BoundaryIndex {
    fn new(boundaries: &[Boundary]) -> Self {
        let mut by_path: std::collections::HashMap<_, Vec<_>> = std::collections::HashMap::new();
        'next: for boundary in boundaries {
            let mut key = Vec::new();
            for component in boundary.path.components() {
                match component {
                    Component::Normal(name) => {
                        if !key.is_empty() {
                            key.push(b'/');
                        }
                        key.extend_from_slice(name.as_bytes());
                    }
                    Component::CurDir => {}
                    Component::RootDir | Component::ParentDir | Component::Prefix(_) => {
                        continue 'next;
                    }
                }
            }
            if !key.is_empty() {
                by_path.entry(key).or_default().push(boundary.id);
            }
        }
        Self { by_path }
    }

    fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }

    /// The identities wanted at `rel`, `None` inside meaning any. Free when
    /// there are no boundaries.
    fn ids(&self, rel: &[u8]) -> Option<&[Option<(u64, u64)>]> {
        if self.by_path.is_empty() {
            return None;
        }
        self.by_path.get(rel).map(Vec::as_slice)
    }
}

#[cfg(test)]
pub(crate) type AfterOpen = Box<dyn FnMut(&Path)>;

#[cfg(test)]
thread_local! {
    /// Test seam: runs with a child directory's root-relative path after it is
    /// opened and before it is listed, on the worker that opened it. No
    /// visitor callback falls in that window, and it is the one a listing
    /// fault below the root needs. Compiled only into this crate's tests.
    pub(crate) static AFTER_OPEN: std::cell::RefCell<Option<AfterOpen>> =
        const { std::cell::RefCell::new(None) };
}

/// Shared, root-keyed syscall seam for coverage tests across real workers.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IoPoint {
    Root,
    Directory,
    OpenDirectory,
    Child,
    Listing(usize),
}
#[cfg(test)]
pub(crate) type IoHook =
    std::sync::Arc<dyn Fn(IoPoint, &Path) -> Option<(IoOp, io::Error)> + Send + Sync>;
#[cfg(test)]
pub(crate) static IO_HOOKS: std::sync::Mutex<Vec<(PathBuf, IoHook)>> =
    std::sync::Mutex::new(Vec::new());
#[cfg(test)]
fn inject(root: &Path, point: IoPoint, rel: &Path) -> Option<(IoOp, io::Error)> {
    let hook = IO_HOOKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|(r, _)| r == root)
        .map(|(_, h)| h.clone());
    hook.and_then(|h| h(point, rel))
}

#[cfg(test)]
pub(crate) type FailReadlink = Box<dyn Fn(&OsStr) -> bool>;

#[cfg(test)]
pub(crate) type FailList = Box<dyn Fn(&Path) -> Option<io::Error>>;

#[cfg(test)]
thread_local! {
    /// Injects a listing error into the real walker on the calling thread.
    pub(crate) static FAIL_LIST: std::cell::RefCell<Option<FailList>> =
        const { std::cell::RefCell::new(None) };
    /// Test seam: makes `readlink` fail for the names it accepts. The walker
    /// reads a link through the `O_PATH` descriptor it just statted, which no
    /// unprivileged test can make fail. Like [`AFTER_OPEN`], it reaches only
    /// walks on the calling thread (one worker).
    pub(crate) static FAIL_READLINK: std::cell::RefCell<Option<FailReadlink>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn after_open(rel: &[u8]) {
    AFTER_OPEN.with_borrow_mut(|hook| {
        if let Some(hook) = hook {
            hook(bytes_path(rel));
        }
    });
}

/// A parent's continuation, and the child directory to walk next.
type Descent<D> = (Job<D>, Job<D>);

/// What `.git` is. A symlink is [`GitProbe::Present`]: nothing is opened
/// through it. A real directory keeps the descriptor from the probe so
/// `info/exclude` cannot be redirected by a later swap of the name.
enum GitProbe {
    Missing,
    Directory(OwnedFd),
    /// Bytes of a regular `.git` file, already read from the opened inode.
    File(Vec<u8>),
    /// A symlink or other non-regular entry. Still starts a work tree
    /// (so `.gitignore` applies) but contributes no exclude.
    Present,
}

impl GitProbe {
    fn is_root(&self) -> bool {
        !matches!(self, Self::Missing)
    }
}

/// A [`WorkTree`] held until [`Event::Entered`] lends it. One per work tree,
/// so the path allocation is not per entry.
struct FoundWorkTree {
    kind: WorkTreeKind,
    common_dir: PathBuf,
    common_id: (u64, u64),
}

struct Ignores {
    ferretignore: Option<String>,
    gitignore: Option<String>,
    git_root: bool,
    git_exclude: Option<String>,
    work_tree: Option<FoundWorkTree>,
}

impl Ignores {
    fn files(&self) -> IgnoreFiles<'_> {
        IgnoreFiles {
            ferretignore: self.ferretignore.as_deref(),
            gitignore: self.gitignore.as_deref(),
            git_root: self.git_root,
            git_exclude: self.git_exclude.as_deref(),
        }
    }
}

enum Opened {
    Missing,
    NotRegular,
    Bytes(Vec<u8>),
}

impl<'b, V: EventVisitor> Walker<'b, V> {
    fn new(root: &Path, boundaries: &'b BoundaryIndex, visit: V) -> Self {
        Self {
            root: root.to_path_buf(),
            root_id: None,
            boundaries,
            rel: Vec::with_capacity(256),
            dents: Vec::with_capacity(DENTS_BYTES),
            visit,
        }
    }

    fn push(&mut self, name: impl AsRef<OsStr>) -> usize {
        let length = self.rel.len();
        if length != 0 {
            self.rel.push(b'/');
        }
        self.rel.extend_from_slice(name.as_ref().as_bytes());
        length
    }

    fn pop(&mut self, length: usize) {
        self.rel.truncate(length);
    }

    fn fail(&mut self, op: IoOp, context: FaultContext<'_, V::Dir>, error: io::Error) {
        self.visit.visit(Event::Io {
            path: bytes_path(&self.rel),
            op,
            context,
            error,
        });
    }

    /// A fault on entry `name` of the directory `parent`.
    fn fail_child(&mut self, op: IoOp, parent: V::Dir, name: &OsStr, error: io::Error) {
        self.fail(op, FaultContext::Child { parent, name }, error);
    }

    fn patterns(&mut self, errors: Vec<PatternError>) {
        for error in errors {
            self.visit.visit(Event::Pattern(error));
        }
    }

    fn entered(&mut self, dir: V::Dir, work_tree: Option<&FoundWorkTree>, entries: Option<u32>) {
        let work_tree = work_tree.map(|found| WorkTree {
            kind: found.kind,
            common_dir: &found.common_dir,
            common_id: found.common_id,
        });
        self.visit.visit(Event::Entered {
            dir,
            work_tree,
            entries,
        });
        #[cfg(test)]
        if let Some((op, error)) = inject(&self.root, IoPoint::Directory, bytes_path(&self.rel)) {
            self.fail(op, FaultContext::Dir(dir), error);
        }
    }

    fn emit(
        &mut self,
        here: &Here<'_, V::Dir>,
        name: &OsStr,
        decision: Decision,
        stat: Option<Stat<'_>>,
        kind: FileType,
    ) -> Option<V::Dir> {
        self.visit.visit(Event::Decided(Decided {
            parent: here.token,
            parent_fd: here.fd,
            name,
            path: bytes_path(&self.rel),
            decision,
            stat,
            kind: catalog_kind(kind),
        }))
    }

    fn emit_skip(&mut self, here: &Here<'_, V::Dir>, name: &OsStr, kind: FileType) {
        self.emit(here, name, Decision::Skip, None, kind);
    }

    fn emit_stat(
        &mut self,
        here: &Here<'_, V::Dir>,
        name: &OsStr,
        decision: Decision,
        stat: &rustix::fs::Stat,
        target: Option<&OsStr>,
    ) -> Option<V::Dir> {
        self.emit(
            here,
            name,
            decision,
            Some(public_stat(stat, target)),
            file_type(stat),
        )
    }

    /// Lists `dir` into the worker's `getdents` buffer. The listing stops at
    /// the first error. An error with no entry yet is one fault and `None`, so
    /// ignore files are not probed for a directory that could not be listed.
    /// An error after some entries is reported and the entries already read
    /// are kept.
    ///
    /// `ENOENT` is an error here: the kernel returns it for a directory
    /// unlinked since it was opened, and for a `/proc` directory whose process
    /// is gone. (`rustix::fs::Dir` reads it as the end of the listing, which
    /// would report a vanished directory as entered and empty.) Callers treat
    /// it as uncertain coverage like any other listing fault (D26 A′): it says
    /// the opened directory is gone, not that its path is.
    fn list(&mut self, dir: BorrowedFd<'_>, context: FaultContext<'_, V::Dir>) -> Option<Children> {
        self.visit.observing(dir, bytes_path(&self.rel));
        #[cfg(test)]
        if let Some(error) =
            FAIL_LIST.with_borrow(|hook| hook.as_ref().and_then(|hook| hook(bytes_path(&self.rel))))
        {
            self.fail(IoOp::List, context, error);
            return None;
        }
        let mut children = Children {
            names: Vec::new(),
            entries: Vec::new(),
            complete: true,
        };
        let mut dents = std::mem::take(&mut self.dents);
        let mut raw = RawDir::new(dir, dents.spare_capacity_mut());
        while let Some(item) = raw.next() {
            match item {
                Ok(entry) => {
                    let bytes = entry.file_name().to_bytes();
                    if bytes == b"." || bytes == b".." {
                        continue;
                    }
                    let start = children.names.len();
                    children.names.extend_from_slice(bytes);
                    children.entries.push(Child {
                        start,
                        end: children.names.len(),
                        kind: entry.file_type(),
                    });
                    #[cfg(test)]
                    if let Some((op, error)) = inject(
                        &self.root,
                        IoPoint::Listing(children.entries.len()),
                        bytes_path(&self.rel),
                    ) {
                        self.fail(op, context, error);
                        children.complete = false;
                        break;
                    }
                }
                Err(error) => {
                    self.fail(IoOp::List, context, io::Error::from(error));
                    if error == Errno::ACCESS {
                        // A denied directory is a catalogued opaque row,
                        // even if the filesystem returned a partial batch.
                        children.entries.clear();
                        children.names.clear();
                    }
                    children.complete = false;
                    break;
                }
            }
        }
        self.dents = dents;
        if !children.complete && children.entries.is_empty() {
            None
        } else {
            Some(children)
        }
    }

    fn process(&mut self, mut job: Job<V::Dir>) -> Option<Descent<V::Dir>> {
        self.rel.clone_from(&job.rel);
        if job.dir.is_none() {
            job.dir = self.reopen(bytes_path(&job.rel), job.id, job.token);
        }
        let here = Here {
            fd: job.dir.as_ref()?.as_fd(),
            rules: &job.rules,
            token: job.token,
        };
        while job.next < job.children.entries.len() {
            let child = &job.children.entries[job.next];
            let name = job.children.name(child);
            job.next += 1;
            let length = self.push(name);
            let descended = if self.visit.consider(here.token, name, bytes_path(&self.rel)) {
                self.consider(&here, name, child.kind)
            } else {
                None
            };
            self.pop(length);
            if let Some(descended) = descended {
                return Some((job, descended));
            }
        }
        None
    }

    /// Reopen a spilled continuation one component at a time. Only the
    /// configured root is opened by path; every component below it uses
    /// `O_NOFOLLOW`. The root and final inode must match the first pass.
    fn reopen(&mut self, rel: &Path, expected: (u64, u64), token: V::Dir) -> Option<OwnedFd> {
        let context = FaultContext::Dir(token);
        let root = match open_path(&self.root, root_dir_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(error) => {
                self.fail(IoOp::Reopen, context, io::Error::from(error));
                return None;
            }
        };
        let Some(root_id) = self.root_id else {
            self.fail(
                IoOp::Reopen,
                context,
                io::Error::new(io::ErrorKind::InvalidData, "missing root identity"),
            );
            return None;
        };
        let mut fd = root;
        if !self.same_dir(&fd, root_id, token) {
            return None;
        }
        for name in rel.iter() {
            fd = match openat(&fd, name, child_dir_flags(), Mode::empty()) {
                Ok(next) => next,
                Err(error) => {
                    self.fail(IoOp::Reopen, context, io::Error::from(error));
                    return None;
                }
            };
        }
        self.same_dir(&fd, expected, token).then_some(fd)
    }

    fn same_dir(&mut self, fd: &OwnedFd, expected: (u64, u64), token: V::Dir) -> bool {
        let context = FaultContext::Dir(token);
        match fstat(fd) {
            Ok(stat) if (stat.st_dev, stat.st_ino) == expected => true,
            Ok(_) => {
                self.fail(
                    IoOp::Reopen,
                    context,
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "directory changed before resume",
                    ),
                );
                false
            }
            Err(error) => {
                self.fail(IoOp::Reopen, context, io::Error::from(error));
                false
            }
        }
    }

    fn consider(
        &mut self,
        here: &Here<'_, V::Dir>,
        name: &OsStr,
        kind: FileType,
    ) -> Option<Job<V::Dir>> {
        #[cfg(test)]
        if let Some((op, error)) = inject(&self.root, IoPoint::Child, bytes_path(&self.rel)) {
            self.fail(
                op,
                FaultContext::Child {
                    parent: here.token,
                    name,
                },
                error,
            );
            return None;
        }
        match kind {
            FileType::RegularFile | FileType::Unknown => {
                let stat = self.stat_child(here, name)?;
                self.decide_statted(here, name, stat)
            }
            FileType::Symlink => {
                let decision = here.rules.decide(bytes_path(&self.rel), Entry::Symlink);
                if decision == Decision::Skip {
                    self.emit_skip(here, name, kind);
                    return None;
                }
                self.finish_link(here, name, decision)
            }
            FileType::Directory => self.consider_dir(here, name),
            _ => {
                if here.rules.decide(bytes_path(&self.rel), Entry::Other) == Decision::Skip {
                    self.emit_skip(here, name, kind);
                    return None;
                }
                let stat = self.stat_child(here, name)?;
                self.decide_statted(here, name, stat)
            }
        }
    }

    fn consider_dir(&mut self, here: &Here<'_, V::Dir>, name: &OsStr) -> Option<Job<V::Dir>> {
        let mut decision = here.rules.decide(bytes_path(&self.rel), Entry::Dir);
        // A boundary is checked whatever the policy says here: the inner root
        // owns it (D34). Only a name that could be one is statted when
        // skipped.
        if decision == Decision::Skip && !self.may_be_boundary() {
            self.emit_skip(here, name, FileType::Directory);
            return None;
        }
        let stat = self.stat_child(here, name)?;
        if self.at_boundary(here, name, &stat) {
            return None;
        }
        if decision == Decision::Skip {
            self.emit_skip(here, name, file_type(&stat));
            return None;
        }
        let seen = entry_from_stat(&stat);
        if seen != Entry::Dir {
            decision = here.rules.decide(bytes_path(&self.rel), seen);
            if decision == Decision::Skip {
                self.emit_skip(here, name, file_type(&stat));
                return None;
            }
            if seen == Entry::Symlink {
                return self.finish_link(here, name, decision);
            }
        }
        let token = self.emit_stat(here, name, decision, &stat, None);
        self.follow(here, name, decision, &stat, token)
    }

    /// `stat` came from `statat` (`d_type` was a file, unknown, or disagreed).
    /// A symlink result is re-opened so the published stat and the target are
    /// one observation.
    fn decide_statted(
        &mut self,
        here: &Here<'_, V::Dir>,
        name: &OsStr,
        stat: rustix::fs::Stat,
    ) -> Option<Job<V::Dir>> {
        if self.at_boundary(here, name, &stat) {
            return None;
        }
        let entry = entry_from_stat(&stat);
        let decision = here.rules.decide(bytes_path(&self.rel), entry);
        if decision == Decision::Skip {
            self.emit_skip(here, name, file_type(&stat));
            return None;
        }
        if entry == Entry::Symlink {
            return self.finish_link(here, name, decision);
        }
        let token = self.emit_stat(here, name, decision, &stat, None);
        self.follow(here, name, decision, &stat, token)
    }

    /// Opens `name` with `O_PATH | O_NOFOLLOW`. The stat and, when it is a
    /// symlink, the target come from that descriptor.
    fn finish_link(
        &mut self,
        here: &Here<'_, V::Dir>,
        name: &OsStr,
        decision: Decision,
    ) -> Option<Job<V::Dir>> {
        let (stat, target) = match observe_link(here.fd, name) {
            Ok(pair) => pair,
            Err((op, error)) => {
                self.fail_child(op, here.token, name, error);
                return None;
            }
        };
        let seen = entry_from_stat(&stat);
        if seen != Entry::Symlink {
            if self.at_boundary(here, name, &stat) {
                return None;
            }
            let decision = here.rules.decide(bytes_path(&self.rel), seen);
            if decision == Decision::Skip {
                self.emit_skip(here, name, file_type(&stat));
                return None;
            }
            let token = self.emit_stat(here, name, decision, &stat, None);
            return self.follow(here, name, decision, &stat, token);
        }
        let Some(target) = target else {
            self.fail_child(
                IoOp::Readlink,
                here.token,
                name,
                io::Error::new(io::ErrorKind::InvalidData, "symlink has no target"),
            );
            return None;
        };
        self.emit_stat(here, name, decision, &stat, Some(target.as_os_str()));
        None
    }

    fn stat_child(&mut self, here: &Here<'_, V::Dir>, name: &OsStr) -> Option<rustix::fs::Stat> {
        match statat(here.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Some(stat),
            Err(error) => {
                self.fail_child(IoOp::Lstat, here.token, name, io::Error::from(error));
                None
            }
        }
    }

    // ── boundaries ──

    /// Whether the current path is some boundary's, before its identity is
    /// known. Free when there are no boundaries.
    fn may_be_boundary(&self) -> bool {
        self.boundaries.ids(&self.rel).is_some()
    }

    /// Reports [`Event::Boundary`] when the current path is a boundary and
    /// `stat` is a directory with that boundary's identity, if it gives one.
    fn at_boundary(
        &mut self,
        here: &Here<'_, V::Dir>,
        name: &OsStr,
        stat: &rustix::fs::Stat,
    ) -> bool {
        if self.boundaries.is_empty() || file_type(stat) != FileType::Directory {
            return false;
        }
        let id = (stat.st_dev, stat.st_ino);
        let hit = self
            .boundaries
            .ids(&self.rel)
            .is_some_and(|ids| ids.iter().any(|want| want.is_none_or(|want| want == id)));
        if hit {
            let path = bytes_path(&self.rel);
            self.visit.visit(Event::Boundary {
                parent: here.token,
                name,
                path,
            });
        }
        hit
    }

    // ── entering ──

    /// `token` is what the visitor returned for this directory's `Decided`;
    /// `None` prunes it.
    fn follow(
        &mut self,
        here: &Here<'_, V::Dir>,
        name: &OsStr,
        decision: Decision,
        expected: &rustix::fs::Stat,
        token: Option<V::Dir>,
    ) -> Option<Job<V::Dir>> {
        match decision {
            Decision::Descend => self.enter_and_list(here, name, expected, token?),
            Decision::Traverse => self.traverse_and_list(here, name, expected, token?),
            Decision::Skip | Decision::Catalog(_) | Decision::Index => None,
        }
    }

    fn enter_and_list(
        &mut self,
        here: &Here<'_, V::Dir>,
        name: &OsStr,
        expected: &rustix::fs::Stat,
        token: V::Dir,
    ) -> Option<Job<V::Dir>> {
        let child = self.open_child(here, name, expected)?;
        #[cfg(test)]
        after_open(&self.rel);
        let children = self.list(child.as_fd(), FaultContext::Dir(token))?;
        let loaded = self.load_ignores(child.as_fd(), &children, here.rules.in_work_tree(), token);
        let (rules, errors) = here.rules.enter(name, loaded.files());
        self.patterns(errors);
        self.entered(token, loaded.work_tree.as_ref(), children.count());
        Some(Job::new(
            child,
            rules,
            children,
            self.rel.clone(),
            (expected.st_dev, expected.st_ino),
            token,
        ))
    }

    fn traverse_and_list(
        &mut self,
        here: &Here<'_, V::Dir>,
        name: &OsStr,
        expected: &rustix::fs::Stat,
        token: V::Dir,
    ) -> Option<Job<V::Dir>> {
        let child = self.open_child(here, name, expected)?;
        #[cfg(test)]
        after_open(&self.rel);
        let children = self.list(child.as_fd(), FaultContext::Dir(token))?;
        let rules = here.rules.traverse(name);
        self.entered(token, None, children.count());
        Some(Job::new(
            child,
            rules,
            children,
            self.rel.clone(),
            (expected.st_dev, expected.st_ino),
            token,
        ))
    }

    /// `O_DIRECTORY | O_NOFOLLOW`, then `fstat`. A symlink is `ELOOP`. A
    /// different inode than `expected` is a fault either way: the `Decided`
    /// event already described the inode `decide` saw.
    fn open_child(
        &mut self,
        here: &Here<'_, V::Dir>,
        name: &OsStr,
        expected: &rustix::fs::Stat,
    ) -> Option<OwnedFd> {
        #[cfg(test)]
        if let Some((op, error)) = inject(&self.root, IoPoint::OpenDirectory, bytes_path(&self.rel))
        {
            self.fail_child(op, here.token, name, error);
            return None;
        }
        let fd = match openat(here.fd, name, child_dir_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(error) => {
                self.fail_child(IoOp::OpenDir, here.token, name, io::Error::from(error));
                return None;
            }
        };
        match fstat(&fd) {
            Ok(stat) if stat.st_dev == expected.st_dev && stat.st_ino == expected.st_ino => {}
            Ok(_) => {
                self.fail_child(
                    IoOp::OpenDir,
                    here.token,
                    name,
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "directory changed between stat and open",
                    ),
                );
                return None;
            }
            Err(error) => {
                self.fail_child(IoOp::OpenDir, here.token, name, io::Error::from(error));
                return None;
            }
        }
        Some(fd)
    }

    // ── ignore files ──

    /// Only names in the listing are opened: a directory without ignore files,
    /// the usual case, costs no failed `open`s.
    ///
    /// `in_work_tree` is the parent directory. The root passes `false`.
    /// `.git`, when it is a directory, is opened here and that descriptor is
    /// what `info/exclude` is read from, including across the `.gitignore`
    /// callback below. `token` is the directory being loaded.
    fn load_ignores(
        &mut self,
        dir: BorrowedFd<'_>,
        children: &Children,
        in_work_tree: bool,
        token: V::Dir,
    ) -> Ignores {
        self.visit.policy_input(dir, OsStr::new(".ferretignore"));
        self.visit.policy_input(dir, OsStr::new(DOT_GIT));
        if in_work_tree {
            self.visit.policy_input(dir, OsStr::new(".gitignore"));
        }
        let listed = |name: &str| children.contains(name);
        let ferretignore = listed(".ferretignore")
            .then(|| self.read_named(dir, ".ferretignore", token))
            .flatten();
        let git = if listed(DOT_GIT) {
            self.probe_git(dir, token)
        } else {
            GitProbe::Missing
        };
        // A `.gitignore` outside a work tree cannot affect decisions, so it is
        // not opened. A FIFO of that name must not stall a walk that is not in
        // a repository.
        if git.is_root() {
            self.visit.policy_input(dir, OsStr::new(".gitignore"));
        }
        let gitignore = if (in_work_tree || git.is_root()) && listed(".gitignore") {
            self.read_named(dir, ".gitignore", token)
        } else {
            None
        };
        let git_root = git.is_root();
        let (work_tree, git_exclude) = match git {
            GitProbe::Directory(fd) => {
                let work_tree = self.main_work_tree(&fd, token);
                (work_tree, self.read_exclude(fd, token))
            }
            GitProbe::File(bytes) => self.read_gitfile(dir, &bytes, in_work_tree, token),
            GitProbe::Missing | GitProbe::Present => (None, None),
        };
        Ignores {
            ferretignore,
            gitignore,
            git_root,
            git_exclude,
            work_tree,
        }
    }

    fn read_named(&mut self, dir: BorrowedFd<'_>, name: &str, token: V::Dir) -> Option<String> {
        self.visit.policy_input(dir, OsStr::new(name));
        let length = self.push(name);
        let text = match open_ignore(dir, OsStr::new(name)) {
            Ok(Opened::Bytes(bytes)) => Some(decode_lossy(bytes)),
            Ok(Opened::Missing | Opened::NotRegular) => None,
            Err(error) => {
                self.fail_child(IoOp::ReadIgnore, token, OsStr::new(name), error);
                None
            }
        };
        self.pop(length);
        text
    }

    /// `info` is opened `O_NOFOLLOW` relative to the held git directory (or
    /// common directory). `exclude` is an ordinary ignore file: a symlink of
    /// that name is followed.
    fn read_exclude(&mut self, git: OwnedFd, token: V::Dir) -> Option<String> {
        self.visit.policy_input(git.as_fd(), OsStr::new("info"));
        let git_length = self.push(DOT_GIT);
        let info_length = self.push("info");
        let info = match openat(git.as_fd(), "info", child_dir_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => {
                self.pop(git_length);
                return None;
            }
            Err(error) => {
                self.push("exclude");
                self.fail_child(
                    IoOp::ReadIgnore,
                    token,
                    OsStr::new(DOT_GIT),
                    io::Error::from(error),
                );
                self.pop(git_length);
                return None;
            }
        };
        drop(git);
        self.visit.policy_input(info.as_fd(), OsStr::new("exclude"));
        self.push("exclude");
        let opened = match openat(info.as_fd(), "exclude", ignore_flags(), Mode::empty()) {
            Ok(fd) => {
                drop(info);
                read_opened(fd)
            }
            Err(Errno::NOENT) => Ok(Opened::Missing),
            Err(error) => Err(io::Error::from(error)),
        };
        let text = match opened {
            Ok(Opened::Bytes(bytes)) => Some(decode_lossy(bytes)),
            Ok(Opened::Missing | Opened::NotRegular) => None,
            Err(error) => {
                self.fail_child(IoOp::ReadIgnore, token, OsStr::new(DOT_GIT), error);
                None
            }
        };
        self.pop(info_length);
        self.pop(git_length);
        text
    }

    fn probe_git(&mut self, dir: BorrowedFd<'_>, token: V::Dir) -> GitProbe {
        match openat(dir, DOT_GIT, child_dir_flags(), Mode::empty()) {
            Ok(fd) => GitProbe::Directory(fd),
            Err(Errno::NOENT) => GitProbe::Missing,
            Err(Errno::LOOP) => GitProbe::Present,
            Err(Errno::NOTDIR) => self.probe_git_file(dir, token),
            Err(error) => self.probe_git_failed(dir, error, token),
        }
    }

    /// The directory open failed for a reason other than "not a directory"
    /// or "a symlink". A directory we cannot search still starts a work
    /// tree; a regular file is read below.
    fn probe_git_failed(&mut self, dir: BorrowedFd<'_>, error: Errno, token: V::Dir) -> GitProbe {
        match statat(dir, DOT_GIT, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if file_type(&stat) == FileType::RegularFile => {
                self.probe_git_file(dir, token)
            }
            Ok(stat) if file_type(&stat) == FileType::Directory => {
                self.fail_at_git(io::Error::from(error), token);
                GitProbe::Present
            }
            Ok(_) => GitProbe::Present,
            Err(Errno::NOENT) => GitProbe::Missing,
            Err(stat_err) => {
                self.fail_at_git(io::Error::from(stat_err), token);
                GitProbe::Missing
            }
        }
    }

    /// `.git` is not a directory. `O_NOFOLLOW` so a symlink that appeared
    /// since the listing is not a gitdir file.
    fn probe_git_file(&mut self, dir: BorrowedFd<'_>, token: V::Dir) -> GitProbe {
        match openat(dir, DOT_GIT, nofollow_file_flags(), Mode::empty()) {
            Ok(fd) => match read_regular(fd) {
                Ok(Some(bytes)) => GitProbe::File(bytes),
                Ok(None) => GitProbe::Present,
                Err(error) => {
                    self.fail_at_git(error, token);
                    GitProbe::Present
                }
            },
            Err(Errno::NOENT) => GitProbe::Missing,
            Err(Errno::LOOP) => GitProbe::Present,
            Err(error) => match statat(dir, DOT_GIT, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) if file_type(&stat) == FileType::RegularFile => {
                    self.fail_at_git(io::Error::from(error), token);
                    GitProbe::Present
                }
                Ok(_) => GitProbe::Present,
                Err(Errno::NOENT) => GitProbe::Missing,
                Err(stat_err) => {
                    self.fail_at_git(io::Error::from(stat_err), token);
                    GitProbe::Missing
                }
            },
        }
    }

    /// A `.git` directory: the work tree is main and `.git` is its common
    /// directory.
    fn main_work_tree(&mut self, git: &OwnedFd, token: V::Dir) -> Option<FoundWorkTree> {
        let common_id = self.git_identity(git, token)?;
        Some(FoundWorkTree {
            kind: WorkTreeKind::Main,
            common_dir: normalize(&self.here_path().join(DOT_GIT)),
            common_id,
        })
    }

    /// Follow git's `gitdir:` and optional `commondir` from held descriptors.
    /// A relative path may include `..`, as git's format requires. An
    /// absolute gitdir is opened by path because it is outside the tree.
    fn read_gitfile(
        &mut self,
        work: BorrowedFd<'_>,
        bytes: &[u8],
        in_work_tree: bool,
        token: V::Dir,
    ) -> (Option<FoundWorkTree>, Option<String>) {
        let Some(raw) = parse_gitdir(bytes) else {
            self.fail_at_git(
                io::Error::new(io::ErrorKind::InvalidData, "not a `gitdir:` file"),
                token,
            );
            return (None, None);
        };
        let Some(gitdir) = self.open_git_directory(work, raw, token) else {
            return (None, None);
        };
        let gitdir_path = self.here_path().join(raw);
        let Some((common, commondir)) = self.common_dir(gitdir, token) else {
            return (None, None);
        };
        let (kind, common_path) = match commondir {
            Some(text) => (WorkTreeKind::Linked, gitdir_path.join(text)),
            None if in_work_tree => (WorkTreeKind::Submodule, gitdir_path),
            None => (WorkTreeKind::Main, gitdir_path),
        };
        let work_tree = self
            .git_identity(&common, token)
            .map(|common_id| FoundWorkTree {
                kind,
                common_dir: normalize(&common_path),
                common_id,
            });
        (work_tree, self.read_exclude(common, token))
    }

    /// The directory being loaded, as a path: the root the caller gave, made
    /// absolute, joined with the root-relative path. Only for work trees.
    fn here_path(&self) -> PathBuf {
        let root = std::path::absolute(&self.root).unwrap_or_else(|_| self.root.clone());
        if self.rel.is_empty() {
            root
        } else {
            root.join(bytes_path(&self.rel))
        }
    }

    fn git_identity(&mut self, git: &OwnedFd, token: V::Dir) -> Option<(u64, u64)> {
        match fstat(git) {
            Ok(stat) => Some((stat.st_dev, stat.st_ino)),
            Err(error) => {
                self.fail_at_git(io::Error::from(error), token);
                None
            }
        }
    }

    /// Symlinks are followed, as git follows them: the gitdir's text can
    /// already name any directory, so `O_NOFOLLOW` here would protect nothing.
    /// What the handle protects is `base`, which a rename cannot redirect.
    fn open_git_directory(
        &mut self,
        base: BorrowedFd<'_>,
        raw: &OsStr,
        token: V::Dir,
    ) -> Option<OwnedFd> {
        let path = Path::new(raw);
        let dependency = if path.is_absolute() {
            path.to_owned()
        } else {
            PathBuf::from(format!(
                "/proc/{}/fd/{}",
                std::process::id(),
                base.as_raw_fd()
            ))
            .join(path)
        };
        self.visit.policy_path(&dependency);
        let opened = if path.is_absolute() {
            open_path(path, root_dir_flags(), Mode::empty())
        } else {
            openat(base, path, root_dir_flags(), Mode::empty())
        };
        // A missing gitdir or common directory is a fault like any other: the
        // `.git` file says a work tree starts here, and its exclude rules and
        // work-tree record are lost without it.
        match opened {
            Ok(fd) => Some(fd),
            Err(error) => {
                self.fail_at_git(io::Error::from(error), token);
                None
            }
        }
    }

    /// The gitdir itself when it has no `commondir` file, with `None` as the
    /// second value; otherwise the directory `commondir` names and its text.
    /// `None`, after a [`IoOp::ProbeGit`] fault, when that file cannot be
    /// read, is not a regular file, names nothing, or names a directory that
    /// cannot be opened: a linked work tree's own directory is not where
    /// exclude lives, so it is not a fallback. Only a missing `commondir`
    /// means the gitdir is the common directory.
    fn common_dir(
        &mut self,
        gitdir: OwnedFd,
        token: V::Dir,
    ) -> Option<(OwnedFd, Option<OsString>)> {
        self.visit
            .policy_input(gitdir.as_fd(), OsStr::new("commondir"));
        let opened = match openat(
            gitdir.as_fd(),
            "commondir",
            nofollow_file_flags(),
            Mode::empty(),
        ) {
            Ok(fd) => read_opened(fd),
            Err(Errno::NOENT) => Ok(Opened::Missing),
            Err(error) => Err(io::Error::from(error)),
        };
        match opened {
            Ok(Opened::Missing) => Some((gitdir, None)),
            Ok(Opened::NotRegular) => {
                self.fail_at_git(
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "commondir is not a regular file",
                    ),
                    token,
                );
                None
            }
            Ok(Opened::Bytes(bytes)) => {
                let Some(raw) = first_line(&bytes) else {
                    self.fail_at_git(
                        io::Error::new(io::ErrorKind::InvalidData, "commondir is empty"),
                        token,
                    );
                    return None;
                };
                let common = self.open_git_directory(gitdir.as_fd(), raw, token)?;
                drop(gitdir);
                Some((common, Some(raw.to_os_string())))
            }
            Err(error) => {
                self.fail_at_git(error, token);
                None
            }
        }
    }

    fn fail_at_git(&mut self, error: io::Error, token: V::Dir) {
        let length = self.push(DOT_GIT);
        self.fail_child(IoOp::ProbeGit, token, OsStr::new(DOT_GIT), error);
        self.pop(length);
    }
}

/// `.` and `..` resolved lexically; `..` at the top stays at the top. Git
/// writes gitdir and `commondir` as real paths, so a symlink that makes `..`
/// mean something else is not expected there; [`WorkTree::common_id`] is the
/// identity either way.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// `O_PATH | O_NOFOLLOW`, then `fstat` and, for a symlink, `readlinkat` of
/// that descriptor. `Ok`'s stat is the inode the descriptor refers to, which
/// may no longer be a symlink; then the target is `None`. An error says which
/// step failed.
fn observe_link(
    dir: BorrowedFd<'_>,
    name: &OsStr,
) -> Result<(rustix::fs::Stat, Option<OsString>), (IoOp, io::Error)> {
    let lstat = |error: Errno| (IoOp::Lstat, io::Error::from(error));
    let fd = openat(dir, name, link_flags(), Mode::empty()).map_err(lstat)?;
    let stat = fstat(&fd).map_err(lstat)?;
    if file_type(&stat) != FileType::Symlink {
        return Ok((stat, None));
    }
    // An empty path reads the link the `O_PATH` descriptor already refers
    // to (Linux 2.6.39), so the target cannot be a different inode from
    // `stat`.
    #[cfg(test)]
    if FAIL_READLINK.with_borrow(|fail| fail.as_ref().is_some_and(|fail| fail(name))) {
        return Err((
            IoOp::Readlink,
            io::Error::other("injected readlink failure"),
        ));
    }
    let raw = readlinkat(&fd, "", Vec::new())
        .map_err(|error| (IoOp::Readlink, io::Error::from(error)))?;
    Ok((stat, Some(OsString::from_vec(raw.into_bytes()))))
}

/// Opens ignore file `name`: `.gitignore` without following a symlink, which
/// then reads as absent; any other name through one.
fn open_ignore(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<Opened> {
    let flags = if name == ".gitignore" {
        gitignore_flags()
    } else {
        ignore_flags()
    };
    match openat(dir, name, flags, Mode::empty()) {
        Ok(fd) => read_opened(fd),
        Err(Errno::NOENT) => Ok(Opened::Missing),
        Err(Errno::LOOP) if flags.contains(OFlags::NOFOLLOW) => Ok(Opened::Missing),
        Err(error) => Err(io::Error::from(error)),
    }
}

fn read_opened(fd: OwnedFd) -> io::Result<Opened> {
    match read_regular(fd)? {
        Some(bytes) => Ok(Opened::Bytes(bytes)),
        None => Ok(Opened::NotRegular),
    }
}

/// Bytes of `fd` when it is a regular file of at most [`MAX_IGNORE_BYTES`].
///
/// `Ok(None)` means the opened inode is not a regular file. The check is
/// `fstat` on the descriptor that was opened, not a prior `lstat`.
fn read_regular(fd: OwnedFd) -> io::Result<Option<Vec<u8>>> {
    let file = File::from(fd);
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Ok(None);
    }
    if meta.len() > MAX_IGNORE_BYTES {
        return Err(ignore_too_large());
    }
    // Size the buffer from the stat. An empty `Vec` makes `read_to_end` grow
    // by small steps, one `read` each, on every ignore file.
    let mut bytes = Vec::new();
    let len = usize::try_from(meta.len()).unwrap_or(0);
    let _ = bytes.try_reserve_exact(len);
    file.take(MAX_IGNORE_BYTES + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_IGNORE_BYTES {
        return Err(ignore_too_large());
    }
    Ok(Some(bytes))
}

fn ignore_too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::FileTooLarge,
        "ignore file is larger than 1 MiB",
    )
}

/// First line of a gitdir or commondir file, without its newline. Spaces are
/// part of the path: git does not trim them.
fn first_line(bytes: &[u8]) -> Option<&OsStr> {
    let line = bytes.split(|byte| *byte == b'\n').next()?;
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.is_empty() {
        None
    } else {
        Some(OsStr::from_bytes(line))
    }
}

/// `gitdir: <path>` from the first line. The path is relative to the directory
/// that holds the `.git` file. Anything else is not a gitdir pointer.
fn parse_gitdir(bytes: &[u8]) -> Option<&OsStr> {
    let raw = first_line(bytes)?.as_bytes().strip_prefix(b"gitdir: ")?;
    if raw.is_empty() {
        None
    } else {
        Some(OsStr::from_bytes(raw))
    }
}

fn file_type(stat: &rustix::fs::Stat) -> FileType {
    FileType::from_raw_mode(stat.st_mode)
}

fn entry_from_stat(stat: &rustix::fs::Stat) -> Entry {
    match file_type(stat) {
        FileType::Symlink => Entry::Symlink,
        FileType::Directory => Entry::Dir,
        FileType::RegularFile => Entry::File {
            size: u64::try_from(stat.st_size).unwrap_or(0),
        },
        _ => Entry::Other,
    }
}

fn public_stat<'a>(stat: &rustix::fs::Stat, target: Option<&'a OsStr>) -> Stat<'a> {
    Stat {
        size: u64::try_from(stat.st_size).unwrap_or(0),
        mtime_sec: stat.st_mtime,
        mtime_nsec: i64::try_from(stat.st_mtime_nsec).unwrap_or(0),
        ctime_sec: stat.st_ctime,
        ctime_nsec: i64::try_from(stat.st_ctime_nsec).unwrap_or(0),
        dev: stat.st_dev,
        ino: stat.st_ino,
        mode: stat.st_mode,
        uid: stat.st_uid,
        gid: stat.st_gid,
        nlink: stat.st_nlink,
        link_target: target,
    }
}

fn decode_lossy(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

fn catalog_kind(kind: FileType) -> ferret_catalog::Kind {
    match kind {
        FileType::Directory => ferret_catalog::Kind::Dir,
        FileType::Symlink => ferret_catalog::Kind::Symlink,
        FileType::Fifo => ferret_catalog::Kind::Fifo,
        FileType::Socket => ferret_catalog::Kind::Socket,
        FileType::BlockDevice => ferret_catalog::Kind::Block,
        FileType::CharacterDevice => ferret_catalog::Kind::Character,
        _ => ferret_catalog::Kind::File,
    }
}
