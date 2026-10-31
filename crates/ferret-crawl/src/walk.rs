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

mod ignores;
mod schedule;

use ignores::FoundWorkTree;
use schedule::{Shared, run_guarded, run_worker};

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
    /// A hidden (dot-named) entry that a `!` line in a repository's ignore
    /// files reveals, as it does to rg (D127). Never set for
    /// [`Decision::Skip`] or for a name without a leading dot.
    pub revealed: bool,
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
/// those except the symlink is also an [`IoOp::ProbeGit`] fault on `.git`: the
/// work tree's exclude rules and record are unknown, not absent. A gitdir, or
/// the directory its `commondir` names, that does not exist (`ENOENT`) is the
/// exception: a stale work tree whose repository was moved or pruned is a
/// known state, with no exclude rules and no record, and raises no fault, as
/// rg reports none (D26).
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

/// Loads one directory's policy with the same ignore reads, Git probes and
/// diagnostics as the crawler, without enumerating or reading its contents.
/// `parent` supplies inherited rules; `None` starts a configured root with
/// `global` and `config`. The directory itself is opened without following a
/// symlink, unless `listed` supplies it already open. Policy names are then
/// probed directly; with `listed`, only names its listing held are opened.
///
/// I/O faults within policy inputs are reported as [`Event::Io`], and bad
/// patterns as [`Event::Pattern`]. Valid patterns still apply. The caller
/// decides whether input faults prevent using the returned rules. Failure to
/// open the directory itself is returned directly.
pub fn load_policy(
    path: &Path,
    parent: Option<&DirRules>,
    global: Option<&str>,
    config: Config,
    listed: Option<Listed<'_>>,
    visit: impl FnMut(Event<'_, ()>),
) -> io::Result<DirRules> {
    load_query_policy(path, parent, global, config, false, listed, visit)
}

/// A directory its caller already holds open and has listed, so loading its
/// policy reads through `dir` and opens only the inputs `contains` names,
/// with no open of its path and no probe per input. `contains` answers for
/// names beginning with `.`; every policy input is one.
#[derive(Clone, Copy)]
pub struct Listed<'a> {
    pub dir: BorrowedFd<'a>,
    pub contains: &'a dyn Fn(&OsStr) -> bool,
}

/// Loads rg's git, `.ignore` and `.rgignore` inputs with the crawler's scoped
/// reads. Ferret ignore files are not consulted; content has no size cap.
/// As in rg, a symlinked `.git` is followed. A target that does not resolve
/// — a loop, a dangling link, or a component that cannot be traversed —
/// starts no work tree. An execute-only directory still does: it is opened
/// `O_PATH` and `info/exclude` is read by name. A `.git` file contributes
/// `info/exclude` only through a `commondir`, including when that directory
/// is execute-only. Diagnostics and directory-open failures follow
/// [`load_policy`].
pub fn load_grep_policy(
    path: &Path,
    parent: Option<&DirRules>,
    global: Option<&str>,
    listed: Option<Listed<'_>>,
    visit: impl FnMut(Event<'_, ()>),
) -> io::Result<DirRules> {
    load_query_policy(
        path,
        parent,
        global,
        Config { size_cap: u64::MAX },
        true,
        listed,
        visit,
    )
}

fn load_query_policy(
    path: &Path,
    parent: Option<&DirRules>,
    global: Option<&str>,
    config: Config,
    grep: bool,
    listed: Option<Listed<'_>>,
    visit: impl FnMut(Event<'_, ()>),
) -> io::Result<DirRules> {
    // Without a listing, policy inputs are probed by name, so grep can load
    // an ancestor it may search but not read, as rg does.
    let opened;
    let fd = match listed {
        Some(listed) => listed.dir,
        None => {
            opened = open_path(
                path,
                if grep {
                    OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC
                } else {
                    child_dir_flags()
                },
                Mode::empty(),
            )?;
            opened.as_fd()
        }
    };
    let boundaries = BoundaryIndex::default();
    let mut walker = Walker::new(path, &boundaries, visit);
    walker.rg_git = grep;
    let loaded = walker.load_ignores(
        fd,
        |name| {
            if grep && name == ".ferretignore" {
                return false;
            }
            match listed {
                Some(listed) => (listed.contains)(OsStr::new(name)),
                None => !matches!(
                    statat(fd, name, AtFlags::SYMLINK_NOFOLLOW),
                    Err(Errno::NOENT)
                ),
            }
        },
        parent.is_some_and(DirRules::in_work_tree),
        (),
    );
    let (rules, errors) = if let Some(parent) = parent {
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "policy directory has no name")
        })?;
        parent.enter(name, loaded.files())
    } else {
        DirRules::root(path, global, loaded.files(), config)
    };
    walker.patterns(errors);
    Ok(rules)
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

    /// [`EventVisitor::policy_input`] for each of `names`, all in `parent`.
    fn policy_inputs(&mut self, parent: BorrowedFd<'_>, names: &[&OsStr]) {
        for name in names {
            self.policy_input(parent, name);
        }
    }

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
        let loaded = self.load_ignores(fd.as_fd(), |name| children.contains(name), false, token);
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

/// A directory location, with no read of the directory itself. Opening needs
/// no permission on the final directory, so an execute-only `.git` or gitdir
/// still yields a handle; named reads then need search permission. Grep uses
/// this so `info/exclude` can be read by name. The crawl uses
/// [`root_dir_flags`], which requires the directory to be readable.
fn path_dir_flags() -> OFlags {
    OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC
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
    /// Rg's reading of `.git`, for grep's policy loads. A symlinked `.git`
    /// is followed: one that does not resolve (a loop, a dangling link, or
    /// a component that cannot be traversed) starts no work tree, while an
    /// execute-only directory is opened `O_PATH` so `info/exclude` can be
    /// read by name. A `.git` file's `info/exclude` applies only through a
    /// `commondir`. D13 opens nothing through a symlinked `.git`, counts
    /// any `.git` entry, and reads a gitdir's own exclude.
    rg_git: bool,
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
    /// Runs after the opened policy file's stat and before its bounded read.
    pub(crate) static AFTER_IGNORE_STAT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
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
    Readlink,
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
fn after_open(rel: &[u8]) {
    AFTER_OPEN.with_borrow_mut(|hook| {
        if let Some(hook) = hook {
            hook(bytes_path(rel));
        }
    });
}

/// A parent's continuation, and the child directory to walk next.
type Descent<D> = (Job<D>, Job<D>);

impl<'b, V: EventVisitor> Walker<'b, V> {
    fn new(root: &Path, boundaries: &'b BoundaryIndex, visit: V) -> Self {
        Self {
            root: root.to_path_buf(),
            root_id: None,
            boundaries,
            rel: Vec::with_capacity(256),
            dents: Vec::with_capacity(DENTS_BYTES),
            visit,
            rg_git: false,
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
        // Only a dot name can be revealed, so others never ask the rules.
        let revealed = decision != Decision::Skip
            && name.as_bytes().starts_with(b".")
            && here.rules.reveals(name, kind == FileType::Directory);
        self.visit.visit(Event::Decided(Decided {
            parent: here.token,
            parent_fd: here.fd,
            name,
            path: bytes_path(&self.rel),
            decision,
            stat,
            kind: catalog_kind(kind),
            revealed,
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
        if let Some((op, error)) = inject(&self.root, IoPoint::Listing(0), bytes_path(&self.rel)) {
            self.fail(op, context, error);
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
        let (stat, target) = match observe_link(here.fd, name, &self.root, bytes_path(&self.rel)) {
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
        let loaded = self.load_ignores(
            child.as_fd(),
            |name| children.contains(name),
            here.rules.in_work_tree(),
            token,
        );
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
}

/// `O_PATH | O_NOFOLLOW`, then `fstat` and, for a symlink, `readlinkat` of
/// that descriptor. `Ok`'s stat is the inode the descriptor refers to, which
/// may no longer be a symlink; then the target is `None`. An error says which
/// step failed.
fn observe_link(
    dir: BorrowedFd<'_>,
    name: &OsStr,
    _root: &Path,
    _relative: &Path,
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
    if let Some(error) = inject(_root, IoPoint::Readlink, _relative) {
        return Err(error);
    }
    let raw = readlinkat(&fd, "", Vec::new())
        .map_err(|error| (IoOp::Readlink, io::Error::from(error)))?;
    Ok((stat, Some(OsString::from_vec(raw.into_bytes()))))
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
