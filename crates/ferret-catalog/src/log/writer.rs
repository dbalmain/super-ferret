//! Appending to the log: the writer session that frames, syncs and
//! publishes each transaction, and the encoding it checks first.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::format::u32_at;
use crate::generation::{Manifest, checksum};
use crate::publication::{self, Point};
use crate::regular::{self, Access};
use crate::{Catalog, DecodeError, Generation, OpenError, RetryFromCurrent};

use super::*;

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
    bulk: Option<std::sync::Arc<dyn crate::bulk::Control>>,
}

#[derive(Debug)]
pub enum Error {
    InputLimit(crate::InputUsage),
    Deferred(crate::bulk::Blocked),
    Rebuild(crate::CommitError),
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
            || matches!(self, Self::Rebuild(error) if error.published())
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Deferred(reason) => reason.fmt(f),
            Self::InputLimit(usage) => write!(f, "unpublished input budget exceeded: {usage:?}"),
            Self::Rebuild(error) => error.fmt(f),
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
        Self::open_locked(dir, lock)
    }
    pub(crate) fn open_locked(dir: &Path, lock: crate::lock::Lock) -> Result<Self, Error> {
        let result: Result<(File, Manifest, Catalog, crate::budget::Budget), Error> = (|| {
            let pinned = Published::open(dir)
                .map_err(Error::Previous)?
                .ok_or(Error::MissingCheckpoint)?;
            recover(dir, &pinned).map_err(Error::Previous)?;
            let mut manifest = pinned.manifest.clone();
            let mut budget = crate::budget::Budget::open(&pinned).map_err(Error::Previous)?;
            let current = pinned.into_catalog();
            let current = current.into_resident().map_err(Error::Previous)?;
            current.name_references();
            let path = dir.join(format!("changes.{}", manifest.generation.checkpoint));
            if manifest.log_end == 0 {
                let file = regular::open(&path, Access::Create).map_err(Error::Io)?;
                file.set_len(0).map_err(Error::Io)?;
                (&file)
                    .write_all(&header(manifest.generation))
                    .map_err(Error::Io)?;
                publication::sync(&file, Point::HeaderSync).map_err(Error::Io)?;
                publication::sync(&File::open(dir).map_err(Error::Io)?, Point::PairSync)
                    .map_err(Error::Io)?;
                manifest.log_end = HEADER;
                publish_manifest(dir, &manifest)?;
            }
            let log = regular::open(&path, Access::Update).map_err(Error::Io)?;
            budget.usage.log_bytes = manifest.log_end;
            Ok((log, manifest, current, budget))
        })();
        let (log, manifest, current, budget) = result?;
        let mut writer = Self {
            dir: dir.to_owned(),
            _lock: lock,
            log,
            manifest,
            poisoned: false,
            current,
            budget,
            bulk: None,
        };
        if writer.current.format_version() == 5 {
            writer.checkpoint_view(writer.view())?;
        }
        Ok(writer)
    }
    /// Adopts a checkpoint just published under this same retained lock.
    pub(crate) fn from_checkpoint(
        dir: PathBuf,
        lock: crate::lock::Lock,
        catalog: Catalog,
    ) -> Result<Self, Error> {
        let mut manifest = catalog.manifest();
        manifest.log_end = HEADER;
        let log = regular::open(
            &dir.join(format!("changes.{}", manifest.generation.checkpoint)),
            Access::Update,
        )
        .map_err(Error::Undurable)?;
        let budget = crate::budget::Budget::empty(catalog.inode_count(), catalog.name_count());
        Ok(Self {
            dir,
            _lock: lock,
            log,
            manifest,
            poisoned: false,
            current: catalog.into_resident().map_err(Error::Previous)?,
            budget,
            bulk: None,
        })
    }

    /// Revalidates the published prefix and retires an uncertain tail while
    /// retaining the same writer lock. A failed recovery leaves this writer
    /// poisoned; its last checked view remains readable.
    pub(crate) fn recover(&mut self) -> Result<(), Error> {
        self.poisoned = true;
        let recovered = Self::open_locked(&self.dir, self._lock.share())?;
        let bulk = self.bulk.take();
        *self = recovered;
        self.bulk = bulk;
        Ok(())
    }
    pub(crate) fn set_bulk_control(
        &mut self,
        control: Option<std::sync::Arc<dyn crate::bulk::Control>>,
    ) {
        self.bulk = control;
    }
    pub(crate) fn admit_bulk(&self, kind: crate::bulk::Kind) -> Result<(), Error> {
        if let Some(control) = &self.bulk {
            control
                .admit(kind, &self.current)
                .map_err(Error::Deferred)?;
        }
        Ok(())
    }
    /// Publishes the effective view at an idle boundary under this writer lock.
    /// A preflighted diff has already been checked by `Catalog::advance`.
    pub(crate) fn checkpoint_view(&mut self, view: Catalog) -> Result<Generation, Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        if let Some(control) = &self.bulk {
            control
                .admit(crate::bulk::Kind::Checkpoint, &view)
                .map_err(Error::Deferred)?;
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
        let file = regular::replace(&temp).map_err(Error::Io)?;
        crate::bulk::writes(
            self.bulk
                .as_ref()
                .filter(|control| control.paced())
                .map(|control| control.limiter()),
            || crate::compact::write(&view, &file, generation),
        )
        .map_err(Error::Io)?;
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
        self.log = regular::open(
            &self.dir.join(format!("changes.{}", generation.checkpoint)),
            Access::Update,
        )
        .map_err(Error::Undurable)?;
        self.manifest = manifest;
        self.current = current.into_resident().map_err(Error::Previous)?;
        self.budget =
            crate::budget::Budget::empty(self.current.inode_count(), self.current.name_count());
        self.poisoned = false;
        // Retirement cannot undo durable publication or leave the session
        // using old epoch caches. Recovery retries an interrupted cleanup.
        let _ = cleanup(&self.dir, generation.checkpoint);
        Ok(generation)
    }

    pub(crate) fn rebuild_checkpoint(
        &mut self,
        batches: Vec<crate::Batch>,
        sniffer: u32,
        policy: crate::Hash,
        coverage: Option<crate::Coverage>,
        dormant_cap: usize,
    ) -> Result<(), Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        let lock = self._lock.share();
        let mut txn =
            crate::Transaction::from_locked(self.dir.clone(), lock, Some(self.view()), sniffer);
        txn.set_policy(policy);
        txn.set_coverage(coverage);
        txn.dormant_cap = dormant_cap;
        for batch in batches {
            txn.add(batch);
        }
        self.poisoned = true;
        let current = match crate::bulk::writes(
            self.bulk
                .as_ref()
                .filter(|control| control.paced())
                .map(|control| control.limiter()),
            || txn.commit(),
        ) {
            Ok(current) => current,
            Err(error) => {
                self.poisoned = error.published();
                return Err(Error::Rebuild(error));
            }
        };
        self.manifest = current.manifest();
        self.manifest.log_end = HEADER;
        self.log = regular::open(
            &self
                .dir
                .join(format!("changes.{}", current.generation().checkpoint)),
            Access::Update,
        )
        .map_err(Error::Undurable)?;
        self.budget = crate::budget::Budget::empty(current.inode_count(), current.name_count());
        self.current = current.into_resident().map_err(Error::Previous)?;
        self.poisoned = false;
        Ok(())
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
    /// The catalog directory this writer holds the lock in.
    pub fn dir(&self) -> &Path {
        &self.dir
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
        let (next, length) = self.next_manifest(expected, changes, sniffer)?;
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
        self.current = current.into_resident().map_err(Error::Previous)?;
        self.budget = budget;
        self.poisoned = false;
        Ok(self.manifest.generation)
    }

    /// The manifest that appending `changes` after `expected` publishes,
    /// checked to round-trip and never to lower an allocation counter, and the
    /// length of the log transaction it appends.
    fn next_manifest(
        &self,
        expected: Generation,
        changes: &ChangeSet,
        sniffer: u32,
    ) -> Result<(Manifest, u64), Error> {
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
        Ok((next, length))
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
    let mut file = regular::replace(&temp).map_err(Error::Io)?;
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
