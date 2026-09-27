//! Reading one file's content: the D26 carry-over check, the D33 stat
//! bracket, the sniff and the hash, and the D31 per-run cache that makes each
//! multiply-linked inode read once.
//!
//! The cache never makes a worker wait. The first worker to reach an inode
//! claims it and reads it; an alias that arrives while the claim is open
//! returns [`Lookup::InFlight`], and its caller records the name after the walk
//! from the finished observation. A worker therefore never blocks inside
//! `visit`, so no set of claims can deadlock the pool, and a worker that
//! panics mid-read strands nobody: its claim is simply never consumed, and the
//! panic ends the walk.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use ferret_catalog::{Content, Hash, Stat};
use rustix::fs::{FileType, OFlags, fstat, openat};

/// Why a file the policy sent to the index was published unhashed (D26).
#[derive(Debug)]
pub enum ContentFault {
    /// `openat` through the parent's descriptor failed.
    Open(io::Error),
    /// `fstat` of the open file failed.
    Stat(io::Error),
    /// Reading the file failed.
    Read(io::Error),
    /// The open file was not the version the walk statted, or changed while
    /// it was read (the D33 bracket).
    Changed,
    /// The names of this inode disagreed: another name was read this run at
    /// a version that differs from this name's `lstat`, or faulted, or
    /// carried a different version. The catalog keeps one row per inode, so
    /// every name publishes unhashed (D31). The build decides this; the
    /// report lists every name of an inode it published as a fault.
    Alias,
}

impl std::fmt::Display for ContentFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open(e) => write!(f, "open: {e}"),
            Self::Stat(e) => write!(f, "fstat: {e}"),
            Self::Read(e) => write!(f, "read: {e}"),
            Self::Changed => f.write_str("changed while it was read"),
            Self::Alias => f.write_str("another name of this inode saw a different version"),
        }
    }
}

/// One observation of an inode's content, stored whole: the stat it was
/// taken under and what was found. A fault is stored too, so an alias
/// consumes it rather than reading again.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Observation {
    pub(crate) stat: Stat,
    /// `None` is a content fault.
    pub(crate) content: Option<Content>,
}

enum Slot {
    InFlight,
    Done(Observation),
}

/// What the cache holds for an inode.
pub(crate) enum Lookup {
    /// Nobody has it: the caller now holds the claim and must
    /// [`Cache::complete`] it.
    Claimed,
    /// Another worker is reading it.
    InFlight,
    /// Read earlier this run.
    Done(Observation),
}

const SHARDS: usize = 64;

/// The D31 per-run observation cache, keyed by `(dev, ino)` and sharded so
/// workers rarely share a lock. Holds only inodes read this run that have
/// more than one link.
pub(crate) struct Cache {
    shards: Vec<Mutex<HashMap<(u64, u64), Slot>>>,
    /// Deferred aliases not yet recorded, across the run's workers, and
    /// the most there have been at once.
    backlog: AtomicI64,
    backlog_peak: AtomicI64,
}

impl Cache {
    pub(crate) fn new() -> Self {
        Self {
            shards: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
            backlog: AtomicI64::new(0),
            backlog_peak: AtomicI64::new(0),
        }
    }

    fn shard(&self, key: (u64, u64)) -> &Mutex<HashMap<(u64, u64), Slot>> {
        let mixed = (key.1 ^ key.0.rotate_left(32)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        &self.shards[(mixed >> 58) as usize % SHARDS]
    }

    /// Claims `key` for reading, or reports who has it.
    pub(crate) fn claim(&self, key: (u64, u64)) -> Lookup {
        let mut shard = lock(self.shard(key));
        match shard.entry(key) {
            Entry::Vacant(slot) => {
                slot.insert(Slot::InFlight);
                Lookup::Claimed
            }
            Entry::Occupied(slot) => match slot.get() {
                Slot::InFlight => Lookup::InFlight,
                Slot::Done(observation) => Lookup::Done(*observation),
            },
        }
    }

    /// Stores the claimed inode's observation.
    pub(crate) fn complete(&self, key: (u64, u64), observation: Observation) {
        lock(self.shard(key)).insert(key, Slot::Done(observation));
    }

    /// The finished observation of `key`, once the walk is over. `None` if
    /// its claim was never completed, which only a panicked worker leaves.
    pub(crate) fn finished(&self, key: (u64, u64)) -> Option<Observation> {
        match lock(self.shard(key)).get(&key) {
            Some(Slot::Done(observation)) => Some(*observation),
            _ => None,
        }
    }

    /// Counts `change` aliases into (positive) or out of the deferred
    /// backlog.
    pub(crate) fn deferred(&self, change: i64) {
        let now = self.backlog.fetch_add(change, Ordering::Relaxed) + change;
        self.backlog_peak.fetch_max(now, Ordering::Relaxed);
    }

    /// The largest the deferred backlog has been.
    pub(crate) fn deferred_peak(&self) -> u64 {
        self.backlog_peak.load(Ordering::Relaxed).max(0) as u64
    }

    /// Inodes held, for the report.
    pub(crate) fn len(&self) -> usize {
        self.shards.iter().map(|s| lock(s).len()).sum()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A poisoned shard means a worker panicked while holding it; the panic
    // ends the walk anyway, and the map itself is never left half-written.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What an alias records, given the stored observation and its own `lstat`:
/// the observation whole when the versions agree, and a fault under its own
/// stat when they do not, which the build turns into a fault for the shared
/// inode whichever name it keeps (D31).
pub(crate) fn consume(stored: Observation, own: &Stat) -> (Stat, Result<Content, ContentFault>) {
    if stored.stat.same_version(own) {
        (stored.stat, stored.content.ok_or(ContentFault::Alias))
    } else {
        (*own, Err(ContentFault::Alias))
    }
}

/// A worker's reusable read buffer and counters.
pub(crate) struct Reader {
    buffer: Vec<u8>,
    pub(crate) files_read: u64,
    pub(crate) bytes_read: u64,
    /// Time spent in [`Reader::read`]: sniffing and hashing.
    pub(crate) read_time: Duration,
}

/// How much one `read` asks for.
const CHUNK: usize = 256 << 10;

/// The outcome of opening a file for reading.
pub(crate) enum Opened {
    /// Open and statted; the stat matched the walk's.
    Ready {
        file: File,
        links: u64,
    },
    Fault(ContentFault),
}

impl Reader {
    pub(crate) fn new() -> Self {
        Self {
            buffer: vec![0; CHUNK],
            files_read: 0,
            bytes_read: 0,
            read_time: Duration::ZERO,
        }
    }

    /// Opens `name` in `parent` and checks, with `fstat`, that it is the
    /// version the walk statted.
    pub(crate) fn open(parent: BorrowedFd<'_>, name: &std::ffi::OsStr, walked: &Stat) -> Opened {
        let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = match openat(parent, name, flags, rustix::fs::Mode::empty()) {
            Ok(fd) => fd,
            Err(e) => return Opened::Fault(ContentFault::Open(e.into())),
        };
        let stat = match fstat(&fd) {
            Ok(stat) => stat,
            Err(e) => return Opened::Fault(ContentFault::Stat(e.into())),
        };
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
            || !catalog_stat(&stat).same_version(walked)
        {
            return Opened::Fault(ContentFault::Changed);
        }
        Opened::Ready {
            file: File::from(fd),
            links: stat.st_nlink,
        }
    }

    /// Sniffs and hashes an open file. The caller then checks the result
    /// with [`bracket`].
    pub(crate) fn read(&mut self, file: &mut File) -> Result<Content, ContentFault> {
        let started = Instant::now();
        let content = self.sniff_and_hash(file);
        self.read_time += started.elapsed();
        content
    }

    fn sniff_and_hash(&mut self, file: &mut File) -> Result<Content, ContentFault> {
        self.files_read += 1;
        let mut head = 0;
        while head < ferret_policy::SNIFF_LEN {
            match file.read(&mut self.buffer[head..]) {
                Ok(0) => break,
                Ok(n) => head += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(ContentFault::Read(e)),
            }
        }
        self.bytes_read += head as u64;
        let sniffed = ferret_policy::sniff(&self.buffer[..head.min(ferret_policy::SNIFF_LEN)]);
        let content = match sniffed {
            ferret_policy::Content::Binary => Content::Binary,
            ferret_policy::Content::Text => {
                let mut hasher = blake3::Hasher::new();
                hasher.update(&self.buffer[..head]);
                loop {
                    match file.read(&mut self.buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            self.bytes_read += n as u64;
                            hasher.update(&self.buffer[..n]);
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(ContentFault::Read(e)),
                    }
                }
                let mut hash: Hash = [0; 16];
                hash.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
                Content::Hashed(hash)
            }
        };
        Ok(content)
    }
}

/// Closes the stat bracket around a read: `content` holds only if a second
/// `fstat` still agrees with `walked`, which the first already matched. A
/// write during the read changes mtime, so the hash may mix two versions.
pub(crate) fn bracket(
    file: &File,
    walked: &Stat,
    content: Content,
) -> Result<Content, ContentFault> {
    match fstat(file.as_fd()) {
        Ok(after) if catalog_stat(&after).same_version(walked) => Ok(content),
        Ok(_) => Err(ContentFault::Changed),
        Err(e) => Err(ContentFault::Stat(e.into())),
    }
}

/// The catalog's fields of a raw `fstat`.
fn catalog_stat(stat: &rustix::fs::Stat) -> Stat {
    Stat {
        dev: stat.st_dev,
        ino: stat.st_ino,
        size: u64::try_from(stat.st_size).unwrap_or(0),
        mtime_sec: stat.st_mtime,
        mtime_nsec: u32::try_from(stat.st_mtime_nsec).unwrap_or(0),
        ctime_sec: stat.st_ctime,
        ctime_nsec: u32::try_from(stat.st_ctime_nsec).unwrap_or(0),
        mode: stat.st_mode,
        uid: stat.st_uid,
        gid: stat.st_gid,
    }
}

/// The catalog's fields of a walker stat.
pub(crate) fn from_walk(stat: &crate::Stat<'_>) -> Stat {
    Stat {
        dev: stat.dev,
        ino: stat.ino,
        size: stat.size,
        mtime_sec: stat.mtime_sec,
        mtime_nsec: u32::try_from(stat.mtime_nsec).unwrap_or(0),
        ctime_sec: stat.ctime_sec,
        ctime_nsec: u32::try_from(stat.ctime_nsec).unwrap_or(0),
        mode: stat.mode,
        uid: stat.uid,
        gid: stat.gid,
    }
}
