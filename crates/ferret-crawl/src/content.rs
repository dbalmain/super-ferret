//! Reading a catalogued document's bytes by DocId, for the content index's
//! follow pass (docs/S2.md § The content flow, § Coverage). `ferret-index`
//! never opens a file; its host hands it these bytes.
//!
//! Follow reads a document only if it is still the version the catalog recorded
//! (D21, D33): every directory on its path is opened `O_NOFOLLOW` and must
//! have the catalogued `(dev, ino)`, the file is opened `O_NOFOLLOW` beneath
//! its parent's descriptor and must match the catalog's stat, and a second
//! `fstat` after the read must still match. Anything else is a
//! [`ContentFault`], which the index records as unreadable and never retries.
//!
//! Query verification uses [`Documents::read_current_name`] instead: the same
//! checked directory walk, with the file bracketed against its current stat.
//! It returns that stat so the host can keep current path answers separate
//! from cacheable catalog-version content facts.
//!
//! [`Documents`] maps each DocId to one name by a single scan of the names,
//! built per pass. M5's daemon will want D30's lazy inverse instead; a full
//! scan is what M3's measurement and tests need.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

use ferret_catalog::bulk::Limiter;
use ferret_catalog::{Catalog, DocId, InoId, NameId, OpenError, Section, Target};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, fstat, open, openat, statat};

use crate::ContentFault;
use crate::observe::{BulkRead, Opened, Reader, bracket, catalog_stat};

/// One catalog view's documents, each with one of its names.
pub struct Documents {
    /// `name[doc]` is the name to read `doc` through, plus one; 0 for none.
    names: Vec<u32>,
    /// The parent directory held for the current checked walk.
    dir: Option<(InoId, OwnedFd)>,
    /// With [`Documents::holding_directories`]: the last checked walk, root
    /// first, each directory with its descriptor, kept for the next path.
    held: Option<Vec<(InoId, OwnedFd)>>,
    limiter: Option<Arc<Limiter>>,
    config: ferret_policy::Config,
    /// Documents read whole and checked.
    pub files_read: u64,
    /// Their bytes.
    pub bytes_read: u64,
}

impl std::fmt::Debug for Documents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Documents")
            .field("documents", &self.names.iter().filter(|&&n| n != 0).count())
            .field("files_read", &self.files_read)
            .field("bytes_read", &self.bytes_read)
            .finish_non_exhaustive()
    }
}

impl Documents {
    /// Scans `catalog`'s names once, loading the sections a read needs.
    pub fn new(catalog: &Catalog) -> Result<Self, OpenError> {
        catalog.load(&Section::INODE)?;
        catalog.load(&[Section::Names, Section::Roots, Section::Doc, Section::Docs])?;
        let mut names = vec![0u32; catalog.next_doc().0 as usize];
        for (name, edge) in catalog.name_reader().runs_from(NameId(0)) {
            let Target::Inode(inode) = edge.target() else {
                continue;
            };
            if let Some(doc) = catalog.doc(inode)
                && let Some(slot) = names.get_mut(doc.0 as usize)
                && *slot == 0
                && catalog.is_live_name(name)
            {
                *slot = name.0 + 1;
            }
        }
        Ok(Self {
            names,
            dir: None,
            held: None,
            limiter: None,
            config: ferret_policy::Config::default(),
            files_read: 0,
            bytes_read: 0,
        })
    }

    /// A reader with no DocId map, for a host that already knows which name
    /// to read a document through (a query verifying a row): it reads only
    /// through [`Documents::read_name`], and skips the scan of every name.
    pub fn by_name(catalog: &Catalog) -> Result<Self, OpenError> {
        catalog.load(&Section::INODE)?;
        catalog.load(&[Section::Names, Section::Roots, Section::Doc, Section::Docs])?;
        Ok(Self {
            names: Vec::new(),
            dir: None,
            held: None,
            limiter: None,
            config: ferret_policy::Config::default(),
            files_read: 0,
            bytes_read: 0,
        })
    }

    /// Another reader by name with this one's policy and limiter, for a
    /// second thread: [`Documents::by_name`] without loading again. Counts
    /// start at zero.
    pub fn fork(&self) -> Self {
        Self {
            names: Vec::new(),
            dir: None,
            held: None,
            limiter: self.limiter.clone(),
            config: self.config,
            files_read: 0,
            bytes_read: 0,
        }
    }

    /// Uses the same size cap as a crawl configured with this policy. Product
    /// indexing and querying both use the default policy (8 MiB).
    pub fn with_config(mut self, config: ferret_policy::Config) -> Self {
        self.config = config;
        self
    }

    /// Keeps each checked directory walk's descriptors for later paths
    /// through the same directories: each path still checks every directory
    /// on it, by a stat of its path, instead of opening it again
    /// ([`hold_directory`]). For a reader that lives for one query: it holds
    /// a descriptor per level of the last path.
    pub fn holding_directories(mut self) -> Self {
        self.held = Some(Vec::new());
        self
    }

    /// Paces every read through the shared byte limiter: background work.
    /// An explicit `ferret index` reads unpaced.
    pub fn with_limiter(mut self, limiter: Arc<Limiter>) -> Self {
        self.limiter = Some(limiter);
        self
    }

    /// Replaces `out` with `doc`'s bytes, if it is still the version
    /// `catalog` recorded.
    pub fn read(
        &mut self,
        catalog: &Catalog,
        doc: DocId,
        out: &mut Vec<u8>,
    ) -> Result<(), ContentFault> {
        out.clear();
        let name = match self.names.get(doc.0 as usize) {
            Some(&n) if n != 0 => NameId(n - 1),
            _ => return Err(ContentFault::Changed),
        };
        self.read_name(catalog, name, out)
    }

    /// Replaces `out` with the bytes of the file `name` names, if it is still
    /// the version `catalog` recorded.
    pub fn read_name(
        &mut self,
        catalog: &Catalog,
        name: NameId,
        out: &mut Vec<u8>,
    ) -> Result<(), ContentFault> {
        out.clear();
        let (parent, bytes) = catalog.name_reader().edge(name);
        let Target::Inode(inode) = catalog.name(name).target() else {
            return Err(ContentFault::Changed);
        };
        let recorded = catalog.inode(inode).stat;
        let dir = self.directory(catalog, parent)?;
        let name = std::ffi::OsStr::from_bytes(bytes);
        let mut file = match Reader::open(dir.as_fd(), name, &recorded) {
            Opened::Ready { file, .. } => file,
            Opened::Fault(fault) => return Err(fault),
        };
        read_all(
            &mut file,
            recorded.size,
            self.config.size_cap,
            self.limiter.as_deref(),
            out,
            &|| false,
        )?;
        bracket(&file, &recorded, ferret_catalog::Content::Binary)?;
        self.files_read += 1;
        self.bytes_read += out.len() as u64;
        Ok(())
    }

    /// Checks the file's version through its own fresh directory walk, with
    /// one no-follow stat and no content read.
    pub fn check_name(&mut self, catalog: &Catalog, name: NameId) -> Result<(), ContentFault> {
        let stat = self.stat_name(catalog, name)?;
        let recorded = catalog.inode(catalog.name(name).child).stat;
        recorded
            .same_version(&stat)
            .then_some(())
            .ok_or(ContentFault::Changed)
    }

    /// The current regular file's stat through a fresh checked directory walk.
    /// A changed file is observed; missing/replaced directory chains fail.
    pub fn stat_name(
        &mut self,
        catalog: &Catalog,
        name: NameId,
    ) -> Result<ferret_catalog::Stat, ContentFault> {
        let (parent, bytes) = catalog.name_reader().edge(name);
        let dir = self.directory(catalog, parent)?;
        let stat = statat(
            dir,
            std::ffi::OsStr::from_bytes(bytes),
            AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(|e| ContentFault::Stat(e.into()))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(ContentFault::Changed);
        }
        Ok(catalog_stat(&stat))
    }

    /// A stable current-byte read for a query's Maybe candidate, bracketed
    /// against the opened file's current stat rather than the catalog key.
    /// Returns that stat so the host can distinguish cacheable catalog bytes
    /// from a path-specific changed version. Follow still uses `read_name`.
    pub fn read_current_name(
        &mut self,
        catalog: &Catalog,
        name: NameId,
        out: &mut Vec<u8>,
    ) -> Result<ferret_catalog::Stat, ContentFault> {
        self.read_current_name_until(catalog, name, out, &|| false)
    }

    /// Like `read_current_name`, bounded by the policy size cap, with fallible
    /// allocation and host cancellation between 256 KiB read chunks.
    pub fn read_current_name_until(
        &mut self,
        catalog: &Catalog,
        name: NameId,
        out: &mut Vec<u8>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ferret_catalog::Stat, ContentFault> {
        out.clear();
        let (cap, limiter) = (self.config.size_cap, self.limiter.clone());
        let current = self.current_until(catalog, name, cancelled, &mut |file, size| {
            out.clear();
            read_all(file, size, cap, limiter.as_deref(), out, cancelled)
        });
        if current.is_ok() {
            self.bytes_read += out.len() as u64;
        }
        current
    }

    /// [`Documents::read_current_name_until`], handing the bytes to `each`
    /// as a reader instead of collecting them: `each` reads as much as it
    /// needs, with the document's size, and should stop between reads once
    /// `cancelled` says so, failing.
    ///
    /// `each` may be called twice for one document, the second time from
    /// its first byte: when the file proves not to be the catalogued
    /// version, it is read again bracketed by its current stat. Only what
    /// the last call saw describes the returned version.
    pub fn stream_current_name_until(
        &mut self,
        catalog: &Catalog,
        name: NameId,
        each: &mut dyn FnMut(&mut dyn Read, u64) -> io::Result<()>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ferret_catalog::Stat, ContentFault> {
        let (cap, limiter) = (self.config.size_cap, self.limiter.clone());
        self.current_until(catalog, name, cancelled, &mut |file, size| {
            if cancelled() {
                return Err(ContentFault::Cancelled);
            }
            if size > cap {
                return Err(ContentFault::TooLarge { size, cap });
            }
            let done = match limiter.as_deref() {
                Some(limiter) => each(
                    &mut BulkRead {
                        file,
                        limiter: Some(limiter),
                        remaining: size,
                    },
                    size,
                ),
                None => each(&mut Read::take(file, size), size),
            };
            done.map_err(|e| match cancelled() {
                true => ContentFault::Cancelled,
                false => ContentFault::Read(e),
            })
        })
    }

    /// The current version of the file `name` names, read by `pass` (given
    /// the file at its start and the size to read), with the version
    /// established by a stat.
    ///
    /// When the catalog recorded the file at most the size cap, `pass` first
    /// reads that size and one `fstat` follows. If it is the catalogued
    /// version, nothing has changed the file since its recorded ctime, so
    /// what `pass` read was that version: no stat is needed before it. Else
    /// `pass` runs again from the start, bracketed by that stat and one
    /// more, as a read of a changed file always was.
    ///
    /// The calls, for a catalogued file in a held directory: `openat`, the
    /// reads, `fstat`, `close`.
    fn current_until(
        &mut self,
        catalog: &Catalog,
        name: NameId,
        cancelled: &dyn Fn() -> bool,
        pass: &mut dyn FnMut(&mut File, u64) -> Result<(), ContentFault>,
    ) -> Result<ferret_catalog::Stat, ContentFault> {
        if cancelled() {
            return Err(ContentFault::Cancelled);
        }
        let (parent, bytes) = catalog.name_reader().edge(name);
        let recorded = catalog.inode(catalog.name(name).child).stat;
        let dir = self.directory(catalog, parent)?;
        let fd = openat(
            dir,
            std::ffi::OsStr::from_bytes(bytes),
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| ContentFault::Open(e.into()))?;
        let mut file = File::from(fd);
        let stat = |file: &File| fstat(file).map_err(|e| ContentFault::Stat(e.into()));
        let regular = |stat: &rustix::fs::Stat| {
            FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
        };
        let seen = if recorded.size <= self.config.size_cap {
            let read = pass(&mut file, recorded.size);
            let after = stat(&file)?;
            if regular(&after) && catalog_stat(&after).same_version(&recorded) {
                read?;
                self.files_read += 1;
                return Ok(catalog_stat(&after));
            }
            if let Err(ContentFault::Cancelled) = read {
                return Err(ContentFault::Cancelled);
            }
            file.seek(SeekFrom::Start(0)).map_err(ContentFault::Read)?;
            after
        } else {
            stat(&file)?
        };
        if !regular(&seen) {
            return Err(ContentFault::Changed);
        }
        let current = catalog_stat(&seen);
        pass(&mut file, current.size)?;
        bracket(&file, &current, ferret_catalog::Content::Binary)?;
        self.files_read += 1;
        Ok(current)
    }

    /// Re-walk from the root for each path. A retained descriptor alone cannot
    /// establish that its original path still names it after a rename: a
    /// reader [holding directories](Documents::holding_directories) checks
    /// each path again before reusing one.
    fn directory(&mut self, catalog: &Catalog, dir: InoId) -> Result<&OwnedFd, ContentFault> {
        if let Some(held) = &mut self.held {
            return hold_directory(held, catalog, dir);
        }
        self.dir = None;
        let fd = open_directory(catalog, dir)?;
        self.dir = Some((dir, fd));
        match &self.dir {
            Some((_, fd)) => Ok(fd),
            None => Err(ContentFault::Changed),
        }
    }
}

/// Opens `dir` from its root, one `O_NOFOLLOW` component at a time, each
/// checked against the catalog's `(dev, ino)`.
fn open_directory(catalog: &Catalog, dir: InoId) -> Result<OwnedFd, ContentFault> {
    let mut held = Vec::new();
    hold_directory(&mut held, catalog, dir)?;
    held.pop().map(|(_, fd)| fd).ok_or(ContentFault::Changed)
}

/// [`open_directory`], reusing the descriptors `held` has for the start of
/// `dir`'s chain and leaving the whole chain in it, root first.
///
/// A held descriptor is reused only once its path is checked again: the
/// root's path by a `stat`, each directory below by a no-follow `fstatat`
/// in its held parent, each against the catalog's `(dev, ino)` that the
/// descriptor was checked against when opened. So, as on a fresh walk, the
/// directory read through is the one its path named, at a moment between
/// the walk's start and the file's open; a renamed or replaced directory
/// fails the same way. One call per level instead of three (open, fstat,
/// close).
fn hold_directory<'h>(
    held: &'h mut Vec<(InoId, OwnedFd)>,
    catalog: &Catalog,
    dir: InoId,
) -> Result<&'h OwnedFd, ContentFault> {
    let names = catalog.name_reader();
    // `dir` and its ancestors below the root, nearest first.
    let mut chain = Vec::new();
    let mut at = dir;
    while let Some(name) = catalog.dir_name(at) {
        let (parent, bytes) = names.edge(name);
        chain.push((at, bytes));
        at = parent;
    }
    let root = at;
    let path = catalog
        .roots()
        .find(|&(id, _)| id == root)
        .map(|(_, path)| path)
        .ok_or(ContentFault::Changed)?;
    let path = std::ffi::OsStr::from_bytes(path);
    let same = |stat: &rustix::fs::Stat, id: InoId| {
        FileType::from_raw_mode(stat.st_mode) == FileType::Directory
            && (stat.st_dev, stat.st_ino) == catalog.identity(id)
    };
    // How much of the held chain is `dir`'s and still at its path.
    let mut kept = 0;
    if held.first().is_some_and(|&(id, _)| id == root) {
        // The root's own path may pass through symlinks: the user's.
        let stat = rustix::fs::stat(path).map_err(|e| ContentFault::Stat(e.into()))?;
        if same(&stat, root) {
            kept = 1;
            for (&(id, bytes), (parent, (held_id, _))) in
                chain.iter().rev().zip(held.iter().zip(&held[1..]))
            {
                if id != *held_id {
                    break;
                }
                let stat = statat(
                    &parent.1,
                    std::ffi::OsStr::from_bytes(bytes),
                    AtFlags::SYMLINK_NOFOLLOW,
                )
                .map_err(|e| ContentFault::Stat(e.into()))?;
                if !same(&stat, id) {
                    return Err(ContentFault::Changed);
                }
                kept += 1;
            }
        }
    }
    held.truncate(kept);
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    if held.is_empty() {
        // The root's own path is the user's; symlinks above it are theirs
        // to keep. Everything below it must not follow one.
        let fd = open(path, flags, Mode::empty()).map_err(|e| ContentFault::Open(e.into()))?;
        check_directory(&fd, catalog, root)?;
        held.push((root, fd));
    }
    for &(id, bytes) in chain.iter().rev().skip(held.len() - 1) {
        let fd = match held.last() {
            Some((_, parent)) => openat(
                parent,
                std::ffi::OsStr::from_bytes(bytes),
                flags | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map_err(|e| ContentFault::Open(e.into()))?,
            None => return Err(ContentFault::Changed),
        };
        check_directory(&fd, catalog, id)?;
        held.push((id, fd));
    }
    match held.last() {
        Some((_, fd)) => Ok(fd),
        None => Err(ContentFault::Changed),
    }
}

fn check_directory(fd: &OwnedFd, catalog: &Catalog, id: InoId) -> Result<(), ContentFault> {
    let stat = fstat(fd).map_err(|e| ContentFault::Stat(e.into()))?;
    let same = FileType::from_raw_mode(stat.st_mode) == FileType::Directory
        && (stat.st_dev, stat.st_ino) == catalog.identity(id);
    same.then_some(()).ok_or(ContentFault::Changed)
}

/// Reads `size` bytes, the size the open file was checked at, through the
/// limiter if any. The bracket afterwards catches growth or shrinkage.
fn read_all(
    file: &mut File,
    size: u64,
    cap: u64,
    limiter: Option<&Limiter>,
    out: &mut Vec<u8>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), ContentFault> {
    if cancelled() {
        return Err(ContentFault::Cancelled);
    }
    if size > cap {
        return Err(ContentFault::TooLarge { size, cap });
    }
    let size_usize = usize::try_from(size).map_err(|_| ContentFault::TooLarge { size, cap })?;
    out.try_reserve_exact(size_usize)
        .map_err(ContentFault::Allocation)?;
    let mut remaining = size;
    loop {
        if cancelled() {
            return Err(ContentFault::Cancelled);
        }
        if remaining == 0 {
            return Ok(());
        }
        // A chunk at a time, for cancellation, and appended without zeroing
        // first: `File` reads into the spare capacity as it is.
        let want = remaining.min(256 << 10);
        let mut chunk = Read::take(&mut *file, want);
        let read = match limiter {
            Some(limiter) => limiter.transfer(want as usize, || chunk.read_to_end(out)),
            None => chunk.read_to_end(out),
        };
        match read {
            Ok(0) => return Ok(()),
            Ok(n) => remaining -= n as u64,
            Err(e) => return Err(ContentFault::Read(e)),
        }
    }
}
