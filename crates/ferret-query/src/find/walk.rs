//! Sequential, prunable depth-first traversal in readdir order. Paths keep the
//! spelling of each start operand. Raw d_type stays optional; an unknown type
//! shares the entry's one lstat cache with metadata predicates.

use std::cell::OnceCell;
use std::ffi::OsStr;
use std::fs::{self, File, FileType, Metadata};
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use rustix::fs::{FileType as RawType, Mode, OFlags, RawDir, open};

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
#[derive(Debug)]
pub struct Entry {
    path: Vec<u8>,
    /// The start operand's spelling is always a prefix of `path`.
    root_len: usize,
    follow: bool,
    directory: Option<Rc<File>>,
    check_directory: bool,
    depth: usize,
    kind: Option<FileKind>,
    metadata: OnceCell<io::Result<Metadata>>,
}

impl Entry {
    /// Builds an entry from fields available to a source without statting it.
    pub fn new(path: PathBuf, depth: usize, kind: FileKind) -> Self {
        let path = path.into_os_string().into_vec();
        Self {
            root_len: path.len(),
            path,
            follow: false,
            directory: None,
            check_directory: false,
            depth,
            kind: Some(kind),
            metadata: OnceCell::new(),
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
        (if self.follow { None } else { self.kind })
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
    pub(super) fn directory_handle(&self) -> Option<Rc<File>> {
        self.directory.clone()
    }

    pub(super) fn opposite_kind(&self) -> io::Result<FileKind> {
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
#[derive(Debug)]
struct Saved {
    follow: bool,
    directory: Option<Rc<File>>,
    depth: usize,
    kind: Option<FileKind>,
    metadata: OnceCell<io::Result<Metadata>>,
}

impl Saved {
    fn take(entry: &mut Entry) -> Self {
        Self {
            follow: entry.follow,
            directory: entry.directory.take(),
            depth: entry.depth,
            kind: entry.kind,
            metadata: std::mem::take(&mut entry.metadata),
        }
    }

    fn restore(self, entry: &mut Entry) {
        entry.follow = self.follow;
        entry.directory = self.directory;
        entry.depth = self.depth;
        entry.kind = self.kind;
        entry.check_directory = false;
        entry.metadata = self.metadata;
    }
}

/// A child read from a directory: its name's range in `LiveWalk::names`.
#[derive(Clone, Copy, Debug)]
struct Child {
    name: (usize, usize),
    kind: Option<FileKind>,
}

/// An open directory. Its children occupy `start..end` of the walk's child
/// stack, so levels pop in the order they were pushed.
#[derive(Debug)]
struct Level {
    handle: Rc<File>,
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

/// The live source: sequential depth-first readdir, without ignore rules.
/// It holds one directory handle per ancestor and never sorts child names.
/// A descriptor exhaustion error is reported just like a listing failure.
pub struct LiveWalk {
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
        if self.options.xdev {
            match entry.metadata() {
                Ok(stat) if stat.dev() != self.root_dev => return Ok(()),
                Ok(_) => {}
                Err(error) => return Err(entry.error(error)),
            }
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let flags = if entry.depth == 0 || entry.follow {
            flags
        } else {
            flags | OFlags::NOFOLLOW
        };
        let handle = match open(entry.path(), flags, Mode::empty()) {
            Ok(fd) => Rc::new(File::from(fd)),
            Err(error) => {
                if self.options.depth_first {
                    self.slot = Slot::After;
                }
                return Err(WalkError {
                    path: entry.path().to_owned(),
                    error: error.into(),
                });
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
            #[cfg(test)]
            let kind = if self.force_unknown { None } else { kind };
            // GNU observes the first child's directory metadata before a cwd
            // batch flush, but later directories are checked when visited.
            // Removed directories can therefore print and still fault at a
            // depth limit; regular file metadata remains lazy.
            if kind == Some(FileKind::Directory) && self.children.len() == start {
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
                let stat = match metadata(&path, follow) {
                    Ok(stat) => stat,
                    Err(error) => return Some(Err(WalkError { path, error })),
                };
                self.root_dev = stat.dev();
                self.entry = Entry::new(path, 0, kind(stat.file_type()));
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
            entry.directory = Some(Rc::clone(&level.handle));
            entry.depth = level.own.depth + 1;
            entry.kind = child.kind;
            entry.check_directory = false;
            entry.metadata = OnceCell::new();
            if child.kind == Some(FileKind::Directory) {
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
        if entry.follow && entry.kind().map_err(|error| entry.error(error))? == FileKind::Directory
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
        if self.options.xdev
            && entry
                .metadata()
                .is_ok_and(|stat| stat.dev() != self.root_dev)
        {
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
