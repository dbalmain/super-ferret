//! What a walk reports: its events, the stat and decision they carry, and
//! the options a parallel walk takes.

use super::*;

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
