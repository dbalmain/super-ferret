//! Actual I/O boundaries shared by checkpoint and log publication.
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Point {
    SnapshotSync,
    SnapshotRename,
    HeaderSync,
    HeaderRename,
    PairSync,
    #[cfg(test)]
    PartialLogWrite,
    LogWrite,
    LogSync,
    ManifestSync,
    ManifestRename,
    DirectorySync,
    RecoverySync,
    RecoveryDirectorySync,
}

pub(crate) fn hit(point: Point) -> io::Result<()> {
    #[cfg(test)]
    {
        VISITED.with_borrow_mut(|points| points.push(point));
        if FAIL.get() == Some(point) {
            return Err(io::Error::other(format!("injected stop after {point:?}")));
        }
    }
    #[cfg(not(test))]
    let _ = point;
    Ok(())
}

pub(crate) fn sync(file: &File, point: Point) -> io::Result<()> {
    file.sync_all()?;
    hit(point)
}

pub(crate) fn rename(from: &Path, to: &Path, point: Point) -> io::Result<()> {
    fs::rename(from, to)?;
    hit(point)
}

#[cfg(test)]
thread_local! {
    pub(crate) static FAIL: std::cell::Cell<Option<Point>> = const { std::cell::Cell::new(None) };
    pub(crate) static VISITED: std::cell::RefCell<Vec<Point>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Only tests can stop a positional append after a real partial write.
pub(crate) fn append(file: &File, bytes: &[u8], at: u64) -> io::Result<()> {
    #[cfg(test)]
    if FAIL.get() == Some(Point::PartialLogWrite) {
        file.write_all_at(&bytes[..bytes.len() / 2], at)?;
        return hit(Point::PartialLogWrite);
    }
    file.write_all_at(bytes, at)?;
    hit(Point::LogWrite)
}
