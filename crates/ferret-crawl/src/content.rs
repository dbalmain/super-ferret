//! Reading a catalogued document's bytes by DocId, for the content index's
//! follow pass (docs/S2.md § The content flow, § Coverage). `ferret-index`
//! never opens a file; its host hands it these bytes.
//!
//! A document is read only if it is still the version the catalog recorded
//! (D21, D33): every directory on its path is opened `O_NOFOLLOW` and must
//! have the catalogued `(dev, ino)`, the file is opened `O_NOFOLLOW` beneath
//! its parent's descriptor and must match the catalog's stat, and a second
//! `fstat` after the read must still match. Anything else is a
//! [`ContentFault`], which the index records as unreadable and never retries.
//!
//! [`Documents`] maps each DocId to one name by a single scan of the names,
//! built per pass. M5's daemon will want D30's lazy inverse instead; a full
//! scan is what M3's measurement and tests need.

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

use ferret_catalog::bulk::Limiter;
use ferret_catalog::{Catalog, DocId, InoId, NameId, OpenError, Section, Target};
use rustix::fs::{FileType, Mode, OFlags, fstat, open, openat};

use crate::ContentFault;
use crate::observe::{BulkRead, Opened, Reader, bracket};

/// One catalog view's documents, each with one of its names.
pub struct Documents {
    /// `name[doc]` is the name to read `doc` through, plus one; 0 for none.
    names: Vec<u32>,
    /// The last parent directory opened and checked, reused by siblings.
    dir: Option<(InoId, OwnedFd)>,
    limiter: Option<Arc<Limiter>>,
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
            limiter: None,
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
            limiter: None,
            files_read: 0,
            bytes_read: 0,
        })
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
        read_all(&mut file, recorded.size, self.limiter.as_deref(), out)?;
        bracket(&file, &recorded, ferret_catalog::Content::Binary)?;
        self.files_read += 1;
        self.bytes_read += out.len() as u64;
        Ok(())
    }

    /// A checked descriptor for directory `dir`: the cached one, or a fresh
    /// walk from its root.
    fn directory(&mut self, catalog: &Catalog, dir: InoId) -> Result<&OwnedFd, ContentFault> {
        if self.dir.as_ref().is_none_or(|(cached, _)| *cached != dir) {
            self.dir = None;
            let fd = open_directory(catalog, dir)?;
            self.dir = Some((dir, fd));
        }
        match &self.dir {
            Some((_, fd)) => Ok(fd),
            None => Err(ContentFault::Changed),
        }
    }
}

/// Opens `dir` from its root, one `O_NOFOLLOW` component at a time, each
/// checked against the catalog's `(dev, ino)`.
fn open_directory(catalog: &Catalog, dir: InoId) -> Result<OwnedFd, ContentFault> {
    let names = catalog.name_reader();
    let mut chain = Vec::new();
    let mut at = dir;
    while let Some(name) = catalog.dir_name(at) {
        let (parent, bytes) = names.edge(name);
        chain.push((at, bytes));
        at = parent;
    }
    let root = catalog
        .roots()
        .find(|&(id, _)| id == at)
        .map(|(_, path)| path)
        .ok_or(ContentFault::Changed)?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    // The root's own path is the user's; symlinks above it are theirs to
    // keep. Everything below it must not follow one.
    let mut fd = open(std::ffi::OsStr::from_bytes(root), flags, Mode::empty())
        .map_err(|e| ContentFault::Open(e.into()))?;
    check_directory(&fd, catalog, at)?;
    for (id, bytes) in chain.into_iter().rev() {
        fd = openat(
            &fd,
            std::ffi::OsStr::from_bytes(bytes),
            flags | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|e| ContentFault::Open(e.into()))?;
        check_directory(&fd, catalog, id)?;
    }
    Ok(fd)
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
    limiter: Option<&Limiter>,
    out: &mut Vec<u8>,
) -> Result<(), ContentFault> {
    out.reserve(usize::try_from(size).unwrap_or(0));
    let mut source = BulkRead {
        file,
        limiter,
        remaining: size,
    };
    loop {
        let start = out.len();
        out.resize(start + (256 << 10), 0);
        match source.read(&mut out[start..]) {
            Ok(0) => {
                out.truncate(start);
                return Ok(());
            }
            Ok(n) => out.truncate(start + n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => out.truncate(start),
            Err(e) => {
                out.truncate(start);
                return Err(ContentFault::Read(e));
            }
        }
    }
}
