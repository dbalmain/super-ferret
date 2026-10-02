//! Prunable depth-first traversal over catalog children or live readdir. Paths
//! keep each start operand's spelling. Stored fields are decoded on demand;
//! fields absent from the catalog share one lazy live metadata observation.
//!
//! Speed (m3b): the walk lends one entry and allocates nothing per name. Its
//! path buffer is truncated and extended per child, child names sit on one
//! stack-shaped arena, and listings reuse one getdents buffer over the
//! directory's own handle rather than a dup. What remains is kernel time: an
//! lstat per directory (GNU's observable checks) and per entry wherever a test
//! needs one.

use std::cell::OnceCell;
use std::ffi::OsStr;
use std::fs::{self, File, FileType, Metadata};
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

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

/// An entry's cheap fields and one lazy lstat observation. Metadata failures
/// are cached too, so repeated predicates never retry a vanished name. The live
/// source lends one entry at a time and reuses its path buffer, so a borrowed
/// entry is gone at the next fetch; nothing in an entry allocates per name.
pub struct Entry {
    path: Vec<u8>,
    /// The start operand's spelling is always a prefix of `path`.
    root_len: usize,
    follow: bool,
    followed_symlink: bool,
    directory: Option<Rc<File>>,
    check_directory: bool,
    depth: usize,
    kind: Option<FileKind>,
    metadata: OnceCell<io::Result<Metadata>>,
    target: Option<Target>,
    pub(super) catalog: Option<Arc<Catalog>>,
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("path", &self.path())
            .field("depth", &self.depth)
            .field("kind", &self.kind)
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl Entry {
    /// Builds an entry from fields available to a source without statting it.
    pub fn new(path: PathBuf, depth: usize, kind: FileKind) -> Self {
        let path = path.into_os_string().into_vec();
        Self {
            root_len: path.len(),
            path,
            follow: false,
            followed_symlink: false,
            directory: None,
            check_directory: false,
            depth,
            kind: Some(kind),
            metadata: OnceCell::new(),
            target: None,
            catalog: None,
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
        self.depth
    }
    /// Type from d_type if known, otherwise from the single cached lstat.
    pub fn kind(&self) -> Result<FileKind, &io::Error> {
        (if self.follow && self.catalog.is_none() {
            None
        } else {
            self.kind
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
    pub fn metadata(&self) -> Result<&Metadata, &io::Error> {
        self.metadata
            .get_or_init(|| metadata(self.path(), self.follow))
            .as_ref()
    }
    pub(super) fn stat(&self) -> io::Result<Stat<'_>> {
        match (&self.catalog, self.target) {
            (Some(catalog), Some(Target::Inode(id))) => Ok(Stat::Stored(catalog, id)),
            _ => self.metadata().map(Stat::Live).map_err(copy_error),
        }
    }
    pub(super) fn has_children(&self) -> Option<bool> {
        let Target::Inode(id) = self.target? else {
            return None;
        };
        self.catalog.as_ref()?.has_children(id)
    }
    pub(super) fn link_target(&self) -> io::Result<Vec<u8>> {
        if let (Some(catalog), Some(Target::Inode(id))) = (&self.catalog, self.target) {
            return Ok(catalog.link_target(id).unwrap_or_default().to_vec());
        }
        Ok(fs::read_link(self.path())?.into_os_string().into_vec())
    }
    pub(super) fn directory_handle(&self) -> Option<Rc<File>> {
        self.directory.clone()
    }

    fn follow_catalog(&mut self) -> io::Result<()> {
        if !self.follow || self.kind != Some(FileKind::Symlink) {
            return Ok(());
        }
        let Some(catalog) = &self.catalog else {
            return Ok(());
        };
        if let Some((Target::Inode(id), false)) = resolve(catalog, self.path(), true)? {
            self.followed_symlink = true;
            self.target = Some(Target::Inode(id));
            self.kind = Some(catalog_kind(catalog.kind(id)));
            return Ok(());
        }
        // A target outside the indexed trees has no catalog metadata.
        self.catalog = None;
        self.target = None;
        Ok(())
    }

    pub(super) fn referenced_kind(&self) -> io::Result<FileKind> {
        if self.catalog.is_some() {
            if self.kind != Some(FileKind::Symlink) {
                return self.kind().map_err(copy_error);
            }
            let mut entry = Entry::new(self.path().to_owned(), self.depth, FileKind::Symlink);
            entry.catalog = self.catalog.clone();
            entry.target = self.target;
            entry.follow = true;
            entry.follow_catalog()?;
            if entry.catalog.is_some() {
                return entry.kind().map_err(copy_error);
            }
        }
        fs::metadata(self.path()).map(|stat| kind(stat.file_type()))
    }

    pub(super) fn opposite_kind(&self) -> io::Result<FileKind> {
        if self.catalog.is_some() {
            if self.followed_symlink {
                return Ok(FileKind::Symlink);
            }
            if self.kind != Some(FileKind::Symlink) {
                return self.kind().map_err(copy_error);
            }
            return self.referenced_kind().or_else(|error| {
                if error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(40) {
                    Ok(FileKind::Symlink)
                } else {
                    Err(error)
                }
            });
        }
        metadata(self.path(), !self.follow)
            .or_else(|error| {
                if error.raw_os_error() == Some(40) {
                    fs::symlink_metadata(self.path())
                } else {
                    Err(error)
                }
            })
            .map(|stat| kind(stat.file_type()))
    }

    fn error(&self, error: &io::Error) -> WalkError {
        WalkError {
            path: self.path().to_owned(),
            error: copy_error(error),
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

fn metadata(path: &Path, follow: bool) -> io::Result<Metadata> {
    if follow {
        match fs::metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::symlink_metadata(path),
            result => result,
        }
    } else {
        fs::symlink_metadata(path)
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

/// getdents64 buffer. One call reads a typical directory whole; 8 KiB took
/// three calls per directory on the timing tree.
const LISTING_BUFFER: usize = 64 * 1024;

/// A directory entry's own fields while its children use the lent entry.
struct Saved {
    follow: bool,
    followed_symlink: bool,
    directory: Option<Rc<File>>,
    depth: usize,
    kind: Option<FileKind>,
    metadata: OnceCell<io::Result<Metadata>>,
    target: Option<Target>,
    catalog: Option<Arc<Catalog>>,
}

impl Saved {
    fn take(entry: &mut Entry) -> Self {
        Self {
            follow: entry.follow,
            followed_symlink: entry.followed_symlink,
            directory: entry.directory.take(),
            depth: entry.depth,
            kind: entry.kind,
            metadata: std::mem::take(&mut entry.metadata),
            target: entry.target,
            catalog: entry.catalog.clone(),
        }
    }

    fn restore(self, entry: &mut Entry) {
        entry.follow = self.follow;
        entry.followed_symlink = self.followed_symlink;
        entry.directory = self.directory;
        entry.depth = self.depth;
        entry.kind = self.kind;
        entry.check_directory = false;
        entry.metadata = self.metadata;
        entry.target = self.target;
        entry.catalog = self.catalog;
    }
}

/// A child read from a directory: its name's range in `LiveWalk::names`.
#[derive(Clone, Copy, Debug)]
struct Child {
    name: (usize, usize),
    kind: Option<FileKind>,
    target: Option<Target>,
}

/// An open directory. Its children occupy `start..end` of the walk's child
/// stack, so levels pop in the order they were pushed.
struct Level {
    handle: Option<Rc<File>>,
    /// The directory's own path is `entry.path[..path_len]`.
    path_len: usize,
    separator: bool,
    start: usize,
    next: usize,
    end: usize,
    names_start: usize,
    /// The first child's metadata, observed while listing when it is a
    /// directory.
    first: Option<io::Result<Metadata>>,
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
    nested_roots: Vec<(InoId, InoId, Vec<u8>)>,
    paths: std::vec::IntoIter<PathBuf>,
    options: Options,
    entry: Entry,
    slot: Slot,
    root_dev: u64,
    levels: Vec<Level>,
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
            nested_roots: Vec::new(),
            paths: paths.into_iter(),
            options,
            entry: Entry::new(PathBuf::new(), 0, FileKind::File),
            slot: Slot::Empty,
            root_dev: 0,
            levels: Vec::new(),
            children: Vec::new(),
            names: Vec::new(),
            buffer: Vec::new(),
            #[cfg(test)]
            force_unknown: false,
        }
    }

    /// Lists the lent entry's directory and makes it the innermost level.
    fn descend(
        &mut self,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
    ) -> Result<(), WalkError> {
        let entry = &mut self.entry;
        if self.options.max_depth.is_some_and(|max| entry.depth >= max) {
            return Ok(());
        }
        if entry.kind().map_err(|error| entry.error(error))? != FileKind::Directory {
            return Ok(());
        }
        if self.options.xdev
            && entry.stat().map_err(|error| entry.error(&error))?.dev() != self.root_dev
        {
            return Ok(());
        }
        if let Some(catalog) = &self.catalog
            && let Some(target @ Target::Inode(dir)) = entry.target
            && catalog.contents(target) == Some(Contents::Catalogued)
        {
            let handle = if self.options.live_checks {
                match open_directory(entry) {
                    Ok(handle) => Some(handle),
                    Err(error) => {
                        if self.options.depth_first {
                            self.slot = Slot::After;
                        }
                        return Err(entry.error(&error));
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
                        .kinds
                        .is_some_and(|mask| mask & (1 << catalog_kind(child.kind) as u8) == 0)
                {
                    continue;
                }
                // Test membership before child actions. A later sibling
                // removal must not erase a name already observed here.
                if self.options.live_checks && child.kind != Kind::Dir {
                    let own_len = entry.path.len();
                    if !entry.path.ends_with(b"/") {
                        entry.path.push(b'/');
                    }
                    entry.path.extend_from_slice(child.bytes);
                    let missing = fs::symlink_metadata(entry.path())
                        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound);
                    entry.path.truncate(own_len);
                    if missing {
                        continue;
                    }
                }
                let name_start = self.names.len();
                self.names.extend_from_slice(child.bytes);
                self.children.push(Child {
                    name: (name_start, self.names.len()),
                    kind: Some(catalog_kind(child.kind)),
                    target: Some(child.target),
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
                });
            }
            if added {
                self.children[start..].sort_unstable_by(|a, b| {
                    self.names[a.name.0..a.name.1].cmp(&self.names[b.name.0..b.name.1])
                });
            }
            if catalog.has_children(dir) == Some(true) || added {
                before_directory().map_err(|error| entry.error(&error))?;
            }
            let path_len = entry.path.len();
            let separator = !entry.path.ends_with(b"/");
            let own = Saved::take(entry);
            self.levels.push(Level {
                handle,
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
                return Err(entry.error(&error));
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
                if separator {
                    entry.path.push(b'/');
                }
                entry.path.extend_from_slice(name);
                first = Some(metadata(entry.path(), follow));
                entry.path.truncate(path_len);
            }
            let name_start = self.names.len();
            self.names.extend_from_slice(name);
            self.children.push(Child {
                name: (name_start, self.names.len()),
                kind,
                target,
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
        self.levels.push(Level {
            handle: Some(handle),
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
                if let Some(catalog) = &self.catalog {
                    let resolved = match resolve(catalog, self.entry.path(), follow || self.entry.path.ends_with(b"/")) {
                        Ok(Some(resolved)) => resolved,
                        Ok(None) => match resolve(catalog, self.entry.path(), false) {
                            Ok(Some(resolved)) => resolved,
                            _ => return Some(Err(self.entry.error(&io::Error::other(
                                "start is outside the catalog or the index is stale; run ferret index DIR or use -I"
                            )))),
                        },
                        Err(error) => return Some(Err(self.entry.error(&error))),
                    };
                    if !resolved.1
                        && let Target::Inode(id) = resolved.0
                    {
                        self.entry.target = Some(resolved.0);
                        self.entry.catalog = Some(catalog.clone());
                        self.entry.kind = Some(catalog_kind(catalog.kind(id)));
                        self.entry.follow = follow || self.entry.path.ends_with(b"/");
                        self.entry.followed_symlink = resolve(catalog, self.entry.path(), false)
                            .ok()
                            .flatten()
                            .is_some_and(|(target, _)| target != resolved.0);
                        if let Err(error) = self.entry.follow_catalog() {
                            return Some(Err(self.entry.error(&error)));
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
                let stat = match metadata(self.entry.path(), follow) {
                    Ok(stat) => stat,
                    Err(error) => return Some(Err(self.entry.error(&error))),
                };
                self.root_dev = stat.dev();
                self.entry.kind = Some(kind(stat.file_type()));
                self.entry.follow = follow;
                self.entry.metadata = OnceCell::from(Ok(stat));
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
            entry.follow = self.options.follow == Follow::All;
            entry.followed_symlink = false;
            entry.directory = level.handle.clone();
            entry.depth = level.own.depth + 1;
            entry.kind = child.kind;
            entry.check_directory = false;
            entry.metadata = OnceCell::new();
            entry.target = child.target;
            if child.target.is_some() {
                if entry.catalog.is_none() {
                    entry.catalog = self.catalog.clone();
                }
            } else {
                entry.catalog = None;
            }
            if let Err(error) = entry.follow_catalog() {
                return Some(Err(entry.error(&error)));
            }
            if child.kind == Some(FileKind::Directory)
                && (child.target.is_none() || self.options.live_checks)
            {
                match level.first.take() {
                    Some(stat) if index == level.start => entry.metadata = OnceCell::from(stat),
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
            && entry.catalog.is_some()
            && entry.kind == Some(FileKind::Directory)
        {
            let stat = entry.stat().map_err(|error| entry.error(&error))?;
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
                return Err(entry.error(&io::Error::other("File system loop detected")));
            }
        }
        if entry.follow
            && entry.catalog.is_none()
            && entry.kind().map_err(|error| entry.error(error))? == FileKind::Directory
        {
            let stat = entry.metadata().map_err(|error| entry.error(error))?;
            let path = &entry.path;
            if self.levels.iter().any(|level| {
                level
                    .own
                    .metadata
                    .get_or_init(|| {
                        metadata(
                            Path::new(OsStr::from_bytes(&path[..level.path_len])),
                            level.own.follow,
                        )
                    })
                    .as_ref()
                    .is_ok_and(|ancestor| {
                        ancestor.dev() == stat.dev() && ancestor.ino() == stat.ino()
                    })
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
        if self.options.max_depth.is_some_and(|max| entry.depth >= max)
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

pub(super) fn copy_error(error: &io::Error) -> io::Error {
    error.raw_os_error().map_or_else(
        || io::Error::new(error.kind(), error.to_string()),
        io::Error::from_raw_os_error,
    )
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
    walk: LiveWalk,
}

impl CatalogSource {
    pub(super) fn new(catalog: Catalog, paths: Vec<PathBuf>, options: Options) -> Self {
        let mut walk = LiveWalk::new(paths, options);
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

fn open_directory(entry: &Entry) -> io::Result<Rc<File>> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let flags = if entry.depth == 0 || entry.follow {
        flags
    } else {
        flags | OFlags::NOFOLLOW
    };
    Ok(Rc::new(File::from(open(
        entry.path(),
        flags,
        Mode::empty(),
    )?)))
}

// Resolve links from the snapshot, including intermediate components. A ..
// after a link applies to the target's parent, not to the link's lexical
// parent.
pub(super) fn resolve(
    catalog: &Catalog,
    path: &Path,
    follow: bool,
) -> io::Result<Option<(Target, bool)>> {
    let mut pending = std::path::absolute(path)?;
    for _ in 0..40 {
        let mut prefix = PathBuf::new();
        let parts: Vec<_> = pending.components().collect();
        let mut redirected = None;
        for (at, part) in parts.iter().enumerate() {
            match part {
                std::path::Component::ParentDir => {
                    prefix.pop();
                }
                std::path::Component::CurDir => {}
                part => prefix.push(part.as_os_str()),
            }
            if (follow || at + 1 < parts.len())
                && let Some(resolved) = catalog.resolve(prefix.as_os_str().as_bytes())
                && resolved.remainder.is_empty()
                && let Target::Inode(id) = resolved.target
                && catalog.kind(id) == Kind::Symlink
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
    Err(io::Error::from_raw_os_error(40))
}
