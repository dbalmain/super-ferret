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
use std::io::{self, Read};
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::sync::{Arc, Mutex};

use ferret_catalog::bulk::Limiter;
use ferret_catalog::{Catalog, DocId, NameId, OpenError, Section, Target};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, fstat, open, openat, statat};

use crate::ContentFault;
use crate::observe::{BulkRead, Opened, Reader, bracket, bracket_search, catalog_stat};

mod held;

use held::Held;

/// Streams an explicit regular file or live-fallback file, using the same
/// current-version bracket as catalogued candidates. `None` imposes no policy
/// size cap. Zero reported size selects an EOF read so virtual-file content
/// is visible; nonzero sizes bound reads. Both retain the closing fstat against
/// the opening version, even after an early stop. The caller owns retry and
/// publication of buffered output.
pub fn stream_file_until(
    path: &std::path::Path,
    cap: Option<u64>,
    each: &mut dyn FnMut(&mut dyn Read, u64) -> io::Result<()>,
    cancelled: &dyn Fn() -> bool,
) -> Result<ferret_catalog::Stat, ContentFault> {
    stream_file_until_with_growth(
        Named::follow(rustix::fs::CWD, path.as_os_str()),
        cap,
        each,
        cancelled,
        false,
    )
}

/// Search-only form of [`stream_file_until`]. The caller publishes the
/// opening-size prefix only after this function accepts its closing stat.
pub fn stream_search_file_until(
    path: &std::path::Path,
    cap: Option<u64>,
    each: &mut dyn FnMut(&mut dyn Read, u64) -> io::Result<()>,
    cancelled: &dyn Fn() -> bool,
) -> Result<ferret_catalog::Stat, ContentFault> {
    stream_file_until_with_growth(
        Named::follow(rustix::fs::CWD, path.as_os_str()),
        cap,
        each,
        cancelled,
        true,
    )
}

/// [`stream_search_file_until`] for a name a walk listed in `directory`,
/// opened beneath that descriptor without following a symlink, and checked
/// by that same name, no-follow, at the close. A name swapped for a symlink
/// after the listing reads as changed rather than through the link (D21).
pub fn stream_search_listed_until(
    directory: BorrowedFd<'_>,
    name: &std::ffi::OsStr,
    cap: Option<u64>,
    each: &mut dyn FnMut(&mut dyn Read, u64) -> io::Result<()>,
    cancelled: &dyn Fn() -> bool,
) -> Result<ferret_catalog::Stat, ContentFault> {
    let named = Named {
        directory,
        name,
        open: OFlags::NOFOLLOW,
        check: AtFlags::SYMLINK_NOFOLLOW,
    };
    stream_file_until_with_growth(named, cap, each, cancelled, true)
}

/// [`stream_search_listed_until`] without the changed-during-read check: reads
/// the opening size once and passes whatever it read, as rg does. For
/// uncovered rg-mode scans only (D123), which answer for the file as it was
/// opened; the check would retry, then fail, a file that shrank or was
/// replaced during the read.
pub fn stream_listed_once_until(
    directory: BorrowedFd<'_>,
    name: &std::ffi::OsStr,
    each: &mut dyn FnMut(&mut dyn Read, u64) -> io::Result<()>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), ContentFault> {
    let named = Named {
        directory,
        name,
        open: OFlags::NOFOLLOW,
        check: AtFlags::SYMLINK_NOFOLLOW,
    };
    stream_once(&named, each, cancelled)
}

/// [`stream_listed_once_until`] for a path, following symlinks: an uncovered
/// scan under `-L`.
pub fn stream_file_once_until(
    path: &std::path::Path,
    each: &mut dyn FnMut(&mut dyn Read, u64) -> io::Result<()>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), ContentFault> {
    stream_once(
        &Named::follow(rustix::fs::CWD, path.as_os_str()),
        each,
        cancelled,
    )
}

fn stream_once(
    named: &Named<'_>,
    each: &mut dyn FnMut(&mut dyn Read, u64) -> io::Result<()>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), ContentFault> {
    let (mut file, current) = open_named(named, None, cancelled)?;
    pass_extent(&mut file, current.size, each, cancelled)
}

/// A file to open by `name` beneath `directory`, with the symlink policy
/// for its open and for its closing name check, which must agree.
struct Named<'a> {
    directory: BorrowedFd<'a>,
    name: &'a std::ffi::OsStr,
    open: OFlags,
    check: AtFlags,
}

impl<'a> Named<'a> {
    /// An explicit operand's path, following symlinks throughout.
    fn follow(directory: BorrowedFd<'a>, name: &'a std::ffi::OsStr) -> Self {
        Self {
            directory,
            name,
            open: OFlags::empty(),
            check: AtFlags::empty(),
        }
    }
}

fn stream_file_until_with_growth(
    named: Named<'_>,
    cap: Option<u64>,
    each: &mut dyn FnMut(&mut dyn Read, u64) -> io::Result<()>,
    cancelled: &dyn Fn() -> bool,
    accept_growth: bool,
) -> Result<ferret_catalog::Stat, ContentFault> {
    let (file, current) = open_named(&named, cap, cancelled)?;
    checked_file(
        file,
        current,
        accept_growth,
        &mut |first| check_read_name(named.directory, named.name, named.check, first),
        &mut |file, size| pass_extent(file, size, each, cancelled),
    )
}

/// Opens `named` as a regular file no larger than `cap`.
fn open_named(
    named: &Named<'_>,
    cap: Option<u64>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(File, ferret_catalog::Stat), ContentFault> {
    if cancelled() {
        return Err(ContentFault::Cancelled);
    }
    let fd = openat(
        named.directory,
        named.name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | named.open,
        Mode::empty(),
    )
    .map_err(|error| match error {
        // A single no-follow name that is now a symlink: not the regular
        // file its directory listed, so changed, like any other type.
        rustix::io::Errno::LOOP if named.open.contains(OFlags::NOFOLLOW) => ContentFault::Changed,
        _ => ContentFault::Open(error.into()),
    })?;
    let (file, current) = regular_file(fd)?;
    if let Some(cap) = cap
        && current.size > cap
    {
        return Err(ContentFault::TooLarge {
            size: current.size,
            cap,
        });
    }
    Ok((file, current))
}

/// Passes the opening `size` of `file`, or all of it when that was zero, as
/// a virtual file's is.
fn pass_extent(
    file: &mut File,
    size: u64,
    each: &mut dyn FnMut(&mut dyn Read, u64) -> io::Result<()>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), ContentFault> {
    let extent = if size == 0 { u64::MAX } else { size };
    each(&mut Read::take(file, extent), extent).map_err(|error| {
        if cancelled() {
            ContentFault::Cancelled
        } else {
            ContentFault::Read(error)
        }
    })
}

/// Validate the open descriptor, rather than path metadata that may already
/// name another inode. Shared by sized transactions and direct virtual streams.
fn regular_file(fd: OwnedFd) -> Result<(File, ferret_catalog::Stat), ContentFault> {
    let stat = fstat(&fd).map_err(|error| ContentFault::Stat(error.into()))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(ContentFault::Changed);
    }
    Ok((File::from(fd), catalog_stat(&stat)))
}

fn checked_file(
    mut file: File,
    current: ferret_catalog::Stat,
    accept_growth: bool,
    check_name: &mut dyn FnMut(&ferret_catalog::Stat) -> Result<(), ContentFault>,
    pass: &mut dyn FnMut(&mut File, u64) -> Result<(), ContentFault>,
) -> Result<ferret_catalog::Stat, ContentFault> {
    pass(&mut file, current.size)?;
    if accept_growth {
        let after = bracket_search(&file, &current)?;
        check_name(&current)?;
        Ok(after)
    } else {
        bracket(&file, &current, ferret_catalog::Content::Binary)?;
        Ok(current)
    }
}

/// A descriptor's nonzero link count can belong to an alias after this name
/// was replaced. Search accepts ctime changes, so it needs this identity check
/// in addition to the descriptor bracket; strict reads reject those changes.
fn check_read_name(
    parent: BorrowedFd<'_>,
    name: &std::ffi::OsStr,
    flags: AtFlags,
    first: &ferret_catalog::Stat,
) -> Result<(), ContentFault> {
    let named = statat(parent, name, flags).map_err(|error| match error {
        rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR => ContentFault::Changed,
        _ => ContentFault::Stat(error.into()),
    })?;
    if (named.st_dev, named.st_ino) == (first.dev, first.ino) {
        Ok(())
    } else {
        Err(ContentFault::Changed)
    }
}

/// One catalog view's documents, each with one of its names.
pub struct Documents {
    /// `name[doc]` is the name to read `doc` through, plus one; 0 for none.
    /// Shared with forks.
    names: Arc<[u32]>,
    /// With [`Documents::holding_directories`]: the last checked walk, root
    /// first, each directory with its descriptor, kept for the next path.
    /// Locked while this reader opens through it, and otherwise free for a
    /// reader out of descriptors to empty ([`held::Budget::relieve`]).
    held: Option<Arc<Mutex<Held>>>,
    /// Called once this reader has locked its held walk to open through it.
    #[cfg(test)]
    pause: Option<Arc<dyn Fn() + Send + Sync>>,
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
            names: names.into(),
            held: None,
            #[cfg(test)]
            pause: None,
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
            names: Arc::new([]),
            held: None,
            #[cfg(test)]
            pause: None,
            limiter: None,
            config: ferret_policy::Config::default(),
            files_read: 0,
            bytes_read: 0,
        })
    }

    /// Another reader with this one's documents, policy and limiter, for a
    /// second thread, without loading again. Counts start at zero, and it
    /// holds directories if this one does, from the same budget, holding
    /// none yet.
    pub fn fork(&self) -> Self {
        Self {
            names: self.names.clone(),
            held: self.held.as_deref().map(Held::sibling),
            #[cfg(test)]
            pause: None,
            limiter: self.limiter.clone(),
            config: self.config,
            files_read: 0,
            bytes_read: 0,
        }
    }

    /// Uses this content size cap. Indexing and structured search retain the
    /// default 8 MiB policy; grep removes the cap for its checked live reads.
    pub fn with_config(mut self, config: ferret_policy::Config) -> Self {
        self.config = config;
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
        let name = std::ffi::OsStr::from_bytes(bytes);
        let mut file = self.in_directory(catalog, parent, &|| false, |dir| {
            match Reader::open(dir, name, &recorded) {
                Opened::Ready { file, .. } => Ok(file),
                Opened::Fault(fault) => Err(fault),
            }
        })?;
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
        let stat = self.in_directory(catalog, parent, &|| false, |dir| {
            statat(
                dir,
                std::ffi::OsStr::from_bytes(bytes),
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(|e| ContentFault::Stat(e.into()))
        })?;
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
        self.read_current_name_with_growth(catalog, name, out, cancelled, false)
    }

    /// Search-only version of [`Documents::read_current_name_until`].
    pub fn read_search_name_until(
        &mut self,
        catalog: &Catalog,
        name: NameId,
        out: &mut Vec<u8>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ferret_catalog::Stat, ContentFault> {
        self.read_current_name_with_growth(catalog, name, out, cancelled, true)
    }

    fn read_current_name_with_growth(
        &mut self,
        catalog: &Catalog,
        name: NameId,
        out: &mut Vec<u8>,
        cancelled: &dyn Fn() -> bool,
        accept_growth: bool,
    ) -> Result<ferret_catalog::Stat, ContentFault> {
        out.clear();
        let (cap, limiter) = (self.config.size_cap, self.limiter.clone());
        let current = self.current_until(
            catalog,
            name,
            cancelled,
            accept_growth,
            &mut |file, size| {
                out.clear();
                read_all(file, size, cap, limiter.as_deref(), out, cancelled)
            },
        );
        if current.is_ok() {
            self.bytes_read += out.len() as u64;
        }
        current
    }

    /// [`Documents::read_current_name_until`], handing the bytes to `each`
    /// as a reader instead of collecting them: `each` reads as much as it
    /// needs, with the document's size, and should stop between reads once
    /// `cancelled` says so, failing. `each` is called once, and only for a
    /// regular file.
    pub fn stream_current_name_until(
        &mut self,
        catalog: &Catalog,
        name: NameId,
        each: &mut dyn FnMut(&mut dyn Read, u64) -> io::Result<()>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ferret_catalog::Stat, ContentFault> {
        self.stream_current_name_with_growth(catalog, name, each, cancelled, false)
    }

    /// Search-only version of [`Documents::stream_current_name_until`].
    pub fn stream_search_name_until(
        &mut self,
        catalog: &Catalog,
        name: NameId,
        each: &mut dyn FnMut(&mut dyn Read, u64) -> io::Result<()>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ferret_catalog::Stat, ContentFault> {
        self.stream_current_name_with_growth(catalog, name, each, cancelled, true)
    }

    fn stream_current_name_with_growth(
        &mut self,
        catalog: &Catalog,
        name: NameId,
        each: &mut dyn FnMut(&mut dyn Read, u64) -> io::Result<()>,
        cancelled: &dyn Fn() -> bool,
        accept_growth: bool,
    ) -> Result<ferret_catalog::Stat, ContentFault> {
        let (cap, limiter) = (self.config.size_cap, self.limiter.clone());
        self.current_until(
            catalog,
            name,
            cancelled,
            accept_growth,
            &mut |file, size| {
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
            },
        )
    }

    /// The current version of the file `name` names, read by `pass` (given
    /// the file at its start and the size to read), bracketed by two stats
    /// of the open file: the first must show a regular file before `pass`
    /// sees a byte, and the second the same version as the first.
    ///
    /// The calls, for a file in a held directory: `openat`, `fstat`, the
    /// reads, `fstat`, and (for search) one no-follow `fstatat`, then `close`.
    fn current_until(
        &mut self,
        catalog: &Catalog,
        name: NameId,
        cancelled: &dyn Fn() -> bool,
        accept_growth: bool,
        pass: &mut dyn FnMut(&mut File, u64) -> Result<(), ContentFault>,
    ) -> Result<ferret_catalog::Stat, ContentFault> {
        if cancelled() {
            return Err(ContentFault::Cancelled);
        }
        let (parent, bytes) = catalog.name_reader().edge(name);
        let current = self.in_directory(catalog, parent, cancelled, |dir| {
            let fd = openat(
                dir,
                std::ffi::OsStr::from_bytes(bytes),
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| ContentFault::Open(e.into()))?;
            let (file, current) = regular_file(fd)?;
            // Keep the checked parent alive through the read without a dup.
            // Its held-descriptor lock also keeps pressure relief from closing
            // it.
            checked_file(
                file,
                current,
                accept_growth,
                &mut |first| {
                    check_read_name(
                        dir,
                        std::ffi::OsStr::from_bytes(bytes),
                        AtFlags::SYMLINK_NOFOLLOW,
                        first,
                    )
                },
                pass,
            )
        })?;
        self.files_read += 1;
        Ok(current)
    }
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
