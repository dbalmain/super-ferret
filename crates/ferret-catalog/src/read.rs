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

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::{fmt, io};

use crate::batch::{Stat, WorkTreeKind};
use crate::fan_out::fan_out;
use crate::format::{
    self, COLUMNS, Column, Facts, Layout, PAIR_ROW, SECTIONS, Section, TABLE_END, u32_at,
};
use crate::generation::Manifest;
use crate::log::{Family, Record};
use crate::overlay::{self, Overlay};
use crate::packed;
use crate::regular::{self, Access};
use crate::seek::partition_point;
use crate::{
    ContentState, DecodeError, DocId, Generation, Handle, Hash, InoId, NameId, RetryFromCurrent,
};

mod changes;
mod cursors;
mod documents;
mod inodes;
mod names;
mod sections;

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
    dormant_base: Arc<OnceLock<crate::dormant::State>>,
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
        /// Held while a section is read and checked, so that a second
        /// loader waits for it rather than reading it again.
        loading: Box<[Mutex<()>; SECTIONS.len()]>,
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
            dormant_base: Default::default(),
            source: Arc::new(Source::File {
                file,
                sections: Default::default(),
                loading: Default::default(),
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
            Some(
                Record::DocDelete { .. } | Record::DocDormant { .. } | Record::DocForget { .. },
            ) => return None,
            _ => {}
        }
        self.column(Column::DocId)
            .sequence_row(u64::from(id.0), self.layout.docs)
            .map(|row| u32_at(self.section(Section::DocRefs), row * 4))
    }

    pub(crate) fn format_version(&self) -> u32 {
        self.layout.version
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
            dormant_base: Default::default(),
            source: Arc::new(Source::Whole(bytes)),
        })
    }

    fn ns_record(&self, category: u64, id: u32) -> Option<&Record> {
        self.family_record(Family::Namespace, category, id)
    }
    fn family_record(&self, family: Family, category: u64, id: u32) -> Option<&Record> {
        self.overlay
            .as_ref()
            .and_then(|o| o.projection(family).get(category, id))
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
}

fn special_kind(rows: &[u8], id: InoId) -> Option<Kind> {
    let count = rows.len() / PAIR_ROW;
    let at = partition_point(count, |i| u32_at(rows, i * PAIR_ROW) < id.0);
    (at < count && u32_at(rows, at * PAIR_ROW) == id.0).then(|| {
        Kind::from_ignored_child(crate::format::NONE - 1 - u32_at(rows, at * PAIR_ROW + 4))
            .unwrap_or(Kind::File)
    })
}
