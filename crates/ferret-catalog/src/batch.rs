//! The rows one walk worker produces (D29).
//!
//! A [`Batch`] is minted by [`Transaction::batch`](crate::Transaction::batch)
//! and filled on one worker without locks. Directories are named by
//! [`DirToken`]s, which the walker carries to each child event on whichever
//! worker reports it, so a batch may name a parent minted in another batch.
//! Tokens are resolved, and every id assigned, only when the transaction
//! commits.
//!
//! Nothing is validated here: names, tokens and roots are checked at commit,
//! where a bad one is a [`BuildError`](crate::BuildError).

use crate::format::NONE;
use crate::{ContentState, Hash};

/// A directory, as minted by the batch that recorded it. Valid only within the
/// transaction whose batch minted it. `Copy + Send`, for the walker's
/// `EventVisitor::Dir`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DirToken {
    pub(crate) batch: u32,
    pub(crate) index: u32,
}

/// The `lstat` fields the catalog keeps. Times are split as the kernel gives
/// them, so equality is exact.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    /// `st_dev`.
    pub dev: u64,
    /// `st_ino`.
    pub ino: u64,
    /// `st_size`. For a symlink, the length of its target.
    pub size: u64,
    /// `st_mtim` seconds.
    pub mtime_sec: i64,
    /// `st_mtim` nanoseconds, below 10⁹.
    pub mtime_nsec: u32,
    /// `st_ctim` seconds.
    pub ctime_sec: i64,
    /// `st_ctim` nanoseconds, below 10⁹.
    pub ctime_nsec: u32,
    /// `st_mode`, file type bits included.
    pub mode: u32,
    /// `st_uid`.
    pub uid: u32,
    /// `st_gid`.
    pub gid: u32,
    /// `st_nlink`. Not part of [`Stat::same_version`]: a link added or removed
    /// bumps ctime, which is. Derived equality does compare it; see
    /// `build::same_observation`.
    pub nlink: u64,
}

impl Stat {
    /// Whether `other` describes the same version of the same inode: equal
    /// `(dev, ino, size, mtime, ctime)`, the D26 carry-over key. Mode and
    /// ownership changes bump ctime, so they are covered too.
    pub fn same_version(&self, other: &Stat) -> bool {
        (self.dev, self.ino, self.size) == (other.dev, other.ino, other.size)
            && (self.mtime_sec, self.mtime_nsec) == (other.mtime_sec, other.mtime_nsec)
            && (self.ctime_sec, self.ctime_nsec) == (other.ctime_sec, other.ctime_nsec)
    }
}

/// What a worker learnt about a file's content (D37).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Content {
    /// Not sent to the index: over the size cap, or a symlink.
    Unindexed,
    /// Sent to the index; the sniffer said binary.
    Binary,
    /// Sent to the index and hashed.
    Hashed(Hash),
    /// Sent to the index, but could not be read or changed while read.
    Fault,
}

impl Content {
    /// The two-bit state stored for this content.
    pub fn state(self) -> ContentState {
        match self {
            Self::Unindexed => ContentState::Unindexed,
            Self::Binary => ContentState::Binary,
            Self::Hashed(_) => ContentState::Hashed,
            Self::Fault => ContentState::Fault,
        }
    }
}

/// Which kind of work tree a directory is the top of (D23).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkTreeKind {
    /// Holds its repository's `.git` directory.
    Main = 0,
    /// A `git worktree` of another repository.
    Linked = 1,
    /// Its own repository, nested in another work tree.
    Submodule = 2,
}

impl WorkTreeKind {
    pub(crate) fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Main),
            1 => Some(Self::Linked),
            2 => Some(Self::Submodule),
            _ => None,
        }
    }
}

/// Where a byte string lies in one of a batch's buffers. 32-bit, so a
/// batch holds at most 4 GiB of names; past that the batch is marked and the
/// commit fails with `TooLarge`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Span {
    pub(crate) start: u32,
    pub(crate) len: u32,
}

impl Span {
    pub(crate) fn of(self, bytes: &[u8]) -> &[u8] {
        &bytes[self.start as usize..self.start as usize + self.len as usize]
    }
}

pub(crate) struct DirEntry {
    /// `None` for a root, whose `name` is then the root's path.
    pub(crate) parent: Option<DirToken>,
    pub(crate) name: Span,
    pub(crate) traversed: bool,
}

/// A file or symlink's place in the tree. Its stat, content and any link
/// target are in the batch's parallel columns, so the build can free the
/// structure once the name sections are written (D40).
pub(crate) struct FileEntry {
    pub(crate) parent: DirToken,
    pub(crate) name: Span,
}

pub(crate) struct WorkTreeEntry {
    pub(crate) dir: DirToken,
    pub(crate) kind: WorkTreeKind,
    /// In the batch's `strings`.
    pub(crate) common_dir: Span,
    pub(crate) common_id: (u64, u64),
}

/// The rows one worker recorded. Fill it, then hand it to
/// [`Transaction::add`](crate::Transaction::add).
///
/// Stored as columns: about 100 B per file plus its name, where one struct
/// per file with byte ranges took 136 B. At 10M entries that difference is
/// most of a gigabyte (D40).
pub struct Batch {
    pub(crate) id: u32,
    /// Set for a root copied forward from the previous generation, whose file
    /// observations yield to fresh ones (D34).
    pub(crate) carried: bool,
    pub(crate) dirs: Vec<DirEntry>,
    pub(crate) dir_stats: Vec<Stat>,
    pub(crate) files: Vec<FileEntry>,
    pub(crate) file_stats: Vec<Stat>,
    pub(crate) contents: Vec<Content>,
    /// (file index, target in `strings`), in file order.
    pub(crate) targets: Vec<(u32, Span)>,
    pub(crate) work_trees: Vec<WorkTreeEntry>,
    /// Raw entry counts, by directory. A directory with none is unknown.
    pub(crate) entry_counts: Vec<(DirToken, u32)>,
    /// Entry names, and root paths.
    pub(crate) names: Vec<u8>,
    /// Link targets and work-tree paths.
    pub(crate) strings: Vec<u8>,
    /// A buffer outgrew 32-bit spans.
    pub(crate) overflow: bool,
}

impl Batch {
    pub(crate) fn new(id: u32, carried: bool) -> Self {
        Self {
            id,
            carried,
            dirs: Vec::new(),
            dir_stats: Vec::new(),
            files: Vec::new(),
            file_stats: Vec::new(),
            contents: Vec::new(),
            targets: Vec::new(),
            work_trees: Vec::new(),
            entry_counts: Vec::new(),
            names: Vec::new(),
            strings: Vec::new(),
            overflow: false,
        }
    }

    /// Records a configured root, by its path as configured (absolute), and
    /// returns its token. A root's path is its identity: committing two roots
    /// with one path is an error.
    pub fn root(&mut self, path: &[u8], stat: Stat) -> DirToken {
        self.push_dir(None, path, stat, false)
    }

    /// Records a directory the policy catalogues (`Descend`).
    pub fn dir(&mut self, parent: DirToken, name: &[u8], stat: Stat) -> DirToken {
        self.push_dir(Some(parent), name, stat, false)
    }

    /// Records a directory the walk passes through without cataloguing it
    /// (`Traverse`): a structural row that holds re-included entries' names
    /// and is excluded from search (D29).
    pub fn traversed_dir(&mut self, parent: DirToken, name: &[u8], stat: Stat) -> DirToken {
        self.push_dir(Some(parent), name, stat, true)
    }

    /// Records a regular file. Each name of a hard-linked file is recorded
    /// with the same observation; the commit keeps one inode row (D31).
    pub fn file(&mut self, parent: DirToken, name: &[u8], stat: Stat, content: Content) {
        let name = push(&mut self.names, name, &mut self.overflow);
        self.files.push(FileEntry { parent, name });
        self.file_stats.push(stat);
        self.contents.push(content);
    }

    /// Records a symlink with its target as `readlink` returned it.
    pub fn symlink(&mut self, parent: DirToken, name: &[u8], stat: Stat, target: &[u8]) {
        let index = self.files.len() as u32;
        self.file(parent, name, stat, Content::Unindexed);
        let target = push(&mut self.strings, target, &mut self.overflow);
        self.targets.push((index, target));
    }

    /// Records that `dir` is the top of a work tree (D23). `common_id` is the
    /// repository's identity; `common_dir` is its path, for display.
    pub fn work_tree(
        &mut self,
        dir: DirToken,
        kind: WorkTreeKind,
        common_dir: &[u8],
        common_id: (u64, u64),
    ) {
        let common_dir = push(&mut self.strings, common_dir, &mut self.overflow);
        self.work_trees.push(WorkTreeEntry {
            dir,
            kind,
            common_dir,
            common_id,
        });
    }

    /// Records how many entries `getdents` returned for `dir`, minus `.` and
    /// `..`, before any ignore rule dropped one. A directory never given a
    /// count (unreadable, or not listed) is unknown to the reader. A count of
    /// `u32::MAX` or more is saturated to `u32::MAX - 1`, since `u32::MAX`
    /// stands for unknown in the file.
    pub fn entry_count(&mut self, dir: DirToken, count: u32) {
        self.entry_counts.push((dir, count.min(NONE - 1)));
    }

    /// A symlink's target, for file `index`.
    pub(crate) fn target(&self, index: usize) -> Option<&[u8]> {
        let at = self
            .targets
            .binary_search_by_key(&(index as u32), |&(i, _)| i)
            .ok()?;
        Some(self.targets[at].1.of(&self.strings))
    }

    /// Drops what only the name sections need: every entry's parent and name.
    pub(crate) fn drop_structure(&mut self) {
        self.files = Vec::new();
        self.names = Vec::new();
    }

    fn push_dir(
        &mut self,
        parent: Option<DirToken>,
        name: &[u8],
        stat: Stat,
        traversed: bool,
    ) -> DirToken {
        let name = push(&mut self.names, name, &mut self.overflow);
        let token = DirToken {
            batch: self.id,
            index: self.dirs.len() as u32,
        };
        self.dirs.push(DirEntry {
            parent,
            name,
            traversed,
        });
        self.dir_stats.push(stat);
        token
    }
}

fn push(buffer: &mut Vec<u8>, bytes: &[u8], overflow: &mut bool) -> Span {
    match (u32::try_from(buffer.len()), u32::try_from(bytes.len())) {
        (Ok(start), Ok(len)) if start.checked_add(len).is_some() => {
            buffer.extend_from_slice(bytes);
            Span { start, len }
        }
        _ => {
            *overflow = true;
            Span::default()
        }
    }
}
