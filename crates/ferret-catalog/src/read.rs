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
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::batch::{Stat, WorkTreeKind};
use crate::format::{
    self, COLUMNS, Column, Facts, HASH_ROW, Layout, PAIR_ROW, SECTIONS, Section, TABLE_END, View,
    WORK_TREE_ROW, u32_at, u64_at,
};
use crate::packed::{self, Blocked};
use crate::{ContentState, DecodeError, DocId, Hash, InoId, NameId};

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
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "reading the catalog: {e}"),
            Self::Decode(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for OpenError {}

/// What an inode row is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A directory; `InoId < dir_count()`.
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
    pub(crate) fn from_child(child: InoId) -> Self {
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
/// replaced mid-query stays readable (D32).
pub struct Catalog {
    layout: Layout,
    source: Source,
}

enum Source {
    /// A whole file in memory, every section validated
    /// ([`Catalog::from_bytes`]).
    Whole(Vec<u8>),
    /// An open file whose sections load on demand.
    File {
        file: File,
        sections: Box<[OnceLock<Box<[u8]>>; SECTIONS.len()]>,
        /// Bytes read from the file so far, header and table included.
        read: AtomicU64,
        /// What the checks of loaded sections found, for later checks.
        facts: Facts,
    },
}

impl Catalog {
    /// Opens the catalog in `dir`, reading and checking only the header and
    /// section table; no section is loaded. `Ok(None)` when nothing has been
    /// committed there yet.
    pub fn open(dir: &Path) -> Result<Option<Catalog>, OpenError> {
        let file = match File::open(dir.join(FILE)) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(OpenError::Io(e)),
        };
        let len = file.metadata().map_err(OpenError::Io)?.len();
        let mut head = vec![0; (len as usize).min(TABLE_END)];
        file.read_exact_at(&mut head, 0).map_err(OpenError::Io)?;
        let layout = format::decode_table(&head, len).map_err(OpenError::Decode)?;
        Ok(Some(Catalog {
            layout,
            source: Source::File {
                file,
                sections: Default::default(),
                read: AtomicU64::new(head.len() as u64),
                facts: Facts::default(),
            },
        }))
    }

    /// Validates `bytes` as a whole snapshot file. Every section is loaded.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Catalog, DecodeError> {
        let layout = format::decode(&bytes)?;
        Ok(Catalog {
            layout,
            source: Source::Whole(bytes),
        })
    }

    /// Loads `sections`, and each section they depend on, validating each as
    /// it arrives. A section already loaded is not read again. After this
    /// succeeds, every accessor that reads only these sections is
    /// infallible. A failure leaves whatever loaded before it loaded.
    pub fn load(&self, sections: &[Section]) -> Result<(), OpenError> {
        sections.iter().try_for_each(|&s| self.load_one(s))
    }

    /// Loads every section: what a caller that reads the whole generation
    /// (the writer's previous generation, a test) needs.
    pub fn load_all(&self) -> Result<(), OpenError> {
        self.load(&SECTIONS)
    }

    fn load_one(&self, section: Section) -> Result<(), OpenError> {
        let Source::File {
            file,
            sections,
            read,
            facts,
        } = &self.source
        else {
            return Ok(());
        };
        let slot = &sections[section as usize];
        if slot.get().is_some() {
            return Ok(());
        }
        self.load(section.needs())?;
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
        let _ = slot.set(bytes);
        Ok(())
    }

    /// Whether `section` is loaded. A reader from [`Catalog::from_bytes`]
    /// has every section.
    pub fn is_loaded(&self, section: Section) -> bool {
        match &self.source {
            Source::Whole(_) => true,
            Source::File { sections, .. } => sections[section as usize].get().is_some(),
        }
    }

    /// Bytes read from the file so far: the head of the file and each loaded
    /// section. For a reader from [`Catalog::from_bytes`], the whole file.
    pub fn bytes_read(&self) -> u64 {
        match &self.source {
            Source::Whole(bytes) => bytes.len() as u64,
            Source::File { read, .. } => read.load(Ordering::Relaxed),
        }
    }

    fn column(&self, column: Column) -> View<'_> {
        self.layout.view(column, self.section(column.section()))
    }

    fn blocked(&self, column: Column) -> Blocked<'_> {
        self.layout.blocked(column, self.section(column.section()))
    }

    fn section(&self, section: Section) -> &[u8] {
        match &self.source {
            Source::Whole(bytes) => self.layout.section(bytes, section),
            Source::File { sections, .. } => sections[section as usize]
                .get()
                .unwrap_or_else(|| panic!("catalog section {section:?} read before it was loaded")),
        }
    }

    // ── counts and header ──

    /// The sniffer version this generation's content states were found with
    /// (D37).
    pub fn sniffer_version(&self) -> u32 {
        self.layout.sniffer
    }

    /// The next `DocId` a new document will get. Never decreases (D36 B).
    pub fn next_doc(&self) -> DocId {
        DocId(self.layout.next_doc)
    }

    /// Directories, which are inodes `0..dir_count()`.
    pub fn dir_count(&self) -> u32 {
        self.layout.dirs as u32
    }

    /// All inode rows: directories, then files and symlinks.
    pub fn inode_count(&self) -> u32 {
        self.layout.inodes as u32
    }

    /// Name edges.
    pub fn name_count(&self) -> u32 {
        self.layout.names as u32
    }

    /// Live documents. Known from the head of the file; loads nothing.
    pub fn doc_count(&self) -> u32 {
        self.layout.docs as u32
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
        self.section(Section::NameHeap)
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
            heap: self.name_heap(),
            offsets: self.layout.blocked(Column::NameOffset, rows),
            parents: self.layout.blocked(Column::NameParent, rows),
            children: self.layout.blocked(Column::NameChild, rows),
            count: self.layout.names,
        }
    }

    /// One name edge. Needs [`Section::Names`].
    pub fn name(&self, id: NameId) -> Name<'_> {
        self.name_reader().get(id)
    }

    /// Where a name's bytes start in [`Catalog::name_heap`]. Needs
    /// [`Section::Names`]; the bytes run to the next name's start, less its
    /// NUL.
    pub fn name_start(&self, id: NameId) -> usize {
        self.blocked(Column::NameOffset).get(id.0 as usize) as usize
    }

    /// The inode a name edge names, without reading its bytes. Needs only
    /// [`Section::Names`].
    pub fn child(&self, id: NameId) -> InoId {
        InoId(self.blocked(Column::NameChild).get(id.0 as usize) as u32)
    }

    /// The name whose bytes (or terminator) hold heap offset `offset`, or
    /// `None` past the end of the heap. Needs [`Section::Names`].
    pub fn name_at(&self, offset: usize) -> Option<NameId> {
        if offset >= self.name_heap().len() {
            return None;
        }
        let offsets = self.blocked(Column::NameOffset);
        let after = partition_point(self.layout.names, |i| offsets.get(i) as usize <= offset);
        Some(NameId(after as u32 - 1))
    }

    /// The names in directory `dir`, in name order. Needs
    /// [`Section::Names`].
    pub fn children(&self, dir: InoId) -> impl Iterator<Item = NameId> + use<> {
        let (start, end) = self.child_range(dir);
        (start as u32..end as u32).map(NameId)
    }

    /// Children, visible and ignored, with raw names, kinds and optional stat
    /// rows. Needs Names and Links. Ignored rows never load stat columns.
    pub fn entries(&self, dir: InoId) -> impl Iterator<Item = Entry<'_>> {
        self.children(dir).map(|id| self.entry(id))
    }

    /// One typed name row. Needs Names and Links for a visible non-directory.
    pub fn entry(&self, id: NameId) -> Entry<'_> {
        let name = self.name(id);
        let target = name.target();
        let kind = match target {
            Target::Inode(inode) => self.kind(inode),
            Target::Ignored(kind) => kind,
        };
        Entry {
            name: id,
            bytes: name.bytes,
            kind,
            target,
        }
    }

    /// Where to read a directory's children. Needs Entries for an inode
    /// target. Unknown counts designate unreadable directories.
    pub fn contents(&self, target: Target) -> Option<Contents> {
        match target {
            Target::Ignored(Kind::Dir) => Some(Contents::Ignored),
            Target::Inode(dir) if dir.0 < self.dir_count() => {
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
    /// Needs Roots and Names. Does not follow symlinks or `..`. A path below
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
            rest = rest.strip_prefix(b"/").unwrap_or(rest);
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
                if dir.0 >= self.dir_count() {
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
        (
            start,
            start + partition_point(self.layout.names - start, |i| parents.get(start + i) <= dir),
        )
    }

    /// The entry called `name` in directory `dir`. Needs [`Section::Names`].
    pub fn lookup(&self, dir: InoId, name: &[u8]) -> Option<NameId> {
        let (start, end) = self.child_range(dir);
        let bytes = |i: usize| self.name(NameId((start + i) as u32)).bytes;
        let at = partition_point(end - start, |i| bytes(i) < name);
        (at < end - start && bytes(at) == name).then_some(NameId((start + at) as u32))
    }

    /// A directory's own name edge; `None` for a root (D30). Needs
    /// [`Section::DirNames`].
    pub fn dir_name(&self, dir: InoId) -> Option<NameId> {
        self.column(Column::DirName)
            .nullable(dir.0 as usize)
            .map(|name| NameId(name as u32))
    }

    /// Whether a directory is a structural row: walked through for
    /// re-included entries, but not catalogued itself (D29). Its name is in
    /// the heap; a query should not report it. Needs [`Section::Traversed`].
    pub fn is_traversed(&self, dir: InoId) -> bool {
        let bits = self.section(Section::Traversed);
        bits[dir.0 as usize / 8] >> (dir.0 % 8) & 1 == 1
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
        let (names, dir_names) = (self.name_reader(), self.column(Column::DirName));
        let mut up = Vec::new();
        let mut at = dir;
        while let Some(name) = dir_names.nullable(at.0 as usize) {
            let (parent, bytes) = names.edge(NameId(name as u32));
            up.push(bytes);
            // Decoding checked that the parent's id is lower, so this ends.
            at = parent;
        }
        out.extend_from_slice(self.root_path(at).unwrap_or_default());
        for name in up.iter().rev() {
            push_component(out, name);
        }
    }

    /// The configured roots, as `(top directory, path)`. Needs
    /// [`Section::Roots`].
    pub fn roots(&self) -> impl Iterator<Item = (InoId, &[u8])> + '_ {
        self.section(Section::Roots)
            .chunks_exact(PAIR_ROW)
            .map(|pair| (InoId(u32_at(pair, 0)), self.string(u32_at(pair, 4))))
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

    // ── inodes ──

    /// One inode's whole row. Needs every section in [`Section::INODE`].
    pub fn inode(&self, id: InoId) -> Inode {
        let (owner, i) = (self.owner(id), id.0 as usize);
        let stat = Stat {
            dev: self.column(Column::Dev).lookup(i),
            ino: self.blocked(Column::Ino).get(i),
            size: self.size(id),
            mtime_sec: self.mtime(id),
            mtime_nsec: self.blocked(Column::MtimeNs).get(i) as u32,
            ctime_sec: self.ctime(id),
            ctime_nsec: self.blocked(Column::CtimeNs).get(i) as u32,
            mode: self.mode(id),
            uid: owner.0,
            gid: owner.1,
            nlink: self.nlink(id),
        };
        Inode {
            stat,
            state: self.state(id),
            doc: self.doc(id),
        }
    }

    /// An inode's `(st_dev, st_ino)`. Needs [`Section::Dev`] and
    /// [`Section::Ino`].
    pub(crate) fn identity(&self, id: InoId) -> (u64, u64) {
        let i = id.0 as usize;
        (
            self.column(Column::Dev).lookup(i),
            self.blocked(Column::Ino).get(i),
        )
    }

    /// An inode's `st_size`. Needs [`Section::Size`].
    pub fn size(&self, id: InoId) -> u64 {
        self.blocked(Column::Size).get(id.0 as usize)
    }

    /// An inode's mtime, in whole seconds. Needs [`Section::Mtime`].
    pub fn mtime(&self, id: InoId) -> i64 {
        format::unorder(self.blocked(Column::Mtime).get(id.0 as usize))
    }

    /// The `st_size`s of inodes `64 * run` onwards, up to 64 of them, decoded
    /// together: one word of a bitset over inodes, for a pass that skips the
    /// words it has already cleared. Empty past the last inode. Needs
    /// [`Section::Size`].
    pub fn size_run<'o>(&self, run: usize, out: &'o mut [u64; RUN]) -> &'o [u64] {
        let out = &mut out[..self.run_len(run)];
        self.blocked(Column::Size)
            .decode(run.saturating_mul(RUN), out);
        out
    }

    /// The mtimes, in whole seconds, of inodes `64 * run` onwards: as
    /// [`Catalog::size_run`]. Needs [`Section::Mtime`].
    pub fn mtime_run<'o>(&self, run: usize, out: &'o mut [i64; RUN]) -> &'o [i64] {
        let mut raw = [0; RUN];
        let raw = &mut raw[..self.run_len(run)];
        self.blocked(Column::Mtime)
            .decode(run.saturating_mul(RUN), raw);
        for (time, &value) in out.iter_mut().zip(&*raw) {
            *time = format::unorder(value);
        }
        &out[..raw.len()]
    }

    /// How many inodes run `run` of [`Catalog::size_run`] holds.
    fn run_len(&self, run: usize) -> usize {
        (self.inode_count() as usize)
            .saturating_sub(run.saturating_mul(RUN))
            .min(RUN)
    }

    /// An inode's ctime, in whole seconds. Needs [`Section::Ctime`].
    pub fn ctime(&self, id: InoId) -> i64 {
        format::unorder(self.blocked(Column::Ctime).get(id.0 as usize))
    }

    /// An inode's `st_mode`: type and permission bits. Needs
    /// [`Section::Mode`].
    pub fn mode(&self, id: InoId) -> u32 {
        self.column(Column::Mode).lookup(id.0 as usize) as u32
    }

    /// An inode's `(st_uid, st_gid)`. Needs [`Section::Owner`].
    pub fn owner(&self, id: InoId) -> (u32, u32) {
        let pair = self.column(Column::Owner).lookup(id.0 as usize);
        ((pair >> 32) as u32, pair as u32)
    }

    /// An inode's `st_nlink`: for a directory, as the filesystem counts it,
    /// ignored children included (D47). Needs [`Section::Nlink`].
    pub fn nlink(&self, id: InoId) -> u64 {
        self.blocked(Column::Nlink).get(id.0 as usize)
    }

    /// An inode's document, when its state is `Hashed`. Needs
    /// [`Section::Doc`].
    pub fn doc(&self, id: InoId) -> Option<DocId> {
        self.blocked(Column::Doc)
            .nullable(id.0 as usize)
            .map(|doc| DocId(doc as u32))
    }

    /// What the last observation learnt about an inode's content (D37).
    /// Needs [`Section::States`].
    pub fn state(&self, id: InoId) -> ContentState {
        let byte = self.section(Section::States)[id.0 as usize / 4];
        ContentState::from_bits(byte >> (id.0 % 4 * 2))
    }

    /// The entries the walk's `getdents` returned for directory `dir`, minus
    /// `.` and `..`, counted before ignore rules dropped any (D47). `None`
    /// when the walk did not list it: unreadable, listing failed partway, or
    /// carried from a generation that did not know. Needs
    /// [`Section::Entries`].
    pub fn entry_count(&self, dir: InoId) -> Option<u32> {
        self.blocked(Column::Entries)
            .nullable(dir.0 as usize)
            .map(|count| count as u32)
    }

    /// Whether an inode is a directory, file or symlink. Needs
    /// [`Section::Links`] for any inode that is not a directory.
    pub fn kind(&self, id: InoId) -> Kind {
        if id.0 < self.dir_count() {
            Kind::Dir
        } else if self.link_target(id).is_some() {
            Kind::Symlink
        } else {
            special_kind(self.section(Section::Specials), id).unwrap_or(Kind::File)
        }
    }

    /// [`Catalog::kind`] for inodes asked for mostly in rising order, as a
    /// pass over names or inodes asks: each search starts where the last
    /// ended. Needs [`Section::Links`].
    pub fn kinds(&self) -> Kinds<'_> {
        Kinds {
            dirs: self.dir_count(),
            links: self.section(Section::Links),
            specials: self.section(Section::Specials),
            at: 0,
        }
    }

    /// A symlink's target, as `readlink` returned it. Needs
    /// [`Section::Links`], which loads [`Section::Strings`].
    pub fn link_target(&self, id: InoId) -> Option<&[u8]> {
        let links = self.section(Section::Links);
        let n = links.len() / PAIR_ROW;
        let at = partition_point(n, |i| u32_at(links, i * PAIR_ROW) < id.0);
        (at < n && u32_at(links, at * PAIR_ROW) == id.0)
            .then(|| self.string(u32_at(links, at * PAIR_ROW + 4)))
    }

    // ── work trees and documents ──

    /// Every work tree whose top is at or below a root, by top directory.
    /// Needs [`Section::WorkTrees`].
    pub fn work_trees(&self) -> impl Iterator<Item = WorkTree<'_>> + '_ {
        self.section(Section::WorkTrees)
            .chunks_exact(WORK_TREE_ROW)
            .map(|row| self.work_tree_row(row))
    }

    /// The work tree whose top is `dir`, if it is one. Needs
    /// [`Section::WorkTrees`].
    pub fn work_tree(&self, dir: InoId) -> Option<WorkTree<'_>> {
        let rows = self.section(Section::WorkTrees);
        let n = rows.len() / WORK_TREE_ROW;
        let at = partition_point(n, |i| u32_at(rows, i * WORK_TREE_ROW) < dir.0);
        (at < n)
            .then(|| self.work_tree_row(&rows[at * WORK_TREE_ROW..][..WORK_TREE_ROW]))
            .filter(|wt| wt.dir == dir)
    }

    fn work_tree_row(&self, row: &[u8]) -> WorkTree<'_> {
        WorkTree {
            dir: InoId(u32_at(row, 0)),
            // Decoding checked the kind byte.
            kind: WorkTreeKind::from_byte(row[24]).unwrap_or(WorkTreeKind::Main),
            common_id: (u64_at(row, 8), u64_at(row, 16)),
            common_dir: self.string(u32_at(row, 4)),
        }
    }

    /// Every live document with its hash, by id. Needs [`Section::Docs`].
    pub fn docs(&self) -> impl Iterator<Item = (DocId, Hash)> + '_ {
        let ids = self.column(Column::DocId);
        (0..self.layout.docs).map(move |row| (DocId(ids.sequence(row) as u32), self.hash(row)))
    }

    /// A live document's hash; `None` if the id is dead or never assigned.
    /// Needs [`Section::Docs`]. Where the generation's ids have no holes, the
    /// row is the id less the first; otherwise a binary search.
    pub fn doc_hash(&self, doc: DocId) -> Option<Hash> {
        let (ids, n, doc) = (
            self.column(Column::DocId),
            self.layout.docs,
            u64::from(doc.0),
        );
        let at = match ids.dense() {
            Some(first) => doc
                .checked_sub(first)
                .map_or(n, |row| row.min(n as u64) as usize),
            None => partition_point(n, |i| ids.sequence(i) < doc),
        };
        (at < n && ids.sequence(at) == doc).then(|| self.hash(at))
    }

    /// Document row `row`'s hash.
    fn hash(&self, row: usize) -> Hash {
        let at = self.layout.hashes + row * HASH_ROW;
        let mut hash = [0; HASH_ROW];
        hash.copy_from_slice(&self.section(Section::Docs)[at..at + HASH_ROW]);
        hash
    }
}

/// The name columns and the name heap of one generation, resolved once
/// ([`Catalog::name_reader`]), so that a query reading many names looks up
/// no section per name.
#[derive(Clone, Copy)]
pub struct NameReader<'c> {
    heap: &'c [u8],
    offsets: Blocked<'c>,
    parents: Blocked<'c>,
    children: Blocked<'c>,
    count: usize,
}

impl<'c> NameReader<'c> {
    /// One name edge, read by itself: for a sparse hit.
    pub fn get(&self, id: NameId) -> Name<'c> {
        let (parent, bytes) = self.edge(id);
        Name {
            parent,
            child: InoId(self.children.get(id.0 as usize) as u32),
            bytes,
        }
    }

    /// A name's directory and bytes, without its child: what a path reads
    /// at each level.
    pub fn edge(&self, id: NameId) -> (InoId, &'c [u8]) {
        let i = id.0 as usize;
        // A name runs to the next one's start, less its NUL: decoding checked
        // that the spans tile the heap. One decode is cheaper than a search.
        let end = match i + 1 < self.count {
            true => self.offsets.get(i + 1) as usize,
            false => self.heap.len(),
        };
        (
            InoId(self.parents.get(i) as u32),
            &self.heap[self.offsets.get(i) as usize..end - 1],
        )
    }

    /// The names from `from` on, in order, decoded a run at a time.
    pub fn runs_from(&self, from: NameId) -> NameRuns<'c> {
        NameRuns {
            names: *self,
            next: from.0 as usize,
            first: 0,
            len: 0,
            offsets: [0; NAME_RUN + 1],
            parents: [0; NAME_RUN],
            children: [0; NAME_RUN],
        }
    }

    /// Every name's child, in name order: a pass over the child column
    /// alone.
    pub fn children(&self) -> impl Iterator<Item = InoId> + 'c {
        let children = self.children;
        packed::runs(self.count, move |first, out| children.decode(first, out))
            .map(|child| InoId(child as u32))
    }
}

/// Rows a [`NameRuns`] decodes at once: one block of the offset column.
const NAME_RUN: usize = packed::BLOCK_ROWS;

/// Names in order from a [`NameReader`], each column decoded a run at a
/// time. A name's end is the next one's start, so each offset is decoded
/// once: the run decodes one offset past its last row.
pub struct NameRuns<'c> {
    names: NameReader<'c>,
    /// The next name to yield.
    next: usize,
    /// The decoded run: rows `first..first + len`.
    first: usize,
    len: usize,
    offsets: [u64; NAME_RUN + 1],
    parents: [u64; NAME_RUN],
    children: [u64; NAME_RUN],
}

impl NameRuns<'_> {
    /// Decodes the run holding `next`, starting at its block's first row.
    fn fill(&mut self) {
        let names = &self.names;
        let first = self.next / NAME_RUN * NAME_RUN;
        let len = NAME_RUN.min(names.count - first);
        let more = first + len < names.count;
        names
            .offsets
            .decode(first, &mut self.offsets[..len + usize::from(more)]);
        if !more {
            self.offsets[len] = names.heap.len() as u64;
        }
        names.parents.decode(first, &mut self.parents[..len]);
        names.children.decode(first, &mut self.children[..len]);
        (self.first, self.len) = (first, len);
    }
}

impl<'c> Iterator for NameRuns<'c> {
    type Item = (NameId, Name<'c>);

    fn next(&mut self) -> Option<Self::Item> {
        let i = self.next;
        if i >= self.names.count {
            return None;
        }
        if i >= self.first + self.len || i < self.first {
            self.fill();
        }
        let j = i - self.first;
        self.next += 1;
        let (start, end) = (self.offsets[j] as usize, self.offsets[j + 1] as usize);
        Some((
            NameId(i as u32),
            Name {
                parent: InoId(self.parents[j] as u32),
                child: InoId(self.children[j] as u32),
                bytes: &self.names.heap[start..end - 1],
            },
        ))
    }
}

/// [`Catalog::kind`] with a memory of where the last search ended
/// ([`Catalog::kinds`]): a rising id gallops forwards from there, and one
/// that falls searches again. Names in order ask for rising file ids, since
/// files are numbered by their first name.
pub struct Kinds<'c> {
    dirs: u32,
    links: &'c [u8],
    specials: &'c [u8],
    /// The first link row whose inode is at or past the last id asked for.
    at: usize,
}

impl Kinds<'_> {
    /// Whether an inode is a directory, file or symlink.
    pub fn kind(&mut self, id: InoId) -> Kind {
        if id.0 < self.dirs {
            return Kind::Dir;
        }
        if let Some(kind) = special_kind(self.specials, id) {
            return kind;
        }
        let links = self.links;
        let n = links.len() / PAIR_ROW;
        let key = |i: usize| u32_at(links, i * PAIR_ROW);
        let below = |i: usize| key(i) < id.0;
        if self.at > 0 && !below(self.at - 1) {
            self.at = partition_point(self.at, below);
        } else {
            // Gallop: `lo` is at or before the answer, `hi` past it or `n`.
            let (mut lo, mut hi, mut step) = (self.at, self.at, 1);
            while hi < n && below(hi) {
                lo = hi + 1;
                hi = (hi + step).min(n);
                step *= 2;
            }
            self.at = lo + partition_point(hi - lo, |i| below(lo + i));
        }
        match self.at < n && key(self.at) == id.0 {
            true => Kind::Symlink,
            false => Kind::File,
        }
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

fn special_kind(rows: &[u8], id: InoId) -> Option<Kind> {
    let count = rows.len() / PAIR_ROW;
    let at = partition_point(count, |i| u32_at(rows, i * PAIR_ROW) < id.0);
    (at < count && u32_at(rows, at * PAIR_ROW) == id.0).then(|| {
        Kind::from_ignored_child(crate::format::NONE - 1 - u32_at(rows, at * PAIR_ROW + 4))
            .unwrap_or(Kind::File)
    })
}
