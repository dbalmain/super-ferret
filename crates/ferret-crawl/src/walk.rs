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
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;

use ferret_policy::{AncestorGit, Config, Decision, DirRules, Entry, IgnoreFiles, PatternError};
use rustix::fs::{
    AtFlags, Dir, FileType, Mode, OFlags, fstat, open as open_path, openat, readlinkat, statat,
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
    /// Raw `readlink` text when this entry is a catalogued symlink.
    pub link_target: Option<&'a OsStr>,
}

/// One entry [`DirRules::decide`] classified.
///
/// `path` is relative to the walk's root and borrows the walker's path
/// buffer. It is valid only for the callback that receives it. `stat` is
/// absent for [`Decision::Skip`] and present for every other decision. If
/// `lstat` fails, or `readlink` fails for a catalogued symlink, the walk
/// emits [`Event::Io`] and does not emit `Decided`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decided<'a> {
    /// Root-relative path of the entry.
    pub path: &'a Path,
    /// What the policy said to do with `path`.
    pub decision: Decision,
    /// `lstat` of `path`. Present for every decision other than
    /// [`Decision::Skip`].
    pub stat: Option<Stat<'a>>,
}

/// What the walk reports, in the order it happens.
///
/// A directory is reported before its children in [`walk`]. Its siblings come
/// out in directory order, which is not sorted. [`walk_parallel`] gives no
/// cross-worker event order. Paths borrow a worker's buffer and must be copied
/// to be kept.
#[derive(Debug)]
pub enum Event<'a> {
    /// An entry `decide` classified.
    Decided(Decided<'a>),
    /// A directory listing, `lstat`, `readlink` or ignore-file read failed.
    ///
    /// `path` is root-relative. It is empty when the root itself cannot be
    /// opened or listed, and it is the directory being listed when `getdents`
    /// yields an error with no name. The walk continues with the next entry.
    Io {
        /// Root-relative path the operation was about.
        path: &'a Path,
        /// The OS error.
        error: io::Error,
    },
    /// A pattern [`DirRules::root`] or [`DirRules::enter`] dropped. The rest
    /// of that ignore file still applies, and the walk continues.
    Pattern(PatternError),
}

/// Walks one configured root.
///
/// `global` is the text of the global ignore file, or `None` when there is no
/// such file (nothing is excluded by default). `visit` is called for every
/// entry and every fault as the walk goes, so a catalog can record about a
/// million entries without this function retaining them. A callback is the
/// shape that allows that: the root-relative path is one reused buffer, and a
/// lending iterator is not expressible in stable Rust.
///
/// The root directory itself is not an event. It is where the walk starts,
/// not an entry `decide` sees; the caller stats it if the roots table needs
/// its inode. Faults that belong to the root use an empty path.
///
/// Ignore files are read when the walk enters a directory
/// ([`Decision::Descend`]). A directory reached only so a `.ferretignore` `!`
/// pattern can re-include beneath it ([`Decision::Traverse`]) is listed, and
/// its ignore files are not read (D13). `.gitignore` is read only inside a
/// work tree, where its rules can apply. The root is inside a work tree when
/// it contains `.git`, or when a parent does, up to a filesystem boundary
/// (D22): those
/// parents' `.gitignore` files and the top's `info/exclude` apply, and a
/// `.ferretignore` above the root does not. The search canonicalises the
/// root and does not cross onto another device. `.git/info/exclude` is read
/// when `.git` is a real
/// directory. When `.git` is a regular file whose first line is `gitdir:
/// <path>` (relative to the directory that holds the file), exclude is read
/// from that gitdir, or from the directory named by a `commondir` file there:
/// one line, relative to the gitdir, which is how a linked work tree points at
/// the main repository. Nothing is opened through a symlinked `.git`.
///
/// `.ferretignore`, `.gitignore` and `info/exclude` are opened without blocking
/// (`O_NONBLOCK`) and read only when that open file is a regular file of at
/// most 1 MiB. A larger file is an [`Event::Io`] and is then treated as
/// absent. A missing file, or an opened inode that is not a regular file, is
/// absent and not a fault: a FIFO or directory of one of these names must not
/// stall the walk. A name that cannot be opened at all (a socket, say) is a
/// fault. A regular file that exists but cannot be read is a fault, and is
/// then treated as absent. Bytes that are not UTF-8 are converted lossily. A
/// symlinked `.gitignore` is followed, as is a symlinked `exclude`; `info` is
/// not, and neither is `.git`.
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
pub fn walk(root: &Path, global: Option<&str>, config: Config, visit: impl FnMut(Event<'_>)) {
    let mut walker = Walker::new(root, visit);
    let Some(root_job) = walker.root_job(root, global, config) else {
        return;
    };
    run_worker(walker, &Shared::new(root_job));
}

/// A worker-local visitor. The returned visitors retain their accumulated
/// state, so callers can merge it after the walk without locking per entry.
pub trait EventVisitor {
    /// Receives one event; borrowed paths are valid only during this call.
    fn visit(&mut self, event: Event<'_>);
}

impl<F: FnMut(Event<'_>)> EventVisitor for F {
    fn visit(&mut self, event: Event<'_>) {
        self(event);
    }
}

/// Walks with `workers` worker-local visitors and returns them for merging.
/// Zero workers means one. Event order is unspecified across workers. When the
/// root fails before threads start, only its fault visitor is returned.
pub fn walk_parallel<V: EventVisitor + Send>(
    root: &Path,
    global: Option<&str>,
    config: Config,
    workers: usize,
    make_visitor: impl Fn() -> V + Sync,
) -> Vec<V> {
    let count = workers.max(1);
    let mut first = Walker::new(root, make_visitor());
    let Some(root_job) = first.root_job(root, global, config) else {
        return vec![first.visit];
    };
    let root_id = first.root_id;
    let shared = Shared::new(root_job);
    if count == 1 {
        return vec![run_worker(first, &shared)];
    }
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(count);
        handles.push(scope.spawn(|| run_guarded(first, &shared)));
        for _ in 1..count {
            let visitor = make_visitor();
            let mut walker = Walker::new(root, visitor);
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

struct Shared {
    jobs: Mutex<Vec<Job>>,
    ready: Condvar,
    idle: AtomicUsize,
    outstanding: AtomicUsize,
    open_jobs: AtomicUsize,
    cancelled: AtomicBool,
}

impl Shared {
    fn new(root: Job) -> Self {
        Self {
            jobs: Mutex::new(vec![root]),
            ready: Condvar::new(),
            idle: AtomicUsize::new(0),
            outstanding: AtomicUsize::new(1),
            open_jobs: AtomicUsize::new(1),
            cancelled: AtomicBool::new(false),
        }
    }

    fn take(&self) -> Option<Job> {
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

    fn reserve_or_spill(&self, parent: &mut Job) {
        let reserved = self
            .open_jobs
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |open| {
                (open < MAX_OPEN_JOBS).then_some(open + 1)
            });
        if reserved.is_err() {
            parent.dir.take();
        }
    }

    fn release_open(&self, job: &Job) {
        if job.dir.is_some() {
            self.open_jobs.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn share_oldest(&self, local: &mut VecDeque<Job>) {
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

fn run_guarded<V: EventVisitor>(walker: Walker<V>, shared: &Shared) -> V {
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

fn run_worker<V: EventVisitor>(mut walker: Walker<V>, shared: &Shared) -> V {
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

impl<F: EventVisitor> Walker<F> {
    fn root_job(&mut self, root: &Path, global: Option<&str>, config: Config) -> Option<Job> {
        let fd = match open_path(root, root_dir_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(error) => {
                self.fail(io::Error::from(error));
                return None;
            }
        };
        let root_stat = match fstat(&fd) {
            Ok(stat) => stat,
            Err(error) => {
                self.fail(io::Error::from(error));
                return None;
            }
        };
        self.root_id = Some((root_stat.st_dev, root_stat.st_ino));
        let ancestors = self.discover(root);
        let within = !ancestors.is_empty();
        let mut dir = match Dir::new(fd) {
            Ok(dir) => dir,
            Err(error) => {
                self.fail(io::Error::from(error));
                return None;
            }
        };
        let children = self.list(&mut dir)?;
        let loaded = match dir.fd() {
            Ok(fd) => self.load_ignores(fd, &children, within),
            Err(error) => {
                self.fail(io::Error::from(error));
                return None;
            }
        };
        let borrowed: Vec<AncestorGit<'_>> = ancestors
            .iter()
            .map(|found| AncestorGit {
                above: found.above.as_path(),
                directory: found.directory.as_path(),
                gitignore: found.gitignore.as_deref(),
                git_exclude: found.git_exclude.as_deref(),
                top: found.top,
            })
            .collect();
        let (rules, errors) = if within {
            DirRules::root_within(root, global, loaded.files(), &borrowed, config)
        } else {
            DirRules::root(root, global, loaded.files(), config)
        };
        self.patterns(errors);
        Some(Job::new(
            dir,
            rules,
            children,
            self.rel.clone(),
            (root_stat.st_dev, root_stat.st_ino),
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

/// Ignore files follow a final symlink. `.gitignore` as a symlink is still
/// read (known, left as it is).
fn ignore_flags() -> OFlags {
    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC
}

fn link_flags() -> OFlags {
    OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn bytes_path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

// ── walk state ──

struct Walker<F> {
    root: PathBuf,
    root_id: Option<(u64, u64)>,
    /// Root-relative path bytes. Empty at the root. Reused for every entry.
    rel: Vec<u8>,
    visit: F,
}

struct Child {
    start: usize,
    end: usize,
    kind: FileType,
}

struct Children {
    names: Vec<u8>,
    entries: Vec<Child>,
}

impl Children {
    fn name(&self, child: &Child) -> &OsStr {
        OsStr::from_bytes(&self.names[child.start..child.end])
    }

    fn contains(&self, name: &str) -> bool {
        self.entries
            .iter()
            .any(|child| self.name(child).as_bytes() == name.as_bytes())
    }
}

struct Job {
    dir: Option<Dir>,
    rules: DirRules,
    children: Children,
    next: usize,
    rel: Vec<u8>,
    id: (u64, u64),
}

impl Job {
    fn new(dir: Dir, rules: DirRules, children: Children, rel: Vec<u8>, id: (u64, u64)) -> Self {
        Self {
            dir: Some(dir),
            rules,
            children,
            next: 0,
            rel,
            id,
        }
    }
}

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

struct Draft {
    directory: PathBuf,
    above: PathBuf,
    top: bool,
}

struct Found {
    directory: PathBuf,
    above: PathBuf,
    gitignore: Option<String>,
    git_exclude: Option<String>,
    top: bool,
}

struct Ignores {
    ferretignore: Option<String>,
    gitignore: Option<String>,
    git_root: bool,
    git_exclude: Option<String>,
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

impl<F: EventVisitor> Walker<F> {
    fn new(root: &Path, visit: F) -> Self {
        Self {
            root: root.to_path_buf(),
            root_id: None,
            rel: Vec::with_capacity(256),
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

    fn fail(&mut self, error: io::Error) {
        self.visit.visit(Event::Io {
            path: bytes_path(&self.rel),
            error,
        });
    }

    fn patterns(&mut self, errors: Vec<PatternError>) {
        for error in errors {
            self.visit.visit(Event::Pattern(error));
        }
    }

    fn emit(&mut self, decision: Decision, stat: Option<Stat<'_>>) {
        self.visit.visit(Event::Decided(Decided {
            path: bytes_path(&self.rel),
            decision,
            stat,
        }));
    }

    fn emit_skip(&mut self) {
        self.emit(Decision::Skip, None);
    }

    fn emit_stat(&mut self, decision: Decision, stat: &rustix::fs::Stat, target: Option<&OsStr>) {
        self.emit(decision, Some(public_stat(stat, target)));
    }

    /// Lists `dir`. An error from `getdents` with no entry yet is one fault
    /// and `None`, so ignore files are not probed for a directory that could
    /// not be listed. An error after some entries is reported and the entries
    /// already read are kept.
    fn list(&mut self, dir: &mut Dir) -> Option<Children> {
        let mut children = Children {
            names: Vec::new(),
            entries: Vec::new(),
        };
        let mut failed = false;
        while let Some(item) = dir.read() {
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
                }
                Err(error) => {
                    self.fail(io::Error::from(error));
                    failed = true;
                }
            }
        }
        if failed && children.entries.is_empty() {
            None
        } else {
            Some(children)
        }
    }

    fn process(&mut self, mut job: Job) -> Option<(Job, Job)> {
        self.rel.clone_from(&job.rel);
        if job.dir.is_none() {
            job.dir = self.reopen(bytes_path(&job.rel), job.id);
        }
        let dir = job.dir.as_ref()?;
        while job.next < job.children.entries.len() {
            let child = &job.children.entries[job.next];
            let name = job.children.name(child);
            job.next += 1;
            let length = self.push(name);
            let descended = match dir.fd() {
                Ok(fd) => self.consider(fd, &job.rules, name, child.kind),
                Err(error) => {
                    self.fail(io::Error::from(error));
                    None
                }
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
    fn reopen(&mut self, rel: &Path, expected: (u64, u64)) -> Option<Dir> {
        let root = match open_path(&self.root, root_dir_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(error) => {
                self.fail(io::Error::from(error));
                return None;
            }
        };
        let Some(root_id) = self.root_id else {
            self.fail(io::Error::new(
                io::ErrorKind::InvalidData,
                "missing root identity",
            ));
            return None;
        };
        let mut fd = root;
        if !self.same_dir(&fd, root_id) {
            return None;
        }
        for name in rel.iter() {
            fd = match openat(&fd, name, child_dir_flags(), Mode::empty()) {
                Ok(next) => next,
                Err(error) => {
                    self.fail(io::Error::from(error));
                    return None;
                }
            };
        }
        if !self.same_dir(&fd, expected) {
            return None;
        }
        match Dir::new(fd) {
            Ok(dir) => Some(dir),
            Err(error) => {
                self.fail(io::Error::from(error));
                None
            }
        }
    }

    fn same_dir(&mut self, fd: &OwnedFd, expected: (u64, u64)) -> bool {
        match fstat(fd) {
            Ok(stat) if (stat.st_dev, stat.st_ino) == expected => true,
            Ok(_) => {
                self.fail(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory changed before resume",
                ));
                false
            }
            Err(error) => {
                self.fail(io::Error::from(error));
                false
            }
        }
    }

    fn consider(
        &mut self,
        dir: BorrowedFd<'_>,
        rules: &DirRules,
        name: &OsStr,
        kind: FileType,
    ) -> Option<Job> {
        match kind {
            FileType::RegularFile | FileType::Unknown => {
                let stat = self.stat_child(dir, name)?;
                self.decide_statted(dir, rules, name, stat)
            }
            FileType::Symlink => {
                let decision = rules.decide(bytes_path(&self.rel), Entry::Symlink);
                if decision == Decision::Skip {
                    self.emit_skip();
                    return None;
                }
                self.finish_link(dir, rules, name, decision)
            }
            FileType::Directory => self.consider_dir(dir, rules, name),
            _ => {
                if rules.decide(bytes_path(&self.rel), Entry::Other) == Decision::Skip {
                    self.emit_skip();
                    return None;
                }
                let stat = self.stat_child(dir, name)?;
                self.decide_statted(dir, rules, name, stat)
            }
        }
    }

    fn consider_dir(&mut self, dir: BorrowedFd<'_>, rules: &DirRules, name: &OsStr) -> Option<Job> {
        let mut decision = rules.decide(bytes_path(&self.rel), Entry::Dir);
        if decision == Decision::Skip {
            self.emit_skip();
            return None;
        }
        let stat = self.stat_child(dir, name)?;
        let seen = entry_from_stat(&stat);
        if seen != Entry::Dir {
            decision = rules.decide(bytes_path(&self.rel), seen);
            if decision == Decision::Skip {
                self.emit_skip();
                return None;
            }
            if seen == Entry::Symlink {
                return self.finish_link(dir, rules, name, decision);
            }
        }
        self.emit_stat(decision, &stat, None);
        self.follow(dir, rules, name, decision, &stat)
    }

    /// `stat` came from `statat` (`d_type` was a file, unknown, or disagreed).
    /// A symlink result is re-opened so the published stat and the target are
    /// one observation.
    fn decide_statted(
        &mut self,
        dir: BorrowedFd<'_>,
        rules: &DirRules,
        name: &OsStr,
        stat: rustix::fs::Stat,
    ) -> Option<Job> {
        let entry = entry_from_stat(&stat);
        let decision = rules.decide(bytes_path(&self.rel), entry);
        if decision == Decision::Skip {
            self.emit_skip();
            return None;
        }
        if entry == Entry::Symlink {
            return self.finish_link(dir, rules, name, decision);
        }
        self.emit_stat(decision, &stat, None);
        self.follow(dir, rules, name, decision, &stat)
    }

    /// Opens `name` with `O_PATH | O_NOFOLLOW`. The stat and, when it is a
    /// symlink, the target come from that descriptor.
    fn finish_link(
        &mut self,
        dir: BorrowedFd<'_>,
        rules: &DirRules,
        name: &OsStr,
        decision: Decision,
    ) -> Option<Job> {
        let (stat, target) = match observe_link(dir, name) {
            Ok(pair) => pair,
            Err(error) => {
                self.fail(error);
                return None;
            }
        };
        let seen = entry_from_stat(&stat);
        if seen != Entry::Symlink {
            let decision = rules.decide(bytes_path(&self.rel), seen);
            if decision == Decision::Skip {
                self.emit_skip();
                return None;
            }
            self.emit_stat(decision, &stat, None);
            return self.follow(dir, rules, name, decision, &stat);
        }
        let Some(target) = target else {
            self.fail(io::Error::new(
                io::ErrorKind::InvalidData,
                "symlink has no target",
            ));
            return None;
        };
        self.emit_stat(decision, &stat, Some(target.as_os_str()));
        None
    }

    fn stat_child(&mut self, dir: BorrowedFd<'_>, name: &OsStr) -> Option<rustix::fs::Stat> {
        match statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Some(stat),
            Err(error) => {
                self.fail(io::Error::from(error));
                None
            }
        }
    }

    fn follow(
        &mut self,
        parent: BorrowedFd<'_>,
        rules: &DirRules,
        name: &OsStr,
        decision: Decision,
        expected: &rustix::fs::Stat,
    ) -> Option<Job> {
        match decision {
            Decision::Descend => self.enter_and_list(parent, rules, name, expected),
            Decision::Traverse => self.traverse_and_list(parent, rules, name, expected),
            Decision::Skip | Decision::Catalog(_) | Decision::Index => None,
        }
    }

    fn enter_and_list(
        &mut self,
        parent: BorrowedFd<'_>,
        parent_rules: &DirRules,
        name: &OsStr,
        expected: &rustix::fs::Stat,
    ) -> Option<Job> {
        let mut child = self.open_child(parent, name, expected)?;
        let children = self.list(&mut child)?;
        let loaded = match child.fd() {
            Ok(fd) => self.load_ignores(fd, &children, parent_rules.in_work_tree()),
            Err(error) => {
                self.fail(io::Error::from(error));
                return None;
            }
        };
        let (rules, errors) = parent_rules.enter(name, loaded.files());
        self.patterns(errors);
        Some(Job::new(
            child,
            rules,
            children,
            self.rel.clone(),
            (expected.st_dev, expected.st_ino),
        ))
    }

    fn traverse_and_list(
        &mut self,
        parent: BorrowedFd<'_>,
        parent_rules: &DirRules,
        name: &OsStr,
        expected: &rustix::fs::Stat,
    ) -> Option<Job> {
        let mut child = self.open_child(parent, name, expected)?;
        let children = self.list(&mut child)?;
        let rules = parent_rules.traverse(name);
        Some(Job::new(
            child,
            rules,
            children,
            self.rel.clone(),
            (expected.st_dev, expected.st_ino),
        ))
    }

    /// `O_DIRECTORY | O_NOFOLLOW`, then `fstat`. A symlink is `ELOOP`. A
    /// different inode than `expected` is a fault either way: the `Decided`
    /// event already described the inode `decide` saw.
    fn open_child(
        &mut self,
        parent: BorrowedFd<'_>,
        name: &OsStr,
        expected: &rustix::fs::Stat,
    ) -> Option<Dir> {
        let fd = match openat(parent, name, child_dir_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(error) => {
                self.fail(io::Error::from(error));
                return None;
            }
        };
        match fstat(&fd) {
            Ok(stat) if stat.st_dev == expected.st_dev && stat.st_ino == expected.st_ino => {}
            Ok(_) => {
                self.fail(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory changed between stat and open",
                ));
                return None;
            }
            Err(error) => {
                self.fail(io::Error::from(error));
                return None;
            }
        }
        match Dir::new(fd) {
            Ok(dir) => Some(dir),
            Err(error) => {
                self.fail(io::Error::from(error));
                None
            }
        }
    }

    /// The work tree above `root`, top first. Empty when `root` itself holds
    /// `.git`, or when no parent on the same device does.
    ///
    /// Git resolves the starting directory (`getcwd` / `git -C`) and refuses
    /// to cross a filesystem boundary unless
    /// `GIT_DISCOVERY_ACROSS_FILESYSTEM` is set. This follows that default:
    /// [`fs::canonicalize`], then parents while `st_dev` matches the root's.
    /// The ancestor directories are opened by that canonical path; `.gitignore`
    /// and `info/exclude` are read relative to those descriptors.
    fn discover(&mut self, root: &Path) -> Vec<Found> {
        let canon = match fs::canonicalize(root) {
            Ok(path) => path,
            Err(error) => {
                self.fail_abs(root, error);
                return Vec::new();
            }
        };
        if self.discovered_git(&canon) {
            return Vec::new();
        }
        let root_dev = match device_of(&canon) {
            Ok(dev) => dev,
            Err(error) => {
                self.fail_abs(&canon, error);
                return Vec::new();
            }
        };
        let mut above = match canon.file_name() {
            Some(name) => PathBuf::from(name),
            None => return Vec::new(),
        };
        let mut current = match canon.parent() {
            Some(parent) => parent.to_path_buf(),
            None => return Vec::new(),
        };
        let mut drafts = Vec::new();
        loop {
            let dev = match device_of(&current) {
                Ok(dev) => dev,
                Err(error) => {
                    self.fail_abs(&current, error);
                    break;
                }
            };
            if dev != root_dev {
                break;
            }
            let top = self.discovered_git(&current);
            drafts.push(Draft {
                directory: current.clone(),
                above: above.clone(),
                top,
            });
            if top {
                break;
            }
            let Some(name) = current.file_name() else {
                break;
            };
            above = Path::new(name).join(above);
            let Some(parent) = current.parent() else {
                break;
            };
            current = parent.to_path_buf();
        }
        if !drafts.iter().any(|draft| draft.top) {
            return Vec::new();
        }
        drafts.reverse();
        drafts
            .into_iter()
            .filter_map(|draft| self.read_ancestor(draft))
            .collect()
    }

    fn discovered_git(&mut self, directory: &Path) -> bool {
        match has_git(directory) {
            Ok(found) => found,
            Err(error) => {
                self.fail_abs(&directory.join(".git"), error);
                false
            }
        }
    }

    /// `None` for a non-top directory with no `.gitignore`. The top is always
    /// returned, so the root is inside the work tree even when the top has
    /// no patterns of its own.
    fn read_ancestor(&mut self, draft: Draft) -> Option<Found> {
        let fd = match open_path(&draft.directory, root_dir_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(error) => {
                self.fail_abs(&draft.directory, io::Error::from(error));
                return draft.top.then_some(Found {
                    directory: draft.directory,
                    above: draft.above,
                    gitignore: None,
                    git_exclude: None,
                    top: true,
                });
            }
        };
        let gitignore_path = draft.directory.join(".gitignore");
        let gitignore = self.read_ancestor_ignore(fd.as_fd(), ".gitignore", &gitignore_path);
        let git_exclude = if draft.top {
            self.ancestor_exclude(fd.as_fd())
        } else {
            None
        };
        if !draft.top && gitignore.is_none() {
            return None;
        }
        Some(Found {
            directory: draft.directory,
            above: draft.above,
            gitignore,
            git_exclude,
            top: draft.top,
        })
    }

    fn read_ancestor_ignore(
        &mut self,
        dir: BorrowedFd<'_>,
        name: &str,
        full: &Path,
    ) -> Option<String> {
        match open_ignore(dir, name) {
            Ok(Opened::Bytes(bytes)) => Some(decode_lossy(bytes)),
            Ok(Opened::Missing | Opened::NotRegular) => None,
            Err(error) => {
                self.fail_abs(full, error);
                None
            }
        }
    }

    fn ancestor_exclude(&mut self, dir: BorrowedFd<'_>) -> Option<String> {
        match self.probe_git(dir) {
            GitProbe::Directory(fd) => self.read_exclude(fd),
            GitProbe::File(bytes) => self.read_gitfile_exclude(dir, &bytes),
            GitProbe::Missing | GitProbe::Present => None,
        }
    }

    fn fail_abs(&mut self, path: &Path, error: io::Error) {
        self.fail(io::Error::new(
            error.kind(),
            format!("{}: {error}", path.display()),
        ));
    }

    // ── ignore files ──

    /// Only names in the listing are opened: a directory without ignore files,
    /// the usual case, costs no failed `open`s.
    ///
    /// `in_work_tree` is the parent directory. The root passes `false`.
    /// `.git`, when it is a directory, is opened here and that descriptor is
    /// what `info/exclude` is read from, including across the `.gitignore`
    /// callback below.
    fn load_ignores(
        &mut self,
        dir: BorrowedFd<'_>,
        children: &Children,
        in_work_tree: bool,
    ) -> Ignores {
        let listed = |name: &str| children.contains(name);
        let ferretignore = listed(".ferretignore")
            .then(|| self.read_named(dir, ".ferretignore"))
            .flatten();
        let git = if listed(".git") {
            self.probe_git(dir)
        } else {
            GitProbe::Missing
        };
        // A `.gitignore` outside a work tree cannot affect decisions, so it is
        // not opened. A FIFO of that name must not stall a walk that is not in
        // a repository.
        let gitignore = if (in_work_tree || git.is_root()) && listed(".gitignore") {
            self.read_named(dir, ".gitignore")
        } else {
            None
        };
        let git_root = git.is_root();
        let git_exclude = match git {
            GitProbe::Directory(fd) => self.read_exclude(fd),
            GitProbe::File(bytes) => self.read_gitfile_exclude(dir, &bytes),
            GitProbe::Missing | GitProbe::Present => None,
        };
        Ignores {
            ferretignore,
            gitignore,
            git_root,
            git_exclude,
        }
    }

    fn read_named(&mut self, dir: BorrowedFd<'_>, name: &str) -> Option<String> {
        let length = self.push(name);
        let text = match open_ignore(dir, name) {
            Ok(Opened::Bytes(bytes)) => Some(decode_lossy(bytes)),
            Ok(Opened::Missing | Opened::NotRegular) => None,
            Err(error) => {
                self.fail(error);
                None
            }
        };
        self.pop(length);
        text
    }

    /// `info` is opened `O_NOFOLLOW` relative to the held git directory (or
    /// common directory). `exclude` is an ordinary ignore file: a symlink of
    /// that name is followed.
    fn read_exclude(&mut self, git: OwnedFd) -> Option<String> {
        let git_length = self.push(".git");
        let info_length = self.push("info");
        let info = match openat(git.as_fd(), "info", child_dir_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => {
                self.pop(info_length);
                self.pop(git_length);
                return None;
            }
            Err(error) => {
                let exclude_length = self.push("exclude");
                self.fail(io::Error::from(error));
                self.pop(exclude_length);
                self.pop(info_length);
                self.pop(git_length);
                return None;
            }
        };
        drop(git);
        let exclude_length = self.push("exclude");
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
                self.fail(error);
                None
            }
        };
        self.pop(exclude_length);
        self.pop(info_length);
        self.pop(git_length);
        text
    }

    fn probe_git(&mut self, dir: BorrowedFd<'_>) -> GitProbe {
        match openat(dir, ".git", child_dir_flags(), Mode::empty()) {
            Ok(fd) => GitProbe::Directory(fd),
            Err(Errno::NOENT) => GitProbe::Missing,
            Err(Errno::LOOP) => GitProbe::Present,
            Err(Errno::NOTDIR) => self.probe_git_file(dir),
            Err(error) => self.probe_git_failed(dir, error),
        }
    }

    /// The directory open failed for a reason other than "not a directory"
    /// or "a symlink". A directory we cannot search still starts a work
    /// tree; a regular file is read below.
    fn probe_git_failed(&mut self, dir: BorrowedFd<'_>, error: Errno) -> GitProbe {
        match statat(dir, ".git", AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if file_type(&stat) == FileType::RegularFile => self.probe_git_file(dir),
            Ok(stat) if file_type(&stat) == FileType::Directory => {
                self.fail_at_git(io::Error::from(error));
                GitProbe::Present
            }
            Ok(_) => GitProbe::Present,
            Err(Errno::NOENT) => GitProbe::Missing,
            Err(stat_err) => {
                self.fail_at_git(io::Error::from(stat_err));
                GitProbe::Missing
            }
        }
    }

    /// `.git` is not a directory. `O_NOFOLLOW` so a symlink that appeared
    /// since the listing is not a gitdir file.
    fn probe_git_file(&mut self, dir: BorrowedFd<'_>) -> GitProbe {
        match openat(dir, ".git", nofollow_file_flags(), Mode::empty()) {
            Ok(fd) => match read_regular(fd) {
                Ok(Some(bytes)) => GitProbe::File(bytes),
                Ok(None) => GitProbe::Present,
                Err(error) => {
                    self.fail_at_git(error);
                    GitProbe::Present
                }
            },
            Err(Errno::NOENT) => GitProbe::Missing,
            Err(Errno::LOOP) => GitProbe::Present,
            Err(error) => match statat(dir, ".git", AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) if file_type(&stat) == FileType::RegularFile => {
                    self.fail_at_git(io::Error::from(error));
                    GitProbe::Present
                }
                Ok(_) => GitProbe::Present,
                Err(Errno::NOENT) => GitProbe::Missing,
                Err(stat_err) => {
                    self.fail_at_git(io::Error::from(stat_err));
                    GitProbe::Missing
                }
            },
        }
    }

    /// Follow git's `gitdir:` and optional `commondir` from held descriptors.
    /// A relative path may include `..`, as git's format requires. An
    /// absolute gitdir is opened by path because it is outside the tree.
    fn read_gitfile_exclude(&mut self, work: BorrowedFd<'_>, bytes: &[u8]) -> Option<String> {
        let raw = parse_gitdir(bytes)?;
        let gitdir = self.open_git_directory(work, raw)?;
        let common = self.common_dir(gitdir)?;
        self.read_exclude(common)
    }

    fn open_git_directory(&mut self, base: BorrowedFd<'_>, raw: &OsStr) -> Option<OwnedFd> {
        let path = Path::new(raw);
        let opened = if path.is_absolute() {
            open_path(path, child_dir_flags(), Mode::empty())
        } else {
            openat(base, path, child_dir_flags(), Mode::empty())
        };
        match opened {
            Ok(fd) => Some(fd),
            Err(Errno::NOENT) => None,
            Err(error) => {
                self.fail_at_git(io::Error::from(error));
                None
            }
        }
    }

    /// The gitdir itself when it has no `commondir` file. `None` when that
    /// file cannot be read, is not a regular file, or names nothing: a linked
    /// work tree's own directory is not where exclude lives, so it is not a
    /// fallback. Only a missing `commondir` means the gitdir is the common
    /// directory.
    fn common_dir(&mut self, gitdir: OwnedFd) -> Option<OwnedFd> {
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
            Ok(Opened::Missing) => Some(gitdir),
            Ok(Opened::NotRegular) => {
                self.fail_at_git(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "commondir is not a regular file",
                ));
                None
            }
            Ok(Opened::Bytes(bytes)) => {
                let raw = first_line(&bytes)?;
                self.open_git_directory(gitdir.as_fd(), raw)
            }
            Err(error) => {
                self.fail_at_git(error);
                None
            }
        }
    }

    fn fail_at_git(&mut self, error: io::Error) {
        let length = self.push(".git");
        self.fail(error);
        self.pop(length);
    }
}

/// `O_PATH | O_NOFOLLOW`, then `fstat` and, for a symlink, `readlinkat` of
/// that descriptor. `Ok`'s stat is the inode the descriptor refers to, which
/// may no longer be a symlink; then the target is `None`.
fn observe_link(
    dir: BorrowedFd<'_>,
    name: &OsStr,
) -> io::Result<(rustix::fs::Stat, Option<OsString>)> {
    let fd = openat(dir, name, link_flags(), Mode::empty())?;
    let stat = fstat(&fd)?;
    if file_type(&stat) != FileType::Symlink {
        return Ok((stat, None));
    }
    // An empty path reads the link the `O_PATH` descriptor already refers
    // to (Linux 2.6.39), so the target cannot be a different inode from
    // `stat`.
    let raw = readlinkat(&fd, "", Vec::new())?;
    Ok((stat, Some(OsString::from_vec(raw.into_bytes()))))
}

fn open_ignore(dir: BorrowedFd<'_>, name: &str) -> io::Result<Opened> {
    match openat(dir, name, ignore_flags(), Mode::empty()) {
        Ok(fd) => read_opened(fd),
        Err(Errno::NOENT) => Ok(Opened::Missing),
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

fn resolve_git_path(base: &Path, raw: &OsStr) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Whether `dir` is the top of a work tree, by the shapes `git rev-parse
/// --is-inside-work-tree` accepts. Checked as a black box, not from git's
/// source: a directory with `HEAD`, `refs` and `objects`; the same with
/// `commondir` naming such a directory instead of `objects`; a `gitdir:`
/// file pointing at one of those; a symlink to any of them. An empty
/// directory named `.git` is not a repository.
fn has_git(dir: &Path) -> io::Result<bool> {
    is_git_path(&dir.join(".git"), 0)
}

fn is_git_path(path: &Path, depth: u32) -> io::Result<bool> {
    if depth > 8 {
        return Ok(false);
    }
    let Some(meta) = discovery_metadata(path, false)? else {
        return Ok(false);
    };
    let kind = meta.file_type();
    if kind.is_symlink() {
        let target = fs::read_link(path)?;
        let base = path.parent().unwrap_or(path);
        return is_git_path(&resolve_git_path(base, target.as_os_str()), depth + 1);
    }
    if kind.is_dir() {
        return is_git_dir(path, depth);
    }
    if kind.is_file() {
        let Some(parent) = path.parent() else {
            return Ok(false);
        };
        return is_git_file(parent, path, depth);
    }
    Ok(false)
}

fn is_git_dir(path: &Path, depth: u32) -> io::Result<bool> {
    if depth > 8 {
        return Ok(false);
    }
    let head = discovery_metadata(&path.join("HEAD"), true)?.is_some_and(|meta| meta.is_file());
    let refs = discovery_metadata(&path.join("refs"), true)?.is_some_and(|meta| meta.is_dir());
    if !(head && refs) {
        return Ok(false);
    }
    if discovery_metadata(&path.join("objects"), true)?.is_some_and(|meta| meta.is_dir()) {
        return Ok(true);
    }
    let commondir = path.join("commondir");
    let bytes = match open_discovery_file(&commondir)? {
        Opened::Bytes(bytes) => bytes,
        Opened::Missing => return Ok(false),
        Opened::NotRegular => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: not a regular file", commondir.display()),
            ));
        }
    };
    let Some(raw) = first_line(&bytes) else {
        return Ok(false);
    };
    is_git_dir(&resolve_git_path(path, raw), depth + 1)
}

fn is_git_file(work_tree: &Path, file: &Path, depth: u32) -> io::Result<bool> {
    let bytes = match open_discovery_file(file)? {
        Opened::Bytes(bytes) => bytes,
        Opened::Missing => return Ok(false),
        Opened::NotRegular => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: not a regular file", file.display()),
            ));
        }
    };
    let Some(raw) = parse_gitdir(&bytes) else {
        return Ok(false);
    };
    is_git_path(&resolve_git_path(work_tree, raw), depth + 1)
}

fn discovery_metadata(path: &Path, follow: bool) -> io::Result<Option<fs::Metadata>> {
    let result = if follow {
        fs::metadata(path)
    } else {
        fs::symlink_metadata(path)
    };
    match result {
        Ok(meta) => Ok(Some(meta)),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn open_discovery_file(path: &Path) -> io::Result<Opened> {
    match open_path(path, ignore_flags(), Mode::empty()) {
        Ok(fd) => read_opened(fd),
        Err(Errno::NOENT | Errno::NOTDIR) => Ok(Opened::Missing),
        Err(error) => Err(io::Error::from(error)),
    }
}

fn device_of(path: &Path) -> io::Result<u64> {
    fs::metadata(path).map(|meta| meta.dev())
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
        link_target: target,
    }
}

fn decode_lossy(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

#[cfg(test)]
mod scheduler_tests {
    use super::*;

    #[test]
    fn sharing_the_oldest_does_not_move_the_remaining_jobs() {
        let root = std::env::temp_dir().join(format!("ferret-scheduler-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let make_job = || {
            let mut walker = Walker::new(&root, |_: Event<'_>| {});
            walker.root_job(&root, None, Config::default()).unwrap()
        };
        let shared = Shared::new(make_job());
        let mut local = VecDeque::from([make_job(), make_job()]);
        let newest = &local[1] as *const Job;
        shared.idle.store(1, Ordering::SeqCst);
        shared.share_oldest(&mut local);
        assert_eq!(local.len(), 1);
        assert!(std::ptr::eq(newest, &local[0]));
        fs::remove_dir(&root).unwrap();
    }
}
