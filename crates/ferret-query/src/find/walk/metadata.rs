//! One stat of an entry: [`Metadata`] from a live `statx`, and [`Stat`], which
//! reads a field from the catalog or from that live stat.

use rustix::fs::{FileType as RawType, Stat as RawStat, Statx, makedev};

use super::*;

pub(in crate::find) enum Stat<'a> {
    Stored(&'a Catalog, InoId),
    Live(&'a Metadata),
}

impl Stat<'_> {
    pub(in crate::find) fn size(&self) -> u64 {
        match *self {
            Self::Stored(catalog, id) => catalog.size(id),
            Self::Live(stat) => stat.size(),
        }
    }
    pub(in crate::find) fn mode(&self) -> u32 {
        match *self {
            Self::Stored(catalog, id) => catalog.mode(id),
            Self::Live(stat) => stat.mode(),
        }
    }
    pub(in crate::find) fn uid(&self) -> u32 {
        match *self {
            Self::Stored(catalog, id) => catalog.owner(id).0,
            Self::Live(stat) => stat.uid(),
        }
    }
    pub(in crate::find) fn gid(&self) -> u32 {
        match *self {
            Self::Stored(catalog, id) => catalog.owner(id).1,
            Self::Live(stat) => stat.gid(),
        }
    }
    pub(in crate::find) fn nlink(&self) -> u64 {
        match *self {
            Self::Stored(catalog, id) => catalog.nlink(id),
            Self::Live(stat) => stat.nlink(),
        }
    }
    pub(in crate::find) fn dev(&self) -> u64 {
        match *self {
            Self::Stored(catalog, id) => catalog.identity(id).0,
            Self::Live(stat) => stat.dev(),
        }
    }
    pub(in crate::find) fn ino(&self) -> u64 {
        match *self {
            Self::Stored(catalog, id) => catalog.identity(id).1,
            Self::Live(stat) => stat.ino(),
        }
    }
    pub(in crate::find) fn mtime(&self) -> i64 {
        match *self {
            Self::Stored(catalog, id) => catalog.mtime(id),
            Self::Live(stat) => stat.mtime(),
        }
    }
    pub(in crate::find) fn mtime_nsec(&self) -> i64 {
        match *self {
            Self::Stored(catalog, id) => catalog.mtime_nsec(id),
            Self::Live(stat) => stat.mtime_nsec(),
        }
    }
    pub(in crate::find) fn ctime(&self) -> i64 {
        match *self {
            Self::Stored(catalog, id) => catalog.ctime(id),
            Self::Live(stat) => stat.ctime(),
        }
    }
    pub(in crate::find) fn ctime_nsec(&self) -> i64 {
        match *self {
            Self::Stored(catalog, id) => catalog.ctime_nsec(id),
            Self::Live(stat) => stat.ctime_nsec(),
        }
    }
}

pub(in crate::find) fn metadata(path: &Path, follow: bool) -> io::Result<Metadata> {
    metadata_at(CWD, path, follow)
}

/// A followed lookup falls back to the link itself when its target is
/// missing, as find reports a dangling link.
pub(super) fn metadata_at(dir: BorrowedFd<'_>, path: &Path, follow: bool) -> io::Result<Metadata> {
    if follow {
        match stat_at(dir, path, AtFlags::empty()) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                stat_at(dir, path, AtFlags::SYMLINK_NOFOLLOW)
            }
            result => result,
        }
    } else {
        stat_at(dir, path, AtFlags::SYMLINK_NOFOLLOW)
    }
}

// statx, as GNU's fstatat behaves: a terminal automount point is not
// triggered. Where statx is unavailable (old kernels, seccomp filters), fall
// back to fstatat as std does.
pub(super) fn stat_at(dir: BorrowedFd<'_>, path: &Path, flags: AtFlags) -> io::Result<Metadata> {
    match statx(
        dir,
        path,
        flags | AtFlags::NO_AUTOMOUNT,
        StatxFlags::BASIC_STATS,
    ) {
        Ok(stat) => Ok(Metadata::from(stat)),
        Err(rustix::io::Errno::NOSYS) => Ok(Metadata::from(statat(dir, path, flags)?)),
        Err(error) => Err(error.into()),
    }
}

/// One live stat of an entry. The accessors keep std's `MetadataExt` names
/// and values.
#[derive(Clone, Copy, Debug)]
pub struct Metadata {
    mode: u32,
    size: u64,
    uid: u32,
    gid: u32,
    nlink: u64,
    dev: u64,
    rdev: u64,
    ino: u64,
    blocks: u64,
    atime: (i64, i64),
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl From<Statx> for Metadata {
    fn from(stat: Statx) -> Self {
        Self {
            mode: u32::from(stat.stx_mode),
            size: stat.stx_size,
            uid: stat.stx_uid,
            gid: stat.stx_gid,
            nlink: u64::from(stat.stx_nlink),
            dev: makedev(stat.stx_dev_major, stat.stx_dev_minor),
            rdev: makedev(stat.stx_rdev_major, stat.stx_rdev_minor),
            ino: stat.stx_ino,
            blocks: stat.stx_blocks,
            atime: (stat.stx_atime.tv_sec, i64::from(stat.stx_atime.tv_nsec)),
            mtime: (stat.stx_mtime.tv_sec, i64::from(stat.stx_mtime.tv_nsec)),
            ctime: (stat.stx_ctime.tv_sec, i64::from(stat.stx_ctime.tv_nsec)),
        }
    }
}

// `struct stat`'s field widths vary by architecture, so the casts are
// conversions on some targets and no-ops on others, as in std.
#[allow(clippy::unnecessary_cast)]
impl From<RawStat> for Metadata {
    fn from(stat: RawStat) -> Self {
        Self {
            mode: stat.st_mode as u32,
            size: stat.st_size as u64,
            uid: stat.st_uid as u32,
            gid: stat.st_gid as u32,
            nlink: stat.st_nlink as u64,
            dev: stat.st_dev as u64,
            rdev: stat.st_rdev as u64,
            ino: stat.st_ino as u64,
            blocks: stat.st_blocks as u64,
            atime: (stat.st_atime as i64, stat.st_atime_nsec as i64),
            mtime: (stat.st_mtime as i64, stat.st_mtime_nsec as i64),
            ctime: (stat.st_ctime as i64, stat.st_ctime_nsec as i64),
        }
    }
}

impl Metadata {
    pub fn kind(&self) -> FileKind {
        match RawType::from_raw_mode(self.mode) {
            RawType::Directory => FileKind::Directory,
            RawType::Symlink => FileKind::Symlink,
            RawType::Fifo => FileKind::Fifo,
            RawType::Socket => FileKind::Socket,
            RawType::BlockDevice => FileKind::Block,
            RawType::CharacterDevice => FileKind::Character,
            RawType::RegularFile | RawType::Unknown => FileKind::File,
        }
    }
    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn mode(&self) -> u32 {
        self.mode
    }
    pub fn uid(&self) -> u32 {
        self.uid
    }
    pub fn gid(&self) -> u32 {
        self.gid
    }
    pub fn nlink(&self) -> u64 {
        self.nlink
    }
    pub fn dev(&self) -> u64 {
        self.dev
    }
    pub fn ino(&self) -> u64 {
        self.ino
    }
    pub fn rdev(&self) -> u64 {
        self.rdev
    }
    pub fn blocks(&self) -> u64 {
        self.blocks
    }
    pub fn atime(&self) -> i64 {
        self.atime.0
    }
    pub fn atime_nsec(&self) -> i64 {
        self.atime.1
    }
    pub fn mtime(&self) -> i64 {
        self.mtime.0
    }
    pub fn mtime_nsec(&self) -> i64 {
        self.mtime.1
    }
    pub fn ctime(&self) -> i64 {
        self.ctime.0
    }
    pub fn ctime_nsec(&self) -> i64 {
        self.ctime.1
    }
}
