//! One root, walked by directory handles, with one [`DirRules`] per directory.
//!
//! Seam: [`ferret_policy`] decides what to do with each entry. This module
//! reads the tree and reports. It does not touch the catalog.
//!
//! The root is opened by the path the caller gave, following a symlink there
//! (that path is the user's), with `O_DIRECTORY`. Every later open, stat,
//! `readlink` and ignore-file read is relative to a directory descriptor.
//! A queued job owns one directory descriptor and a listing cursor. The queue
//! holds at most 128 jobs; each worker holds one active job and at most two
//! extra descriptors while opening a child or reading ignore files. Thus a
//! parallel walk with N workers holds at most 128 + 3N descriptors, independent
//! of tree depth and width. A single-worker walk uses the same jobs.
//! `EMFILE` opening a child is an [`Event::Io`] for that child and the walk
//! continues with the next sibling. There is no path-based fallback.
//!
//! A catalogued symlink is one observation: `openat` with `O_PATH |
//! O_NOFOLLOW`, then `fstat` and `readlinkat` on that descriptor (an empty
//! path, which on Linux reads the link the descriptor refers to). The stat
//! in the event and the stored target cannot disagree.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
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
/// A directory is reported before its children. Siblings come out in
/// directory order, which is not sorted. Paths borrow the walker's buffers
/// and must be copied to be kept.
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
    let queue = (
        Mutex::new(Queue {
            jobs: vec![root_job],
            outstanding: 1,
        }),
        Condvar::new(),
    );
    run_worker(walker, &queue);
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
/// Zero workers means one. Event order is unspecified across workers.
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
    let queue = (
        Mutex::new(Queue {
            jobs: vec![root_job],
            outstanding: 1,
        }),
        Condvar::new(),
    );
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(count);
        handles.push(scope.spawn(|| run_worker(first, &queue)));
        for _ in 1..count {
            let visitor = make_visitor();
            handles.push(scope.spawn(|| run_worker(Walker::new(root, visitor), &queue)));
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

const MAX_QUEUED: usize = 128;

struct Queue {
    jobs: Vec<Job>,
    outstanding: usize,
}

fn run_worker<V: EventVisitor>(mut walker: Walker<V>, shared: &(Mutex<Queue>, Condvar)) -> V {
    let (lock, ready) = shared;
    loop {
        let job = {
            let mut queue = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            loop {
                if let Some(job) = queue.jobs.pop() {
                    break job;
                }
                if queue.outstanding == 0 {
                    return walker.visit;
                }
                queue = ready
                    .wait(queue)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        };
        let mut current = job;
        loop {
            match walker.process(current) {
                Some((parent, child)) => {
                    let mut queue = lock
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    queue.outstanding += 1;
                    queue.jobs.push(parent);
                    if queue.jobs.len() < MAX_QUEUED {
                        queue.jobs.push(child);
                        ready.notify_all();
                        break;
                    }
                    ready.notify_one();
                    current = child;
                }
                None => {
                    let mut queue = lock
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    queue.outstanding -= 1;
                    if queue.outstanding == 0 {
                        ready.notify_all();
                    }
                    break;
                }
            }
        }
    }
}

impl<F: EventVisitor> Walker<F> {
    fn root_job(&mut self, root: &Path, global: Option<&str>, config: Config) -> Option<Job> {
        let ancestors = self.discover(root);
        let within = !ancestors.is_empty();
        let fd = match open_path(root, root_dir_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(error) => {
                self.fail(io::Error::from(error));
                return None;
            }
        };
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
            self.abs.clone(),
            self.rel.clone(),
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

// ── walk state ──

struct Walker<F> {
    /// Path of the directory under consideration, as the caller spelled the
    /// root plus each child name. Used to resolve a `.git` file's `gitdir:`
    /// path, which is relative to that directory and may leave the root.
    /// Kept in lockstep with [`Self::rel`].
    abs: PathBuf,
    /// Root-relative path. Empty at the root. Reused for every entry.
    rel: PathBuf,
    visit: F,
}

struct Child {
    name: OsString,
    kind: FileType,
}

struct Job {
    dir: Dir,
    rules: DirRules,
    children: Vec<Child>,
    next: usize,
    abs: PathBuf,
    rel: PathBuf,
}

impl Job {
    fn new(dir: Dir, rules: DirRules, children: Vec<Child>, abs: PathBuf, rel: PathBuf) -> Self {
        Self {
            dir,
            rules,
            children,
            next: 0,
            abs,
            rel,
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
        let mut abs = PathBuf::from(root);
        abs.reserve(256);
        let mut rel = PathBuf::new();
        rel.reserve(256);
        Self { abs, rel, visit }
    }

    fn push(&mut self, name: impl AsRef<OsStr>) {
        let name = name.as_ref();
        self.rel.push(name);
        self.abs.push(name);
    }

    fn pop(&mut self) {
        let rel = self.rel.pop();
        let abs = self.abs.pop();
        debug_assert!(rel && abs, "pop past the walk root");
    }

    fn fail(&mut self, error: io::Error) {
        self.visit.visit(Event::Io {
            path: &self.rel,
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
            path: &self.rel,
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
    fn list(&mut self, dir: &mut Dir) -> Option<Vec<Child>> {
        let mut children = Vec::new();
        let mut failed = false;
        while let Some(item) = dir.read() {
            match item {
                Ok(entry) => {
                    let bytes = entry.file_name().to_bytes();
                    if bytes == b"." || bytes == b".." {
                        continue;
                    }
                    children.push(Child {
                        name: OsStr::from_bytes(bytes).to_os_string(),
                        kind: entry.file_type(),
                    });
                }
                Err(error) => {
                    self.fail(io::Error::from(error));
                    failed = true;
                }
            }
        }
        if failed && children.is_empty() {
            None
        } else {
            Some(children)
        }
    }

    fn process(&mut self, mut job: Job) -> Option<(Job, Job)> {
        self.abs.clone_from(&job.abs);
        self.rel.clone_from(&job.rel);
        while job.next < job.children.len() {
            let child = &job.children[job.next];
            job.next += 1;
            self.push(&child.name);
            let descended = match job.dir.fd() {
                Ok(fd) => self.consider(fd, &job.rules, &child.name, child.kind),
                Err(error) => {
                    self.fail(io::Error::from(error));
                    None
                }
            };
            self.pop();
            if let Some(descended) = descended {
                return Some((job, descended));
            }
        }
        None
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
                let decision = rules.decide(&self.rel, Entry::Symlink);
                if decision == Decision::Skip {
                    self.emit_skip();
                    return None;
                }
                self.finish_link(dir, rules, name, decision)
            }
            FileType::Directory => self.consider_dir(dir, rules, name),
            _ => {
                if rules.decide(&self.rel, Entry::Other) == Decision::Skip {
                    self.emit_skip();
                    return None;
                }
                let stat = self.stat_child(dir, name)?;
                self.decide_statted(dir, rules, name, stat)
            }
        }
    }

    fn consider_dir(&mut self, dir: BorrowedFd<'_>, rules: &DirRules, name: &OsStr) -> Option<Job> {
        let mut decision = rules.decide(&self.rel, Entry::Dir);
        if decision == Decision::Skip {
            self.emit_skip();
            return None;
        }
        let stat = self.stat_child(dir, name)?;
        let seen = entry_from_stat(&stat);
        if seen != Entry::Dir {
            decision = rules.decide(&self.rel, seen);
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
        let decision = rules.decide(&self.rel, entry);
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
            let decision = rules.decide(&self.rel, seen);
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
            self.abs.clone(),
            self.rel.clone(),
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
            self.abs.clone(),
            self.rel.clone(),
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
            Err(_) => return Vec::new(),
        };
        if has_git(&canon) {
            return Vec::new();
        }
        let Some(root_dev) = device_of(&canon) else {
            return Vec::new();
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
        while let Some(dev) = device_of(&current) {
            if dev != root_dev {
                break;
            }
            let top = has_git(&current);
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
            self.ancestor_exclude(fd.as_fd(), &draft.directory)
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

    fn ancestor_exclude(&mut self, dir: BorrowedFd<'_>, directory: &Path) -> Option<String> {
        match self.probe_git(dir) {
            GitProbe::Directory(fd) => self.read_exclude(fd.as_fd()),
            GitProbe::File(bytes) => self.read_gitfile_exclude(directory, &bytes),
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
        children: &[Child],
        in_work_tree: bool,
    ) -> Ignores {
        let listed = |name: &str| children.iter().any(|child| child.name == name);
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
        let git_exclude = match &git {
            GitProbe::Directory(fd) => self.read_exclude(fd.as_fd()),
            GitProbe::File(bytes) => {
                let base = self.abs.clone();
                self.read_gitfile_exclude(&base, bytes)
            }
            GitProbe::Missing | GitProbe::Present => None,
        };
        Ignores {
            ferretignore,
            gitignore,
            git_root: git.is_root(),
            git_exclude,
        }
    }

    fn read_named(&mut self, dir: BorrowedFd<'_>, name: &str) -> Option<String> {
        self.push(name);
        let text = match open_ignore(dir, name) {
            Ok(Opened::Bytes(bytes)) => Some(decode_lossy(bytes)),
            Ok(Opened::Missing | Opened::NotRegular) => None,
            Err(error) => {
                self.fail(error);
                None
            }
        };
        self.pop();
        text
    }

    /// `info` is opened `O_NOFOLLOW` relative to the `.git` descriptor held
    /// since [`probe_git`](Self::probe_git). `exclude` itself is an ordinary
    /// ignore file: a symlink of that name is followed.
    fn read_exclude(&mut self, git: BorrowedFd<'_>) -> Option<String> {
        self.push(".git");
        self.push("info");
        let info = match openat(git, "info", child_dir_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => {
                self.pop();
                self.pop();
                return None;
            }
            Err(error) => {
                self.push("exclude");
                self.fail(io::Error::from(error));
                self.pop();
                self.pop();
                self.pop();
                return None;
            }
        };
        self.push("exclude");
        let text = match open_ignore(info.as_fd(), "exclude") {
            Ok(Opened::Bytes(bytes)) => Some(decode_lossy(bytes)),
            Ok(Opened::Missing | Opened::NotRegular) => None,
            Err(error) => {
                self.fail(error);
                None
            }
        };
        self.pop();
        self.pop();
        self.pop();
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

    /// Exclude for a regular `.git` file: `gitdir:` then an optional
    /// `commondir`, then `<common>/info/exclude`. A fault is reported against
    /// the `.git` file. The gitdir may sit outside the walk root (a linked
    /// work tree, a submodule's module directory) and is opened by path, with
    /// `O_NOFOLLOW` on the final component.
    fn read_gitfile_exclude(&mut self, base: &Path, bytes: &[u8]) -> Option<String> {
        let raw = parse_gitdir(bytes)?;
        let gitdir = resolve_git_path(base, raw);
        let common = self.common_dir(&gitdir)?;
        self.read_external(&common.join("info").join("exclude"))
    }

    /// The gitdir itself when it has no `commondir` file. `None` when that
    /// file cannot be read, is not a regular file, or names nothing: a linked
    /// work tree's own directory is not where exclude lives, so it is not a
    /// fallback. Only a missing `commondir` means the gitdir is the common
    /// directory.
    fn common_dir(&mut self, gitdir: &Path) -> Option<PathBuf> {
        let commondir = gitdir.join("commondir");
        match open_regular_path(&commondir) {
            Ok(Opened::Missing) => Some(gitdir.to_path_buf()),
            Ok(Opened::NotRegular) => {
                self.fail_at_git(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: not a regular file", commondir.display()),
                ));
                None
            }
            Ok(Opened::Bytes(bytes)) => {
                let raw = first_line(&bytes)?;
                Some(resolve_git_path(gitdir, raw))
            }
            Err(error) => {
                self.fail_at_git(error);
                None
            }
        }
    }

    fn read_external(&mut self, path: &Path) -> Option<String> {
        match open_regular_path(path) {
            Ok(Opened::Bytes(bytes)) => Some(decode_lossy(bytes)),
            Ok(Opened::Missing | Opened::NotRegular) => None,
            Err(error) => {
                self.fail_at_git(io::Error::new(
                    error.kind(),
                    format!("{}: {error}", path.display()),
                ));
                None
            }
        }
    }

    fn fail_at_git(&mut self, error: io::Error) {
        self.push(".git");
        self.fail(error);
        self.pop();
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

fn open_regular_path(path: &Path) -> io::Result<Opened> {
    match open_path(path, nofollow_file_flags(), Mode::empty()) {
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
fn has_git(dir: &Path) -> bool {
    is_git_path(&dir.join(".git"), 0)
}

fn is_git_path(path: &Path, depth: u32) -> bool {
    if depth > 8 {
        return false;
    }
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    let kind = meta.file_type();
    if kind.is_symlink() {
        let Ok(target) = fs::read_link(path) else {
            return false;
        };
        let base = path.parent().unwrap_or(path);
        return is_git_path(&resolve_git_path(base, target.as_os_str()), depth + 1);
    }
    if kind.is_dir() {
        return is_git_dir(path, depth);
    }
    if kind.is_file() {
        let Some(parent) = path.parent() else {
            return false;
        };
        return is_git_file(parent, path, depth);
    }
    false
}

fn is_git_dir(path: &Path, depth: u32) -> bool {
    if depth > 8 {
        return false;
    }
    let head = fs::metadata(path.join("HEAD")).is_ok_and(|meta| meta.is_file());
    let refs = fs::metadata(path.join("refs")).is_ok_and(|meta| meta.is_dir());
    if !(head && refs) {
        return false;
    }
    if fs::metadata(path.join("objects")).is_ok_and(|meta| meta.is_dir()) {
        return true;
    }
    let Ok(bytes) = fs::read(path.join("commondir")) else {
        return false;
    };
    let Some(raw) = first_line(&bytes) else {
        return false;
    };
    is_git_dir(&resolve_git_path(path, raw), depth + 1)
}

fn is_git_file(work_tree: &Path, file: &Path, depth: u32) -> bool {
    let Ok(bytes) = fs::read(file) else {
        return false;
    };
    let Some(raw) = parse_gitdir(&bytes) else {
        return false;
    };
    is_git_path(&resolve_git_path(work_tree, raw), depth + 1)
}

fn device_of(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|meta| meta.dev())
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
