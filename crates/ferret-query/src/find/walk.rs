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
use std::fs::{self, File};
use std::io;
use std::mem::MaybeUninit;
use std::ops::Deref;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rustix::fs::{
    AtFlags, CWD, FileType as RawType, Mode, OFlags, RawDir, Stat as RawStat, Statx, StatxFlags,
    makedev, open, openat, readlinkat, statat, statx,
};

use ferret_catalog::{Catalog, Contents, InoId, Kind, NameId, Target};

use super::schedule::Share;
use super::{Follow, Options};

mod live;
mod metadata;
mod resolve;

use crate::threads::lock;
pub use live::LiveWalk;
pub use metadata::Metadata;
pub(super) use metadata::{Stat, metadata};
use metadata::{metadata_at, stat_at};
pub use resolve::{Namespace, resolve_in};
pub(super) use resolve::{resolve, resolve_observed};

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

/// A listed directory held open, and the hidden names its listing held; see
/// [`Entry::listing`].
#[derive(Clone, Copy, Debug)]
pub struct Listing<'a> {
    dir: BorrowedFd<'a>,
    /// Each name beginning with `.`, a NUL after each.
    hidden: &'a [u8],
}

impl<'a> Listing<'a> {
    pub fn dir(&self) -> BorrowedFd<'a> {
        self.dir
    }

    /// Whether the listing held `name`, which begins with `.`; `None` for
    /// any other name, which is not recorded.
    pub fn holds(&self, name: &[u8]) -> Option<bool> {
        name.starts_with(b".")
            .then(|| self.hidden.split(|&b| b == 0).any(|listed| listed == name))
    }
}

/// A live listing's hidden names, recorded for [`Entry::listing`].
#[derive(Clone, Debug, Default)]
enum Hidden {
    /// Not a live listing by a walk keeping hidden names.
    #[default]
    Unrecorded,
    Empty,
    /// Each name beginning with `.`, a NUL after each.
    Names(Arc<[u8]>),
}

/// A retained directory descriptor. Clones share both the descriptor and its
/// budget charge, so queued work keeps the descriptor charged until last use.
#[derive(Clone, Debug)]
pub struct DirectoryHandle {
    file: Arc<File>,
    _charge: Option<Arc<Charge>>,
}

impl DirectoryHandle {
    /// Wraps an existing directory descriptor without a walk budget charge.
    pub fn from_file(file: Arc<File>) -> Self {
        Self::uncharged(file)
    }

    fn uncharged(file: Arc<File>) -> Self {
        Self {
            file,
            _charge: None,
        }
    }

    fn charged(file: Arc<File>, charge: Charge) -> Self {
        Self {
            file,
            _charge: Some(Arc::new(charge)),
        }
    }
}

impl Deref for DirectoryHandle {
    type Target = File;

    fn deref(&self) -> &Self::Target {
        &self.file
    }
}

impl AsFd for DirectoryHandle {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
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
                id: None,
                catalog: None,
                listed: None,
                hidden: Hidden::Unrecorded,
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

    /// Catalog name identity when this entry came from stored children.
    pub fn catalog_name(&self) -> Option<ferret_catalog::NameId> {
        let catalog = self.state.catalog.as_ref()?;
        let parent = self.state.parent?;
        // A catalog listing already knows the id; looking it up again was most
        // of the serial producer's time in a flat directory (grep-why P1).
        if let Some(id) = self.state.id {
            debug_assert_eq!(Some(id), catalog.lookup(parent, self.name()));
            return Some(id);
        }
        catalog.lookup(parent, self.name())
    }

    /// The target a catalog listing read with this entry's name: `None` where
    /// [`Entry::catalog_name`] looks the name up instead.
    pub fn catalog_target(&self) -> Option<ferret_catalog::Target> {
        self.state.catalog.as_ref()?;
        self.state.id.and(self.state.target)
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
        .map_or_else(|| self.metadata().map(Metadata::kind), Ok)
    }
    /// Final path component as bytes, including `.` and `..` in start operands.
    /// Trailing slashes are omitted, except for an all-slash root operand.
    pub fn name(&self) -> &[u8] {
        let bytes = self.path.as_slice();
        let Some(last) = bytes.iter().rposition(|&b| b != b'/') else {
            return b"/";
        };
        let trimmed = &bytes[..=last];
        let start = trimmed
            .iter()
            .rposition(|&b| b == b'/')
            .map_or(0, |i| i + 1);
        &trimmed[start..]
    }
    /// lstat, at most once for this entry (including failures). A start operand
    /// has the metadata that established its existence already cached.
    pub fn metadata(&self) -> io::Result<&Metadata> {
        self.state
            .metadata
            .get_or_init(|| {
                self.with_observed_at(|dir, path| metadata_at(dir, path, self.state.follow))
                    .map_err(Arc::new)
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
            lock(children).children.get(&id).copied().unwrap_or(0)
        });
        Some(count > removed)
    }
    pub(super) fn note_deleted(&self) {
        if let (Some(parent), Some(children)) = (self.state.parent, &self.removed_children) {
            let mut removed = lock(children);
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
            .with_observed_at(|dir, path| readlinkat(dir, path, Vec::new()))?
            .into_bytes())
    }
    // A retained parent is the capability for every live re-resolution of
    // this observed name. /proc exposes it to std APIs without unsafe code.
    pub(super) fn with_observed_path<T>(&self, lookup: impl FnOnce(&Path) -> T) -> T {
        if let Some((parent, name)) = self.observed_at() {
            let path = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()))
                .join(OsStr::from_bytes(&name));
            lookup(&path)
        } else {
            lookup(self.observed_path().as_ref())
        }
    }

    // As `with_observed_path`, but as a directory handle and a path beneath
    // it, for `*at` calls: they resolve the same name without a walk through
    // procfs.
    pub(super) fn with_observed_at<T>(&self, lookup: impl FnOnce(BorrowedFd<'_>, &Path) -> T) -> T {
        if let Some((parent, name)) = self.observed_at() {
            return lookup(parent.as_fd(), Path::new(OsStr::from_bytes(&name)));
        }
        match &self.cwd_handle {
            Some(handle) if self.path().is_relative() => lookup(handle.as_fd(), self.path()),
            _ => lookup(CWD, &self.lookup_path()),
        }
    }

    // The retained parent and this entry's name beneath it. `name()` drops a
    // trailing slash; restore it so the lookup keeps the operand's own
    // directory requirement (GNU reports ENOTDIR for a non-directory named
    // with a trailing `/`).
    fn observed_at(&self) -> Option<(&File, Vec<u8>)> {
        let parent = self.state.directory.as_deref()?;
        let mut joined = self.name().to_vec();
        if self.path.ends_with(b"/") && joined != b"/" {
            joined.push(b'/');
        }
        Some((parent, joined))
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

    pub(super) fn command_cwd(&self) -> Option<DirectoryHandle> {
        if self.inherit_cwd {
            None
        } else {
            self.cwd_handle.clone().map(DirectoryHandle::uncharged)
        }
    }

    /// The directory this entry was listed in, still open, when its walk
    /// retains parents ([`LiveWalk::retaining_parents`]); `None` for a
    /// catalogued listing, which opens no directory. A start operand's is the
    /// directory named by its spelling's parent.
    pub fn directory_handle(&self) -> Option<DirectoryHandle> {
        self.state.directory.clone()
    }

    /// The `(dev, ino)` of the directory this entry was listed in, when a
    /// walk [retaining parents](LiveWalk::retaining_parents) was past its
    /// descriptor budget and closed it after listing: a host that reopens
    /// the directory by path checks it against this.
    pub fn directory_identity(&self) -> Option<(u64, u64)> {
        self.state.listed
    }

    /// The directory this entry was listed in, still open, with the hidden
    /// names its listing held, for a walk [keeping hidden
    /// names](LiveWalk::keeping_hidden_names) and [retaining
    /// parents](LiveWalk::retaining_parents). A host loading that
    /// directory's dot-file configuration reads through it and opens only
    /// what the listing held, rather than probing each name. `None` for a
    /// start operand, a catalogued listing, a directory closed for want of
    /// descriptor budget, or any other walk.
    pub fn listing(&self) -> Option<Listing<'_>> {
        let hidden = match &self.state.hidden {
            Hidden::Unrecorded => return None,
            Hidden::Empty => &[][..],
            Hidden::Names(names) => names,
        };
        Some(Listing {
            dir: self.state.directory.as_ref()?.as_fd(),
            hidden,
        })
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
        self.with_observed_at(|dir, path| stat_at(dir, path, AtFlags::empty()))
            .map(|stat| stat.kind())
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
        self.with_observed_at(|dir, path| {
            metadata_at(dir, path, !self.state.follow).or_else(|error| {
                if error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) {
                    stat_at(dir, path, AtFlags::SYMLINK_NOFOLLOW)
                } else {
                    Err(error)
                }
            })
        })
        .map(|stat| stat.kind())
    }

    fn error(&self, error: io::Error) -> WalkError {
        WalkError {
            path: self.path().to_owned(),
            error,
        }
    }
}

/// An I/O error with its path. The evaluator reports it and keeps walking.
#[derive(Debug)]
pub struct WalkError {
    /// The affected path, using the start operand's spelling.
    pub path: PathBuf,
    /// The underlying filesystem or output error.
    pub error: io::Error,
}

#[derive(Debug)]
struct OutsideCatalog;
impl std::fmt::Display for OutsideCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "start is outside the catalog or the index is stale; run ferret index DIR or use -I",
        )
    }
}
impl std::error::Error for OutsideCatalog {}
impl WalkError {
    /// The explicit start has no catalog resolution. Hosts can supply their
    /// own live-mode advice without interpreting the find diagnostic text.
    pub fn is_outside_catalog(&self) -> bool {
        self.error
            .get_ref()
            .is_some_and(|error| error.is::<OutsideCatalog>())
    }
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
    /// Fetches the next entry while allowing long source operations to poll
    /// for cancellation. Sources that cannot poll internally retain their
    /// existing fetch behavior.
    fn next_with_cancel(
        &mut self,
        descend: bool,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
        _cancelled: &mut dyn FnMut() -> bool,
    ) -> Option<Result<&Entry, WalkError>> {
        self.next_with(descend, before_directory)
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
    fn next_with_cancel(
        &mut self,
        descend: bool,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Option<Result<&Entry, WalkError>> {
        (**self).next_with_cancel(descend, before_directory, cancelled)
    }
}

/// Bounds cancellation work without polling the host once per name.
pub(super) const CANCELLATION_INTERVAL: usize = 256;

/// A directory entry's own fields while its children use the lent entry.
#[derive(Clone, Default)]
struct Saved {
    follow: bool,
    followed_symlink: bool,
    directory: Option<DirectoryHandle>,
    depth: usize,
    kind: Option<FileKind>,
    metadata: OnceCell<Result<Metadata, Arc<io::Error>>>,
    target: Option<Target>,
    parent: Option<InoId>,
    id: Option<NameId>,
    catalog: Option<Arc<Catalog>>,
    /// The listing directory's `(dev, ino)`, where a walk bounded by
    /// [`Charge`] closed it instead of retaining it.
    listed: Option<(u64, u64)>,
    /// The listing directory's hidden names; see `live::Level::hidden`.
    hidden: Hidden,
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

/// Catalog order, visibility and stored metadata. Explicit ignored starts and
/// opaque directories use live entries. The caller loads the plan's sections.
pub struct CatalogSource {
    pub(super) walk: LiveWalk,
}

impl CatalogSource {
    /// Namespace sections needed by physical traversal, including their
    /// validated dependencies. No document/content sections are required.
    pub fn sections() -> &'static [ferret_catalog::Section] {
        use ferret_catalog::Section;
        &[
            Section::Names,
            Section::Links,
            Section::Roots,
            Section::Entries,
        ]
    }

    /// Catalog visibility with find's ordinary physical traversal and live
    /// fallback for explicit ignored/opaque starts. No expression is parsed.
    pub fn paths(catalog: Catalog, paths: Vec<PathBuf>, cwd: PathBuf) -> Self {
        Self::new(catalog, paths, Options::at(cwd))
    }

    /// [`LiveWalk::retaining_parents`] for the live listings of opaque and
    /// ignored directories. Catalogued listings open no directory to retain.
    pub fn retaining_parents(mut self) -> Self {
        self.walk.options.retain_parent = true;
        self.walk.options.bounded_parents = true;
        self
    }

    pub(super) fn new(catalog: Catalog, paths: Vec<PathBuf>, options: Options) -> Self {
        Self::known(catalog, paths, Vec::new(), options)
    }

    /// As [`CatalogSource::new`], with each start's physical catalog target
    /// where the caller already resolved it, in step with `paths`.
    pub(super) fn known(
        catalog: Catalog,
        paths: Vec<PathBuf>,
        known: Vec<Option<Target>>,
        options: Options,
    ) -> Self {
        let mut walk = LiveWalk::new(paths, options);
        walk.known = known.into_iter();
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
    fn next_with_cancel(
        &mut self,
        descend: bool,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Option<Result<&Entry, WalkError>> {
        self.walk
            .next_with_cancel(descend, before_directory, cancelled)
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

/// One retained parent descriptor's share of the budget that
/// [`LiveWalk::retaining_parents`] walks share: a quarter of the process's
/// open-file limit, at most 1,024, as for the crawl's held directories
/// (D21). Returned when dropped.
#[derive(Debug)]
struct Charge;

static PARENTS: OnceLock<AtomicUsize> = OnceLock::new();

impl Charge {
    fn budget() -> &'static AtomicUsize {
        PARENTS.get_or_init(|| {
            let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile).current;
            AtomicUsize::new(limit.map_or(1024, |limit| (limit / 4).min(1024)) as usize)
        })
    }

    /// A share, if any is left.
    fn take() -> Option<Self> {
        Self::budget()
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                left.checked_sub(1)
            })
            .ok()
            .map(|_| Charge)
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        Self::budget().fetch_add(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_the_last_component_without_trailing_slashes() {
        for (path, name) in [
            ("/", "/"),
            ("///", "/"),
            ("", "/"),
            ("a", "a"),
            ("a/", "a"),
            ("a//", "a"),
            ("./a/bc", "bc"),
            ("/a/bc//", "bc"),
            ("//bc", "bc"),
            (".", "."),
            ("../..", ".."),
        ] {
            let entry = Entry::new(PathBuf::from(path), 0, FileKind::File);
            assert_eq!(entry.name(), name.as_bytes(), "{path:?}");
        }
    }

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
    fn the_fstatat_fallback_reads_what_statx_does() {
        // Where statx is unavailable, stat_at falls back to fstatat. Both
        // conversions must give the same fields, and std's values. /dev/null
        // carries a device number; its times move when anything writes it.
        use std::os::unix::fs::MetadataExt;
        let exe = std::env::current_exe().unwrap();
        for path in [exe.as_path(), Path::new("/"), Path::new("/dev/null")] {
            let flags = AtFlags::SYMLINK_NOFOLLOW;
            let new = Metadata::from(statx(CWD, path, flags, StatxFlags::BASIC_STATS).unwrap());
            let old = Metadata::from(statat(CWD, path, flags).unwrap());
            let std = fs::symlink_metadata(path).unwrap();
            for metadata in [new, old] {
                let fields = |m: &Metadata| (m.dev(), m.ino(), m.rdev(), m.mode(), m.nlink());
                assert_eq!(
                    fields(&metadata),
                    (std.dev(), std.ino(), std.rdev(), std.mode(), std.nlink()),
                    "{path:?}"
                );
                assert_eq!(metadata.kind(), kind_of(std.file_type()));
            }
            if path != Path::new("/dev/null") {
                assert_eq!(format!("{new:?}"), format!("{old:?}"), "{path:?}");
                assert_eq!(
                    (new.size(), new.blocks(), new.uid(), new.gid()),
                    (std.size(), std.blocks(), std.uid(), std.gid())
                );
                assert_eq!(
                    (new.mtime(), new.mtime_nsec()),
                    (std.mtime(), std.mtime_nsec())
                );
            }
        }
        fn kind_of(kind: fs::FileType) -> FileKind {
            if kind.is_dir() {
                FileKind::Directory
            } else if kind.is_file() {
                FileKind::File
            } else {
                FileKind::Character
            }
        }
    }

    #[test]
    fn cloning_an_entry_preserves_cached_metadata_errors() {
        // Caching only errno once turned a NUL path's InvalidInput into EIO.
        // A live stat reports what std does for a missing file; a NUL path,
        // which no operand can spell, is EINVAL from the kernel's side.
        let missing = PathBuf::from("/ferret-r1-missing-metadata-entry");
        let std = fs::symlink_metadata(&missing).unwrap_err();
        for path in [missing, PathBuf::from(OsStr::from_bytes(b"bad\0path"))] {
            let entry = Entry::new(path, 0, FileKind::File);
            let observed = entry.metadata().unwrap_err();
            let cached = entry.clone().metadata().unwrap_err();
            assert_eq!(cached.raw_os_error(), observed.raw_os_error());
            assert_eq!(cached.kind(), observed.kind());
            assert_eq!(cached.to_string(), observed.to_string());
            if entry.path() == Path::new("/ferret-r1-missing-metadata-entry") {
                assert_eq!(observed.raw_os_error(), std.raw_os_error());
                assert_eq!(observed.to_string(), std.to_string());
            } else {
                assert_eq!(observed.kind(), io::ErrorKind::InvalidInput);
            }
        }
    }
}
