//! [`Catalog`]: one opened generation, and everything a query reads from it.
//!
//! Opening reads the header, the section table and the column descriptors;
//! each section is read with a positional read on first use and validated
//! then (D38 B). Nothing else is derived (D30 C). Accessors decode columns and
//! rows in place. The catalog applies no matching: `ferret-query` scans
//! [`Catalog::name_heap`] and reads its hits through a [`NameReader`], or
//! passes over every name with [`NameReader::runs_from`], which decodes the
//! name columns a block at a time.
//!
//! Loading is explicit and fallible: [`Catalog::load`] reads and validates
//! the sections a caller names, with any they depend on. Every accessor after
//! that is infallible. Reading a section that was never loaded panics, like
//! an id out of range: both are caller bugs. Each inode field is its own
//! section, so a query that tests one field loads only that one; a whole
//! [`Inode`] needs [`Section::INODE`].
//!
//! Ids passed in must come from this generation; an id out of range panics,
//! as indexing a slice does. Anything read from the file cannot panic: that is
//! what loading validated.

use std::fmt;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use crate::batch::{Stat, WorkTreeKind};
use crate::fan_out::fan_out;
use crate::format::{
    self, COLUMNS, Column, Facts, HASH_ROW, Layout, PAIR_ROW, SECTIONS, Section, TABLE_END, View,
    WORK_TREE_ROW, u32_at, u64_at,
};
use crate::generation::Manifest;
use crate::log::{Family, Record};
use crate::overlay::{self, Overlay};
use crate::packed::{self, Blocked};
use crate::regular::{self, Access};
use crate::{
    ContentState, DecodeError, DocId, Generation, Handle, Hash, InoId, NameId, RetryFromCurrent,
};

mod cursors;
mod documents;
mod inodes;
#[cfg(test)]
mod tests;

pub use cursors::{Kinds, NameReader, NameRuns};

/// Inodes in one run of [`Catalog::size_run`] and [`Catalog::mtime_run`]:
/// a bitset word's worth.
pub const RUN: usize = packed::RUN;

/// The snapshot's file name inside the catalog directory.
pub(crate) const FILE: &str = "catalog";

/// Why [`Catalog::open`] or [`Catalog::load`] failed.
#[derive(Debug)]
pub enum OpenError {
    /// Reading the file failed. A file truncated under an open reader is
    /// `UnexpectedEof` here.
    Io(io::Error),
    /// The file is not a valid catalog.
    Decode(DecodeError),
    /// Damaged framing or payload, with the file and block to re-index.
    Log {
        checkpoint: u64,
        sequence: Option<u64>,
        family: Option<crate::log::Family>,
        cause: Box<OpenError>,
    },
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "reading the catalog: {e}"),
            Self::Decode(e) => e.fmt(f),
            Self::Log {
                checkpoint,
                sequence,
                family,
                cause,
            } => write!(
                f,
                "changes.{checkpoint}, sequence {sequence:?}, family {family:?}: {cause}"
            ),
        }
    }
}

impl std::error::Error for OpenError {}

/// What an inode row is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A directory; only checkpoint base directories form an id prefix.
    Dir,
    /// A regular file.
    File,
    /// A symlink, never followed; see [`Catalog::link_target`].
    Symlink,
    /// A named pipe; content is never read.
    Fifo,
    /// A Unix socket; content is never read.
    Socket,
    /// A block device; content is never read.
    Block,
    /// A character device; content is never read.
    Character,
}

impl Kind {
    /// The file type bits of `mode`, independent of permissions.
    pub fn from_mode(mode: u32) -> Self {
        match mode & 0o170_000 {
            0o040_000 => Self::Dir,
            0o120_000 => Self::Symlink,
            0o010_000 => Self::Fifo,
            0o140_000 => Self::Socket,
            0o060_000 => Self::Block,
            0o020_000 => Self::Character,
            _ => Self::File,
        }
    }

    pub(crate) fn ignored_child(self) -> u32 {
        crate::format::NONE - 1 - self as u32
    }

    pub(crate) fn from_ignored_child(child: u32) -> Option<Self> {
        match crate::format::NONE - child {
            1 => Some(Self::Dir),
            2 => Some(Self::File),
            3 => Some(Self::Symlink),
            4 => Some(Self::Fifo),
            5 => Some(Self::Socket),
            6 => Some(Self::Block),
            7 => Some(Self::Character),
            _ => None,
        }
    }
}

/// A name's target. Ignored targets have no stat row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// An ordinary catalogued inode.
    Inode(InoId),
    /// A name only; an ignored directory is opaque.
    Ignored(Kind),
}

impl Target {
    /// What a name's child column value names.
    pub fn from_child(child: InoId) -> Self {
        Kind::from_ignored_child(child.0).map_or(Self::Inode(child), Self::Ignored)
    }
}

/// A directory's contents source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Contents {
    /// The directory was listed; children are in the catalog.
    Catalogued,
    /// Ignored directory; only its name and type are stored.
    Ignored,
    /// The directory could not be listed; walk live to report the error.
    Unreadable,
}

/// One directory child, including ignored names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    /// Its name id in this generation.
    pub name: NameId,
    /// Raw basename bytes.
    pub bytes: &'a [u8],
    /// The d_type-equivalent, available even for ignored names.
    pub kind: Kind,
    /// Stat row or ignored type marker.
    pub target: Target,
}

/// One name edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Name<'a> {
    /// The directory holding the name.
    pub parent: InoId,
    /// Raw child column. Use [`Name::target`] before reading stat columns:
    /// ignored names hold reserved type values rather than inode ids.
    pub child: InoId,
    /// The name, as the kernel returned it; never contains NUL or `/`.
    pub bytes: &'a [u8],
}

impl Name<'_> {
    /// A valid inode id, or an ignored type with no stat row.
    pub fn target(&self) -> Target {
        Target::from_child(self.child)
    }
}

/// A path resolution, exact when `remainder` is empty. Otherwise the target
/// is an opaque directory and the caller must walk the suffix live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved<'a> {
    /// None for a configured root.
    pub name: Option<NameId>,
    /// Resolved inode or ignored marker.
    pub target: Target,
    /// Components below an opaque directory that the catalog cannot answer.
    pub remainder: &'a [u8],
}

/// One inode row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inode {
    /// As `lstat` reported it.
    pub stat: Stat,
    /// What the last observation learnt about its content (D37).
    pub state: ContentState,
    /// Its document, when `state` is `Hashed`.
    pub doc: Option<DocId>,
}

/// One `worktrees` row (D23).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkTree<'a> {
    /// The work tree's top directory.
    pub dir: InoId,
    /// Main, linked or submodule.
    pub kind: WorkTreeKind,
    /// The repository's identity: its common directory's `(dev, ino)`. Equal
    /// ids mean one repository, however its path was spelled.
    pub common_id: (u64, u64),
    /// The common directory's path as the walker spelled it, for display.
    pub common_dir: &'a [u8],
}

/// One generation of the catalog. It stays valid however many commits
/// follow: an opened catalog holds its file, not its path, so a generation
/// replaced mid-query stays readable (D32). Cloning shares the descriptor and
/// checked buffers; it never copies checkpoint payloads.
#[derive(Clone)]
pub struct Catalog {
    layout: Layout,
    source: Arc<Source>,
    overlay: Option<Arc<Overlay>>,
    name_references: Arc<OnceLock<Vec<u32>>>,
}

enum Source {
    Resident {
        names: Box<crate::ResidentNames>,
        sections: Box<[Arc<[u8]>; SECTIONS.len()]>,
        read: u64,
    },
    /// A whole file in memory, every section validated
    /// ([`Catalog::from_bytes`]).
    Whole(Vec<u8>),
    /// An open file whose sections load on demand.
    File {
        file: File,
        sections: Box<[OnceLock<Arc<[u8]>>; SECTIONS.len()]>,
        /// Bytes read from the file so far, header and table included.
        read: AtomicU64,
        /// What the checks of loaded sections found, for later checks.
        facts: Facts,
    },
}

pub(crate) fn read_manifest(dir: &Path) -> Result<Option<Manifest>, OpenError> {
    let file = match open_state(&dir.join("current"), DecodeError::Corrupt("manifest")) {
        Ok(file) => file,
        Err(OpenError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
            // Legacy snapshots remain explicitly refused; only import reads v3.
            match open_state(&dir.join(FILE), DecodeError::NotACatalog) {
                Ok(file) => {
                    let mut head = [0; 12];
                    file.read_exact_at(&mut head, 0).map_err(OpenError::Io)?;
                    return Err(OpenError::Decode(DecodeError::Version(u32_at(&head, 8))));
                }
                Err(OpenError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    if file.metadata().map_err(OpenError::Io)?.len() != crate::generation::MANIFEST_LEN as u64 {
        return Err(OpenError::Decode(DecodeError::Corrupt("manifest")));
    }
    let mut bytes = [0; crate::generation::MANIFEST_LEN];
    file.read_exact_at(&mut bytes, 0).map_err(OpenError::Io)?;
    Manifest::decode(&bytes)
        .map(Some)
        .map_err(OpenError::Decode)
}

/// Opens a published state file for reading without blocking on a special
/// file ([`crate::regular`]). Something other than a regular file at the name
/// is `not_regular`, the error a wrong regular file there would decode to.
pub(crate) fn open_state(path: &Path, not_regular: DecodeError) -> Result<File, OpenError> {
    regular::open(path, Access::Read).map_err(|e| {
        if regular::is_not_regular(&e) {
            OpenError::Decode(not_regular)
        } else {
            OpenError::Io(e)
        }
    })
}

impl Catalog {
    /// Opens the catalog in `dir`. Checks the manifest, the checkpoint header
    /// and the change log's framing; no section payload is loaded. `Ok(None)`
    /// when nothing has been committed there yet.
    /// [`Self::published_generation`] reads the manifest alone.
    pub fn open(dir: &Path) -> Result<Option<Catalog>, OpenError> {
        Ok(crate::log::Published::open(dir)?.map(crate::log::Published::into_catalog))
    }

    /// The generation stored in `dir`'s manifest. Does not open the snapshot
    /// or the change log. `Ok(None)` when nothing has been published, including
    /// a missing directory — the same answer as [`Self::open`]. When `open`
    /// succeeds, [`Self::generation`] equals this value. A manifest that
    /// decodes while the snapshot or log does not still yields that
    /// generation; `open` returns the error instead.
    pub fn published_generation(dir: &Path) -> Result<Option<Generation>, OpenError> {
        Ok(read_manifest(dir)?.map(|manifest| manifest.generation))
    }

    pub(crate) fn open_checkpoint(file: File, manifest: &Manifest) -> Result<Catalog, OpenError> {
        let len = file.metadata().map_err(OpenError::Io)?.len();
        let mut head = vec![0; (len as usize).min(TABLE_END)];
        file.read_exact_at(&mut head, 0).map_err(OpenError::Io)?;
        let layout = format::decode_table(&head, len).map_err(OpenError::Decode)?;
        manifest.check(&layout).map_err(OpenError::Decode)?;
        Ok(Catalog {
            layout,
            overlay: None,
            name_references: Default::default(),
            source: Arc::new(Source::File {
                file,
                sections: Default::default(),
                read: AtomicU64::new(head.len() as u64),
                facts: Facts::default(),
            }),
        })
    }

    pub(crate) fn name_references(&self) -> &[u32] {
        self.name_references.get_or_init(|| {
            let mut refs = vec![0u32; self.base_inode_count() as usize];
            for (_, child) in self.name_reader().child_ids() {
                if let Some(n) = refs.get_mut(child.0 as usize) {
                    *n += 1;
                }
            }
            refs
        })
    }
    pub(crate) fn with_log(mut self, manifest: Manifest, log: crate::log::Log) -> Self {
        if log.transaction_count() != 0 {
            self.overlay = Some(Arc::new(Overlay::new(self.clone(), manifest, log)));
        }
        self
    }
    /// Converts a fully checked generation to interned names, releasing its raw
    /// heap and offset column. Same-epoch successors share this source.
    /// Existing pins retain their own representation; no borrowed section
    /// is invalidated.
    pub fn into_resident(mut self) -> Result<Self, OpenError> {
        if self.resident_names().is_some() {
            self.load_all()?;
            return Ok(self);
        }
        let mut base = self.clone();
        base.overlay = None;
        let rows = self.base_name_count();
        let build = || crate::ResidentNames::build(base.name_reader(), rows);
        // The build reads only the names; when it runs on several threads,
        // the other sections load alongside it. `load_all` then checks
        // everything in order, as before, and reports the first failure a
        // serial load would.
        let names = match crate::ResidentNames::threads(rows) {
            1 => None,
            _ => {
                let tasks = vec![true, false];
                let mut built = fan_out(tasks, 2, |names| match names {
                    true => self.load_one(Section::Names).ok().map(|()| build()),
                    false => {
                        self.prefetch(&[Section::NameHeap, Section::Names]);
                        None
                    }
                });
                built.swap_remove(0)
            }
        };
        self.load_all()?;
        let names = names.unwrap_or_else(build);
        let sections = Box::new(std::array::from_fn(|i| {
            let section = SECTIONS[i];
            if section == Section::NameHeap {
                return Arc::from([]);
            }
            if section == Section::Names {
                let end = self.layout.columns[Column::NameOffset as usize].start;
                return Arc::from(&self.section(section)[..end]);
            }
            match self.source.as_ref() {
                Source::File { sections, .. } => sections[i]
                    .get()
                    .unwrap_or_else(|| unreachable!("checked section"))
                    .clone(),
                _ => Arc::from(self.section(section)),
            }
        }));
        let read = match self.source.as_ref() {
            Source::Whole(bytes) => bytes.len() as u64,
            Source::File { read, .. } => read.load(Ordering::Relaxed),
            Source::Resident { read, .. } => *read,
        };
        self.source = Arc::new(Source::Resident {
            names: Box::new(names),
            sections,
            read,
        });
        if let Some(overlay) = &mut self.overlay {
            let overlay = Arc::make_mut(overlay);
            overlay.base.source = self.source.clone();
        }
        Ok(self)
    }

    /// The checked immutable checkpoint, excluding the selected log overlay.
    /// Used only to construct epoch-local derived indexes, never as the query's
    /// effective view. Its generation has the checkpoint's original sequence.
    pub fn checkpoint_base(&self) -> Self {
        let mut base = self.clone();
        base.overlay = None;
        base
    }

    /// The interned names [`Catalog::into_resident`] would build for this
    /// checkpoint, built without converting it: for a one-shot host that
    /// keeps the raw heap and needs interned names only if a name query
    /// runs. Needs [`Section::Names`] and [`Section::NameHeap`].
    pub fn build_resident_names(&self) -> crate::ResidentNames {
        let base = self.checkpoint_base();
        crate::ResidentNames::build(base.name_reader(), self.base_name_count())
    }

    /// The immutable epoch base; sparse effective names are in `delta_names`.
    pub fn resident_names(&self) -> Option<&crate::ResidentNames> {
        match self.source.as_ref() {
            Source::Resident { names, .. } => Some(names),
            _ => None,
        }
    }

    /// Directory coverage/count rows replaced in the selected log prefix.
    pub fn changed_directory_scopes(&self) -> impl Iterator<Item = InoId> + '_ {
        self.overlay
            .iter()
            .flat_map(|overlay| overlay.projection(Family::Namespace).records(overlay::DIR))
            .filter_map(|record| match record {
                Record::DirPut { id, .. } => Some(InoId(*id)),
                _ => None,
            })
    }

    /// Name rows the selected overlay replaced since the checkpoint, each
    /// with whether it is live in this view: tombstones included, also for
    /// names born earlier in the epoch. Bounded by the overlay's name rows.
    /// Requires Names.
    pub fn changed_names(&self) -> impl Iterator<Item = (NameId, bool)> + '_ {
        self.overlay
            .iter()
            .flat_map(|overlay| overlay.projection(Family::Namespace).records(overlay::NAME))
            .filter_map(|record| match record {
                Record::NamePut { id, .. } => Some((NameId(*id), true)),
                Record::NameDelete { id } => Some((NameId(*id), false)),
                _ => None,
            })
    }

    /// Inodes whose stat row, document included, the selected overlay
    /// replaced since the checkpoint. Bounded by the overlay's inode rows.
    /// Requires [`Section::Doc`].
    pub fn changed_inodes(&self) -> impl Iterator<Item = InoId> + '_ {
        self.overlay
            .iter()
            .flat_map(|overlay| overlay.projection(Family::Inodes).records(overlay::INODE))
            .filter_map(|record| match record {
                Record::InodePut { id, .. } => Some(InoId(*id)),
                _ => None,
            })
    }

    /// Base edges overwritten or removed by the selected generation.
    pub fn suppressed_base_names(&self) -> impl Iterator<Item = NameId> + '_ {
        self.overlay
            .iter()
            .flat_map(|overlay| overlay.namespace().suppressed.iter().copied().map(NameId))
    }

    /// Applies one checked same-epoch transaction without copying base buffers.
    /// A mismatched generation is rejected before any supplied id is read.
    pub fn advance(
        &self,
        expected: Generation,
        changes: &crate::log::ChangeSet,
    ) -> Result<Self, crate::log::Error> {
        self.advance_with_sniffer(expected, changes, self.sniffer_version())
    }
    pub(crate) fn advance_with_sniffer(
        &self,
        expected: Generation,
        changes: &crate::log::ChangeSet,
        sniffer: u32,
    ) -> Result<Self, crate::log::Error> {
        use crate::log::Error;
        self.generation().check(expected).map_err(Error::Stale)?;
        self.load_all().map_err(Error::Previous)?;
        let mut manifest = self
            .overlay
            .as_ref()
            .map_or_else(|| self.manifest(), |o| o.manifest.clone());
        if changes.records.is_empty() {
            if changes.counters != manifest.counters || changes.counts != manifest.counts {
                return Err(Error::Invalid(DecodeError::Corrupt(
                    "empty change set counters",
                )));
            }
            return Ok(self.clone());
        }
        manifest.generation.sequence = expected
            .sequence
            .checked_add(1)
            .filter(|&n| n != u64::MAX)
            .ok_or(Error::Invalid(DecodeError::Corrupt("sequence exhausted")))?;
        if changes
            .counters
            .iter()
            .zip(manifest.counters)
            .any(|(&a, b)| a < b)
        {
            return Err(Error::Invalid(DecodeError::Corrupt(
                "allocation counters decreased",
            )));
        }
        manifest.sniffer = sniffer;
        manifest.counters = changes.counters;
        manifest.counts = changes.counts;
        Manifest::decode(&manifest.encode()).map_err(Error::Invalid)?;
        crate::log::checked_size(changes, manifest.generation.sequence).map_err(Error::Invalid)?;
        let mut view = self.clone();
        let parent = self
            .overlay
            .clone()
            .unwrap_or_else(|| Arc::new(Overlay::empty(self.clone(), self.manifest())));
        view.overlay = Some(Arc::new(parent.advance(
            parent.clone(),
            manifest,
            &changes.records,
        )));
        view.load_all().map_err(Error::Previous)?;
        if let Some(overlay) = view.overlay.as_mut().and_then(Arc::get_mut) {
            overlay.detach();
        } else {
            unreachable!("new view has an unshared overlay");
        }
        Ok(view)
    }
    /// Number of immutable replacement runs, for carry measurements.
    /// Requires all families loaded.
    pub fn overlay_run_count(&self) -> usize {
        self.overlay.as_ref().map_or(0, |o| {
            [Family::Namespace, Family::Inodes, Family::Aux, Family::Docs]
                .iter()
                .map(|&f| o.projection(f).run_count())
                .sum()
        })
    }
    fn overlay_inode(&self, id: InoId) -> Option<&Record> {
        self.overlay.as_ref().and_then(|o| o.inode(id.0))
    }
    /// Number of physical inode rows in the immutable checkpoint.
    pub fn base_inode_count(&self) -> u32 {
        self.layout.inodes as u32
    }
    /// Number of physical directory rows in the checkpoint prefix.
    pub fn base_dir_count(&self) -> u32 {
        self.layout.dirs as u32
    }
    /// Number of physical name rows, also the base heap scan bound.
    pub fn base_name_count(&self) -> u32 {
        self.layout.names as u32
    }
    /// Live inode ids; requires a section loading the Life projection.
    pub fn inode_ids(&self) -> impl Iterator<Item = InoId> + '_ {
        (0..self.next_inode().0)
            .filter(|&id| self.is_live_inode(InoId(id)))
            .map(InoId)
    }
    /// Live directories; requires Names (or Links for base kinds).
    pub fn dir_ids(&self) -> impl Iterator<Item = InoId> + '_ {
        self.inode_ids().filter(|&id| self.is_directory(id))
    }
    /// Whether an epoch inode is live. Requires Life when a log is present.
    pub fn is_live_inode(&self, id: InoId) -> bool {
        self.overlay
            .as_ref()
            .map_or(id.0 < self.base_inode_count(), |o| o.live_inode(id.0))
    }
    /// Indexed names referring to a live inode, independent of `st_nlink`.
    /// Requires Names; the base inverse is cached and log LifePut replaces it.
    pub fn indexed_name_count(&self, id: InoId) -> u32 {
        if !self.is_live_inode(id) {
            return 0;
        }
        if let Some(Record::LifePut { names, .. }) =
            self.overlay.as_ref().and_then(|o| o.life(id.0))
        {
            return *names;
        }
        self.name_references()
            .get(id.0 as usize)
            .copied()
            .unwrap_or(0)
    }
    /// Base deaths, in id order; requires Life. Used to clear base bitsets.
    pub fn deleted_base_inodes(&self) -> impl Iterator<Item = InoId> + '_ {
        self.overlay
            .iter()
            .flat_map(|o| o.projection(Family::Namespace).records(overlay::LIFE))
            .filter_map(|r| match r {
                Record::InodeDelete { id } if *id < self.base_inode_count() => Some(InoId(*id)),
                _ => None,
            })
    }
    /// Directory membership, independent of live counts and allocation order.
    pub fn is_directory(&self, id: InoId) -> bool {
        self.overlay
            .as_ref()
            .and_then(|o| o.kind(id.0))
            .map_or(id.0 < self.base_dir_count(), |k| k == Kind::Dir)
    }
    /// Whether a base heap span still supplies an effective name.
    pub fn base_name_live(&self, id: NameId) -> bool {
        id.0 < self.base_name_count()
            && self
                .overlay
                .as_ref()
                .is_none_or(|o| !o.namespace().suppresses(id.0))
    }
    /// Contiguous effective delta heap and explicit (offset, epoch id) spans.
    /// Requires Names. The base heap is never copied here.
    pub fn delta_names(&self) -> (&[u8], &[(usize, u32)]) {
        self.overlay.as_ref().map_or((&[][..], &[][..]), |o| {
            (&o.namespace().heap, &o.namespace().spans)
        })
    }
    /// Next epoch inode id; deleted ids are never reused.
    pub fn next_inode(&self) -> InoId {
        InoId(
            self.overlay
                .as_ref()
                .map_or(self.base_inode_count(), |o| o.manifest.counters[0]),
        )
    }

    /// Next epoch name id; deleted edges leave holes.
    pub fn next_name(&self) -> NameId {
        NameId(
            self.overlay
                .as_ref()
                .map_or(self.base_name_count(), |o| o.manifest.counters[1]),
        )
    }

    /// This view's identity, including the inode/name epoch.
    pub fn generation(&self) -> Generation {
        self.overlay
            .as_ref()
            .map_or(self.layout.generation, |o| o.manifest.generation)
    }

    /// Resolves a checked inode handle only after validating its source view.
    pub fn checked_inode(&self, handle: Handle<InoId>) -> Result<InoId, RetryFromCurrent> {
        self.generation().check(handle.generation)?;
        assert!(
            handle.id.0 < self.next_inode().0,
            "inode handle out of range"
        );
        Ok(handle.id)
    }

    /// Resolves a checked name handle only after validating its source view.
    pub fn checked_name(&self, handle: Handle<NameId>) -> Result<NameId, RetryFromCurrent> {
        self.generation().check(handle.generation)?;
        assert!(handle.id.0 < self.next_name().0, "name handle out of range");
        Ok(handle.id)
    }

    /// The published snapshot path, for diagnostics and benchmark I/O.
    /// Readers retain a descriptor; reopening this path does not pin a view.
    pub fn snapshot_path(dir: &Path) -> Result<Option<PathBuf>, OpenError> {
        let Some(manifest) = read_manifest(dir)? else {
            return Ok(None);
        };
        Ok(Some(dir.join(format!(
            "snapshot.{}",
            manifest.generation.checkpoint
        ))))
    }

    /// Last trustworthy subtree sequence. Requires RetainedAt.
    pub fn retained_at(&self, dir: InoId) -> Option<u64> {
        if let Some(Record::DirPut { retained_at, .. }) = self.ns_record(overlay::DIR, dir.0) {
            return *retained_at;
        }
        self.layout
            .blocked(Column::RetainedAt, self.section(Section::RetainedAt))
            .nullable(dir.0 as usize)
    }

    /// Writer policy fingerprint. Requires Policy.
    pub fn policy(&self) -> Hash {
        if let Some(Record::PolicyPut { hash }) = self.ns_record(overlay::POLICY, 0) {
            return *hash;
        }
        let mut fingerprint = [0; 16];
        fingerprint.copy_from_slice(self.section(Section::Policy));
        fingerprint
    }

    /// Indexed inode reference count for a live document. Requires DocRefs
    /// (which loads Docs and Doc for validation).
    pub fn doc_references(&self, id: DocId) -> Option<u32> {
        match self.family_record(Family::Docs, overlay::DOC, id.0) {
            Some(Record::DocPut { references, .. }) => return Some(*references),
            Some(Record::DocDelete { .. }) => return None,
            _ => {}
        }
        self.column(Column::DocId)
            .sequence_row(u64::from(id.0), self.layout.docs)
            .map(|row| u32_at(self.section(Section::DocRefs), row * 4))
    }

    pub(crate) fn manifest(&self) -> Manifest {
        Manifest::from_layout(&self.layout)
    }

    /// Validates `bytes` as a whole snapshot file. Every section is loaded.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Catalog, DecodeError> {
        let layout = format::decode(&bytes)?;
        Ok(Catalog {
            layout,
            overlay: None,
            name_references: Default::default(),
            source: Arc::new(Source::Whole(bytes)),
        })
    }

    /// Loads `sections`, and each section they depend on, validating each as
    /// it arrives. A section already loaded is not read again. After this
    /// succeeds, every accessor that reads only these sections is
    /// infallible. A failure leaves whatever loaded before it loaded.
    pub fn load(&self, sections: &[Section]) -> Result<(), OpenError> {
        for &section in sections {
            self.load_one(section)?;
            if let Some(o) = &self.overlay {
                o.load(Family::Namespace)?;
                match section {
                    Section::Names
                    | Section::NameHeap
                    | Section::DirNames
                    | Section::Roots
                    | Section::Entries
                    | Section::Traversed
                    | Section::RetainedAt
                    | Section::Revealed => o.load_namespace()?,
                    Section::Links | Section::Specials => {
                        o.load(Family::Aux)?;
                        o.check_aux()?;
                    }
                    Section::WorkTrees => {
                        o.load(Family::Aux)?;
                        o.check_aux()?;
                    }
                    Section::Docs => o.load(Family::Docs)?,
                    Section::DocRefs => {
                        o.load(Family::Inodes)?;
                        o.check_fields()?;
                        o.load(Family::Docs)?;
                    }
                    Section::Strings | Section::Policy => {}
                    _ => {
                        o.load(Family::Inodes)?;
                        o.check_fields()?;
                    }
                }
                o.check_documents(self)?;
            }
        }
        Ok(())
    }
    fn ns_record(&self, category: u64, id: u32) -> Option<&Record> {
        self.family_record(Family::Namespace, category, id)
    }
    fn family_record(&self, family: Family, category: u64, id: u32) -> Option<&Record> {
        self.overlay
            .as_ref()
            .and_then(|o| o.projection(family).get(category, id))
    }

    /// Loads every section: what a caller that reads the whole generation
    /// (the writer's previous generation, a one-shot query, a test) needs.
    /// A large file's sections are read and checked on a few threads first.
    pub fn load_all(&self) -> Result<(), OpenError> {
        // Below this, starting threads costs more than the checks they share.
        const PREFETCH_BYTES: usize = 1 << 20;
        let bytes: usize = SECTIONS
            .iter()
            .map(|&section| {
                let (start, end) = self.layout.range(section);
                end - start
            })
            .sum();
        if bytes >= PREFETCH_BYTES {
            self.prefetch(&[]);
        }
        self.load(&SECTIONS)?;
        if let Some(o) = &self.overlay {
            o.validate_all(self)?;
        }
        Ok(())
    }

    /// Reads and checks the base sections but `skip` on a few threads,
    /// ignoring failures: [`Catalog::load_all`] then finds them loaded, and
    /// loads any that failed again in order, so it reports what a serial
    /// load would. Sections with nothing to wait for go first, then those
    /// needing only them; the rest are a few bytes and load serially.
    fn prefetch(&self, skip: &[Section]) {
        const WORKERS: usize = 4;
        if !matches!(self.source.as_ref(), Source::File { .. }) {
            return;
        }
        let size = |section: Section| {
            let (start, end) = self.layout.range(section);
            std::cmp::Reverse(end - start)
        };
        let sections = SECTIONS.into_iter().filter(|s| !skip.contains(s));
        let mut first: Vec<_> = sections.clone().filter(|s| s.needs().is_empty()).collect();
        let mut second: Vec<_> = sections
            .filter(|s| !s.needs().is_empty() && s.needs().iter().all(|n| n.needs().is_empty()))
            .collect();
        first.sort_by_key(|&s| size(s));
        second.sort_by_key(|&s| size(s));
        for wave in [first, second] {
            fan_out(wave, WORKERS, |section| {
                let _ = self.load_one(section);
            });
        }
    }

    fn load_one(&self, section: Section) -> Result<(), OpenError> {
        let Source::File {
            file,
            sections,
            read,
            facts,
        } = self.source.as_ref()
        else {
            return Ok(());
        };
        let slot = &sections[section as usize];
        if slot.get().is_some() {
            return Ok(());
        }
        for &need in section.needs() {
            self.load_one(need)?;
        }
        let (start, end) = self.layout.range(section);
        let mut bytes = vec![0; end - start].into_boxed_slice();
        file.read_exact_at(&mut bytes, start as u64)
            .map_err(OpenError::Io)?;
        read.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        format::check(section, &self.layout, facts, |s| {
            if s == section {
                &bytes[..]
            } else {
                self.section(s)
            }
        })
        .map_err(OpenError::Decode)?;
        // A racing loader may have set it first; its bytes are the same.
        let _ = slot.set(bytes.into());
        Ok(())
    }

    /// Whether `section` is loaded. A reader from [`Catalog::from_bytes`]
    /// has every section.
    pub fn is_loaded(&self, section: Section) -> bool {
        let base = match self.source.as_ref() {
            Source::Whole(_) | Source::Resident { .. } => true,
            Source::File { sections, .. } => sections[section as usize].get().is_some(),
        };
        base && self.overlay.as_ref().is_none_or(|o| o.loaded(section))
    }

    /// Bytes read from the file so far: the head of the file and each loaded
    /// section. For a reader from [`Catalog::from_bytes`], the whole file.
    pub fn bytes_read(&self) -> u64 {
        let base = match self.source.as_ref() {
            Source::Whole(bytes) => bytes.len() as u64,
            Source::Resident { read, .. } => *read,
            Source::File { read, .. } => read.load(Ordering::Relaxed),
        };
        base + self.overlay.as_ref().map_or(0, |o| o.bytes_read())
    }

    fn column(&self, column: Column) -> View<'_> {
        self.layout.view(column, self.section(column.section()))
    }

    fn blocked(&self, column: Column) -> Blocked<'_> {
        self.layout.blocked(column, self.section(column.section()))
    }

    fn section(&self, section: Section) -> &[u8] {
        match self.source.as_ref() {
            Source::Whole(bytes) => self.layout.section(bytes, section),
            Source::Resident { sections, .. } => &sections[section as usize],
            Source::File { sections, .. } => sections[section as usize]
                .get()
                .unwrap_or_else(|| panic!("catalog section {section:?} read before it was loaded")),
        }
    }

    // ── counts and header ──

    /// The sniffer version this generation's content states were found with
    /// (D37).
    pub fn sniffer_version(&self) -> u32 {
        self.overlay
            .as_ref()
            .map_or(self.layout.sniffer, |o| o.manifest.sniffer)
    }

    /// The next `DocId` a new document will get. Never decreases (D36 B).
    pub fn next_doc(&self) -> DocId {
        DocId(
            self.overlay
                .as_ref()
                .map_or(self.layout.next_doc, |o| o.manifest.counters[2]),
        )
    }

    /// Live directories; enumerate with `dir_ids`, not this count.
    pub fn dir_count(&self) -> u32 {
        self.overlay
            .as_ref()
            .map_or(self.layout.dirs as u32, |o| o.manifest.counts[2])
    }

    /// Live inode rows; physical scan bounds are `base_inode_count`.
    pub fn inode_count(&self) -> u32 {
        self.overlay
            .as_ref()
            .map_or(self.layout.inodes as u32, |o| o.manifest.counts[0])
    }

    /// Name edges.
    pub fn name_count(&self) -> u32 {
        self.overlay
            .as_ref()
            .map_or(self.layout.names as u32, |o| o.manifest.counts[1])
    }

    /// Live documents. Known from the head of the file; loads nothing.
    pub fn doc_count(&self) -> u32 {
        self.overlay
            .as_ref()
            .map_or(self.layout.docs as u32, |o| o.manifest.counts[3])
    }

    /// The bytes before the first section: header, section table and column
    /// descriptors.
    pub fn head_len(&self) -> u64 {
        TABLE_END as u64
    }

    /// Each bit-packed column: its section, its width in bits, and its
    /// dictionary's length (0 unless it is a dictionary). What `ferret
    /// stats` reports beside the section sizes. Known from the head of the
    /// file; loads nothing.
    pub fn column_widths(&self) -> impl Iterator<Item = (Section, u32, u32)> + '_ {
        COLUMNS
            .iter()
            .zip(&self.layout.columns)
            .map(|(column, placed)| (column.section(), placed.desc.width, placed.desc.dict_len))
    }

    /// Each section and its size in bytes, in file order: what `ferret
    /// stats` reports. Known from the section table; loads nothing.
    pub fn section_sizes(&self) -> impl Iterator<Item = (Section, u64)> + '_ {
        SECTIONS.iter().map(|&section| {
            let (start, end) = self.layout.range(section);
            (section, (end - start) as u64)
        })
    }

    // ── names ──

    /// Every name, NUL-terminated, in `NameId` order: the bytes a filename
    /// scan reads (D14, D28). Needs [`Section::NameHeap`]. A NUL never occurs
    /// inside a name, so a literal match cannot straddle two;
    /// [`Catalog::name_at`] maps a match's offset back to its name.
    pub fn name_heap(&self) -> &[u8] {
        self.resident_names().map_or_else(
            || self.section(Section::NameHeap),
            |names| names.legacy_heap(),
        )
    }

    /// Every name with its id, in heap order. Needs [`Section::Names`].
    pub fn names(&self) -> impl Iterator<Item = (NameId, &[u8])> + '_ {
        self.name_reader()
            .runs_from(NameId(0))
            .map(|(id, name)| (id, name.bytes))
    }

    /// The name columns and the heap, resolved once: what a pass over many
    /// names holds. Needs [`Section::Names`].
    pub fn name_reader(&self) -> NameReader<'_> {
        let rows = self.section(Section::Names);
        NameReader {
            heap: if self.resident_names().is_some() {
                &[]
            } else {
                self.name_heap()
            },
            resident: self.resident_names(),
            offsets: self
                .resident_names()
                .is_none()
                .then(|| self.layout.blocked(Column::NameOffset, rows)),
            parents: self.layout.blocked(Column::NameParent, rows),
            children: self.layout.blocked(Column::NameChild, rows),
            count: self.layout.names,
            overlay: self.overlay.as_ref().map(|o| (o.namespace(), &o.base)),
        }
    }

    /// One name edge. Needs [`Section::Names`].
    pub fn name(&self, id: NameId) -> Name<'_> {
        self.name_reader().get(id)
    }

    /// Whether an epoch name remains live, including sparse births/deaths.
    /// Requires Names. Check the generation before interpreting an external id.
    pub fn is_live_name(&self, id: NameId) -> bool {
        match self.ns_record(overlay::NAME, id.0) {
            Some(Record::NamePut { .. }) => true,
            Some(Record::NameDelete { .. }) => false,
            _ => id.0 < self.base_name_count(),
        }
    }

    /// Where a name's bytes start in [`Catalog::name_heap`]. Needs
    /// [`Section::Names`]; the bytes run to the next name's start, less its
    /// NUL.
    pub fn name_start(&self, id: NameId) -> usize {
        self.resident_names().map_or_else(
            || self.blocked(Column::NameOffset).get(id.0 as usize) as usize,
            |names| names.legacy_start(id),
        )
    }

    /// The raw child column, without reading name bytes. Ignored rows carry
    /// type tags, so use `Name::target()` before reading stat columns. Needs
    /// [`Section::Names`].
    pub fn child(&self, id: NameId) -> InoId {
        self.name_reader().get(id).child
    }

    /// The name whose bytes (or terminator) hold heap offset `offset`, or
    /// `None` past the end of the heap. Needs [`Section::Names`].
    pub fn name_at(&self, offset: usize) -> Option<NameId> {
        if let Some(names) = self.resident_names() {
            return names.legacy_at(offset);
        }
        if offset >= self.name_heap().len() {
            return None;
        }
        let offsets = self.blocked(Column::NameOffset);
        let after = partition_point(self.layout.names, |i| offsets.get(i) as usize <= offset);
        Some(NameId(after as u32 - 1))
    }

    /// The names in directory `dir`, in name order. Needs
    /// [`Section::Names`].
    pub fn children(&self, dir: InoId) -> impl Iterator<Item = NameId> + '_ {
        let (start, end) = self.child_range(dir);
        // Checkpoint children are already sorted by basename. An overlay that
        // touches `dir` merges its sparse names with the surviving base
        // range; an untouched directory, most of them, lists the range.
        let merged = self
            .overlay
            .as_ref()
            .filter(|o| o.namespace().touches(dir.0))
            .map(|o| {
                let mut ids: Vec<_> = (start as u32..end as u32)
                    .map(NameId)
                    .filter(|&id| self.base_name_live(id))
                    .collect();
                ids.extend(o.namespace().children(dir.0).map(NameId));
                ids.sort_unstable_by(|&a, &b| self.name(a).bytes.cmp(self.name(b).bytes));
                ids
            });
        let base = if merged.is_none() {
            start as u32..end as u32
        } else {
            0..0
        };
        base.map(NameId).chain(merged.into_iter().flatten())
    }

    /// Children, visible and ignored, with raw names, kinds and optional stat
    /// rows. Needs Names and Links. Ignored rows never load stat columns.
    pub fn entries(&self, dir: InoId) -> impl Iterator<Item = Entry<'_>> {
        let names = self.name_reader();
        let mut kinds = self.kinds();
        self.children(dir)
            .map(move |id| Self::typed_entry(id, names.child_edge(id), &mut kinds))
    }

    /// One typed name row. Needs Names and Links for a visible non-directory.
    pub fn entry(&self, id: NameId) -> Entry<'_> {
        Self::typed_entry(id, self.name_reader().child_edge(id), &mut self.kinds())
    }

    fn typed_entry<'a>(
        id: NameId,
        (child, bytes): (InoId, &'a [u8]),
        kinds: &mut Kinds<'_>,
    ) -> Entry<'a> {
        let target = Target::from_child(child);
        let kind = match target {
            Target::Inode(inode) => kinds.kind(inode),
            Target::Ignored(kind) => kind,
        };
        Entry {
            name: id,
            bytes,
            kind,
            target,
        }
    }

    /// Where to read a directory's children. Needs Entries for an inode
    /// target. Unknown counts designate unreadable directories.
    pub fn contents(&self, target: Target) -> Option<Contents> {
        match target {
            Target::Ignored(Kind::Dir) => Some(Contents::Ignored),
            Target::Inode(dir) if self.is_directory(dir) => {
                Some(if self.entry_count(dir).is_some() {
                    Contents::Catalogued
                } else {
                    Contents::Unreadable
                })
            }
            _ => None,
        }
    }

    /// Whether a visible directory has a child on disk, ignored names
    /// included. None means unreadable/unknown. Needs Entries.
    pub fn has_children(&self, dir: InoId) -> Option<bool> {
        self.entry_count(dir).map(|count| count != 0)
    }

    /// Resolves an absolute byte path in the innermost configured root.
    /// Needs Roots and Entries (Roots also loads Names). Does not follow
    /// symlinks or `..`. A path below
    /// an opaque marker returns that marker and its unresolved suffix.
    pub fn resolve<'p>(&self, path: &'p [u8]) -> Option<Resolved<'p>> {
        let (root, prefix) = self
            .roots()
            .filter(|&(_, prefix)| {
                path == prefix
                    || path
                        .strip_prefix(prefix)
                        .is_some_and(|rest| prefix == b"/" || rest.starts_with(b"/"))
            })
            .max_by_key(|&(_, prefix)| prefix.len())?;
        let mut target = Target::Inode(root);
        let mut name = None;
        let mut rest = &path[prefix.len()..];
        loop {
            let separator = rest.starts_with(b"/");
            while let Some(next) = rest.strip_prefix(b"/") {
                rest = next;
            }
            if separator
                && !matches!(target, Target::Ignored(Kind::Dir))
                && !matches!(target, Target::Inode(dir) if self.is_directory(dir))
            {
                return None;
            }
            if rest.is_empty() {
                return Some(Resolved {
                    name,
                    target,
                    remainder: rest,
                });
            }
            let end = rest
                .iter()
                .position(|&byte| byte == b'/')
                .unwrap_or(rest.len());
            let part = &rest[..end];
            if part == b".." {
                return None;
            }
            if part != b"." {
                let Target::Inode(dir) = target else {
                    return Some(Resolved {
                        name,
                        target,
                        remainder: rest,
                    });
                };
                if !self.is_directory(dir) {
                    return None;
                }
                if self.entry_count(dir).is_none() {
                    return Some(Resolved {
                        name,
                        target,
                        remainder: rest,
                    });
                }
                let id = self.lookup(dir, part)?;
                name = Some(id);
                target = self.name(id).target();
            }
            rest = &rest[end..];
        }
    }

    fn child_range(&self, dir: InoId) -> (usize, usize) {
        let parents = self.blocked(Column::NameParent);
        let dir = u64::from(dir.0);
        let start = partition_point(self.layout.names, |i| parents.get(i) < dir);
        // Most directories are small: find the end from the start rather
        // than across every name.
        (
            start,
            start + gallop(self.layout.names - start, |i| parents.get(start + i) <= dir),
        )
    }

    /// The entry called `name` in directory `dir`. Needs [`Section::Names`].
    pub fn lookup(&self, dir: InoId, name: &[u8]) -> Option<NameId> {
        if let Some(o) = &self.overlay {
            return o.namespace().lookup(&o.base, dir.0, name).map(NameId);
        }
        let (start, end) = self.child_range(dir);
        let bytes = |i: usize| self.name(NameId((start + i) as u32)).bytes;
        let at = partition_point(end - start, |i| bytes(i) < name);
        (at < end - start && bytes(at) == name).then_some(NameId((start + at) as u32))
    }

    /// A directory's own name edge; `None` for a root (D30). Needs
    /// [`Section::DirNames`].
    pub fn dir_name(&self, dir: InoId) -> Option<NameId> {
        if let Some(Record::DirPut { name, .. }) = self.ns_record(overlay::DIR, dir.0) {
            return name.map(NameId);
        }
        self.column(Column::DirName)
            .nullable(dir.0 as usize)
            .map(|name| NameId(name as u32))
    }

    /// Whether search should suppress this ancestor of a re-included entry
    /// (D29 compatibility). Find sees an ordinary directory inode. Needs
    /// [`Section::Traversed`].
    pub fn is_traversed(&self, dir: InoId) -> bool {
        if let Some(Record::DirPut { flags, .. }) = self.ns_record(overlay::DIR, dir.0) {
            return flags & 1 != 0;
        }
        let bits = self.section(Section::Traversed);
        bits[dir.0 as usize / 8] >> (dir.0 % 8) & 1 == 1
    }

    /// Whether a repository ignore file's `!` line reveals this hidden name
    /// (D127): rg lists it without `--hidden`. A name's overlay row decides,
    /// as for [`Self::is_traversed`]; then the base's sorted list. Needs
    /// [`Section::Revealed`].
    pub fn is_revealed(&self, name: NameId) -> bool {
        match self.ns_record(overlay::NAME, name.0) {
            Some(Record::NamePut { revealed, .. }) => *revealed,
            Some(_) => false,
            None => self
                .section(Section::Revealed)
                .as_chunks::<4>()
                .0
                .binary_search_by(|row| u32::from_le_bytes(*row).cmp(&name.0))
                .is_ok(),
        }
    }

    /// Search suppression is independent of traversal and retained coverage.
    /// Requires Traversed (which loads namespace flags).
    pub fn is_search_suppressed(&self, dir: InoId) -> bool {
        if let Some(Record::DirPut { flags, .. }) = self.ns_record(overlay::DIR, dir.0) {
            return flags & 2 != 0;
        }
        let bits = self.section(Section::Traversed);
        let offset = self.layout.dirs.div_ceil(8);
        if bits.len() == offset * 2 && offset != 0 {
            return bits[offset + dir.0 as usize / 8] >> (dir.0 % 8) & 1 == 1;
        }
        self.is_traversed(dir)
    }
    // ── paths ──

    /// Appends the path of the entry `name` names: its root's path, then each
    /// name below it, joined by `/`. Needs [`Section::Roots`], which loads
    /// every section a path reads.
    pub fn path(&self, name: NameId, out: &mut Vec<u8>) {
        let (parent, bytes) = self.name_reader().edge(name);
        self.dir_path(parent, out);
        push_component(out, bytes);
    }

    /// Appends a directory's path. Needs [`Section::Roots`].
    pub fn dir_path(&self, dir: InoId, out: &mut Vec<u8>) {
        let names = self.name_reader();
        let mut up = Vec::new();
        let mut at = dir;
        while let Some(name) = self.dir_name(at) {
            let (parent, bytes) = names.edge(name);
            up.push(bytes);
            // Decoding checked that the parent's id is lower, so this ends.
            at = parent;
        }
        out.extend_from_slice(self.root_path(at).unwrap_or_default());
        for name in up.iter().rev() {
            push_component(out, name);
        }
    }

    /// Configured root paths for routing, without loading name tables or
    /// building the namespace graph. Checks the root/string checksums and
    /// path bounds and applies committed root changes. This does not prove
    /// inode liveness: use [`Self::load`] and [`Self::roots`] for traversal.
    pub fn configured_paths(&self) -> Result<Vec<Vec<u8>>, OpenError> {
        self.load_one(Section::Strings)?;
        let roots = if let Source::File { file, read, .. } = self.source.as_ref() {
            let (start, end) = self.layout.range(Section::Roots);
            let mut bytes = vec![0; end - start];
            file.read_exact_at(&mut bytes, start as u64)
                .map_err(OpenError::Io)?;
            read.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            bytes
        } else {
            self.section(Section::Roots).to_vec()
        };
        let corrupt = || OpenError::Decode(DecodeError::Corrupt("roots"));
        if crate::generation::checksum(&roots) != self.layout.checksums[Section::Roots as usize] {
            return Err(corrupt());
        }
        let strings = self.section(Section::Strings);
        let mut paths = std::collections::BTreeMap::new();
        let mut last = None;
        for row in roots.chunks_exact(PAIR_ROW) {
            let id = u32_at(row, 0);
            let offset = u32_at(row, 4) as usize;
            if id as usize >= self.layout.dirs || last.is_some_and(|last| id <= last) {
                return Err(corrupt());
            }
            let rest = strings.get(offset..).ok_or_else(corrupt)?;
            let end = rest
                .iter()
                .position(|&byte| byte == 0)
                .ok_or_else(corrupt)?;
            paths.insert(id, rest[..end].to_vec());
            last = Some(id);
        }
        if let Some(overlay) = &self.overlay {
            overlay.load(Family::Namespace)?;
            for record in overlay.projection(Family::Namespace).records(overlay::ROOT) {
                match record {
                    Record::RootPut { id, path } => {
                        paths.insert(*id, path.clone());
                    }
                    Record::RootDelete { id } => {
                        paths.remove(id);
                    }
                    _ => unreachable!("root projection contains only root records"),
                }
            }
        }
        Ok(paths.into_values().collect())
    }

    /// The configured roots, as `(top directory, path)`. Needs
    /// [`Section::Roots`].
    pub fn roots(&self) -> impl Iterator<Item = (InoId, &[u8])> + '_ {
        let base = self
            .section(Section::Roots)
            .chunks_exact(PAIR_ROW)
            .map(|pair| (InoId(u32_at(pair, 0)), self.string(u32_at(pair, 4))))
            .filter(|(id, _)| self.ns_record(overlay::ROOT, id.0).is_none());
        let delta = self
            .overlay
            .iter()
            .flat_map(|o| o.projection(Family::Namespace).records(overlay::ROOT))
            .filter_map(|r| match r {
                Record::RootPut { id, path } => Some((InoId(*id), path.as_slice())),
                _ => None,
            });
        base.chain(delta)
    }

    fn root_path(&self, dir: InoId) -> Option<&[u8]> {
        self.roots()
            .find(|&(root, _)| root == dir)
            .map(|(_, path)| path)
    }

    fn string(&self, offset: u32) -> &[u8] {
        let rest = &self.section(Section::Strings)[offset as usize..];
        &rest[..rest.iter().position(|&b| b == 0).unwrap_or(0)]
    }
}

fn push_component(out: &mut Vec<u8>, name: &[u8]) {
    if out.last() != Some(&b'/') {
        out.push(b'/');
    }
    out.extend_from_slice(name);
}

/// The first index in `0..n` for which `pred` is false, given that `pred` is
/// true then false over the range.
fn partition_point(n: usize, pred: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pred(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// As [`partition_point`], for a boundary near the start: probes 1, 2, 4…
/// rows ahead, then searches the last span.
fn gallop(n: usize, pred: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut step) = (0, 1);
    while lo + step <= n && pred(lo + step - 1) {
        lo += step;
        step *= 2;
    }
    // `pred(lo + step - 1)` failed or lies past the end.
    let hi = (lo + step - 1).min(n);
    lo + partition_point(hi - lo, |i| pred(lo + i))
}

fn special_kind(rows: &[u8], id: InoId) -> Option<Kind> {
    let count = rows.len() / PAIR_ROW;
    let at = partition_point(count, |i| u32_at(rows, i * PAIR_ROW) < id.0);
    (at < count && u32_at(rows, at * PAIR_ROW) == id.0).then(|| {
        Kind::from_ignored_child(crate::format::NONE - 1 - u32_at(rows, at * PAIR_ROW + 4))
            .unwrap_or(Kind::File)
    })
}
