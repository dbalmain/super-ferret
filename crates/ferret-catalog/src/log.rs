//! Durable transactions and a pinned, lazily checked published prefix.
//! This is framing and replacement records; applying them is the M3 overlay.
mod records;
pub use records::Record;

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::format::{NONE, u32_at, u64_at};
use crate::generation::{Manifest, checksum};
use crate::publication::{self, Point};
use crate::{Catalog, DecodeError, Generation, OpenError, RetryFromCurrent};

pub(crate) const HEADER: u64 = 64;
const LOG_MAGIC: &[u8; 8] = b"FERRETCL";
const TX_MAGIC: &[u8; 8] = b"FERRETTX";

/// Independently checked payload projections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Family {
    Namespace,
    Inodes,
    Aux,
    Docs,
}
const FAMILIES: [Family; 4] = [Family::Namespace, Family::Inodes, Family::Aux, Family::Docs];

/// A producer's final row replacements and resulting allocation/live counts.
/// Graph and liveness consistency belongs to the M3 producer/overlay.
#[derive(Clone, Debug)]
pub struct ChangeSet {
    pub records: Vec<Record>,
    pub counters: [u32; 3],
    /// Inodes, names, directories, documents.
    pub counts: [u32; 4],
}

struct Block {
    family: Family,
    count: u32,
    offset: u64,
    length: usize,
    digest: [u8; 16],
    records: OnceLock<Vec<Record>>,
}
struct Envelope {
    sequence: u64,
    counters: [u32; 3],
    blocks: Vec<Block>,
}

/// A fixed committed prefix. Its file descriptor survives checkpoint unlink.
pub struct Log {
    file: Option<File>,
    transactions: Vec<Envelope>,
    end: u64,
    read: AtomicU64,
    checkpoint: u64,
}

/// A pinned manifest, checkpoint descriptor and log descriptor. Opening reads
/// framing only; neither recovery nor payload checks occur until requested.
pub struct Published {
    manifest: Manifest,
    checkpoint: Catalog,
    log: Log,
}

fn corrupt(label: &'static str) -> OpenError {
    OpenError::Decode(DecodeError::Corrupt(label))
}
fn io_error(e: io::Error) -> OpenError {
    OpenError::Io(e)
}

pub(crate) fn header(generation: Generation) -> [u8; 64] {
    let mut bytes = [0; 64];
    bytes[..8].copy_from_slice(LOG_MAGIC);
    bytes[8..12].copy_from_slice(&crate::format::VERSION.to_le_bytes());
    bytes[16..32].copy_from_slice(&generation.incarnation);
    bytes[32..40].copy_from_slice(&generation.checkpoint.to_le_bytes());
    bytes[40..48].copy_from_slice(&generation.sequence.to_le_bytes());
    let digest = checksum(&bytes[..48]);
    bytes[48..].copy_from_slice(&digest);
    bytes
}

impl Published {
    pub fn open(dir: &Path) -> Result<Option<Self>, OpenError> {
        loop {
            let Some(manifest) = crate::read::read_manifest(dir)? else {
                return Ok(None);
            };
            #[cfg(test)]
            if let Some(hook) = BEFORE_PAIR.with_borrow_mut(Option::take) {
                hook(dir);
            }
            let pair = (|| {
                let snapshot =
                    File::open(dir.join(format!("snapshot.{}", manifest.generation.checkpoint)))
                        .map_err(io_error)?;
                let file = if manifest.log_end == 0 {
                    None
                } else {
                    Some(
                        File::open(dir.join(format!("changes.{}", manifest.generation.checkpoint)))
                            .map_err(io_error)?,
                    )
                };
                let checkpoint = Catalog::open_checkpoint(snapshot, &manifest)?;
                let log = Log::open(file, &manifest, checkpoint.manifest().counters).map_err(
                    |cause| OpenError::Log {
                        checkpoint: manifest.generation.checkpoint,
                        sequence: None,
                        family: None,
                        cause: Box::new(cause),
                    },
                )?;
                Ok(Self {
                    manifest: manifest.clone(),
                    checkpoint,
                    log,
                })
            })();
            match pair {
                Err(OpenError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                    if crate::read::read_manifest(dir)?
                        .is_some_and(|next| next.generation != manifest.generation)
                    {
                        continue;
                    }
                    return Err(OpenError::Io(e));
                }
                other => return other.map(Some),
            }
        }
    }
    pub fn generation(&self) -> Generation {
        self.manifest.generation
    }
    /// Explicit base checkpoint access, never an effective overlay view.
    pub fn checkpoint(&self) -> &Catalog {
        &self.checkpoint
    }
    pub fn log(&self) -> &Log {
        &self.log
    }
    pub fn counters(&self) -> [u32; 3] {
        self.manifest.counters
    }
    pub fn counts(&self) -> [u32; 4] {
        self.manifest.counts
    }
    /// Consumes this pinned pair into an effective, lazily checked reader.
    pub fn into_catalog(self) -> Catalog {
        self.checkpoint.with_log(self.manifest, self.log)
    }
}

impl Log {
    fn open(file: Option<File>, m: &Manifest, base_counters: [u32; 3]) -> Result<Self, OpenError> {
        let Some(file) = file else {
            if m.log_end != 0 || m.generation.sequence != m.checkpoint_sequence {
                return Err(corrupt("log prefix"));
            }
            return Ok(Self {
                file: None,
                transactions: Vec::new(),
                end: 0,
                read: AtomicU64::new(0),
                checkpoint: m.generation.checkpoint,
            });
        };
        if m.log_end < HEADER || file.metadata().map_err(io_error)?.len() < m.log_end {
            return Err(corrupt("log prefix truncated"));
        }
        let mut bytes = [0; 64];
        file.read_exact_at(&mut bytes, 0).map_err(io_error)?;
        let base = Generation {
            sequence: m.checkpoint_sequence,
            ..m.generation
        };
        if bytes != header(base) {
            return Err(corrupt("log header identity/checksum"));
        }
        let mut at = HEADER;
        let mut sequence = m.checkpoint_sequence;
        let mut counters = base_counters;
        let mut transactions = Vec::new();
        let mut read = HEADER;
        while at < m.log_end {
            if m.log_end - at < 96 {
                return Err(corrupt("log transaction truncated"));
            }
            let mut head = [0; 64];
            file.read_exact_at(&mut head, at).map_err(io_error)?;
            if &head[..8] != TX_MAGIC
                || u32_at(&head, 8) != crate::format::VERSION
                || head[52..64] != [0; 12]
            {
                return Err(corrupt("log transaction header"));
            }
            let count = u32_at(&head, 12) as usize;
            let length = u64_at(&head, 16);
            let next = u64_at(&head, 24);
            if count == 0
                || count > 4
                || length < 96 + 48 * count as u64
                || !length.is_multiple_of(8)
                || length > m.log_end - at
                || sequence.checked_add(1) != Some(next)
                || next == u64::MAX
                || u64_at(&head, 32) != sequence
            {
                return Err(corrupt("log transaction framing/sequence"));
            }
            let next_counters = std::array::from_fn(|i| u32_at(&head, 40 + i * 4));
            if next_counters[0] > NONE - 16
                || next_counters[1] == NONE
                || next_counters.iter().zip(counters).any(|(a, b)| *a < b)
            {
                return Err(corrupt("log allocation counters"));
            }
            let mut framing = head.to_vec();
            framing.resize(64 + count * 48, 0);
            file.read_exact_at(&mut framing[64..], at + 64)
                .map_err(io_error)?;
            let mut footer = [0; 32];
            file.read_exact_at(&mut footer, at + length - 32)
                .map_err(io_error)?;
            if u64_at(&footer, 0) != next
                || u64_at(&footer, 8) != length
                || footer[16..] != checksum(&framing)
            {
                return Err(corrupt("log transaction footer/checksum"));
            }
            let mut cursor = framing.len() as u64;
            let mut seen = [false; 4];
            let mut blocks = Vec::new();
            for d in framing[64..].chunks_exact(48) {
                let family = u16::from_le_bytes([d[0], d[1]]) as usize;
                let offset = u64_at(d, 8);
                let size = u64_at(d, 16);
                let records = u32_at(d, 4);
                if family >= 4
                    || seen[family]
                    || d[2..4] != [0; 2]
                    || d[40..48] != [0; 8]
                    || offset != cursor
                    || size == 0
                    || !size.is_multiple_of(8)
                    || records == 0
                    || u64::from(records) > size / 16
                    || size > length - 32 - cursor
                {
                    return Err(corrupt("log block descriptor"));
                }
                seen[family] = true;
                cursor += size;
                let mut digest = [0; 16];
                digest.copy_from_slice(&d[24..40]);
                blocks.push(Block {
                    family: FAMILIES[family],
                    count: records,
                    offset: at + offset,
                    length: usize::try_from(size).map_err(|_| corrupt("log block length"))?,
                    digest,
                    records: OnceLock::new(),
                });
            }
            if cursor != length - 32 {
                return Err(corrupt("log block tiling"));
            }
            read += framing.len() as u64 + 32;
            transactions.push(Envelope {
                sequence: next,
                counters: next_counters,
                blocks,
            });
            at += length;
            sequence = next;
            counters = next_counters;
        }
        if sequence != m.generation.sequence || counters != m.counters {
            return Err(corrupt("log manifest sequence/counters"));
        }
        Ok(Self {
            file: Some(file),
            transactions,
            end: m.log_end,
            read: AtomicU64::new(read),
            checkpoint: m.generation.checkpoint,
        })
    }
    pub(crate) fn frames(&self) -> impl Iterator<Item = (u64, [u32; 3])> {
        self.transactions.iter().map(|t| (t.sequence, t.counters))
    }
    pub fn record_count(&self) -> u64 {
        self.transactions
            .iter()
            .flat_map(|frame| &frame.blocks)
            .map(|block| u64::from(block.count))
            .sum()
    }
    pub fn transaction_count(&self) -> usize {
        self.transactions.len()
    }
    pub fn committed_end(&self) -> u64 {
        self.end
    }
    pub fn bytes_read(&self) -> u64 {
        self.read.load(Ordering::Relaxed)
    }
    /// Checks only this family, leaving unrelated payloads unread. Successfully
    /// loaded records are cached and immutable for this pinned prefix.
    pub fn load(&self, family: Family) -> Result<(), OpenError> {
        for t in &self.transactions {
            for b in t.blocks.iter().filter(|b| b.family == family) {
                if b.records.get().is_some() {
                    continue;
                }
                let mut bytes = vec![0; b.length];
                let Some(file) = &self.file else {
                    return Err(corrupt("missing log descriptor"));
                };
                let checked = (|| {
                    file.read_exact_at(&mut bytes, b.offset).map_err(io_error)?;
                    self.read.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    if checksum(&bytes) != b.digest {
                        return Err(corrupt("log payload checksum"));
                    }
                    records::decode(&bytes, b.count, family, t.counters, t.sequence)
                        .map_err(OpenError::Decode)
                })();
                let records = checked.map_err(|cause| OpenError::Log {
                    checkpoint: self.checkpoint,
                    sequence: Some(t.sequence),
                    family: Some(family),
                    cause: Box::new(cause),
                })?;
                let _ = b.records.set(records);
            }
        }
        Ok(())
    }
    pub fn load_all(&self) -> Result<(), OpenError> {
        FAMILIES
            .into_iter()
            .try_for_each(|family| self.load(family))
    }
    /// Records in commit order; call `load(family)` first.
    pub fn records(&self, family: Family) -> impl Iterator<Item = (u64, &Record)> {
        self.transactions.iter().flat_map(move |t| {
            t.blocks
                .iter()
                .filter(move |b| b.family == family)
                .flat_map(move |b| {
                    let Some(records) = b.records.get() else {
                        panic!("log family {family:?} was not loaded");
                    };
                    records.iter().map(move |r| (t.sequence, r))
                })
        })
    }
}

/// A failed append never remains a usable session. Reopen under the lock to
/// validate the published payloads and truncate/sync an unpublished suffix.
pub struct Writer {
    dir: PathBuf,
    _lock: crate::lock::Lock,
    log: File,
    manifest: Manifest,
    poisoned: bool,
    current: Catalog,
    budget: crate::budget::Budget,
}

#[derive(Debug)]
pub enum Error {
    Locked,
    MissingCheckpoint,
    Previous(OpenError),
    Stale(RetryFromCurrent),
    Invalid(DecodeError),
    Io(io::Error),
    Undurable(io::Error),
    Poisoned,
}
impl Error {
    pub fn published(&self) -> bool {
        matches!(self, Self::Undurable(_))
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Locked => write!(f, "another writer holds the catalog lock"),
            Self::MissingCheckpoint => {
                write!(f, "a checkpoint is required before log transactions")
            }
            Self::Previous(e) => e.fmt(f),
            Self::Stale(e) => write!(f, "retry from {:?}", e.current),
            Self::Invalid(e) => e.fmt(f),
            Self::Io(e) => write!(f, "log publication: {e}"),
            Self::Undurable(e) => write!(f, "published log generation may be undurable: {e}"),
            Self::Poisoned => write!(f, "reopen the writer to recover its uncertain tail"),
        }
    }
}
impl std::error::Error for Error {}
impl Writer {
    pub fn open(dir: &Path) -> Result<Self, Error> {
        let lock = crate::lock::Lock::open(dir).map_err(|e| match e {
            crate::lock::Error::Locked => Error::Locked,
            crate::lock::Error::Io(e) => Error::Io(e),
        })?;
        let result: Result<(File, Manifest, Catalog, crate::budget::Budget), Error> = (|| {
            let pinned = Published::open(dir)
                .map_err(Error::Previous)?
                .ok_or(Error::MissingCheckpoint)?;
            recover(dir, &pinned).map_err(Error::Previous)?;
            let mut manifest = pinned.manifest.clone();
            let mut budget = crate::budget::Budget::open(&pinned).map_err(Error::Previous)?;
            let current = pinned.into_catalog();
            current.load_all().map_err(Error::Previous)?;
            current.name_references();
            let path = dir.join(format!("changes.{}", manifest.generation.checkpoint));
            if manifest.log_end == 0 {
                let file = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&path)
                    .map_err(Error::Io)?;
                (&file)
                    .write_all(&header(manifest.generation))
                    .map_err(Error::Io)?;
                publication::sync(&file, Point::HeaderSync).map_err(Error::Io)?;
                publication::sync(&File::open(dir).map_err(Error::Io)?, Point::PairSync)
                    .map_err(Error::Io)?;
                manifest.log_end = HEADER;
                publish_manifest(dir, &manifest)?;
            }
            let log = OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .map_err(Error::Io)?;
            budget.usage.log_bytes = manifest.log_end;
            Ok((log, manifest, current, budget))
        })();
        let (log, manifest, current, budget) = result?;
        Ok(Self {
            dir: dir.to_owned(),
            _lock: lock,
            log,
            manifest,
            poisoned: false,
            current,
            budget,
        })
    }
    /// Publishes the effective view at an idle boundary under this writer lock.
    /// A preflighted diff has already been checked by `Catalog::advance`.
    pub(crate) fn checkpoint_view(&mut self, view: Catalog) -> Result<Generation, Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        let mut generation = view.generation();
        generation.checkpoint = generation
            .checkpoint
            .checked_add(1)
            .filter(|&n| n != u64::MAX)
            .ok_or(Error::Invalid(DecodeError::Corrupt("checkpoint exhausted")))?;
        while ["snapshot", "changes"].iter().any(|prefix| {
            self.dir
                .join(format!("{prefix}.{}", generation.checkpoint))
                .exists()
        }) {
            generation.checkpoint = generation
                .checkpoint
                .checked_add(1)
                .filter(|&n| n != u64::MAX)
                .ok_or(Error::Invalid(DecodeError::Corrupt("checkpoint exhausted")))?;
        }
        let temp = self.dir.join("catalog.tmp");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)
            .map_err(Error::Io)?;
        crate::compact::write(&view, &file, generation).map_err(Error::Io)?;
        publication::sync(&file, Point::SnapshotSync).map_err(Error::Io)?;
        // Planning buffers have gone before readback. The checked sections are
        // the new resident view, rather than a second whole-file allocation.
        let mut bytes = [0; crate::format::TABLE_END];
        file.read_exact_at(&mut bytes, 0).map_err(Error::Io)?;
        let layout = crate::format::decode_table(&bytes, file.metadata().map_err(Error::Io)?.len())
            .map_err(Error::Invalid)?;
        let mut manifest = Manifest::from_layout(&layout);
        let current = Catalog::open_checkpoint(file, &manifest).map_err(Error::Previous)?;
        current.load_all().map_err(Error::Previous)?;
        current.name_references();
        self.poisoned = true;
        crate::transaction::publish(&self.dir, &temp, &current).map_err(|e| match e {
            crate::CommitError::Undurable(e) => Error::Undurable(e),
            other => Error::Io(io::Error::other(other)),
        })?;
        manifest.log_end = HEADER;
        self.log = OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.dir.join(format!("changes.{}", generation.checkpoint)))
            .map_err(Error::Undurable)?;
        self.manifest = manifest;
        self.current = current;
        self.budget =
            crate::budget::Budget::empty(self.current.inode_count(), self.current.name_count());
        self.poisoned = false;
        // Retirement cannot undo durable publication or leave the session
        // using old epoch caches. Recovery retries an interrupted cleanup.
        let _ = cleanup(&self.dir, generation.checkpoint);
        Ok(generation)
    }

    pub(crate) fn into_checkpoint(self, sniffer: u32) -> crate::Transaction {
        crate::Transaction::from_locked(self.dir, self._lock, Some(self.current), sniffer)
    }
    pub fn generation(&self) -> Generation {
        self.manifest.generation
    }
    /// Shares the writer's current checked view; old clones remain pinned.
    pub fn view(&self) -> Catalog {
        self.current.clone()
    }
    pub fn commit(
        &mut self,
        expected: Generation,
        changes: &ChangeSet,
    ) -> Result<Generation, Error> {
        self.commit_with_sniffer(expected, changes, self.manifest.sniffer)
    }
    /// Publishes observations made under a completely refreshed sniffer
    /// version. A transition requires a nonempty change set, even when
    /// classifications remain equal; callers include their policy record in
    /// that case.
    pub fn commit_with_sniffer(
        &mut self,
        expected: Generation,
        changes: &ChangeSet,
        sniffer: u32,
    ) -> Result<Generation, Error> {
        self.publish_changes(expected, changes, sniffer, None)
    }

    pub(crate) fn commit_budgeted(
        &mut self,
        expected: Generation,
        changes: &ChangeSet,
        sniffer: u32,
        limits: crate::CompactionLimits,
    ) -> Result<Generation, Error> {
        self.publish_changes(expected, changes, sniffer, Some(limits))
    }

    pub fn budget_usage(&self) -> crate::BudgetUsage {
        self.budget.usage
    }

    fn publish_changes(
        &mut self,
        expected: Generation,
        changes: &ChangeSet,
        sniffer: u32,
        limits: Option<crate::CompactionLimits>,
    ) -> Result<Generation, Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        self.manifest
            .generation
            .check(expected)
            .map_err(Error::Stale)?;
        if changes.records.is_empty() {
            if changes.counters != self.manifest.counters
                || changes.counts != self.manifest.counts
                || sniffer != self.manifest.sniffer
            {
                return Err(Error::Invalid(DecodeError::Corrupt(
                    "empty change set counters",
                )));
            }
            return Ok(expected);
        }
        let mut next = self.manifest.clone();
        next.generation.sequence = expected
            .sequence
            .checked_add(1)
            .filter(|&n| n != u64::MAX)
            .ok_or(Error::Invalid(DecodeError::Corrupt("sequence exhausted")))?;
        next.sniffer = sniffer;
        next.counters = changes.counters;
        next.counts = changes.counts;
        Manifest::decode(&next.encode()).map_err(Error::Invalid)?;
        if next
            .counters
            .iter()
            .zip(self.manifest.counters)
            .any(|(a, b)| *a < b)
        {
            return Err(Error::Invalid(DecodeError::Corrupt(
                "allocation counters decreased",
            )));
        }
        let length = checked_size(changes, next.generation.sequence).map_err(Error::Invalid)?;
        next.log_end = self
            .manifest
            .log_end
            .checked_add(length)
            .ok_or(Error::Invalid(DecodeError::Corrupt("log exhausted")))?;
        let current = self
            .current
            .advance_with_sniffer(expected, changes, sniffer)
            .map_err(|e| match e {
                Error::Previous(OpenError::Decode(e)) => Error::Invalid(e),
                other => other,
            })?;
        if limits.is_some_and(|limits| {
            next.log_end >= limits.log_bytes
                || self.budget.usage.records + changes.records.len() as u64 >= limits.records
        }) || changes.counters[0] == crate::format::NONE - 16
            || changes.counters[1] == crate::format::NONE - 1
        {
            return self.checkpoint_view(current);
        }
        let budget = self.budget.project(changes, length);
        if limits.is_some_and(|limits| budget.usage.reached(limits)) {
            return self.checkpoint_view(current);
        }
        // Serialize only a transaction that will actually be appended. A
        // large preflighted diff needs bounded per-record codec scratch,
        // rather than a discarded whole log transaction plus decoded copy.
        let bytes =
            encode(changes, next.generation.sequence, expected.sequence).map_err(Error::Invalid)?;
        self.poisoned = true;
        publication::append(&self.log, &bytes, self.manifest.log_end).map_err(Error::Io)?;
        publication::sync(&self.log, Point::LogSync).map_err(Error::Io)?;
        publish_manifest(&self.dir, &next)?;
        self.manifest = next;
        self.current = current;
        self.budget = budget;
        self.poisoned = false;
        Ok(self.manifest.generation)
    }
}

/// Validate with the real record codec, keeping only one row's scratch.
/// The framed length is exact, including nonempty-family descriptors/footer.
pub(crate) fn checked_size(changes: &ChangeSet, sequence: u64) -> Result<u64, DecodeError> {
    let mut scratch = Vec::new();
    let mut counts = [0u32; FAMILIES.len()];
    let mut length = 96u64;
    for record in &changes.records {
        scratch.clear();
        record.encode(&mut scratch)?;
        records::decode(&scratch, 1, record.family(), changes.counters, sequence)?;
        let family = record.family() as usize;
        counts[family] = counts[family]
            .checked_add(1)
            .ok_or(DecodeError::Corrupt("log record count"))?;
        length = length
            .checked_add(scratch.len() as u64)
            .ok_or(DecodeError::Corrupt("log exhausted"))?;
    }
    Ok(length + counts.iter().filter(|&&n| n != 0).count() as u64 * 48)
}

pub(crate) fn encode(
    changes: &ChangeSet,
    sequence: u64,
    previous: u64,
) -> Result<Vec<u8>, DecodeError> {
    let mut blocks = Vec::new();
    for family in FAMILIES {
        let mut bytes = Vec::new();
        let mut count = 0u32;
        for r in changes.records.iter().filter(|r| r.family() == family) {
            r.encode(&mut bytes)?;
            count = count
                .checked_add(1)
                .ok_or(DecodeError::Corrupt("log record count"))?;
        }
        if count != 0 {
            records::decode(&bytes, count, family, changes.counters, sequence)?;
            blocks.push((family, count, bytes));
        }
    }
    let mut out = vec![0; 64 + blocks.len() * 48];
    out[..8].copy_from_slice(TX_MAGIC);
    out[8..12].copy_from_slice(&crate::format::VERSION.to_le_bytes());
    out[12..16].copy_from_slice(&(blocks.len() as u32).to_le_bytes());
    out[24..32].copy_from_slice(&sequence.to_le_bytes());
    out[32..40].copy_from_slice(&previous.to_le_bytes());
    for (i, n) in changes.counters.into_iter().enumerate() {
        out[40 + i * 4..44 + i * 4].copy_from_slice(&n.to_le_bytes());
    }
    for (i, (family, count, bytes)) in blocks.into_iter().enumerate() {
        let d = 64 + i * 48;
        let offset = out.len() as u64;
        out[d..d + 2].copy_from_slice(&(family as u16).to_le_bytes());
        out[d + 4..d + 8].copy_from_slice(&count.to_le_bytes());
        out[d + 8..d + 16].copy_from_slice(&offset.to_le_bytes());
        out[d + 16..d + 24].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
        out[d + 24..d + 40].copy_from_slice(&checksum(&bytes));
        out.extend_from_slice(&bytes);
    }
    let length = out.len() as u64 + 32;
    out[16..24].copy_from_slice(&length.to_le_bytes());
    let framing = 64 + u32_at(&out, 12) as usize * 48;
    let digest = checksum(&out[..framing]);
    out.extend_from_slice(&sequence.to_le_bytes());
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(&digest);
    Ok(out)
}

fn publish_manifest(dir: &Path, m: &Manifest) -> Result<(), Error> {
    let temp = dir.join("current.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)
        .map_err(Error::Io)?;
    file.write_all(&m.encode()).map_err(Error::Io)?;
    publication::sync(&file, Point::ManifestSync).map_err(Error::Io)?;
    fs::rename(&temp, dir.join("current")).map_err(Error::Io)?;
    publication::hit(Point::ManifestRename).map_err(Error::Undurable)?;
    publication::sync(
        &File::open(dir).map_err(Error::Undurable)?,
        Point::DirectorySync,
    )
    .map_err(Error::Undurable)
}

/// Caller holds the common writer lock. Validate published payloads before
/// modifying any suffix or clearing private/orphan files.
pub(crate) fn recover(dir: &Path, pinned: &Published) -> Result<(), OpenError> {
    pinned.log.load_all()?;
    if pinned.manifest.log_end != 0 {
        let file = OpenOptions::new()
            .write(true)
            .open(dir.join(format!("changes.{}", pinned.manifest.generation.checkpoint)))
            .map_err(io_error)?;
        if file.metadata().map_err(io_error)?.len() > pinned.manifest.log_end {
            file.set_len(pinned.manifest.log_end).map_err(io_error)?;
            publication::sync(&file, Point::RecoverySync).map_err(io_error)?;
        }
    }
    cleanup(dir, pinned.manifest.generation.checkpoint).map_err(io_error)
}

pub(crate) fn cleanup(dir: &Path, checkpoint: u64) -> io::Result<()> {
    let mut obsolete = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let orphan = ["snapshot.", "changes."].into_iter().any(|prefix| {
            name.strip_prefix(prefix)
                .and_then(|n| n.parse::<u64>().ok())
                .is_some()
                && name != format!("{prefix}{checkpoint}")
        });
        if orphan || matches!(name, "catalog.tmp" | "changes.tmp" | "current.tmp") {
            obsolete.push(entry.path());
        }
    }
    if !obsolete.is_empty() {
        // A prior writer may have returned Undurable without a process/OS
        // restart. Make current's selected pair durable before retiring any
        // alternative pair. File contents were synced before its rename.
        publication::sync(&File::open(dir)?, Point::RecoveryDirectorySync)?;
        for path in obsolete {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
type OpenHook = Box<dyn FnOnce(&Path)>;
#[cfg(test)]
thread_local! {
    pub(crate) static BEFORE_PAIR: std::cell::RefCell<Option<OpenHook>> = const { std::cell::RefCell::new(None) };
}
