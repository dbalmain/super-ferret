//! The rows one walk worker produces (D29).
//!
//! A [`Batch`] is minted by [`Transaction::batch`](crate::Transaction::batch)
//! or [`WriterSession::batch`](crate::WriterSession::batch)
//! and filled on one worker without locks. Directories are named by
//! [`DirToken`]s, which the walker carries to each child event on whichever
//! worker reports it, so a batch may name a parent minted in another batch.
//! Tokens are resolved, and every id assigned, only when the transaction
//! commits.
//!
//! Names, tokens and root coverage are validated at reconciliation/commit.
//! Resident batches reduce equal rows against their pinned view one listing
//! at a time; this proof saves observation storage, not coverage validation.
//! Checkpoint fallback expands those references into full observations.

/// Maximum temporary file rows before local reconciliation releases them.
pub const OBSERVATION_ROWS: usize = 4096;
/// Maximum temporary name/target bytes between local reductions.
pub const OBSERVATION_BYTES: usize = 1 << 20;

mod seen;
use seen::Seen;

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
    name: Span,
    stat: Stat,
    content: Content,
    target: Option<Span>,
}

/// The rows one worker recorded. Fill it, then hand it to
/// [`Transaction::add`](crate::Transaction::add).
///
/// Stored as columns: about 100 B per file plus its name, where one struct
/// per file with byte ranges took 136 B. At 10M entries that difference is
/// most of a gigabyte (D40).
pub struct Batch {
    input_budget: Option<std::sync::Arc<crate::InputBudget>>,
    full: bool,
    file_capacity: usize,
    previous: Option<Catalog>,
    pending: Vec<PendingFile>,
    pending_bytes: Vec<u8>,
    reused: Seen,
    preserved: Seen,
    pending_peak: usize,
    pending_bytes_peak: usize,
    chunked: bool,
    pub(crate) id: u32,
    /// Set for a root copied forward from the previous generation, whose file
    /// observations yield to fresh ones (D34).
    pub(crate) carried: bool,
    /// Untouched inode edges require the resident final-set reconciler.
    pub(crate) scoped: bool,
    pub(crate) dirs: Vec<DirEntry>,
    pub(crate) dir_stats: Vec<Stat>,
    pub(crate) files: Vec<FileEntry>,
    retained_files: Vec<u64>,
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
            input_budget: None,
            full: false,
            file_capacity: 0,
            previous: None,
            pending: Vec::new(),
            pending_bytes: Vec::new(),
            reused: Seen::default(),
            preserved: Seen::default(),
            pending_peak: 0,
            pending_bytes_peak: 0,
            chunked: false,
            id,
            carried,
            scoped: false,
            dirs: Vec::new(),
            dir_stats: Vec::new(),
            files: Vec::new(),
            retained_files: Vec::new(),
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

    /// Copies a checked retained namespace occurrence. Fresh aliases of the
    /// same inode supersede this observation (D31/D34), rather than conflict.
    pub fn retained_file(
        &mut self,
        parent: DirToken,
        name: &[u8],
        stat: Stat,
        content: Content,
        target: Option<&[u8]>,
    ) {
        let index = self.files.len();
        self.push_file(parent, name, stat, content, target);
        if self.files.len() > index {
            self.retained_files
                .resize(self.retained_files.len().max(index / 64 + 1), 0);
            self.retained_files[index / 64] |= 1 << (index % 64);
        }
    }
    pub(crate) fn file_carried(&self, index: usize) -> bool {
        self.carried
            || self
                .retained_files
                .get(index / 64)
                .is_some_and(|word| word & (1 << (index % 64)) != 0)
    }

    /// Attaches one run's changed-input guard before collecting observations.
    pub fn with_input_budget(mut self, budget: std::sync::Arc<crate::InputBudget>) -> Self {
        self.input_budget = Some(budget);
        self
    }
    /// Whether any worker exhausted the shared guard. Outputs from that
    /// attempt must be discarded, never interpreted as missing entries.
    pub fn input_exceeded(&self) -> bool {
        self.input_budget.as_ref().is_some_and(|b| b.exceeded())
    }
    /// Collects complete checkpoint observations while retaining old parent
    /// hints for fault anchoring. No equal rows are replaced by seen bits.
    pub(crate) fn full_observations(mut self, file_capacity: usize) -> Self {
        self.full = true;
        self.file_capacity = file_capacity;
        self
    }
    /// Dense token remapping for a subset of a full checkpoint batch. All
    /// batches' mappings must be combined before retaining cross-worker edges.
    pub fn checkpoint_tokens<'a>(
        &'a self,
        mut keep: impl FnMut(DirToken) -> bool + 'a,
    ) -> impl Iterator<Item = (DirToken, DirToken)> + 'a {
        self.directories()
            .filter(move |d| keep(d.token))
            .enumerate()
            .map(|(index, d)| {
                (
                    d.token,
                    DirToken {
                        index: index as u32,
                        ..d.token
                    },
                )
            })
    }

    /// Prunes full checkpoint columns in place and remaps surviving parents.
    /// The map must contain every retained directory across all worker batches.
    /// `keep_edge` receives original parent tokens and borrowed basenames.
    /// No file/name/stat column is copied into another batch.
    pub fn retain_checkpoint(
        &mut self,
        mapping: &std::collections::BTreeMap<DirToken, DirToken>,
        keep_edge: impl Fn(DirToken, &[u8]) -> bool,
    ) {
        assert!(self.full, "checkpoint pruning requires full observations");
        self.seal();
        let id = self.id;
        let mut read = 0;
        let mut write = 0;
        self.dirs.retain_mut(|dir| {
            let token = DirToken {
                batch: id,
                index: read as u32,
                old: dir.old,
            };
            let index = read;
            read += 1;
            if !mapping.contains_key(&token) {
                return false;
            }
            dir.parent = dir.parent.map(|parent| mapping[&parent]);
            self.dir_stats[write] = self.dir_stats[index];
            write += 1;
            true
        });
        self.dir_stats.truncate(write);
        let retained = std::mem::take(&mut self.retained_files);
        let mut read = 0;
        let mut write = 0;
        let mut target_read = 0;
        let mut target_write = 0;
        self.files.retain_mut(|file| {
            let index = read;
            read += 1;
            let target = self
                .targets
                .get(target_read)
                .filter(|&&(i, _)| i as usize == index)
                .copied();
            if target.is_some() {
                target_read += 1;
            }
            let Some(&parent) = mapping.get(&file.parent) else {
                return false;
            };
            if !keep_edge(file.parent, file.name.of(&self.names)) {
                return false;
            }
            file.parent = parent;
            self.file_stats[write] = self.file_stats[index];
            self.contents[write] = self.contents[index];
            if let Some((_, span)) = target {
                self.targets[target_write] = (write as u32, span);
                target_write += 1;
            }
            if retained
                .get(index / 64)
                .is_some_and(|word| word & (1 << (index % 64)) != 0)
            {
                self.retained_files
                    .resize(self.retained_files.len().max(write / 64 + 1), 0);
                self.retained_files[write / 64] |= 1 << (write % 64);
            }
            write += 1;
            true
        });
        self.file_stats.truncate(write);
        self.contents.truncate(write);
        self.targets.truncate(target_write);
        self.ignored.retain_mut(|edge| {
            let Some(&parent) = mapping.get(&edge.parent) else {
                return false;
            };
            if !keep_edge(edge.parent, edge.name.of(&self.names)) {
                return false;
            }
            edge.parent = parent;
            true
        });
        self.work_trees.retain_mut(|work| {
            let Some(&dir) = mapping.get(&work.dir) else {
                return false;
            };
            if !keep_edge(work.dir, b"") {
                return false;
            }
            work.dir = dir;
            true
        });
        self.entry_counts.retain_mut(|(token, _)| {
            let Some(&mapped) = mapping.get(token) else {
                return false;
            };
            *token = mapped;
            true
        });
    }

    /// Sets unknown counts and retention markers for original directory tokens.
    /// None represents covered opacity; absent tokens keep their observations.
    /// Apply this before token remapping.
    pub fn checkpoint_coverage(
        &mut self,
        markers: &std::collections::BTreeMap<DirToken, Option<u64>>,
    ) {
        for (index, dir) in self.dirs.iter_mut().enumerate() {
            let token = DirToken {
                batch: self.id,
                index: index as u32,
                old: dir.old,
            };
            if let Some(&marker) = markers.get(&token) {
                dir.retained_at = marker;
            }
        }
        self.entry_counts
            .retain(|(token, _)| !markers.contains_key(token));
    }

    pub(crate) fn release_full_source(&mut self) {
        // Full observations own all rows; their old hints have already served
        // fault anchoring. Compact batches still require the pinned source.
        if self.full {
            self.previous = None;
        }
    }
    fn reserve_input(&self, records: usize, bytes: usize) -> bool {
        self.input_budget
            .as_ref()
            .is_none_or(|b| b.charge(records, bytes).is_ok())
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
        if !self.full && self.previous.is_some() && parent.old != NONE {
            if self.pending.first().is_some_and(|f| f.parent != parent) {
                self.finish_observations();
                self.chunked = false;
            }
            if self.pending.len() >= OBSERVATION_ROWS
                || self.pending_bytes.len() + name.len() + target.map_or(0, <[u8]>::len)
                    > OBSERVATION_BYTES
            {
                self.finish_observations();
                self.chunked = true;
            }
            let name = push(&mut self.pending_bytes, name, &mut self.overflow);
            let target =
                target.map(|target| push(&mut self.pending_bytes, target, &mut self.overflow));
            self.pending.push(PendingFile {
                parent,
                name,
                stat,
                content,
                target,
            });
            self.pending_peak = self.pending_peak.max(self.pending.len());
            self.pending_bytes_peak = self.pending_bytes_peak.max(self.pending_bytes.len());
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
        if !self.reserve_input(
            1,
            std::mem::size_of::<Stat>()
                + std::mem::size_of::<FileEntry>()
                + std::mem::size_of::<Content>()
                + name.len()
                + target.map_or(0, |target| {
                    std::mem::size_of::<(u32, Span)>() + target.len()
                }),
        ) {
            return;
        }
        if self.file_capacity > 0 {
            self.files.reserve_exact(self.file_capacity);
            self.file_stats.reserve_exact(self.file_capacity);
            self.contents.reserve_exact(self.file_capacity);
            self.file_capacity = 0;
        }
        if self.full && self.files.len() == self.files.capacity() {
            // Full rewalks know approximately how many rows they replace.
            // Extra rows grow in bounded increments, not by doubling every
            // large column. Each reserve completes before any column push.
            const ROWS: usize = 16_384;
            self.files.reserve_exact(ROWS);
            self.file_stats.reserve_exact(ROWS);
            self.contents.reserve_exact(ROWS);
        }
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
    /// rows retain seen bits and a checked parent hint. Relevant aliases are
    /// reconstructed for final-set reconciliation; changed rows stay complete.
    /// Unflushed observations remain visible through the borrowed accessors.
    pub fn finish_observations(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let mut pending = std::mem::take(&mut self.pending);
        let mut bytes = std::mem::take(&mut self.pending_bytes);
        pending.sort_unstable_by(|a, b| a.name.of(&bytes).cmp(b.name.of(&bytes)));
        let old = self.previous.clone();
        if let Some(old) = old {
            let names = old.name_reader();
            let mut children = old
                .children(InoId(pending[0].parent.old))
                .map(|id| (id, names.get(id)))
                .peekable();
            // Duplicate observations cannot become a single equal-row proof.
            let duplicate = pending
                .windows(2)
                .any(|p| p[0].name.of(&bytes) == p[1].name.of(&bytes));
            for file in &pending {
                let name = file.name.of(&bytes);
                let target = file.target.map(|span| span.of(&bytes));
                let matched = if self.chunked {
                    old.lookup(InoId(file.parent.old), name)
                        .map(|id| (id, names.get(id)))
                } else {
                    while children.peek().is_some_and(|(_, edge)| edge.bytes < name) {
                        children.next();
                    }
                    children
                        .peek()
                        .copied()
                        .filter(|(_, edge)| edge.bytes == name)
                };
                let reusable = !duplicate
                    && matched.is_some_and(|(_, edge)| {
                        let crate::Target::Inode(child) = edge.target() else {
                            return false;
                        };
                        if old.is_directory(child) {
                            return false;
                        }
                        let inode = old.inode(child);
                        let kind = old.kind(child);
                        inode.stat == file.stat
                            && kind == Kind::from_mode(file.stat.mode)
                            && inode.state == file.content.state()
                            && match file.content {
                                Content::Hashed(hash) => {
                                    inode.doc.and_then(|doc| old.doc_hash(doc)) == Some(hash)
                                }
                                _ => true,
                            }
                            && if kind == Kind::Symlink {
                                old.link_target(child) == target
                            } else {
                                target.is_none()
                            }
                    });
                if reusable {
                    if let Some((id, edge)) = matched {
                        self.reused.insert(file.parent, id, Some(edge.child));
                    }
                } else {
                    self.push_file(file.parent, name, file.stat, file.content, target);
                }
            }
        }
        pending.clear();
        bytes.clear();
        self.pending = pending;
        self.pending_bytes = bytes;
    }

    /// Seals a worker's output and releases its temporary buffers. Metrics
    /// survive; equal rows have already become seen bits and changed rows
    /// remain available to the final reconciler.
    pub fn seal(&mut self) {
        self.finish_observations();
        self.pending = Vec::new();
        self.pending_bytes = Vec::new();
    }

    pub(crate) fn materialize(&mut self) {
        self.finish_observations();
        let old = self.previous.take();
        if let Some(old) = old {
            let preserved = std::mem::take(&mut self.preserved);
            for id in preserved.ids() {
                let edge = old.name(id);
                let parent = preserved.parents[&edge.parent].0;
                if let crate::Target::Ignored(kind) = edge.target() {
                    self.ignored(parent, edge.bytes, kind);
                }
            }
            let reused = std::mem::take(&mut self.reused);
            for id in reused.ids() {
                let edge = old.name(id);
                let parent = reused.parents[&edge.parent].0;
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
        if !self.full
            && let Some(old) = &self.previous
            && let Some(id) = parent
                .previous_directory()
                .and_then(|p| old.lookup(p, name))
            && old.name(id).target() == crate::Target::Ignored(kind)
        {
            self.preserved.insert(parent, id, None);
            return;
        }
        if !self.reserve_input(1, 32 + name.len()) {
            return;
        }
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
        let changed = self.input_budget.is_some()
            && self.previous.as_ref().is_none_or(|old| {
                dir.previous_directory()
                    .and_then(|id| old.work_tree(id))
                    .is_none_or(|old| {
                        old.kind != kind
                            || old.common_dir != common_dir
                            || old.common_id != common_id
                    })
            });
        if changed
            && !self.reserve_input(1, std::mem::size_of::<WorkTreeEntry>() + common_dir.len())
        {
            return;
        }
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
        let changed = self.input_budget.is_some()
            && (old == NONE
                || self.previous.as_ref().is_some_and(|view| {
                    view.inode(InoId(old)).stat != stat
                        || view.is_traversed(InoId(old)) != traversed
                }));
        if changed
            && !self.reserve_input(
                1,
                std::mem::size_of::<DirEntry>() + std::mem::size_of::<Stat>() + name.len(),
            )
        {
            return DirToken {
                batch: self.id,
                index: NONE,
                old: NONE,
            };
        }
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
    pub stat: &'a Stat,
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
                stat: &self.dir_stats[i],
                traversed: dir.traversed,
                retained_at: dir.retained_at,
            })
    }
    /// Number of full file observations, including an unfinished local listing.
    /// Equal compact references are exposed separately by `reused_files`.
    pub fn file_count(&self) -> usize {
        self.files.len() + self.pending.len()
    }
    /// Borrows one file observation; `index` must be below `file_count`.
    pub fn file_observation(&self, index: usize) -> FileObservation<'_> {
        if index >= self.files.len() {
            let f = &self.pending[index - self.files.len()];
            return FileObservation {
                parent: f.parent,
                name: f.name.of(&self.pending_bytes),
                stat: f.stat,
                content: f.content,
                target: f.target.map(|span| span.of(&self.pending_bytes)),
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
        self.reused
            .ids()
            .map(|name| (self.reused_parent(name), name))
    }
    fn reused_parent(&self, name: NameId) -> DirToken {
        let Some(old) = &self.previous else {
            unreachable!("reused names require a pinned generation");
        };
        self.reused.parents[&old.name(name).parent].0
    }
    /// Included parents, without retaining one reference per equal file.
    pub fn reused_directories(&self) -> impl Iterator<Item = DirToken> + '_ {
        self.reused.included()
    }
    /// Parents whose continuing hints must validate before bulk reuse.
    pub fn reused_parents(&self) -> impl Iterator<Item = DirToken> + '_ {
        self.reused.parents.values().map(|&(parent, _)| parent)
    }
    /// Equal-name/inode seen words, valid only after parent validation.
    pub fn reused_words(&self) -> (&[u64], &[u64]) {
        (&self.reused.names, &self.reused.inodes)
    }
    /// Number of compact equal observations.
    pub fn reused_file_count(&self) -> usize {
        self.reused.count
    }
    /// Reconstructs an equal observation by its seen NameId, for alias
    /// grouping.
    pub fn reused_name_observation(&self, name: NameId) -> FileObservation<'_> {
        let parent = self.reused_parent(name);
        let Some(old) = &self.previous else {
            unreachable!("reused observations require a pinned generation");
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
    /// Preserves an untouched edge under a checked same-path parent. This is
    /// scope retention, not a fresh observation or proof of child identity.
    /// Batches with preserved inode edges require resident log reconciliation;
    /// checkpoint transactions refuse them rather than dropping old subtrees.
    pub fn preserve(&mut self, parent: DirToken, name: &[u8]) {
        let Some(old) = &self.previous else {
            return;
        };
        let Some(id) = parent
            .previous_directory()
            .and_then(|p| old.lookup(p, name))
        else {
            return;
        };
        let child = match old.name(id).target() {
            crate::Target::Inode(id) => Some(id),
            crate::Target::Ignored(_) => None,
        };
        self.scoped |= child.is_some();
        self.preserved.insert(parent, id, child);
    }
    /// Untouched edges and their validated parent tokens.
    pub fn preserved_files(&self) -> impl Iterator<Item = (DirToken, NameId)> + '_ {
        self.preserved.ids().map(|name| {
            let Some(old) = &self.previous else {
                unreachable!("preserved edges require a pinned view")
            };
            (self.preserved.parents[&old.name(name).parent].0, name)
        })
    }
    /// Parent scopes containing preserved included rows (D29).
    pub fn preserved_parents(&self) -> impl Iterator<Item = DirToken> + '_ {
        self.preserved.included()
    }
    /// Peak local observation rows/bytes, excluding changed rows and seen bits.
    pub fn observation_peak(&self) -> (usize, usize) {
        (
            self.pending_peak,
            self.pending_peak * std::mem::size_of::<PendingFile>() + self.pending_bytes_peak,
        )
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
                unreachable!(
                    "writer validates hashed inode document bindings before batch creation"
                );
            };
            Content::Hashed(hash)
        }
    }
}
