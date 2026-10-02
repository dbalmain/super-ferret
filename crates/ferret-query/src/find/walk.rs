//! Sequential, prunable depth-first traversal in readdir order. Paths keep the
//! spelling of each start operand. Raw d_type stays optional; an unknown type
//! shares the entry's one lstat cache with metadata predicates.

use std::cell::OnceCell;
use std::ffi::{OsStr, OsString};
use std::fs::{self, FileType, Metadata};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use rustix::fs::{Dir, FileType as RawType, Mode, OFlags, open};

use super::Options;

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

fn kind(value: FileType) -> FileKind {
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
    depth: usize,
    kind: Option<FileKind>,
    metadata: Rc<OnceCell<io::Result<Metadata>>>,
}

impl Entry {
    /// Builds an entry from fields available to a source without statting it.
    pub fn new(path: PathBuf, depth: usize, kind: FileKind) -> Self {
        Self {
            path,
            depth,
            kind: Some(kind),
            metadata: Rc::new(OnceCell::new()),
        }
    }

    /// Exact spelling used for matching and output.
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Depth relative to the current start path, which has depth zero.
    pub fn depth(&self) -> usize {
        self.depth
    }
    /// Type from d_type if known, otherwise from the single cached lstat.
    pub fn kind(&self) -> Result<FileKind, &io::Error> {
        self.kind
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
            .get_or_init(|| fs::symlink_metadata(&self.path))
            .as_ref()
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
}

struct Directory {
    entry: Entry,
    children: Dir,
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

    fn descend(&mut self, entry: Entry, root_dev: u64) -> Result<(), WalkError> {
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
        let flags = if entry.depth == 0 {
            flags
        } else {
            flags | OFlags::NOFOLLOW
        };
        match open(&entry.path, flags, Mode::empty()).and_then(Dir::new) {
            Ok(children) => self.tasks.push(Task::Children(Directory {
                entry,
                children,
                root_dev,
            })),
            Err(error) => {
                let path = entry.path.clone();
                if self.options.depth_first {
                    self.tasks.push(Task::After(entry));
                }
                return Err(WalkError {
                    path,
                    error: io::Error::from(error),
                });
            }
        }
        Ok(())
    }
}

impl EntrySource for LiveWalk {
    fn next(&mut self, descend: bool) -> Option<Result<Entry, WalkError>> {
        if let Some((entry, root_dev)) = self.pending.take()
            && descend
            && let Err(error) = self.descend(entry, root_dev)
        {
            return Some(Err(error));
        }
        loop {
            let task = match self.tasks.pop() {
                Some(task) => task,
                None => {
                    let path = self.paths.next()?;
                    let stat = match fs::symlink_metadata(&path) {
                        Ok(stat) => stat,
                        Err(error) => return Some(Err(WalkError { path, error })),
                    };
                    let root_dev = stat.dev();
                    let entry = Entry::new(path, 0, kind(stat.file_type()));
                    let _ = entry.metadata.set(Ok(stat));
                    Task::Visit(entry, root_dev)
                }
            };
            match task {
                Task::Visit(entry, root_dev) => {
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
                        if let Err(error) = self.descend(entry, root_dev) {
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
                Task::Children(mut directory) => match directory.children.read() {
                    Some(Ok(child)) => {
                        let name = child.file_name().to_bytes();
                        if name == b"." || name == b".." {
                            self.tasks.push(Task::Children(directory));
                            continue;
                        }
                        let path = join(directory.entry.path(), OsStr::from_bytes(name));
                        let depth = directory.entry.depth + 1;
                        let root_dev = directory.root_dev;
                        self.tasks.push(Task::Children(directory));
                        let mut entry = Entry::new(path, depth, FileKind::File);
                        entry.kind = raw_kind(child.file_type());
                        #[cfg(test)]
                        if self.force_unknown {
                            entry.kind = None;
                        }
                        self.tasks.push(Task::Visit(entry, root_dev));
                    }
                    Some(Err(error)) => {
                        let path = directory.entry.path.clone();
                        self.tasks.push(Task::Children(directory));
                        return Some(Err(WalkError {
                            path,
                            error: io::Error::from(error),
                        }));
                    }
                    None => {
                        // An open directory unlinked after enumeration can
                        // finish without a diagnostic for name-only queries.
                        if self.options.depth_first {
                            return Some(Ok(directory.entry));
                        }
                    }
                },
            }
        }
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
