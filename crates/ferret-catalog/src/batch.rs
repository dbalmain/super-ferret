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
use crate::{Catalog, ContentState, Generation, Hash, InoId, Kind, NameId};

/// A directory, as minted by the batch that recorded it. Valid only within the
/// transaction whose batch minted it. `Copy + Send`, for the walker's
/// `EventVisitor::Dir`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DirToken {
    pub(crate) batch: u32,
    pub(crate) index: u32,
    /// Checked same-path directory hint in the batch's pinned view.
    pub(crate) old: u32,
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
    pub(crate) old: u32,
    /// `None` for a root, whose `name` is then the root's path.
    pub(crate) parent: Option<DirToken>,
    pub(crate) name: Span,
    pub(crate) traversed: bool,
    pub(crate) retained_at: Option<u64>,
}

/// A file or symlink's place in the tree. Its stat, content and any link
/// target are in the batch's parallel columns, so the build can free the
/// structure once the name sections are written (D40).
pub(crate) struct FileEntry {
    pub(crate) parent: DirToken,
    pub(crate) name: Span,
}

pub(crate) struct IgnoredEntry {
    pub(crate) parent: DirToken,
    pub(crate) name: Span,
    pub(crate) kind: Kind,
}

pub(crate) struct WorkTreeEntry {
    pub(crate) dir: DirToken,
    pub(crate) kind: WorkTreeKind,
    /// In the batch's `strings`.
    pub(crate) common_dir: Span,
    pub(crate) common_id: (u64, u64),
}

struct PendingFile {
    parent: DirToken,
    name: Vec<u8>,
    stat: Stat,
    content: Content,
    target: Option<Vec<u8>>,
}

/// The rows one worker recorded. Fill it, then hand it to
/// [`Transaction::add`](crate::Transaction::add).
///
/// Stored as columns: about 100 B per file plus its name, where one struct
/// per file with byte ranges took 136 B. At 10M entries that difference is
/// most of a gigabyte (D40).
pub struct Batch {
    previous: Option<Catalog>,
    pending: Vec<PendingFile>,
    reused: Vec<(DirToken, NameId)>,
    pub(crate) id: u32,
    /// Set for a root copied forward from the previous generation, whose file
    /// observations yield to fresh ones (D34).
    pub(crate) carried: bool,
    pub(crate) dirs: Vec<DirEntry>,
    pub(crate) dir_stats: Vec<Stat>,
    pub(crate) files: Vec<FileEntry>,
    pub(crate) ignored: Vec<IgnoredEntry>,
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
            previous: None,
            pending: Vec::new(),
            reused: Vec::new(),
            id,
            carried,
            dirs: Vec::new(),
            dir_stats: Vec::new(),
            files: Vec::new(),
            ignored: Vec::new(),
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
    /// (`Traverse`). The writer retains it as an ordinary directory when it
    /// has a visible descendant, preserving its search-suppression flag
    /// (D29). Otherwise it becomes one ignored marker without an inode.
    pub fn traversed_dir(&mut self, parent: DirToken, name: &[u8], stat: Stat) -> DirToken {
        self.push_dir(Some(parent), name, stat, true)
    }

    /// Records a regular file. Each name of a hard-linked file is recorded
    /// with the same observation; the commit keeps one inode row (D31).
    pub fn file(&mut self, parent: DirToken, name: &[u8], stat: Stat, content: Content) {
        self.observe_file(parent, name, stat, content, None);
    }

    pub(crate) fn with_previous(mut self, previous: Catalog) -> Self {
        self.previous = Some(previous);
        self
    }

    fn observe_file(
        &mut self,
        parent: DirToken,
        name: &[u8],
        stat: Stat,
        content: Content,
        target: Option<&[u8]>,
    ) {
        if self.previous.is_some() && parent.old != NONE && stat.nlink <= 1 {
            if self.pending.first().is_some_and(|f| f.parent != parent) {
                self.finish_observations();
            }
            self.pending.push(PendingFile {
                parent,
                name: name.to_vec(),
                stat,
                content,
                target: target.map(<[u8]>::to_vec),
            });
        } else {
            self.push_file(parent, name, stat, content, target);
        }
    }

    fn push_file(
        &mut self,
        parent: DirToken,
        name: &[u8],
        stat: Stat,
        content: Content,
        target: Option<&[u8]>,
    ) {
        let index = self.files.len() as u32;
        let name = push(&mut self.names, name, &mut self.overflow);
        self.files.push(FileEntry { parent, name });
        self.file_stats.push(stat);
        self.contents.push(content);
        if let Some(target) = target {
            let span = push(&mut self.strings, target, &mut self.overflow);
            self.targets.push((index, span));
        }
    }

    /// Reduces a completed local listing against sorted old children. Equal
    /// single-name rows retain only an old-name reference. Alias candidates
    /// and changed observations stay complete for final-set reconciliation.
    /// Unflushed observations remain visible through the borrowed accessors.
    pub fn finish_observations(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let mut pending = std::mem::take(&mut self.pending);
        pending.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        let old = self.previous.clone();
        if let Some(old) = old {
            let mut children = old.children(InoId(pending[0].parent.old)).peekable();
            let names = old.name_reader();
            // A duplicate observation is invalid even if both rows are equal.
            let duplicate = pending.windows(2).any(|p| p[0].name == p[1].name);
            for file in &pending {
                while children
                    .peek()
                    .is_some_and(|&id| names.get(id).bytes < file.name.as_slice())
                {
                    children.next();
                }
                let matched = children
                    .peek()
                    .copied()
                    .filter(|&id| names.get(id).bytes == file.name);
                let reusable = !duplicate
                    && matched.is_some_and(|id| {
                        let edge = names.get(id);
                        let crate::Target::Inode(child) = edge.target() else {
                            return false;
                        };
                        if old.is_directory(child) || old.indexed_name_count(child) != 1 {
                            return false;
                        }
                        let inode = old.inode(child);
                        inode.stat == file.stat
                            && old.kind(child) == Kind::from_mode(file.stat.mode)
                            && content(&old, child) == file.content
                            && old.link_target(child) == file.target.as_deref()
                    });
                if reusable {
                    if let Some(id) = matched {
                        self.reused.push((file.parent, id));
                    }
                } else {
                    self.push_file(
                        file.parent,
                        &file.name,
                        file.stat,
                        file.content,
                        file.target.as_deref(),
                    );
                }
            }
        }
        pending.clear();
        self.pending = pending;
    }

    pub(crate) fn materialize(&mut self) {
        self.finish_observations();
        let old = self.previous.take();
        if let Some(old) = old {
            for (parent, id) in std::mem::take(&mut self.reused) {
                let edge = old.name(id);
                let child = edge.child;
                self.push_file(
                    parent,
                    edge.bytes,
                    old.inode(child).stat,
                    content(&old, child),
                    old.link_target(child),
                );
            }
        }
    }

    /// Records an ignored name without stat or content. A directory is an
    /// opaque marker: nothing below it is recorded.
    pub fn ignored(&mut self, parent: DirToken, name: &[u8], kind: Kind) {
        let name = push(&mut self.names, name, &mut self.overflow);
        self.ignored.push(IgnoredEntry { parent, name, kind });
    }

    /// Records a symlink with its target as `readlink` returned it.
    pub fn symlink(&mut self, parent: DirToken, name: &[u8], stat: Stat, target: &[u8]) {
        self.observe_file(parent, name, stat, Content::Unindexed, Some(target));
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

    /// Records a retained subtree's last trustworthy sequence. The token must
    /// belong to this batch. The writer rejects sequences beyond its view.
    pub fn retained_at(&mut self, dir: DirToken, sequence: Option<u64>) {
        assert_eq!(
            dir.batch, self.id,
            "retained directory belongs to another batch"
        );
        self.dirs[dir.index as usize].retained_at = sequence;
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
        self.ignored = Vec::new();
        self.names = Vec::new();
    }

    fn push_dir(
        &mut self,
        parent: Option<DirToken>,
        name: &[u8],
        stat: Stat,
        traversed: bool,
    ) -> DirToken {
        let old = self
            .previous
            .as_ref()
            .and_then(|old| {
                let id = match parent {
                    Some(parent) if parent.old != NONE => old
                        .lookup(InoId(parent.old), name)
                        .map(|id| old.name(id).child),
                    Some(_) => None,
                    None => old
                        .roots()
                        .find(|(_, path)| *path == name)
                        .map(|(id, _)| id),
                }?;
                (old.is_live_inode(id)
                    && old.is_directory(id)
                    && old.identity(id) == (stat.dev, stat.ino))
                    .then_some(id.0)
            })
            .unwrap_or(NONE);
        let name = push(&mut self.names, name, &mut self.overflow);
        let token = DirToken {
            batch: self.id,
            index: self.dirs.len() as u32,
            old,
        };
        self.dirs.push(DirEntry {
            old,
            parent,
            name,
            traversed,
            retained_at: None,
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

/// Borrowed directory observation. Tokens remain local to the producing run.
#[derive(Clone, Copy)]
pub struct DirectoryObservation<'a> {
    pub token: DirToken,
    pub parent: Option<DirToken>,
    pub name: &'a [u8],
    pub stat: Stat,
    pub traversed: bool,
    pub retained_at: Option<u64>,
}

/// Borrowed file observation, including symlinks and special files.
#[derive(Clone, Copy)]
pub struct FileObservation<'a> {
    pub parent: DirToken,
    pub name: &'a [u8],
    pub stat: Stat,
    pub content: Content,
    pub target: Option<&'a [u8]>,
}

impl Batch {
    /// Directory observations, without copying their strings or stat columns.
    pub fn directories(&self) -> impl Iterator<Item = DirectoryObservation<'_>> {
        self.dirs
            .iter()
            .enumerate()
            .map(|(i, dir)| DirectoryObservation {
                token: DirToken {
                    batch: self.id,
                    index: i as u32,
                    old: dir.old,
                },
                parent: dir.parent,
                name: dir.name.of(&self.names),
                stat: self.dir_stats[i],
                traversed: dir.traversed,
                retained_at: dir.retained_at,
            })
    }
    /// Number of file observations in this batch.
    pub fn file_count(&self) -> usize {
        self.files.len() + self.pending.len()
    }
    /// Borrows one file observation; `index` must be below `file_count`.
    pub fn file_observation(&self, index: usize) -> FileObservation<'_> {
        if index >= self.files.len() {
            let f = &self.pending[index - self.files.len()];
            return FileObservation {
                parent: f.parent,
                name: &f.name,
                stat: f.stat,
                content: f.content,
                target: f.target.as_deref(),
            };
        }
        let file = &self.files[index];
        FileObservation {
            parent: file.parent,
            name: file.name.of(&self.names),
            stat: self.file_stats[index],
            content: self.contents[index],
            target: self.target(index),
        }
    }
    /// Equal rows proven against this batch's pinned generation. The token
    /// still needs to resolve to its hinted old directory before reuse.
    pub fn reused_files(&self) -> impl Iterator<Item = (DirToken, NameId)> + '_ {
        self.reused.iter().copied()
    }
    /// Parents whose included files survived local equal-row reduction.
    pub fn reused_directories(&self) -> impl Iterator<Item = DirToken> + '_ {
        let mut last = None;
        self.reused.iter().filter_map(move |&(parent, _)| {
            if last == Some(parent) {
                None
            } else {
                last = Some(parent);
                Some(parent)
            }
        })
    }
    /// Number of compact equal observations.
    pub fn reused_file_count(&self) -> usize {
        self.reused.len()
    }
    /// Reconstructs a compact observation from the pinned view, for alias
    /// conflict resolution or a changed parent. Index must be in range.
    pub fn reused_file_observation(&self, index: usize) -> FileObservation<'_> {
        let (parent, name) = self.reused[index];
        let Some(old) = self.previous.as_ref() else {
            unreachable!("reused observations are recorded only with a pinned view");
        };
        let edge = old.name(name);
        FileObservation {
            parent,
            name: edge.bytes,
            stat: old.inode(edge.child).stat,
            content: content(old, edge.child),
            target: old.link_target(edge.child),
        }
    }
    /// Generation against which compact observations were checked.
    pub fn observation_generation(&self) -> Option<Generation> {
        self.previous.as_ref().map(Catalog::generation)
    }
    /// Opaque ignored edges, without allocating inode observations.
    pub fn ignored_entries(&self) -> impl Iterator<Item = (DirToken, &[u8], Kind)> {
        self.ignored
            .iter()
            .map(|e| (e.parent, e.name.of(&self.names), e.kind))
    }
    /// Raw listing counts; a token without a count has incomplete coverage.
    pub fn entry_counts(&self) -> impl Iterator<Item = (DirToken, u32)> + '_ {
        self.entry_counts.iter().copied()
    }
    /// Work-tree observations, borrowing repository paths.
    pub fn work_tree_observations(
        &self,
    ) -> impl Iterator<Item = (DirToken, WorkTreeKind, &[u8], (u64, u64))> {
        self.work_trees
            .iter()
            .map(|w| (w.dir, w.kind, w.common_dir.of(&self.strings), w.common_id))
    }
    /// Whether an observation buffer exceeded the wire-format span limit.
    pub fn overflowed(&self) -> bool {
        self.overflow
    }
}

impl DirToken {
    /// Coordinates for a run-local dense token table.
    pub fn coordinates(self) -> (u32, u32) {
        (self.batch, self.index)
    }
    /// Same-path directory hint checked in the observation batch's view.
    pub fn previous_directory(self) -> Option<InoId> {
        (self.old != NONE).then_some(InoId(self.old))
    }
}

fn content(old: &Catalog, id: InoId) -> Content {
    match old.state(id) {
        ContentState::Unindexed => Content::Unindexed,
        ContentState::Binary => Content::Binary,
        ContentState::Fault => Content::Fault,
        ContentState::Hashed => {
            let Some(hash) = old.doc(id).and_then(|doc| old.doc_hash(doc)) else {
                unreachable!("writer validates hashed inode document bindings before batch creation");
            };
            Content::Hashed(hash)
        }
    }
}
