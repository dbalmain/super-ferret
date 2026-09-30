//! The single writer: one run of `ferret index` (D26, D32, D34, D37).
//!
//! ```text
//! begin(dir)   create dir, take dir/lock (refused if held); before a
//!              first generation, fsync every ancestor of dir; read the old
//!              generation and index it for carry-over
//! batch()      mint an empty Batch; any thread, any number
//! carry(stat)  the old content of an unchanged inode; any thread
//! add(batch)   hand a filled batch back
//! keep(root)   copy an old root forward unchanged
//! commit()     plan, stream dir/catalog.tmp, fsync, read it back and
//!              self-check, rename over dir/catalog, fsync dir, release the
//!              lock
//! ```
//!
//! Roots are refreshed by adding batches that record them, and kept with
//! [`Transaction::keep`]. An old root that is neither is dropped: that is
//! `ferret roots remove`. Each root owns its directory rows and name edges;
//! file inodes and documents are global, and a file seen fresh anywhere
//! supersedes the copy a kept root carries (D34).
//!
//! A kept root is copied as it was, so it is only valid while the roots
//! nested inside it are unchanged: the old walk stopped at each old inner
//! root and descended into everything else. Adding or removing a root strictly
//! inside a kept one would duplicate or lose that subtree, so commit refuses
//! it; the outer root must be refreshed in the same transaction.
//!
//! Readers never lock: they read whichever file `dir/catalog` names when they
//! open it, and the rename is atomic (D32).

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufWriter};
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
    /// A root strictly inside the kept root `kept` was added or removed by
    /// this transaction, so the kept copy's boundaries are stale: refresh
    /// `kept` instead (D34). Nothing was published.
    KeptRootOverlaps { kept: Vec<u8>, changed: Vec<u8> },
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
            Self::KeptRootOverlaps { kept, changed } => write!(
                f,
                "catalog not written: root {} was added or removed inside kept root {}; \
                 refresh {} instead",
                String::from_utf8_lossy(changed),
                String::from_utf8_lossy(kept),
                String::from_utf8_lossy(kept),
            ),
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
    /// Held for the transaction's life and released explicitly on drop; see
    /// the `Drop` impl.
    lock: File,
    sniffer: u32,
    previous: Option<Catalog>,
    /// Old file and symlink rows sorted by `(dev, ino)`, for carry-over:
    /// 4 B per inode where a map measured 40 (D40). Empty when the sniffer
    /// changed, since old classifications no longer hold.
    by_identity: Vec<u32>,
    /// Old live documents as `(hash, DocId)` sorted by hash, so known content
    /// keeps its `DocId`.
    docs: Vec<(Hash, u32)>,
    next_batch: AtomicU32,
    batches: Vec<Batch>,
    /// Paths passed to [`Transaction::keep`], checked against the root
    /// changes at commit.
    kept: Vec<Vec<u8>>,
}

impl Transaction {
    /// Starts a write to the catalog in `dir`, creating the directory if it
    /// is missing. Before a first generation, the writer syncs every
    /// ancestor of `dir` under the lock: a directory's name is an entry in
    /// its parent, and whoever created the directories (this writer, or one
    /// that lost the lock race before syncing) cannot be trusted to have
    /// made them durable. That is one fsync per level, once per index.
    /// `sniffer` is the current
    /// sniffer's version; when it differs from the previous generation's,
    /// nothing is carried and every root must be refreshed (D37).
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
        if previous.is_none() {
            sync_ancestors(dir).map_err(BeginError::Io)?;
        }
        if let Some(old) = &previous {
            old.load_all().map_err(BeginError::Previous)?;
        }

        let mut by_identity = Vec::new();
        let mut docs = Vec::new();
        if let Some(old) = &previous {
            if old.sniffer_version() == sniffer {
                by_identity.extend(old.dir_count()..old.inode_count());
                by_identity.sort_unstable_by_key(|&id| identity(old, id));
            }
            docs.reserve(old.doc_count() as usize);
            docs.extend(old.docs().map(|(doc, hash)| (hash, doc.0)));
            docs.sort_unstable();
        }
        Ok(Transaction {
            dir: dir.to_owned(),
            lock,
            sniffer,
            previous,
            by_identity,
            docs,
            next_batch: AtomicU32::new(0),
            batches: Vec::new(),
            kept: Vec::new(),
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
        let at = self
            .by_identity
            .binary_search_by_key(&(stat.dev, stat.ino), |&id| identity(old, id))
            .ok()?;
        let inode = old.inode(InoId(self.by_identity[at]));
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
            if let Some(count) = old.entry_count(dir) {
                batch.entry_count(token, count);
            }
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
        self.kept.push(path.to_vec());
        Ok(())
    }

    /// Refuses a kept root with a root added or removed strictly inside it.
    fn check_kept_roots(&self) -> Result<(), CommitError> {
        let Some(old) = &self.previous else {
            return Ok(());
        };
        let fresh = self.batches.iter().filter(|b| !b.carried).flat_map(|b| {
            b.dirs
                .iter()
                .filter(|d| d.parent.is_none())
                .map(|d| d.name.of(&b.names))
        });
        let new: HashSet<&[u8]> = fresh.chain(self.kept.iter().map(Vec::as_slice)).collect();
        let before: HashSet<&[u8]> = old.roots().map(|(_, path)| path).collect();
        for changed in new.symmetric_difference(&before) {
            if let Some(kept) = self.kept.iter().find(|k| is_inside(changed, k)) {
                return Err(CommitError::KeptRootOverlaps {
                    kept: kept.clone(),
                    changed: changed.to_vec(),
                });
            }
        }
        Ok(())
    }

    /// Builds the new generation and publishes it: stream it to a temp
    /// file, fsync it, read it back and decode it as a self-check, rename it
    /// over the old one, fsync the directory. Returns the new generation,
    /// already open. The lock is released either way.
    ///
    /// Every [`BuildError`] is found before the temp file is created. The
    /// batches are freed while the file is written, before it is read back,
    /// so the batches and the encoded file are never held at once (D40).
    pub fn commit(mut self) -> Result<Catalog, CommitError> {
        self.check_kept_roots()?;
        // The old generation is done with once the batches are filled; only
        // its documents and id counter reach the build.
        let next_doc = self.previous.take().map_or(0, |old| old.next_doc().0);
        self.by_identity = Vec::new();
        let known = Known {
            docs: &self.docs,
            next_doc,
        };
        let plan = build::plan(&self.batches, self.sniffer, known).map_err(CommitError::Build)?;
        let batches = std::mem::take(&mut self.batches);
        self.docs = Vec::new();

        let temp = self.dir.join(TEMP);
        let checked = write_synced(&temp, |out| build::write(plan, batches, out))
            .map_err(CommitError::Write)
            .and_then(|()| fs::read(&temp).map_err(CommitError::Write))
            .and_then(|bytes| Catalog::from_bytes(bytes).map_err(CommitError::Encode))
            .and_then(|catalog| {
                fs::rename(&temp, self.dir.join(read::FILE)).map_err(CommitError::Write)?;
                Ok(catalog)
            });
        let catalog = match checked {
            Ok(catalog) => catalog,
            Err(e) => {
                // Best effort: the error that matters is the first.
                let _ = remove_temp(&self.dir);
                return Err(e);
            }
        };
        sync_dir(&self.dir).map_err(CommitError::Undurable)?;
        Ok(catalog)
    }
}

/// Releases the lock with `LOCK_UN` rather than by closing the file. `flock`
/// belongs to the open file description, which a child forked by any thread
/// shares until it execs; closing only this process's descriptor then leaves
/// the lock held, and the next `begin` sees `Locked`. Seen in the crawl tests,
/// where other tests spawn `git` and `chmod` (slice 4). Unlocking releases it
/// for every copy.
impl Drop for Transaction {
    fn drop(&mut self) {
        // Nothing to do if it fails: closing the file is the fallback.
        let _ = self.lock.unlock();
    }
}

#[cfg(test)]
impl Transaction {
    /// Test seam: a second descriptor for the lock's open file description,
    /// as a forked child would hold.
    pub(crate) fn lock_copy(&self) -> File {
        self.lock.try_clone().expect("dup the lock descriptor")
    }
}

/// Whether root path `inner` lies strictly inside root path `outer`.
fn is_inside(inner: &[u8], outer: &[u8]) -> bool {
    inner.len() > outer.len()
        && inner.starts_with(outer)
        && (outer.ends_with(b"/") || inner[outer.len()] == b'/')
}

/// A file inode's `(dev, ino)`, the carry-over key.
fn identity(old: &Catalog, id: u32) -> (u64, u64) {
    let stat = old.inode(InoId(id)).stat;
    (stat.dev, stat.ino)
}

fn write_synced(
    path: &Path,
    write: impl FnOnce(&mut BufWriter<File>) -> io::Result<()>,
) -> io::Result<()> {
    let mut out = BufWriter::with_capacity(1 << 20, File::create(path)?);
    write(&mut out)?;
    let file = out.into_inner().map_err(io::IntoInnerError::into_error)?;
    file.sync_all()
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_SYNC_DIR.get() {
        return Err(io::Error::other("injected directory fsync failure"));
    }
    #[cfg(test)]
    SYNCED_DIRS.with_borrow_mut(|synced| synced.push(dir.to_owned()));
    File::open(dir)?.sync_all()
}

/// Syncs every ancestor of `dir`, from its parent up to `/`, so the chain of
/// entries naming it is on disk. Resolved first, so a symlinked component
/// syncs the directory that actually holds the entry.
fn sync_ancestors(dir: &Path) -> io::Result<()> {
    let real = fs::canonicalize(dir)?;
    real.ancestors().skip(1).try_for_each(sync_dir)
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
    /// Test seam: every directory synced on this thread, in order.
    pub(crate) static SYNCED_DIRS: std::cell::RefCell<Vec<PathBuf>> =
        const { std::cell::RefCell::new(Vec::new()) };
}
