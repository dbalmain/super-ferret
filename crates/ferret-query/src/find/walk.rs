//! Prunable depth-first traversal over catalog children or live readdir. Paths
//! keep each start operand's spelling. Stored fields are decoded on demand;
//! fields absent from the catalog share one lazy live metadata observation.
//!
//! Both sources lend one entry and reuse its path buffer and stack-shaped name
//! arena. Catalog leaves rejected by a leading pure guard need no path buffer
//! work. Live listings reuse one getdents buffer over the directory's own
//! handle. Effectful catalog plans observe names when entering a directory and
//! open it for descent errors and execdir; read-only stored-field queries do
//! neither.

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, File, FileType, Metadata};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rustix::fs::{FileType as RawType, Mode, OFlags, RawDir, open};

use ferret_catalog::{Catalog, Contents, InoId, Kind, Target};

use super::{Follow, Options};

/// Find's file kinds on Linux.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// Regular file.
    File,
    /// Directory.
    Directory,
    /// Symbolic link (not followed in physical mode).
    Symlink,
    /// Named pipe.
    Fifo,
    /// Unix socket.
    Socket,
    /// Block device.
    Block,
    /// Character device.
    Character,
}

pub(super) fn kind(value: FileType) -> FileKind {
    if value.is_dir() {
        FileKind::Directory
    } else if value.is_symlink() {
        FileKind::Symlink
    } else if value.is_fifo() {
        FileKind::Fifo
    } else if value.is_socket() {
        FileKind::Socket
    } else if value.is_block_device() {
        FileKind::Block
    } else if value.is_char_device() {
        FileKind::Character
    } else {
        FileKind::File
    }
}

#[derive(Default)]
struct Removals {
    children: BTreeMap<InoId, u32>,
    entries: BTreeSet<InoId>,
}

/// An entry's cheap fields and one lazy lstat observation. Metadata failures
/// are cached too, so repeated predicates never retry a vanished name. The live
/// source lends one entry at a time and reuses its path buffer, so a borrowed
/// entry is gone at the next fetch. Successful metadata observations need no
/// per-name allocation; cached failures share an owned error.
#[derive(Clone)]
pub struct Entry {
    cwd: Option<Arc<PathBuf>>,
    cwd_handle: Option<Arc<File>>,
    inherit_cwd: bool,
    path: Vec<u8>,
    /// The start operand's spelling is always a prefix of `path`.
    root_len: usize,
    check_directory: bool,
    /// Shared across a walk so action workers can update counts safely.
    removed_children: Option<Arc<Mutex<Removals>>>,
    state: Saved,
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("path", &self.path())
            .field("depth", &self.state.depth)
            .field("kind", &self.state.kind)
            .field("target", &self.state.target)
            .finish_non_exhaustive()
    }
}

impl Entry {
    /// Builds an entry from fields available to a source without statting it.
    pub fn new(path: PathBuf, depth: usize, kind: FileKind) -> Self {
        let path = path.into_os_string().into_vec();
        Self {
            cwd: None,
            cwd_handle: None,
            inherit_cwd: false,
            root_len: path.len(),
            path,
            check_directory: false,
            removed_children: None,
            state: Saved {
                follow: false,
                followed_symlink: false,
                directory: None,
                depth,
                kind: Some(kind),
                metadata: OnceCell::new(),
                target: None,
                parent: None,
                catalog: None,
            },
        }
    }

    /// Exact spelling used for matching and output.
    pub fn path(&self) -> &Path {
        Path::new(OsStr::from_bytes(&self.path))
    }
    pub(super) fn root(&self) -> &Path {
        Path::new(OsStr::from_bytes(&self.path[..self.root_len]))
    }

    /// Depth relative to the current start path, which has depth zero.
    pub fn depth(&self) -> usize {
        self.state.depth
    }
    /// Type from d_type if known, otherwise from the single cached lstat.
    pub fn kind(&self) -> io::Result<FileKind> {
        (if self.state.follow && self.state.catalog.is_none() {
            None
        } else {
            self.state.kind
        })
        .map_or_else(|| self.metadata().map(|stat| kind(stat.file_type())), Ok)
    }
    /// Final path component as bytes, including `.` and `..` in start operands.
    /// Trailing slashes are omitted, except for an all-slash root operand.
    pub fn name(&self) -> &[u8] {
        let bytes = self.path.as_slice();
        let end = bytes
            .iter()
            .rposition(|&b| b != b'/')
            .map_or(bytes.len(), |i| i + 1);
        let trimmed = &bytes[..end];
        if trimmed.iter().all(|&b| b == b'/') {
            return b"/";
        }
        trimmed.rsplit(|&b| b == b'/').next().unwrap_or(trimmed)
    }
    /// lstat, at most once for this entry (including failures). A start operand
    /// has the metadata that established its existence already cached.
    pub fn metadata(&self) -> io::Result<&Metadata> {
        self.state
            .metadata
            .get_or_init(|| {
                self.with_observed_path(|path| cached_metadata(path, self.state.follow))
            })
            .as_ref()
            .map_err(|error| {
                error.raw_os_error().map_or_else(
                    || io::Error::new(error.kind(), error.to_string()),
                    io::Error::from_raw_os_error,
                )
            })
    }
    pub(super) fn catalog(&self) -> Option<&Catalog> {
        self.state.catalog.as_deref()
    }
    pub(super) fn stat(&self) -> io::Result<Stat<'_>> {
        match (&self.state.catalog, self.state.target) {
            (Some(catalog), Some(Target::Inode(id))) => Ok(Stat::Stored(catalog, id)),
            _ => self.metadata().map(Stat::Live),
        }
    }
    pub(super) fn has_children(&self) -> Option<bool> {
        let Target::Inode(id) = self.state.target? else {
            return None;
        };
        let count = self.state.catalog.as_ref()?.entry_count(id)?;
        let removed = self.removed_children.as_ref().map_or(0, |children| {
            children
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .children
                .get(&id)
                .copied()
                .unwrap_or(0)
        });
        Some(count > removed)
    }
    pub(super) fn note_deleted(&self) {
        if let (Some(parent), Some(children)) = (self.state.parent, &self.removed_children) {
            let mut removed = children
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(Target::Inode(id)) = self.state.target {
                removed.entries.insert(id);
            }
            let count = removed.children.entry(parent).or_default();
            *count = count.saturating_add(1);
        }
    }
    pub(super) fn link_target(&self) -> io::Result<Vec<u8>> {
        if let (Some(catalog), Some(Target::Inode(id))) = (&self.state.catalog, self.state.target) {
            return Ok(catalog.link_target(id).unwrap_or_default().to_vec());
        }
        Ok(self
            .with_observed_path(|path| fs::read_link(path))?
            .into_os_string()
            .into_vec())
    }
    // A retained parent is the capability for every live re-resolution of
    // this observed name. /proc exposes it to std APIs without unsafe code.
    pub(super) fn with_observed_path<T>(&self, lookup: impl FnOnce(&Path) -> T) -> T {
        if let Some(parent) = &self.state.directory {
            // `name()` drops a trailing slash; restore it so the lookup keeps
            // the operand's own directory requirement (GNU reports ENOTDIR
            // for a non-directory named with a trailing `/`).
            let mut joined = self.name().to_vec();
            if self.path.ends_with(b"/") && joined != b"/" {
                joined.push(b'/');
            }
            let path = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()))
                .join(OsStr::from_bytes(&joined));
            lookup(&path)
        } else {
            lookup(self.observed_path().as_ref())
        }
    }

    fn observed_path(&self) -> std::borrow::Cow<'_, Path> {
        match &self.cwd_handle {
            Some(handle) if self.path().is_relative() => std::borrow::Cow::Owned(
                PathBuf::from(format!("/proc/self/fd/{}", handle.as_raw_fd())).join(self.path()),
            ),
            _ => self.lookup_path(),
        }
    }

    fn lookup_path(&self) -> std::borrow::Cow<'_, Path> {
        match &self.cwd {
            Some(cwd) => std::borrow::Cow::Owned(cwd.join(self.path())),
            None => std::borrow::Cow::Borrowed(self.path()),
        }
    }

    pub(super) fn cwd(&self) -> Option<&Path> {
        if self.inherit_cwd {
            None
        } else {
            self.cwd.as_deref().map(PathBuf::as_path)
        }
    }

    pub(super) fn command_cwd(&self) -> Option<Arc<File>> {
        if self.inherit_cwd {
            None
        } else {
            self.cwd_handle.clone()
        }
    }

    pub(super) fn directory_handle(&self) -> Option<Arc<File>> {
        self.state.directory.clone()
    }

    fn follow_catalog(&mut self) -> io::Result<()> {
        if !self.state.follow || self.state.kind != Some(FileKind::Symlink) {
            return Ok(());
        }
        let Some(catalog) = &self.state.catalog else {
            return Ok(());
        };
        if let Some((Target::Inode(id), false)) = self.with_observed_path(|observed| {
            resolve_observed(catalog, self.lookup_path().as_ref(), true, observed)
        })? {
            if catalog.kind(id) == Kind::Symlink {
                return Ok(());
            }
            self.state.followed_symlink = true;
            self.state.target = Some(Target::Inode(id));
            self.state.kind = Some(catalog_kind(catalog.kind(id)));
            return Ok(());
        }
        // A target outside the indexed trees has no catalog metadata.
        self.state.catalog = None;
        self.state.target = None;
        Ok(())
    }

    pub(super) fn referenced_kind(&self) -> io::Result<FileKind> {
        if self.state.catalog.is_some() {
            if self.state.kind != Some(FileKind::Symlink) {
                return self.kind();
            }
            let mut entry = Entry::new(self.path().to_owned(), self.state.depth, FileKind::Symlink);
            entry.cwd = self.cwd.clone();
            entry.cwd_handle = self.cwd_handle.clone();
            entry.inherit_cwd = self.inherit_cwd;
            entry.state.catalog = self.state.catalog.clone();
            entry.state.directory = self.state.directory.clone();
            entry.state.target = self.state.target;
            entry.state.follow = true;
            entry.follow_catalog()?;
            if entry.state.catalog.is_some() && entry.state.kind != Some(FileKind::Symlink) {
                return entry.kind();
            }
        }
        self.with_observed_path(|path| fs::metadata(path))
            .map(|stat| kind(stat.file_type()))
    }

    pub(super) fn opposite_kind(&self) -> io::Result<FileKind> {
        if self.state.catalog.is_some() {
            if self.state.followed_symlink {
                return Ok(FileKind::Symlink);
            }
            if self.state.kind != Some(FileKind::Symlink) {
                return self.kind();
            }
            return self.referenced_kind().or_else(|error| {
                if error.kind() == io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error())
                {
                    Ok(FileKind::Symlink)
                } else {
                    Err(error)
                }
            });
        }
        self.with_observed_path(|path| {
            metadata(path, !self.state.follow).or_else(|error| {
                if error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) {
                    fs::symlink_metadata(path)
                } else {
                    Err(error)
                }
            })
        })
        .map(|stat| kind(stat.file_type()))
    }

    fn error(&self, error: io::Error) -> WalkError {
        WalkError {
            path: self.path().to_owned(),
            error,
        }
    }
}

pub(super) enum Stat<'a> {
    Stored(&'a Catalog, InoId),
    Live(&'a Metadata),
}

impl Stat<'_> {
    pub(super) fn size(&self) -> u64 {
        match *self {
            Self::Stored(catalog, id) => catalog.size(id),
            Self::Live(stat) => stat.size(),
        }
    }
    pub(super) fn mode(&self) -> u32 {
        match *self {
            Self::Stored(catalog, id) => catalog.mode(id),
            Self::Live(stat) => stat.mode(),
        }
    }
    pub(super) fn uid(&self) -> u32 {
        match *self {
            Self::Stored(catalog, id) => catalog.owner(id).0,
            Self::Live(stat) => stat.uid(),
        }
    }
    pub(super) fn gid(&self) -> u32 {
        match *self {
            Self::Stored(catalog, id) => catalog.owner(id).1,
            Self::Live(stat) => stat.gid(),
        }
    }
    pub(super) fn nlink(&self) -> u64 {
        match *self {
            Self::Stored(catalog, id) => catalog.nlink(id),
            Self::Live(stat) => stat.nlink(),
        }
    }
    pub(super) fn dev(&self) -> u64 {
        match *self {
            Self::Stored(catalog, id) => catalog.identity(id).0,
            Self::Live(stat) => stat.dev(),
        }
    }
    pub(super) fn ino(&self) -> u64 {
        match *self {
            Self::Stored(catalog, id) => catalog.identity(id).1,
            Self::Live(stat) => stat.ino(),
        }
    }
    pub(super) fn mtime(&self) -> i64 {
        match *self {
            Self::Stored(catalog, id) => catalog.mtime(id),
            Self::Live(stat) => stat.mtime(),
        }
    }
    pub(super) fn mtime_nsec(&self) -> i64 {
        match *self {
            Self::Stored(catalog, id) => catalog.mtime_nsec(id),
            Self::Live(stat) => stat.mtime_nsec(),
        }
    }
    pub(super) fn ctime(&self) -> i64 {
        match *self {
            Self::Stored(catalog, id) => catalog.ctime(id),
            Self::Live(stat) => stat.ctime(),
        }
    }
    pub(super) fn ctime_nsec(&self) -> i64 {
        match *self {
            Self::Stored(catalog, id) => catalog.ctime_nsec(id),
            Self::Live(stat) => stat.ctime_nsec(),
        }
    }
}

pub(super) fn metadata(path: &Path, follow: bool) -> io::Result<Metadata> {
    if follow {
        match fs::metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::symlink_metadata(path),
            result => result,
        }
    } else {
        fs::symlink_metadata(path)
    }
}

fn cached_metadata(path: &Path, follow: bool) -> Result<Metadata, Arc<io::Error>> {
    metadata(path, follow).map_err(Arc::new)
}

/// An I/O error with its path. The evaluator reports it and keeps walking.
#[derive(Debug)]
pub struct WalkError {
    /// The affected path, using the start operand's spelling.
    pub path: PathBuf,
    /// The underlying filesystem or output error.
    pub error: io::Error,
}

/// Ordered entries with a feedback channel for prune. A source is configured
/// with the plan's global traversal options before execution. After yielding an
/// entry, `next(false)` prevents descent; post-order sources already descended.
/// Entries are lent: each one is valid until the next fetch.
pub trait EntrySource {
    /// Catalog snapshot used by this source, if any, for reference operands.
    fn catalog(&self) -> Option<&Catalog> {
        None
    }
    /// Fetches the next entry or recoverable error. No work follows exhaustion.
    fn next(&mut self, descend: bool) -> Option<Result<&Entry, WalkError>>;
    /// Flushes directory-local batches after names are cached and before
    /// entering a nonempty directory. Other sources may have no such boundary.
    fn next_with(
        &mut self,
        descend: bool,
        _before_directory: &mut dyn FnMut() -> io::Result<()>,
    ) -> Option<Result<&Entry, WalkError>> {
        self.next(descend)
    }
}

impl<S: EntrySource + ?Sized> EntrySource for &mut S {
    fn catalog(&self) -> Option<&Catalog> {
        (**self).catalog()
    }
    fn next(&mut self, descend: bool) -> Option<Result<&Entry, WalkError>> {
        (**self).next(descend)
    }
    fn next_with(
        &mut self,
        descend: bool,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
    ) -> Option<Result<&Entry, WalkError>> {
        (**self).next_with(descend, before_directory)
    }
}

/// getdents64 buffer. One call reads a typical directory whole; 8 KiB took
/// three calls per directory on the timing tree.
const LISTING_BUFFER: usize = 64 * 1024;

/// A directory entry's own fields while its children use the lent entry.
#[derive(Clone, Default)]
struct Saved {
    follow: bool,
    followed_symlink: bool,
    directory: Option<Arc<File>>,
    depth: usize,
    kind: Option<FileKind>,
    metadata: OnceCell<Result<Metadata, Arc<io::Error>>>,
    target: Option<Target>,
    parent: Option<InoId>,
    catalog: Option<Arc<Catalog>>,
}

impl Saved {
    fn take(entry: &mut Entry) -> Self {
        std::mem::take(&mut entry.state)
    }

    fn restore(self, entry: &mut Entry) {
        entry.state = self;
        entry.check_directory = false;
    }
}

/// A child read from a directory: its name's range in `LiveWalk::names`.
#[derive(Clone, Copy, Debug)]
struct Child {
    name: (usize, usize),
    kind: Option<FileKind>,
    target: Option<Target>,
    parent: Option<InoId>,
}

/// An open directory. Its children occupy `start..end` of the walk's child
/// stack, so levels pop in the order they were pushed.
#[derive(Clone)]
struct Level {
    catalogued: bool,
    pending: Option<Arc<AtomicUsize>>,
    handle: Option<Arc<File>>,
    /// The directory's own path is `entry.path[..path_len]`.
    path_len: usize,
    separator: bool,
    start: usize,
    next: usize,
    end: usize,
    names_start: usize,
    /// The first child's metadata, observed while listing when it is a
    /// directory.
    first: Option<Result<Metadata, Arc<io::Error>>>,
    own: Saved,
}

/// What the lent entry holds between fetches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    /// Nothing owed; advance to the next name.
    Empty,
    /// A pre-order entry was yielded; descend if the evaluator allows.
    Pending,
    /// An entry whose directory stat failed is still to be evaluated.
    Visit,
    /// A post-order directory that could not be opened is still to be yielded.
    After,
}

enum Loaded {
    Entry,
    Directory,
}

/// Sequential DFS. The unrestricted source uses readdir without ignore rules;
/// the catalog source shares the stack and substitutes catalog child listings.
/// It holds one directory handle per ancestor and never sorts child names.
/// A descriptor exhaustion error is reported just like a listing failure.
pub struct LiveWalk {
    catalog: Option<Arc<Catalog>>,
    /// Counts only successful catalog-child removals made by this walk.
    removed_children: Option<Arc<Mutex<Removals>>>,
    nested_roots: Vec<(InoId, InoId, Vec<u8>)>,
    paths: std::vec::IntoIter<PathBuf>,
    options: Options,
    entry: Entry,
    slot: Slot,
    root_dev: u64,
    levels: Vec<Level>,
    boundary: usize,
    suspended: bool,
    children: Vec<Child>,
    names: Vec<u8>,
    buffer: Vec<MaybeUninit<u8>>,
    #[cfg(test)]
    pub(super) force_unknown: bool,
}

impl LiveWalk {
    pub(super) fn new(paths: Vec<PathBuf>, options: Options) -> Self {
        Self {
            catalog: None,
            removed_children: None,
            nested_roots: Vec::new(),
            paths: paths.into_iter(),
            options,
            entry: Entry::new(PathBuf::new(), 0, FileKind::File),
            slot: Slot::Empty,
            root_dev: 0,
            levels: Vec::new(),
            boundary: 0,
            suspended: false,
            children: Vec::new(),
            names: Vec::new(),
            buffer: Vec::new(),
            #[cfg(test)]
            force_unknown: false,
        }
    }

    // Five warm samples on the 300k fixture: 16 workers win live stat-heavy
    // rows (48 ms versus 69 ms at 8); 8 win typical catalog scans (21–23 ms
    // versus 23–24). Shallow live traversal also avoids the extra startup.
    pub(super) fn worker_limit(&self, workers: usize) -> usize {
        if self.catalog.is_some() || self.options.max_depth.is_some_and(|depth| depth <= 2) {
            workers.min(8)
        } else {
            workers
        }
    }

    pub(super) fn has_starts(&self) -> bool {
        !self.paths.as_slice().is_empty()
    }

    pub(super) fn in_live_directory(&self) -> bool {
        self.levels.last().is_some_and(|level| !level.catalogued)
    }

    pub(super) fn suspended(&self) -> bool {
        self.suspended
    }

    pub(super) fn waiting(&self) -> bool {
        self.slot == Slot::Empty
            && self.levels.last().is_some_and(|level| {
                level.next == level.end
                    && level
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.load(Ordering::Acquire) != 0)
            })
    }

    // Read-only starts may overlap. Effectful starts stay in the donor and
    // advance only after all donated descendants have completed.
    pub(super) fn split_start(&mut self) -> Option<Self> {
        let path = self.paths.next()?;
        let mut walk = Self::new(vec![path], self.options.clone());
        walk.catalog = self.catalog.clone();
        walk.removed_children = self.removed_children.clone();
        walk.nested_roots = self.nested_roots.clone();
        Some(walk)
    }

    // Donate siblings already observed by readdir. Ancestor levels remain in
    // the donated walk solely for loop detection and start/device identity.
    // Its boundary excludes their evaluation; the donor owns their completion.
    pub(super) fn split(&mut self) -> Option<(Self, Arc<AtomicUsize>)> {
        // At depth two, catalog donation costs 7.4 ms versus 6.6 ms on the
        // caller. Live fallback levels still donate, even in this mode.
        let index = self
            .levels
            .iter()
            .enumerate()
            .skip(self.boundary.saturating_sub(1))
            .find_map(|(index, level)| {
                (level.end - level.next >= 2
                    && !(level.catalogued && self.options.max_depth.is_some_and(|max| max <= 2)))
                .then_some(index)
            })?;
        let pending = self.levels[index]
            .pending
            .get_or_insert_with(|| Arc::new(AtomicUsize::new(0)))
            .clone();
        let level = &self.levels[index];
        let middle = level.next + (level.end - level.next) / 2;
        let mut levels: Vec<_> = self.levels[..=index].to_vec();
        let donated = &mut levels[index];
        donated.next = middle;
        donated.pending = None;
        donated.first = None;
        let mut walk = Self::new(Vec::new(), self.options.clone());
        walk.catalog = self.catalog.clone();
        walk.removed_children = self.removed_children.clone();
        walk.nested_roots = self.nested_roots.clone();
        walk.entry = self.entry.clone();
        walk.root_dev = self.root_dev;
        walk.levels = levels;
        walk.boundary = index + 1;
        walk.children = self.children[..level.end].to_vec();
        walk.names = self.names.clone();
        #[cfg(test)]
        {
            walk.force_unknown = self.force_unknown;
        }
        self.levels[index].end = middle;
        pending.fetch_add(1, Ordering::Relaxed);
        Some((walk, pending))
    }

    /// Lists the lent entry's directory and makes it the innermost level.
    fn descend(
        &mut self,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
    ) -> Result<(), WalkError> {
        let entry = &mut self.entry;
        if self
            .options
            .max_depth
            .is_some_and(|max| entry.state.depth >= max)
        {
            return Ok(());
        }
        if entry.kind().map_err(|error| entry.error(error))? != FileKind::Directory {
            return Ok(());
        }
        if self.options.xdev
            && entry.stat().map_err(|error| entry.error(error))?.dev() != self.root_dev
        {
            return Ok(());
        }
        if let Some(catalog) = &self.catalog
            && let Some(target @ Target::Inode(dir)) = entry.state.target
            && catalog.contents(target) == Some(Contents::Catalogued)
        {
            let handle = if self.options.live_checks {
                match open_directory(entry) {
                    Ok(handle) => Some(handle),
                    Err(error) => {
                        if self.options.depth_first {
                            self.slot = Slot::After;
                        }
                        return Err(entry.error(error));
                    }
                }
            } else {
                None
            };
            let start = self.children.len();
            let names_start = self.names.len();
            for child in catalog.entries(dir) {
                if matches!(child.target, Target::Ignored(_)) {
                    continue;
                }
                if child.kind != Kind::Dir
                    && !(self.options.follow == Follow::All && child.kind == Kind::Symlink)
                    && self
                        .options
                        .guard
                        .as_ref()
                        .is_some_and(|guard| !guard.matches(catalog_kind(child.kind), child.bytes))
                {
                    continue;
                }
                // Test membership before child actions. A later sibling
                // removal must not erase a name already observed here. This
                // applies to a cataloged directory child too (#7's repro):
                // an earlier explicit start's own `-delete` can remove a
                // directory another start is about to descend into, and the
                // catalog's stored listing has no way to know that already
                // happened - GNU's live readdir simply never lists it.
                if self.options.live_checks {
                    let own_len = entry.path.len();
                    if !entry.path.ends_with(b"/") {
                        entry.path.push(b'/');
                    }
                    entry.path.extend_from_slice(child.bytes);
                    let observed = handle.as_ref().map(|directory| {
                        PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
                            .join(OsStr::from_bytes(child.bytes))
                    });
                    let missing = fs::symlink_metadata(
                        observed.as_deref().unwrap_or(entry.lookup_path().as_ref()),
                    )
                    .is_err_and(|error| error.kind() == io::ErrorKind::NotFound);
                    entry.path.truncate(own_len);
                    let removed_here = matches!(child.target, Target::Inode(id) if self.removed_children.as_ref().is_some_and(|removed| removed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).entries.contains(&id)));
                    if missing && (child.kind != Kind::Dir || removed_here) {
                        continue;
                    }
                }
                let name_start = self.names.len();
                self.names.extend_from_slice(child.bytes);
                self.children.push(Child {
                    name: (name_start, self.names.len()),
                    kind: Some(catalog_kind(child.kind)),
                    target: Some(child.target),
                    parent: Some(dir),
                });
            }
            // Nested roots have no name row in the outer crawl (D34).
            let first = self
                .nested_roots
                .partition_point(|(parent, _, _)| *parent < dir);
            let roots = self.nested_roots[first..]
                .iter()
                .take_while(|(parent, _, _)| *parent == dir);
            let mut added = false;
            for (_, id, name) in roots {
                added = true;
                let name_start = self.names.len();
                self.names.extend_from_slice(name);
                self.children.push(Child {
                    name: (name_start, self.names.len()),
                    kind: Some(FileKind::Directory),
                    target: Some(Target::Inode(*id)),
                    parent: Some(dir),
                });
            }
            if added {
                self.children[start..].sort_unstable_by(|a, b| {
                    self.names[a.name.0..a.name.1].cmp(&self.names[b.name.0..b.name.1])
                });
            }
            if catalog.has_children(dir) == Some(true) || added {
                before_directory().map_err(|error| entry.error(error))?;
            }
            let path_len = entry.path.len();
            let separator = !entry.path.ends_with(b"/");
            let own = Saved::take(entry);
            self.levels.push(Level {
                catalogued: true,
                pending: None,
                handle: if self.options.retain_parent {
                    handle
                } else {
                    None
                },
                path_len,
                separator,
                start,
                next: start,
                end: self.children.len(),
                names_start,
                first: None,
                own,
            });
            return Ok(());
        }
        let handle = match open_directory(entry) {
            Ok(handle) => handle,
            Err(error) => {
                if self.options.depth_first {
                    self.slot = Slot::After;
                }
                return Err(entry.error(error));
            }
        };
        let path_len = entry.path.len();
        let separator = !entry.path.ends_with(b"/");
        let follow = self.options.follow == Follow::All;
        let start = self.children.len();
        let names_start = self.names.len();
        let mut first = None;
        if self.buffer.is_empty() {
            self.buffer.resize(LISTING_BUFFER, MaybeUninit::uninit());
        }
        let mut reader = RawDir::new(&*handle, &mut self.buffer);
        while let Some(item) = reader.next() {
            let item = match item {
                Ok(item) => item,
                Err(error) => {
                    self.children.truncate(start);
                    self.names.truncate(names_start);
                    return Err(WalkError {
                        path: entry.path().to_owned(),
                        error: error.into(),
                    });
                }
            };
            let name = item.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            let kind = raw_kind(item.file_type());
            let target = None;
            #[cfg(test)]
            let kind = if self.force_unknown { None } else { kind };
            // GNU observes the first child's directory metadata before a cwd
            // batch flush, but later directories are checked when visited.
            // Removed directories can therefore print and still fault at a
            // depth limit; regular file metadata remains lazy.
            if target.is_none() && kind == Some(FileKind::Directory) && self.children.len() == start
            {
                // Observed relative to the handle just opened for this
                // listing, not the entry's pathname: an action run while
                // listing (e.g. `-exec` renaming an ancestor) can make the
                // pathname stale before this first child is even visited.
                let observed = PathBuf::from(format!("/proc/self/fd/{}", handle.as_raw_fd()))
                    .join(OsStr::from_bytes(name));
                first = Some(cached_metadata(&observed, follow));
            }
            let name_start = self.names.len();
            self.names.extend_from_slice(name);
            self.children.push(Child {
                name: (name_start, self.names.len()),
                kind,
                target,
                parent: None,
            });
        }
        if self.children.len() > start
            && let Err(error) = before_directory()
        {
            self.children.truncate(start);
            self.names.truncate(names_start);
            return Err(WalkError {
                path: entry.path().to_owned(),
                error,
            });
        }
        let own = Saved::take(entry);
        // A pure, action-free walk never needs this directory open past its
        // own listing - no -delete/-execdir will ask a child for its
        // parent's fd. Dropping it here, rather than carrying it for the
        // whole subtree the way an effectful walk must, bounds live fd use
        // by width instead of depth (#10): a 100-level chain under a tight
        // RLIMIT_NOFILE no longer exhausts descriptors one per ancestor.
        let handle = if self.options.retain_parent {
            Some(handle)
        } else {
            drop(handle);
            None
        };
        self.levels.push(Level {
            catalogued: false,
            pending: None,
            handle,
            path_len,
            separator,
            start,
            next: start,
            end: self.children.len(),
            names_start,
            first,
            own,
        });
        Ok(())
    }

    /// Loads the next start operand, child or finished post-order directory
    /// into the lent entry.
    fn advance(&mut self) -> Option<Result<Loaded, WalkError>> {
        loop {
            if self.waiting() {
                self.suspended = true;
                return None;
            }
            if self.boundary != 0
                && self.levels.len() == self.boundary
                && self
                    .levels
                    .last()
                    .is_some_and(|level| level.next == level.end)
            {
                return None;
            }
            if let Some(level) = self.levels.pop_if(|level| level.next == level.end) {
                self.children.truncate(level.start);
                self.names.truncate(level.names_start);
                if self.options.depth_first {
                    self.entry.path.truncate(level.path_len);
                    level.own.restore(&mut self.entry);
                    return Some(Ok(Loaded::Directory));
                }
                continue;
            }
            let Some(level) = self.levels.last_mut() else {
                let path = self.paths.next()?;
                let follow = self.options.follow != Follow::Physical;
                self.entry = Entry::new(path, 0, FileKind::File);
                self.entry.cwd = self.options.cwd.clone();
                self.entry.cwd_handle = self.options.cwd_handle.clone();
                self.entry.inherit_cwd = self.options.inherit_cwd;
                self.entry.removed_children = self.removed_children.clone();
                if self.options.retain_parent
                    && (self.catalog.is_none() || self.options.live_checks)
                {
                    match parent_handle(&self.entry) {
                        Ok(handle) => self.entry.state.directory = Some(handle),
                        Err(error) => return Some(Err(self.entry.error(error))),
                    }
                }
                if let Some(catalog) = &self.catalog {
                    if self.options.delete {
                        self.entry.state.parent =
                            std::path::absolute(self.entry.lookup_path().as_ref())
                                .ok()
                                .as_deref()
                                .and_then(Path::parent)
                                .and_then(|parent| resolve(catalog, parent, true).ok().flatten())
                                .and_then(|(target, remainder)| match target {
                                    Target::Inode(id) if !remainder => Some(id),
                                    _ => None,
                                });
                    }
                    let resolved = match resolve(catalog, self.entry.lookup_path().as_ref(), follow || self.entry.path.ends_with(b"/")) {
                        Ok(Some(resolved)) => resolved,
                        Ok(None) => return Some(Err(self.entry.error(io::Error::other(
                            "start is outside the catalog or the index is stale; run ferret index DIR or use -I"
                        )))),
                        Err(error) => return Some(Err(self.entry.error(error))),
                    };
                    if !resolved.1
                        && let Target::Inode(id) = resolved.0
                    {
                        self.entry.state.target = Some(resolved.0);
                        self.entry.state.catalog = Some(catalog.clone());
                        self.entry.state.kind = Some(catalog_kind(catalog.kind(id)));
                        self.entry.state.follow = follow || self.entry.path.ends_with(b"/");
                        self.entry.state.followed_symlink =
                            resolve(catalog, self.entry.lookup_path().as_ref(), false)
                                .ok()
                                .flatten()
                                .is_some_and(|(target, _)| target != resolved.0);
                        if let Err(error) = self.entry.follow_catalog() {
                            return Some(Err(self.entry.error(error)));
                        }
                        if self.options.live_checks
                            && let Err(error) = self.entry.metadata()
                        {
                            return Some(Err(self.entry.error(error)));
                        }
                        if self.options.xdev {
                            self.root_dev = catalog.identity(id).0;
                        }
                        return Some(Ok(Loaded::Entry));
                    }
                }
                if self.options.retain_parent && self.entry.state.directory.is_none() {
                    match parent_handle(&self.entry) {
                        Ok(handle) => self.entry.state.directory = Some(handle),
                        Err(error) => return Some(Err(self.entry.error(error))),
                    }
                }
                let follow = follow || self.entry.path.ends_with(b"/");
                let stat = match self.entry.with_observed_path(|path| metadata(path, follow)) {
                    Ok(stat) => stat,
                    Err(error) => return Some(Err(self.entry.error(error))),
                };
                self.root_dev = stat.dev();
                self.entry.state.kind = Some(kind(stat.file_type()));
                self.entry.state.follow = follow;
                self.entry.state.metadata = OnceCell::from(Ok(stat));
                return Some(Ok(Loaded::Entry));
            };
            let index = level.next;
            level.next += 1;
            let child = self.children[index];
            let entry = &mut self.entry;
            entry.path.truncate(level.path_len);
            if level.separator {
                entry.path.push(b'/');
            }
            entry
                .path
                .extend_from_slice(&self.names[child.name.0..child.name.1]);
            entry.state.follow = self.options.follow == Follow::All;
            entry.state.followed_symlink = false;
            entry.state.directory = level.handle.clone();
            entry.state.depth = level.own.depth + 1;
            entry.state.kind = child.kind;
            entry.check_directory = false;
            entry.state.metadata = OnceCell::new();
            entry.state.target = child.target;
            entry.state.parent = child.parent;
            // Deletion accounting is walk-wide and stays in the reusable entry;
            // re-cloning its shared counter here contends across workers.
            if child.target.is_some() {
                if entry.state.catalog.is_none() {
                    entry.state.catalog = self.catalog.clone();
                }
            } else {
                entry.state.catalog = None;
            }
            if let Err(error) = entry.follow_catalog() {
                return Some(Err(entry.error(error)));
            }
            if child.kind == Some(FileKind::Directory)
                && (child.target.is_none() || self.options.live_checks)
            {
                match level.first.take() {
                    Some(stat) if index == level.start => {
                        entry.state.metadata = OnceCell::from(stat)
                    }
                    _ => entry.check_directory = true,
                }
            }
            return Some(Ok(Loaded::Entry));
        }
    }

    /// Applies visit-time checks to the lent entry. `Ok(true)` yields it;
    /// `Ok(false)` means a post-order directory was entered instead.
    fn visit(
        &mut self,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
    ) -> Result<bool, WalkError> {
        let entry = &mut self.entry;
        if entry.check_directory {
            entry.check_directory = false;
            if let Err(error) = entry.metadata() {
                // GNU still evaluates cheap name/type fields after this
                // directory stat error, including at maxdepth.
                let error = entry.error(error);
                self.slot = Slot::Visit;
                return Err(error);
            }
        }
        if self.options.follow != Follow::Physical
            && entry.state.catalog.is_some()
            && entry.state.kind == Some(FileKind::Directory)
        {
            let stat = entry.stat().map_err(|error| entry.error(error))?;
            if self.levels.iter().any(|level| {
                level
                    .own
                    .catalog
                    .as_ref()
                    .zip(level.own.target)
                    .is_some_and(|(catalog, target)| {
                        let Target::Inode(id) = target else {
                            return false;
                        };
                        catalog.identity(id) == (stat.dev(), stat.ino())
                    })
            }) {
                return Err(entry.error(io::Error::other("File system loop detected")));
            }
        }
        if entry.state.follow
            && entry.state.catalog.is_none()
            && entry.kind().map_err(|error| entry.error(error))? == FileKind::Directory
        {
            let stat = entry.metadata().map_err(|error| entry.error(error))?;
            if self.levels.iter().any(|level| {
                // Live ancestors own an observed directory descriptor. Pure
                // catalog ancestors have a stored identity and need no stat.
                if let Some(cached) = level.own.metadata.get() {
                    // Pure live walks close directory descriptors after
                    // listing. Their already observed
                    // metadata still proves ancestry.
                    cached.as_ref().is_ok_and(|ancestor| {
                        ancestor.dev() == stat.dev() && ancestor.ino() == stat.ino()
                    })
                } else if let Some(handle) = &level.handle {
                    level
                        .own
                        .metadata
                        .get_or_init(|| handle.metadata().map_err(Arc::new))
                        .as_ref()
                        .is_ok_and(|ancestor| {
                            ancestor.dev() == stat.dev() && ancestor.ino() == stat.ino()
                        })
                } else {
                    level
                        .own
                        .catalog
                        .as_ref()
                        .zip(level.own.target)
                        .is_some_and(|(catalog, target)| {
                            matches!(target,
                            Target::Inode(id) if catalog.identity(id) == (stat.dev(), stat.ino()))
                        })
                }
            }) {
                return Err(WalkError {
                    path: entry.path().to_owned(),
                    error: io::Error::other("File system loop detected"),
                });
            }
        }
        if !self.options.depth_first {
            self.slot = Slot::Pending;
            return Ok(true);
        }
        if self
            .options
            .max_depth
            .is_some_and(|max| entry.state.depth >= max)
            || entry.kind().map_err(|error| entry.error(error))? != FileKind::Directory
        {
            return Ok(true);
        }
        // xdev directories still evaluate, even when not descended.
        if self.options.xdev && entry.stat().is_ok_and(|stat| stat.dev() != self.root_dev) {
            return Ok(true);
        }
        self.descend(before_directory)?;
        Ok(false)
    }

    fn next_entry(
        &mut self,
        descend: bool,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
    ) -> Option<Result<&Entry, WalkError>> {
        self.suspended = false;
        let mut loaded = match std::mem::replace(&mut self.slot, Slot::Empty) {
            Slot::Pending if descend => {
                if let Err(error) = self.descend(before_directory) {
                    return Some(Err(error));
                }
                None
            }
            Slot::After => return Some(Ok(&self.entry)),
            Slot::Visit => Some(Loaded::Entry),
            Slot::Empty | Slot::Pending => None,
        };
        loop {
            let current = match loaded.take() {
                Some(current) => current,
                None => match self.advance()? {
                    Ok(current) => current,
                    Err(error) => return Some(Err(error)),
                },
            };
            match current {
                Loaded::Directory => return Some(Ok(&self.entry)),
                Loaded::Entry => match self.visit(before_directory) {
                    Ok(true) => return Some(Ok(&self.entry)),
                    Ok(false) => {}
                    Err(error) => return Some(Err(error)),
                },
            }
        }
    }
}

impl EntrySource for LiveWalk {
    fn catalog(&self) -> Option<&Catalog> {
        self.catalog.as_deref()
    }
    fn next(&mut self, descend: bool) -> Option<Result<&Entry, WalkError>> {
        self.next_entry(descend, &mut || Ok(()))
    }
    fn next_with(
        &mut self,
        descend: bool,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
    ) -> Option<Result<&Entry, WalkError>> {
        self.next_entry(descend, before_directory)
    }
}

fn raw_kind(value: RawType) -> Option<FileKind> {
    Some(match value {
        RawType::RegularFile => FileKind::File,
        RawType::Directory => FileKind::Directory,
        RawType::Symlink => FileKind::Symlink,
        RawType::Fifo => FileKind::Fifo,
        RawType::Socket => FileKind::Socket,
        RawType::BlockDevice => FileKind::Block,
        RawType::CharacterDevice => FileKind::Character,
        RawType::Unknown => return None,
    })
}

/// Catalog order, visibility and stored metadata. Explicit ignored starts and
/// opaque directories use live entries. The caller loads the plan's sections.
pub struct CatalogSource {
    pub(super) walk: LiveWalk,
}

impl CatalogSource {
    pub(super) fn new(catalog: Catalog, paths: Vec<PathBuf>, options: Options) -> Self {
        let mut walk = LiveWalk::new(paths, options);
        walk.removed_children = walk
            .options
            .delete
            .then(|| Arc::new(Mutex::new(Removals::default())));
        for (id, path) in catalog.roots() {
            let path = Path::new(OsStr::from_bytes(path));
            if let Some(parent) = path.parent()
                && let Some(resolved) = catalog.resolve(parent.as_os_str().as_bytes())
                && resolved.remainder.is_empty()
                && let Target::Inode(parent) = resolved.target
            {
                walk.nested_roots.push((
                    parent,
                    id,
                    path.file_name()
                        .unwrap_or(OsStr::new(""))
                        .as_bytes()
                        .to_vec(),
                ));
            }
        }
        walk.nested_roots
            .sort_unstable_by_key(|(parent, _, _)| *parent);
        walk.catalog = Some(Arc::new(catalog));
        Self { walk }
    }
}

impl EntrySource for CatalogSource {
    fn catalog(&self) -> Option<&Catalog> {
        self.walk.catalog.as_deref()
    }
    fn next(&mut self, descend: bool) -> Option<Result<&Entry, WalkError>> {
        self.walk.next(descend)
    }
    fn next_with(
        &mut self,
        descend: bool,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
    ) -> Option<Result<&Entry, WalkError>> {
        self.walk.next_with(descend, before_directory)
    }
}

fn catalog_kind(kind: Kind) -> FileKind {
    match kind {
        Kind::Dir => FileKind::Directory,
        Kind::File => FileKind::File,
        Kind::Symlink => FileKind::Symlink,
        Kind::Fifo => FileKind::Fifo,
        Kind::Socket => FileKind::Socket,
        Kind::Block => FileKind::Block,
        Kind::Character => FileKind::Character,
    }
}

fn parent_handle(entry: &Entry) -> io::Result<Arc<File>> {
    let (parent, _) = super::action::exec_path(entry.observed_path().as_ref());
    Ok(Arc::new(File::from(open(
        &parent,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)))
}

fn open_directory(entry: &Entry) -> io::Result<Arc<File>> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let flags = if entry.state.follow {
        flags
    } else {
        flags | OFlags::NOFOLLOW
    };
    Ok(Arc::new(File::from(entry.with_observed_path(|path| {
        open(path, flags, Mode::empty())
    })?)))
}

// Resolve links from the snapshot, including intermediate components. A ..
// after a link applies to the target's parent, not to the link's lexical
// parent.
//
// `..` is collapsed lexically against `prefix` only while every component
// resolved so far is exact: a real, catalogued directory, not an opaque
// (ignored) directory and not a symlink still waiting to be followed. Once
// resolution crosses an opaque boundary, a later `..` no longer has a
// trustworthy lexical parent (the opaque directory's real parent, after any
// symlinks inside it are followed, is something only the live filesystem
// knows), so the walk stops there and hands the whole reference to the
// caller's live fallback (#5) rather than guessing a lexical answer.
pub(super) fn resolve(
    catalog: &Catalog,
    path: &Path,
    follow: bool,
) -> io::Result<Option<(Target, bool)>> {
    resolve_observed(catalog, path, follow, path)
}

pub(super) fn resolve_observed(
    catalog: &Catalog,
    path: &Path,
    follow: bool,
    observed: &Path,
) -> io::Result<Option<(Target, bool)>> {
    let resolved = match resolve_catalog(catalog, path, follow) {
        Err(error) if follow && error.kind() == io::ErrorKind::NotFound => None,
        result => result?,
    };
    if resolved.is_some() || !follow {
        return Ok(resolved);
    }
    let physical = resolve_catalog(catalog, path, false)?;
    let Some((target, remainder)) = physical else {
        return Ok(None);
    };
    match fs::metadata(observed) {
        Ok(_) => Ok(Some((target, true))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Some((target, remainder))),
        Err(error) => Err(error),
    }
}

fn resolve_catalog(
    catalog: &Catalog,
    path: &Path,
    follow: bool,
) -> io::Result<Option<(Target, bool)>> {
    let mut pending = std::path::absolute(path)?;
    for _ in 0..40 {
        let mut prefix = PathBuf::new();
        let parts: Vec<_> = pending.components().collect();
        let mut redirected = None;
        // Whether `prefix` so far is an exact, catalogued directory - safe
        // to pop a later `..` against lexically.
        let mut exact = true;
        let mut opaque = None;
        // Whether the walk is currently inside catalog coverage. A `..` that
        // steps back out of a catalogued directory onto an uncatalogued
        // prefix is leaving coverage, not failing to find a child inside it,
        // so it clears `inside` instead of erroring; a miss on any other
        // component while `inside` is a genuinely missing child and errors.
        let mut inside = false;
        for (at, part) in parts.iter().enumerate() {
            let via_parent = matches!(part, std::path::Component::ParentDir);
            match part {
                std::path::Component::ParentDir => {
                    if !exact {
                        return Ok(opaque.map(|target| (target, true)));
                    }
                    prefix.pop();
                }
                std::path::Component::CurDir => {}
                part => prefix.push(part.as_os_str()),
            }
            let Some(resolved) = catalog.resolve(prefix.as_os_str().as_bytes()) else {
                if inside && !via_parent {
                    return Err(rustix::io::Errno::NOENT.into());
                }
                inside = false;
                continue;
            };
            inside = true;
            if !resolved.remainder.is_empty() {
                exact = false;
                opaque = Some(resolved.target);
                continue;
            }
            if let Target::Inode(id) = resolved.target
                && catalog.kind(id) == Kind::Symlink
                && (follow || at + 1 < parts.len())
            {
                let target = Path::new(OsStr::from_bytes(
                    catalog.link_target(id).unwrap_or_default(),
                ));
                let mut next = prefix.parent().unwrap_or(Path::new("/")).join(target);
                for part in &parts[at + 1..] {
                    next.push(part.as_os_str());
                }
                redirected = Some(std::path::absolute(next)?);
                break;
            }
            exact = matches!(resolved.target, Target::Inode(dir) if catalog.is_directory(dir));
            opaque = Some(resolved.target);
        }
        match redirected {
            Some(next) => pending = next,
            None => {
                return Ok(catalog
                    .resolve(prefix.as_os_str().as_bytes())
                    .map(|resolved| (resolved.target, !resolved.remainder.is_empty())));
            }
        }
    }
    Err(rustix::io::Errno::LOOP.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_tests_leave_the_metadata_cache_unobserved() {
        // Access predicates once performed an extra lstat before access(2).
        // A missing path is a false access test, not a metadata error.
        let entries = [
            (
                Entry::new(
                    PathBuf::from("/ferret-r1-missing-access-entry"),
                    0,
                    FileKind::File,
                ),
                false,
            ),
            (
                Entry::new(std::env::current_exe().unwrap(), 0, FileKind::File),
                true,
            ),
        ];
        for (entry, expected) in entries {
            for access in [
                rustix::fs::Access::READ_OK,
                rustix::fs::Access::WRITE_OK,
                rustix::fs::Access::EXEC_OK,
            ] {
                assert_eq!(
                    crate::find::test::Test::Access(access)
                        .evaluate(&entry)
                        .unwrap(),
                    expected
                );
                assert!(entry.state.metadata.get().is_none());
            }
        }
    }

    #[test]
    fn cloning_an_entry_preserves_cached_os_and_non_os_metadata_errors() {
        // Caching only errno turned std's NUL-path InvalidInput into EIO.
        for path in [
            PathBuf::from("/ferret-r1-missing-metadata-entry"),
            PathBuf::from(OsStr::from_bytes(b"bad\0path")),
        ] {
            let expected = fs::symlink_metadata(&path).unwrap_err();
            let entry = Entry::new(path, 0, FileKind::File);
            let observed = entry.metadata().unwrap_err();
            let copied = entry.clone();
            let cached = copied.metadata().unwrap_err();
            for error in [observed, cached] {
                assert_eq!(error.raw_os_error(), expected.raw_os_error());
                assert_eq!(error.kind(), expected.kind());
                assert_eq!(error.to_string(), expected.to_string());
            }
        }
    }
}
