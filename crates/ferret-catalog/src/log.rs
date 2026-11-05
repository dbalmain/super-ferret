//! Durable transactions and a pinned, lazily checked published prefix.
//! This is framing and replacement records; applying them is the M3 overlay.
mod records;
mod writer;
pub use records::Record;
pub(crate) use writer::checked_size;
#[cfg(test)]
pub(crate) use writer::encode;
pub use writer::{Error, Writer};

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::format::{NONE, u32_at, u64_at};
use crate::generation::{Manifest, checksum};
use crate::publication::{self, Point};
use crate::read::open_state;
use crate::regular::{self, Access};
use crate::{Catalog, DecodeError, Generation, OpenError};

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
                let snapshot = open_state(
                    &dir.join(format!("snapshot.{}", manifest.generation.checkpoint)),
                    DecodeError::Corrupt("snapshot"),
                )?;
                let file = if manifest.log_end == 0 {
                    None
                } else {
                    Some(open_state(
                        &dir.join(format!("changes.{}", manifest.generation.checkpoint)),
                        DecodeError::Corrupt("change log"),
                    )?)
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
        let mut expected = header(base);
        if u32_at(&bytes, 8) == 5 {
            expected[8..12].copy_from_slice(&5u32.to_le_bytes());
            let digest = checksum(&expected[..48]);
            expected[48..].copy_from_slice(&digest);
        }
        if bytes != expected {
            return Err(corrupt("log header identity/checksum"));
        }
        let mut at = HEADER;
        let mut sequence = m.checkpoint_sequence;
        let mut counters = base_counters;
        let mut transactions = Vec::new();
        let mut read = HEADER;
        while at < m.log_end {
            let (envelope, length, framed) =
                read_envelope(&file, at, m.log_end - at, sequence, counters)?;
            read += framed;
            at += length;
            sequence = envelope.sequence;
            counters = envelope.counters;
            transactions.push(envelope);
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

/// Reads and checks the transaction at `at`, with `room` bytes of log
/// left, following `sequence` and `counters`. Returns it, its length, and
/// the bytes of its header, descriptors and footer, which open reads.
fn read_envelope(
    file: &File,
    at: u64,
    room: u64,
    sequence: u64,
    counters: [u32; 3],
) -> Result<(Envelope, u64, u64), OpenError> {
    if room < 96 {
        return Err(corrupt("log transaction truncated"));
    }
    let mut head = [0; 64];
    file.read_exact_at(&mut head, at).map_err(io_error)?;
    if &head[..8] != TX_MAGIC
        || !matches!(u32_at(&head, 8), 5 | crate::format::VERSION)
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
        || length > room
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
    let blocks = read_blocks(&framing, at, length)?;
    let framed = framing.len() as u64 + 32;
    let envelope = Envelope {
        sequence: next,
        counters: next_counters,
        blocks,
    };
    Ok((envelope, length, framed))
}

/// The blocks a transaction's descriptors place, which must tile its body.
fn read_blocks(framing: &[u8], at: u64, length: u64) -> Result<Vec<Block>, OpenError> {
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
    Ok(blocks)
}

/// Caller holds the common writer lock. Validate published payloads before
/// modifying any suffix or clearing private/orphan files.
pub(crate) fn recover(dir: &Path, pinned: &Published) -> Result<(), OpenError> {
    pinned.log.load_all()?;
    if pinned.manifest.log_end != 0 {
        let file = regular::open(
            &dir.join(format!("changes.{}", pinned.manifest.generation.checkpoint)),
            Access::Update,
        )
        .map_err(io_error)?;
        if file.metadata().map_err(io_error)?.len() > pinned.manifest.log_end {
            file.set_len(pinned.manifest.log_end).map_err(io_error)?;
            publication::sync(&file, Point::RecoverySync).map_err(io_error)?;
        }
    }
    // A prior append can stop after current's rename without leaving obsolete
    // files. Resolve its durability uncertainty before returning a writer.
    publication::sync(
        &File::open(dir).map_err(io_error)?,
        Point::RecoveryDirectorySync,
    )
    .map_err(io_error)?;
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
