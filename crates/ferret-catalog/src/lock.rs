//! The common advisory writer lock, including early-return release.
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

pub(crate) struct Lock(File);
pub(crate) enum Error {
    Locked,
    Io(io::Error),
}
impl Lock {
    pub(crate) fn open(dir: &Path) -> Result<Self, Error> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("lock"))
            .map_err(Error::Io)?;
        match file.try_lock() {
            Ok(()) => Ok(Self(file)),
            Err(TryLockError::WouldBlock) => Err(Error::Locked),
            Err(TryLockError::Error(e)) => Err(Error::Io(e)),
        }
    }
    #[cfg(test)]
    pub(crate) fn copy(&self) -> io::Result<File> {
        self.0.try_clone()
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        // flock belongs to the open file description. A child forked by
        // another thread may hold a copy until exec; merely closing our fd
        // leaves the lock held. Explicitly unlock on every success/error path.
        let _ = self.0.unlock();
    }
}
