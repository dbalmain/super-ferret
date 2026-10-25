//! Opening the files a catalog directory holds: `current`, the snapshot and
//! change-log pair, the writer `lock`, and the publication temporaries.
//!
//! Any of those names can hold something other than a regular file, and a
//! plain `open` of a FIFO with no peer blocks forever, before a length check,
//! checksum or `try_lock` gets to run. So an existing file is opened
//! `O_NONBLOCK` and kept only when the descriptor that was opened is a regular
//! file; checking the path first would race a rename. Symlinks are followed,
//! so a symlink to a regular file still works. The flag stays set: Linux
//! ignores it for regular-file reads, writes and `flock`, and clearing it
//! needs `fcntl`, which this crate has no binding for.
//!
//! A temporary is never reopened: [`replace`] unlinks whatever is at the name
//! and creates a new file `O_EXCL`, so it cannot open anything it did not
//! create. The names are private to the writer-lock holder, and startup
//! recovery removes them anyway.
//!
//! `O_NONBLOCK` promises nothing about regular files on a slow filesystem; it
//! only keeps special files from blocking the open.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Linux `O_NONBLOCK`; this crate has no libc binding.
const NONBLOCK: i32 = 0x800;
/// Linux `ENXIO`: a FIFO opened for writing with no reader, or a socket.
const ENXIO: i32 = 6;

/// How an existing state file is opened.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Access {
    /// Read only.
    Read,
    /// Read and write a file that must already exist.
    Update,
    /// Write, creating the file when it is missing and never truncating it:
    /// the writer `lock`.
    Create,
}

/// The cause inside the `io::Error` for a name that holds something other
/// than a regular file: a FIFO, socket, device or directory, or a symlink to
/// one. The error's kind is `InvalidData`.
#[derive(Debug)]
pub(crate) struct NotRegular;

impl fmt::Display for NotRegular {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a regular file")
    }
}

impl std::error::Error for NotRegular {}

/// Whether `error` came from [`open`] refusing something not a regular file.
pub(crate) fn is_not_regular(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|cause| cause.is::<NotRegular>())
}

/// Opens the regular file at `path` without blocking on a special file.
/// Anything else is an `InvalidData` error that [`is_not_regular`] names;
/// a missing file is `NotFound`, as a plain open reports it.
pub(crate) fn open(path: &Path, access: Access) -> io::Result<File> {
    let mut options = OpenOptions::new();
    match access {
        Access::Read => options.read(true),
        Access::Update => options.read(true).write(true),
        Access::Create => options.write(true).create(true).truncate(false),
    };
    let file = options.custom_flags(NONBLOCK).open(path).map_err(|e| {
        // A writable open fails on a directory, a reader-less FIFO and a
        // socket before there is a descriptor to check.
        if e.raw_os_error() == Some(ENXIO) || e.kind() == io::ErrorKind::IsADirectory {
            not_regular()
        } else {
            e
        }
    })?;
    if !file.metadata()?.is_file() {
        return Err(not_regular());
    }
    Ok(file)
}

/// Reads the whole regular file at `path`, as [`open`] opens it.
pub(crate) fn read(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut file = open(path, Access::Read)?;
    let mut bytes = Vec::new();
    let len = usize::try_from(file.metadata()?.len()).map_err(io::Error::other)?;
    bytes.try_reserve_exact(len).map_err(io::Error::other)?;
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// A new, empty regular file at temporary name `path`, opened for reading
/// and writing. Whatever was at the name is unlinked first; a directory
/// there is an error.
pub(crate) fn replace(path: &Path) -> io::Result<File> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
}

fn not_regular() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, NotRegular)
}
