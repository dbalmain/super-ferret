//! The common advisory writer lock, including early-return release.
use std::fs::{File, TryLockError};
use std::io;
use std::path::Path;

use crate::regular::{self, Access};

pub(crate) struct Lock(std::sync::Arc<Held>);
struct Held(File);
pub(crate) enum Error {
    Locked,
    Io(io::Error),
}
impl Lock {
    pub(crate) fn open(dir: &Path) -> Result<Self, Error> {
        let file = regular::open(&dir.join("lock"), Access::Create).map_err(Error::Io)?;
        match file.try_lock() {
            Ok(()) => Ok(Self(std::sync::Arc::new(Held(file)))),
            Err(TryLockError::WouldBlock) => {
                #[cfg(debug_assertions)]
                refusal::record(&dir.join("lock"), &file);
                Err(Error::Locked)
            }
            Err(TryLockError::Error(e)) => Err(Error::Io(e)),
        }
    }
    // Full rebuild transactions borrow ownership without unlocking the
    // resident writer when their temporary guard is dropped.
    pub(crate) fn share(&self) -> Self {
        Self(self.0.clone())
    }
    #[cfg(test)]
    pub(crate) fn copy(&self) -> io::Result<File> {
        self.0.0.try_clone()
    }
}
impl Drop for Held {
    fn drop(&mut self) {
        // flock belongs to the open file description. A child forked by
        // another thread may hold a copy until exec; merely closing our fd
        // leaves the lock held. Explicitly unlock on every success/error path.
        let _ = self.0.unlock();
    }
}

/// Evidence of who held the lock, taken at the moment `try_lock` refused it.
/// Taken after the refused process exits, `/proc/locks` can no longer say who
/// held the lock then, and an unexplained refusal on a fresh test fixture
/// needs exactly that. Debug builds only, and only when `FERRET_LOCK_DIAG`
/// names a file to append to; the test fixtures set it and print the file.
#[cfg(debug_assertions)]
mod refusal {
    use std::fmt::Write as _;
    use std::fs::{self, File};
    use std::io::Write as _;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    pub(super) fn record(path: &Path, file: &File) {
        let Some(target) = std::env::var_os("FERRET_LOCK_DIAG") else {
            return;
        };
        let report = report(path, file);
        let _ = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(target)
            .and_then(|mut out| out.write_all(report.as_bytes()));
    }

    fn report(path: &Path, file: &File) -> String {
        let mut out = String::new();
        let thread = fs::read_link("/proc/thread-self")
            .map(|link| link.display().to_string())
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "lock refused: pid {} ({thread}) {:?} path {path:?}",
            std::process::id(),
            std::thread::current().name(),
        );
        let (dev, ino) = match file.metadata() {
            Ok(metadata) => (metadata.dev(), metadata.ino()),
            Err(error) => {
                let _ = writeln!(out, "  metadata: {error}");
                return out;
            }
        };
        // /proc/locks names the device as hex major:minor, then the inode.
        let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
        let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
        let key = format!(" {major:02x}:{minor:02x}:{ino} ");
        let _ = writeln!(out, "  dev {dev:#x} ino {ino} key{key}");
        // A flock record names the process that took it, which may since have
        // exited while a descriptor it shared keeps the lock alive.
        for line in fs::read_to_string("/proc/locks")
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(&key))
        {
            let holder = line
                .split_whitespace()
                .nth(4)
                .map_or_else(String::new, command_line);
            let _ = writeln!(out, "  /proc/locks: {line} -> {holder}");
        }
        // Every descriptor open on the inode, with its flags: these are the
        // processes that can hold the open file description, and fdinfo's
        // flags say whether each copy is close-on-exec (octal 02000000).
        for (pid, fd) in descriptors(dev, ino) {
            let flags = fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}"))
                .unwrap_or_default()
                .lines()
                .filter(|line| line.starts_with("flags:") || line.starts_with("lock:"))
                .collect::<Vec<_>>()
                .join("; ");
            let _ = writeln!(out, "  fd {pid}/{fd}: {flags} -> {}", command_line(&pid));
        }
        out
    }

    fn descriptors(dev: u64, ino: u64) -> Vec<(String, String)> {
        let mut found = Vec::new();
        for process in fs::read_dir("/proc").into_iter().flatten().flatten() {
            let pid = process.file_name().to_string_lossy().into_owned();
            if !pid.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            for entry in fs::read_dir(process.path().join("fd"))
                .into_iter()
                .flatten()
                .flatten()
            {
                if fs::metadata(entry.path())
                    .is_ok_and(|metadata| metadata.dev() == dev && metadata.ino() == ino)
                {
                    found.push((
                        pid.clone(),
                        entry.file_name().to_string_lossy().into_owned(),
                    ));
                }
            }
        }
        found
    }

    fn command_line(pid: &str) -> String {
        fs::read(format!("/proc/{pid}/cmdline")).map_or_else(
            |error| format!("<{error}>"),
            |bytes| String::from_utf8_lossy(&bytes).replace('\0', " "),
        )
    }
}
