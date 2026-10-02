//! Sequential, prunable depth-first traversal in readdir order. Paths keep the
//! spelling of each start operand. Raw d_type stays optional; an unknown type
//! shares the entry's one lstat cache with metadata predicates.

use std::cell::OnceCell;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, FileType, Metadata};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use rustix::fs::{Dir, FileType as RawType, Mode, OFlags, open};

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
/// are cached too, so repeated predicates never retry a vanished name.
#[derive(Clone, Debug)]
pub struct Entry {
    path: PathBuf,
    root: PathBuf,
    follow: bool,
    directory: Option<Rc<File>>,
    check_directory: bool,
    depth: usize,
    kind: Option<FileKind>,
    metadata: Rc<OnceCell<io::Result<Metadata>>>,
}

impl Entry {
    /// Builds an entry from fields available to a source without statting it.
    pub fn new(path: PathBuf, depth: usize, kind: FileKind) -> Self {
        Self {
            root: path.clone(),
            path,
            follow: false,
            directory: None,
            check_directory: false,
            depth,
            kind: Some(kind),
            metadata: Rc::new(OnceCell::new()),
        }
    }

    /// Exact spelling used for matching and output.
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub(super) fn root(&self) -> &Path {
        &self.root
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
        let bytes = self.path.as_os_str().as_bytes();
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
            .get_or_init(|| metadata(&self.path, self.follow))
            .as_ref()
    }
    pub(super) fn directory_handle(&self) -> Option<Rc<File>> {
        self.directory.clone()
    }

    pub(super) fn opposite_kind(&self) -> io::Result<FileKind> {
        metadata(&self.path, !self.follow)
            .or_else(|error| {
                if error.raw_os_error() == Some(40) {
                    fs::symlink_metadata(&self.path)
                } else {
                    Err(error)
                }
            })
            .map(|stat| kind(stat.file_type()))
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
pub trait EntrySource {
    /// Fetches the next entry or recoverable error. No work follows exhaustion.
    fn next(&mut self, descend: bool) -> Option<Result<Entry, WalkError>>;
    /// Flushes directory-local batches after names are cached and before
    /// entering a nonempty directory. Other sources may have no such boundary.
    fn next_with(
        &mut self,
        descend: bool,
        _before_directory: &mut dyn FnMut() -> io::Result<()>,
    ) -> Option<Result<Entry, WalkError>> {
        self.next(descend)
    }
}

struct Directory {
    entry: Entry,
    children: std::vec::IntoIter<Entry>,
    root_dev: u64,
}

enum Task {
    Visit(Entry, u64),
    Children(Directory),
    After(Entry),
}

/// The live source: sequential depth-first readdir, without ignore rules.
/// It holds one directory handle per ancestor and never sorts child names.
/// A descriptor exhaustion error is reported just like a listing failure.
pub struct LiveWalk {
    paths: std::vec::IntoIter<PathBuf>,
    tasks: Vec<Task>,
    pending: Option<(Entry, u64)>,
    options: Options,
    #[cfg(test)]
    pub(super) force_unknown: bool,
}

impl LiveWalk {
    pub(super) fn new(paths: Vec<PathBuf>, options: Options) -> Self {
        Self {
            paths: paths.into_iter(),
            tasks: Vec::new(),
            pending: None,
            options,
            #[cfg(test)]
            force_unknown: false,
        }
    }

    fn descend(
        &mut self,
        entry: Entry,
        root_dev: u64,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
    ) -> Result<(), WalkError> {
        if self.options.max_depth.is_some_and(|max| entry.depth >= max) {
            return Ok(());
        }
        if entry.kind().map_err(|error| WalkError {
            path: entry.path.clone(),
            error: copy_error(error),
        })? != FileKind::Directory
        {
            return Ok(());
        }
        if self.options.xdev {
            match entry.metadata() {
                Ok(stat) if stat.dev() != root_dev => return Ok(()),
                Ok(_) => {}
                Err(error) => {
                    return Err(WalkError {
                        path: entry.path.clone(),
                        error: copy_error(error),
                    });
                }
            }
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let flags = if entry.depth == 0 || entry.follow {
            flags
        } else {
            flags | OFlags::NOFOLLOW
        };
        let directory = match open(&entry.path, flags, Mode::empty()) {
            Ok(fd) => Rc::new(File::from(fd)),
            Err(error) => {
                let path = entry.path.clone();
                if self.options.depth_first {
                    self.tasks.push(Task::After(entry));
                }
                return Err(WalkError {
                    path,
                    error: error.into(),
                });
            }
        };
        let mut reader = Dir::new(directory.try_clone().map_err(|error| WalkError {
            path: entry.path.clone(),
            error,
        })?)
        .map_err(|error| WalkError {
            path: entry.path.clone(),
            error: error.into(),
        })?;
        let mut children = Vec::new();
        while let Some(child) = reader.read() {
            let child = child.map_err(|error| WalkError {
                path: entry.path.clone(),
                error: error.into(),
            })?;
            let name = child.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            let mut child_entry = Entry::new(
                join(entry.path(), OsStr::from_bytes(name)),
                entry.depth + 1,
                FileKind::File,
            );
            child_entry.root = entry.root.clone();
            child_entry.follow = self.options.follow == Follow::All;
            child_entry.directory = Some(directory.clone());
            child_entry.kind = raw_kind(child.file_type());
            #[cfg(test)]
            if self.force_unknown {
                child_entry.kind = None;
            }
            // GNU observes the first child's directory metadata before a cwd
            // batch flush, but later directories are checked when visited.
            // Removed directories can therefore print and still fault at a
            // depth limit; regular file metadata remains lazy.
            if child_entry.kind == Some(FileKind::Directory) {
                if children.is_empty() {
                    let _ = child_entry.metadata();
                } else {
                    child_entry.check_directory = true;
                }
            }
            children.push(child_entry);
        }
        if !children.is_empty() {
            before_directory().map_err(|error| WalkError {
                path: entry.path.clone(),
                error,
            })?;
        }
        self.tasks.push(Task::Children(Directory {
            entry,
            children: children.into_iter(),
            root_dev,
        }));
        Ok(())
    }
}

impl LiveWalk {
    fn next_entry(
        &mut self,
        descend: bool,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
    ) -> Option<Result<Entry, WalkError>> {
        if let Some((entry, root_dev)) = self.pending.take()
            && descend
            && let Err(error) = self.descend(entry, root_dev, before_directory)
        {
            return Some(Err(error));
        }
        loop {
            let task = match self.tasks.pop() {
                Some(task) => task,
                None => {
                    let path = self.paths.next()?;
                    let follow = self.options.follow != Follow::Physical;
                    let stat = match metadata(&path, follow) {
                        Ok(stat) => stat,
                        Err(error) => return Some(Err(WalkError { path, error })),
                    };
                    let root_dev = stat.dev();
                    let mut entry = Entry::new(path, 0, kind(stat.file_type()));
                    entry.follow = follow;
                    let _ = entry.metadata.set(Ok(stat));
                    Task::Visit(entry, root_dev)
                }
            };
            match task {
                Task::Visit(mut entry, root_dev) => {
                    if entry.check_directory {
                        entry.check_directory = false;
                        if let Err(error) = entry.metadata() {
                            let error = WalkError {
                                path: entry.path.clone(),
                                error: copy_error(error),
                            };
                            // GNU still evaluates cheap name/type fields after
                            // this directory stat error, including at maxdepth.
                            self.tasks.push(Task::Visit(entry, root_dev));
                            return Some(Err(error));
                        }
                    }
                    let followed_kind = if entry.follow {
                        Some(entry.kind().map_err(|error| WalkError {
                            path: entry.path.clone(),
                            error: copy_error(error),
                        }))
                    } else {
                        None
                    };
                    if let Some(Err(error)) = followed_kind {
                        return Some(Err(error));
                    }
                    if matches!(followed_kind, Some(Ok(FileKind::Directory))) {
                        let stat = match entry.metadata() {
                            Ok(stat) => stat,
                            Err(error) => {
                                return Some(Err(WalkError {
                                    path: entry.path.clone(),
                                    error: copy_error(error),
                                }));
                            }
                        };
                        if self.tasks.iter().any(|task| match task {
                            Task::Children(directory) => {
                                directory.entry.metadata().is_ok_and(|ancestor| {
                                    ancestor.dev() == stat.dev() && ancestor.ino() == stat.ino()
                                })
                            }
                            _ => false,
                        }) {
                            return Some(Err(WalkError {
                                path: entry.path.clone(),
                                error: io::Error::other("File system loop detected"),
                            }));
                        }
                    }
                    if self.options.depth_first
                        && self.options.max_depth.is_none_or(|max| entry.depth < max)
                        && match entry.kind() {
                            Ok(kind) => kind == FileKind::Directory,
                            Err(error) => {
                                return Some(Err(WalkError {
                                    path: entry.path.clone(),
                                    error: copy_error(error),
                                }));
                            }
                        }
                    {
                        // xdev directories still evaluate, even when not
                        // descended.
                        if self.options.xdev
                            && entry.metadata().is_ok_and(|stat| stat.dev() != root_dev)
                        {
                            return Some(Ok(entry));
                        }
                        if let Err(error) = self.descend(entry, root_dev, before_directory) {
                            return Some(Err(error));
                        }
                    } else {
                        if !self.options.depth_first {
                            self.pending = Some((entry.clone(), root_dev));
                        }
                        return Some(Ok(entry));
                    }
                }
                Task::After(entry) => return Some(Ok(entry)),
                Task::Children(mut directory) => match directory.children.next() {
                    Some(entry) => {
                        let root_dev = directory.root_dev;
                        self.tasks.push(Task::Children(directory));
                        self.tasks.push(Task::Visit(entry, root_dev));
                    }
                    None => {
                        if self.options.depth_first {
                            return Some(Ok(directory.entry));
                        }
                    }
                },
            }
        }
    }
}

impl EntrySource for LiveWalk {
    fn next(&mut self, descend: bool) -> Option<Result<Entry, WalkError>> {
        self.next_entry(descend, &mut || Ok(()))
    }
    fn next_with(
        &mut self,
        descend: bool,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
    ) -> Option<Result<Entry, WalkError>> {
        self.next_entry(descend, before_directory)
    }
}

fn join(parent: &Path, name: &OsStr) -> PathBuf {
    let mut bytes = parent.as_os_str().as_bytes().to_vec();
    if !bytes.ends_with(b"/") {
        bytes.push(b'/');
    }
    bytes.extend_from_slice(name.as_bytes());
    PathBuf::from(OsString::from_vec(bytes))
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
