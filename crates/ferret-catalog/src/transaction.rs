//! The single writer: one run of `ferret index` (D26, D32, D34, D37).
//!
//! ```text
//! begin(dir)   create dir, take dir/lock (refused if held), read the old
//!              generation and index it for carry-over
//! batch()      mint an empty Batch; any thread, any number
//! carry(stat)  the old content of an unchanged inode; any thread
//! add(batch)   hand a filled batch back
//! keep(root)   copy an old root forward unchanged
//! commit()     build, encode, self-check, write dir/catalog.tmp, fsync,
//!              rename over dir/catalog, fsync dir, release the lock
//! ```
//!
//! Roots are refreshed by adding batches that record them, and kept with
//! [`Transaction::keep`]. An old root that is neither is dropped: that is
//! `ferret roots remove`. Each root owns its directory rows and name edges;
//! file inodes and documents are global, and a file seen fresh anywhere
//! supersedes the copy a kept root carries (D34).
//!
//! Readers never lock: they read whichever file `dir/catalog` names when they
//! open it, and the rename is atomic (D32).

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::batch::{Batch, Content, Stat};
use crate::build::{self, BuildError, Known};
use crate::read::{self, Catalog, Kind, OpenError};
use crate::{ContentState, DecodeError, Hash, InoId};

const TEMP: &str = "catalog.tmp";
const LOCK: &str = "lock";

/// Why [`Transaction::begin`] failed.
#[derive(Debug)]
pub enum BeginError {
    /// Another writer holds the catalog's lock.
    Locked,
    /// Creating the directory or the lock file failed.
    Io(io::Error),
    /// The existing generation could not be read. Nothing can be carried or
    /// kept from it; removing the `catalog` file in the directory starts over.
    Previous(OpenError),
}

impl fmt::Display for BeginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Locked => write!(f, "another `ferret index` holds the catalog lock"),
            Self::Io(e) => write!(f, "opening the catalog directory: {e}"),
            Self::Previous(e) => write!(f, "reading the previous catalog: {e}"),
        }
    }
}

impl std::error::Error for BeginError {}

/// Why [`Transaction::keep`] refused a root.
#[derive(Debug, PartialEq, Eq)]
pub enum KeepError {
    /// The previous generation has no root with this path.
    UnknownRoot(Vec<u8>),
    /// The sniffer version changed, so every root must be refreshed (D37).
    SnifferChanged,
}

impl fmt::Display for KeepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownRoot(p) => write!(f, "no root {} to keep", String::from_utf8_lossy(p)),
            Self::SnifferChanged => write!(f, "the sniffer changed; every root must be refreshed"),
        }
    }
}

impl std::error::Error for KeepError {}

/// Why [`Transaction::commit`] failed. Only [`CommitError::Undurable`] means
/// the new generation was published; check [`CommitError::published`] rather
/// than assuming an error means nothing changed.
#[derive(Debug)]
pub enum CommitError {
    /// The batches were inconsistent. Nothing was published.
    Build(BuildError),
    /// The encoded generation failed its own decode check: a bug. Nothing
    /// was published.
    Encode(DecodeError),
    /// Writing, syncing or renaming the new file failed. Nothing was
    /// published; the previous generation is intact and the temp file is
    /// removed.
    Write(io::Error),
    /// The new generation was renamed into place, so readers may already see
    /// it, but syncing the directory failed: it may not survive a crash.
    Undurable(io::Error),
}

impl CommitError {
    /// Whether the new generation is visible to readers despite the error.
    pub fn published(&self) -> bool {
        matches!(self, Self::Undurable(_))
    }
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Build(e) => write!(f, "catalog not written: {e}"),
            Self::Encode(e) => write!(f, "catalog not written, it failed its own check: {e}"),
            Self::Write(e) => write!(f, "catalog not written: {e}"),
            Self::Undurable(e) => {
                write!(f, "catalog written, but syncing its directory failed: {e}")
            }
        }
    }
}

impl std::error::Error for CommitError {}

/// A write in progress. Holds the catalog's lock until it is committed or
/// dropped; dropping it publishes nothing.
pub struct Transaction {
    dir: PathBuf,
    _lock: File,
    sniffer: u32,
    previous: Option<Catalog>,
    /// Old file and symlink rows by `(dev, ino)`, for carry-over. Empty when
    /// the sniffer changed, since old classifications no longer hold.
    by_identity: HashMap<(u64, u64), InoId>,
    /// Old live documents by hash, so known content keeps its `DocId`.
    docs: HashMap<Hash, u32>,
    next_batch: AtomicU32,
    batches: Vec<Batch>,
}

impl Transaction {
    /// Starts a write to the catalog in `dir`, creating the directory if it
    /// is missing. `sniffer` is the current sniffer's version; when it
    /// differs from the previous generation's, nothing is carried and every
    /// root must be refreshed (D37).
    pub fn begin(dir: &Path, sniffer: u32) -> Result<Transaction, BeginError> {
        fs::create_dir_all(dir).map_err(BeginError::Io)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(LOCK))
            .map_err(BeginError::Io)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(BeginError::Locked),
            Err(TryLockError::Error(e)) => return Err(BeginError::Io(e)),
        }
        remove_temp(dir).map_err(BeginError::Io)?;
        let previous = Catalog::open(dir).map_err(BeginError::Previous)?;

        let mut by_identity = HashMap::new();
        let mut docs = HashMap::new();
        if let Some(old) = &previous {
            if old.sniffer_version() == sniffer {
                by_identity.reserve((old.inode_count() - old.dir_count()) as usize);
                for id in old.dir_count()..old.inode_count() {
                    let stat = old.inode(InoId(id)).stat;
                    by_identity.insert((stat.dev, stat.ino), InoId(id));
                }
            }
            docs.reserve(old.doc_count() as usize);
            docs.extend(old.docs().map(|(doc, hash)| (hash, doc.0)));
        }
        Ok(Transaction {
            dir: dir.to_owned(),
            _lock: lock,
            sniffer,
            previous,
            by_identity,
            docs,
            next_batch: AtomicU32::new(0),
            batches: Vec::new(),
        })
    }

    /// The generation this transaction replaces, if any: its roots, and
    /// anything else a caller wants to compare against.
    pub fn previous(&self) -> Option<&Catalog> {
        self.previous.as_ref()
    }

    /// A new, empty batch for one worker.
    pub fn batch(&self) -> Batch {
        Batch::new(self.next_batch.fetch_add(1, Ordering::Relaxed), false)
    }

    /// The previous generation's content for this inode when `stat` matches
    /// its old row on `(dev, ino, size, mtime, ctime)` (D26): its hash, or
    /// that it was binary or unindexed, so it need not be read again. `None`
    /// when it changed, is new, faulted last time, or the sniffer changed.
    ///
    /// The caller still applies current policy: an old `Unindexed` for a
    /// file the policy now sends to the index must be read (D37).
    pub fn carry(&self, stat: &Stat) -> Option<Content> {
        let old = self.previous.as_ref()?;
        let &id = self.by_identity.get(&(stat.dev, stat.ino))?;
        let inode = old.inode(id);
        if !inode.stat.same_version(stat) {
            return None;
        }
        match inode.state {
            ContentState::Unindexed => Some(Content::Unindexed),
            ContentState::Binary => Some(Content::Binary),
            ContentState::Hashed => inode
                .doc
                .and_then(|doc| old.doc_hash(doc))
                .map(Content::Hashed),
            ContentState::Fault => None,
        }
    }

    /// Hands back a filled batch.
    pub fn add(&mut self, batch: Batch) {
        self.batches.push(batch);
    }

    /// Copies the previous generation's root `path`, and everything under
    /// it, into this one unchanged. Its files yield to any fresh observation
    /// of the same inode.
    pub fn keep(&mut self, path: &[u8]) -> Result<(), KeepError> {
        let unknown = || KeepError::UnknownRoot(path.to_vec());
        let old = self.previous.as_ref().ok_or_else(unknown)?;
        if old.sniffer_version() != self.sniffer {
            return Err(KeepError::SnifferChanged);
        }
        let (root, _) = old.roots().find(|&(_, p)| p == path).ok_or_else(unknown)?;
        let mut batch = Batch::new(self.next_batch.fetch_add(1, Ordering::Relaxed), true);
        let token = batch.root(path, old.inode(root).stat);
        let mut queue = vec![(root, token)];
        while let Some((dir, token)) = queue.pop() {
            if let Some(wt) = old.work_tree(dir) {
                batch.work_tree(token, wt.kind, wt.common_dir, wt.common_id);
            }
            for name_id in old.children(dir) {
                let name = old.name(name_id);
                let inode = old.inode(name.child);
                match old.kind(name.child) {
                    Kind::Dir if old.is_traversed(name.child) => {
                        queue.push((
                            name.child,
                            batch.traversed_dir(token, name.bytes, inode.stat),
                        ));
                    }
                    Kind::Dir => queue.push((name.child, batch.dir(token, name.bytes, inode.stat))),
                    Kind::Symlink => {
                        let target = old.link_target(name.child).unwrap_or_default();
                        batch.symlink(token, name.bytes, inode.stat, target);
                    }
                    Kind::File => {
                        let content = match inode.state {
                            ContentState::Unindexed => Content::Unindexed,
                            ContentState::Binary => Content::Binary,
                            ContentState::Hashed => inode
                                .doc
                                .and_then(|doc| old.doc_hash(doc))
                                .map_or(Content::Fault, Content::Hashed),
                            ContentState::Fault => Content::Fault,
                        };
                        batch.file(token, name.bytes, inode.stat, content);
                    }
                }
            }
        }
        self.batches.push(batch);
        Ok(())
    }

    /// Builds the new generation and publishes it: write a temp file, fsync
    /// it, rename it over the old one, fsync the directory. Returns the new
    /// generation, already open. The lock is released either way.
    pub fn commit(mut self) -> Result<Catalog, CommitError> {
        let known = Known {
            docs: &self.docs,
            next_doc: self.previous.as_ref().map_or(0, |old| old.next_doc().0),
        };
        let tables =
            build::build(&self.batches, self.sniffer, known).map_err(CommitError::Build)?;
        self.batches = Vec::new();
        let bytes = crate::format::encode(&tables);
        drop(tables);
        let catalog = Catalog::from_bytes(bytes).map_err(CommitError::Encode)?;

        let temp = self.dir.join(TEMP);
        let written = write_synced(&temp, catalog.bytes())
            .and_then(|()| fs::rename(&temp, self.dir.join(read::FILE)));
        if let Err(e) = written {
            // Best effort: the error that matters is the write's.
            let _ = remove_temp(&self.dir);
            return Err(CommitError::Write(e));
        }
        sync_dir(&self.dir).map_err(CommitError::Undurable)?;
        Ok(catalog)
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_SYNC_DIR.get() {
        return Err(io::Error::other("injected directory fsync failure"));
    }
    File::open(dir)?.sync_all()
}

fn remove_temp(dir: &Path) -> io::Result<()> {
    match fs::remove_file(dir.join(TEMP)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
thread_local! {
    /// Test seam: makes the directory fsync after the rename fail, the one
    /// commit step no unprivileged test can make fail for real.
    pub(crate) static FAIL_SYNC_DIR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
