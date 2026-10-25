//! Reading a file the user configures ferret with: the global ignore file,
//! find's `config`, the index's settings, git's global excludes.
//!
//! Such a path may name anything, and a plain `open` of a FIFO with no writer
//! blocks forever, before any cancellation check runs. So the file is opened
//! `O_NONBLOCK` and kept only when the descriptor that was opened is a
//! regular file; checking the path first would race a rename. Unlike a scoped
//! ignore file read during a crawl (`ferret-crawl`'s walk), there is no size
//! limit. Symlinks are followed.
//!
//! [`open_append`] applies the same rule to a file ferret appends to, the
//! query log.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Linux `O_NONBLOCK`, as in the other std-only hosts in this crate.
const NONBLOCK: i32 = 0x800;

/// What is at a configuration path.
#[derive(Debug)]
pub(crate) enum Contents {
    /// A regular file, or a symlink to one, and its bytes.
    Bytes(Vec<u8>),
    /// Nothing at all, not even a dangling symlink.
    Missing,
    /// Something that is not a regular file: a directory, FIFO, socket or
    /// device, or a symlink to one.
    NotRegular,
}

/// The contents of `path`. A dangling symlink is an error (`NotFound`), not
/// [`Contents::Missing`]: what it pointed at is unknown.
pub(crate) fn read(path: &Path) -> io::Result<Contents> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e)
            if e.kind() == io::ErrorKind::NotFound
                && fs::symlink_metadata(path)
                    .is_err_and(|e| e.kind() == io::ErrorKind::NotFound) =>
        {
            return Ok(Contents::Missing);
        }
        // A directory opens; anything else that cannot (a socket is ENXIO)
        // is not a file either. An error that is just about access is kept.
        Err(e) if e.raw_os_error() == Some(ENXIO) => return Ok(Contents::NotRegular),
        Err(e) => return Err(e),
    };
    if !file.metadata()?.is_file() {
        return Ok(Contents::NotRegular);
    }
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        bytes.try_reserve(n).map_err(io::Error::other)?;
        bytes.extend_from_slice(&chunk[..n]);
    }
    Ok(Contents::Bytes(bytes))
}

/// Opens `path` for appending, creating it with `mode` if it is missing,
/// and keeps it only when the descriptor that was opened is a regular file.
/// Anything else is [`not_regular`], including a FIFO that has a reader: it
/// would open, but a write to it can block. Symlinks are followed.
///
/// The descriptor stays `O_NONBLOCK`, which a regular file ignores.
pub(crate) fn open_append(path: &Path, mode: u32) -> io::Result<File> {
    let file = match OpenOptions::new()
        .append(true)
        .create(true)
        .mode(mode)
        .custom_flags(NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        // A write-only open of a FIFO with no reader, or of a socket.
        Err(e) if e.raw_os_error() == Some(ENXIO) => return Err(not_regular()),
        Err(e) => return Err(e),
    };
    if !file.metadata()?.is_file() {
        return Err(not_regular());
    }
    Ok(file)
}

/// Linux `ENXIO`: opening a socket, or a device with nothing behind it.
const ENXIO: i32 = 6;

/// The error a reader reports for [`Contents::NotRegular`].
pub(crate) fn not_regular() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "not a regular file")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::process::Command;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ferret-config-file-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mkfifo(path: &Path) {
        let status = Command::new("mkfifo").arg(path).status().unwrap();
        assert!(status.success(), "mkfifo {}", path.display());
    }

    /// Each kind of thing at the path, followed through a symlink too: only
    /// a regular file has bytes, and nothing blocks. A blocking open would
    /// hang on the FIFOs, so the reads run on a thread with a deadline.
    #[test]
    fn only_a_regular_file_is_read_and_nothing_blocks() {
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            classify_each_kind();
            let _ = done.send(());
        });
        match finished.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(()) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("a read blocked"),
            // The thread panicked; its message is already on stderr.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => panic!("a read failed"),
        }
    }

    fn classify_each_kind() {
        let dir = scratch("kinds");
        fs::write(dir.join("file"), "rules\n").unwrap();
        mkfifo(&dir.join("fifo"));
        fs::create_dir(dir.join("dir")).unwrap();
        let _listener = UnixListener::bind(dir.join("socket")).unwrap();
        for name in ["file", "fifo", "dir", "socket"] {
            std::os::unix::fs::symlink(name, dir.join(format!("{name}-link"))).unwrap();
        }
        std::os::unix::fs::symlink("gone", dir.join("dangling")).unwrap();
        for (name, expected) in [
            ("file", "Bytes"),
            ("file-link", "Bytes"),
            ("fifo", "NotRegular"),
            ("fifo-link", "NotRegular"),
            ("dir", "NotRegular"),
            ("dir-link", "NotRegular"),
            ("socket", "NotRegular"),
            ("socket-link", "NotRegular"),
            ("absent", "Missing"),
        ] {
            let got = match read(&dir.join(name)).unwrap() {
                Contents::Bytes(bytes) => {
                    assert_eq!(bytes, b"rules\n");
                    "Bytes"
                }
                Contents::Missing => "Missing",
                Contents::NotRegular => "NotRegular",
            };
            assert_eq!(got, expected, "{name}");
        }
        let error = read(&dir.join("dangling")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
