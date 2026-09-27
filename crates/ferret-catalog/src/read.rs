//! [`Catalog`]: one opened generation, and everything a query reads from it.
//!
//! Opening reads the header and the section table; each section is read with
//! a positional read on first use and validated then (D38 B). Nothing else
//! is derived (D30 C). Accessors decode fixed-width rows in place. The
//! catalog applies no matching: `ferret-query` scans [`Catalog::name_heap`]
//! or iterates [`Catalog::names`], and comes back here with the `NameId`s it
//! hit.
//!
//! Loading is explicit and fallible: [`Catalog::load`] reads and validates
//! the sections a caller names, with any they depend on. Every accessor after
//! that is infallible. Reading a section that was never loaded panics, like
//! an id out of range: both are caller bugs. [`Catalog::read_inode`] is the
//! one fallible accessor; it reads a single inode row when the whole section
//! would cost more than the query needs.
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
    self, DOC_ROW, INODE_ROW, Layout, NAME_ROW, NONE, PAIR_ROW, SECTIONS, Section, TABLE_END,
    WORK_TREE_ROW, u32_at, u64_at,
};
use crate::{ContentState, DecodeError, DocId, Hash, InoId, NameId};

/// The snapshot's file name inside the catalog directory.
pub(crate) const FILE: &str = "catalog";

/// Why [`Catalog::open`], [`Catalog::load`] or [`Catalog::read_inode`]
/// failed.
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
}

/// One name edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Name<'a> {
    /// The directory holding the name.
    pub parent: InoId,
    /// The inode it names.
    pub child: InoId,
    /// The name, as the kernel returned it; never contains NUL or `/`.
    pub bytes: &'a [u8],
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
        format::check(section, &self.layout, |s| {
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

    /// Bytes read from the file so far: the header and table, each loaded
    /// section and each single row [`Catalog::read_inode`] read. For a
    /// reader from [`Catalog::from_bytes`], the whole file.
    pub fn bytes_read(&self) -> u64 {
        match &self.source {
            Source::Whole(bytes) => bytes.len() as u64,
            Source::File { read, .. } => read.load(Ordering::Relaxed),
        }
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

    /// Live documents. Known from the section table; loads nothing.
    pub fn doc_count(&self) -> u32 {
        let (start, end) = self.layout.range(Section::Docs);
        ((end - start) / DOC_ROW) as u32
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
        (0..self.name_count()).map(|i| (NameId(i), self.name(NameId(i)).bytes))
    }

    /// One name edge. Needs [`Section::Names`].
    pub fn name(&self, id: NameId) -> Name<'_> {
        let row = &self.section(Section::Names)[id.0 as usize * NAME_ROW..][..NAME_ROW];
        let heap = self.name_heap();
        let start = u32_at(row, 8) as usize;
        let len = heap[start..].iter().position(|&b| b == 0).unwrap_or(0);
        Name {
            parent: InoId(u32_at(row, 0)),
            child: InoId(u32_at(row, 4)),
            bytes: &heap[start..start + len],
        }
    }

    /// Where a name's bytes start in [`Catalog::name_heap`]. Needs
    /// [`Section::Names`]; the bytes run to the next name's start, less its
    /// NUL.
    pub fn name_start(&self, id: NameId) -> usize {
        u32_at(self.section(Section::Names), id.0 as usize * NAME_ROW + 8) as usize
    }

    /// The inode a name edge names, without reading its bytes. Needs only
    /// [`Section::Names`].
    pub fn child(&self, id: NameId) -> InoId {
        InoId(u32_at(
            self.section(Section::Names),
            id.0 as usize * NAME_ROW + 4,
        ))
    }

    /// The name whose bytes (or terminator) hold heap offset `offset`, or
    /// `None` past the end of the heap. Needs [`Section::Names`].
    pub fn name_at(&self, offset: usize) -> Option<NameId> {
        if offset >= self.name_heap().len() {
            return None;
        }
        let rows = self.section(Section::Names);
        let after = partition_point(self.layout.names, |i| {
            u32_at(rows, i * NAME_ROW + 8) as usize <= offset
        });
        Some(NameId(after as u32 - 1))
    }

    /// The names in directory `dir`, in name order. Needs
    /// [`Section::Names`].
    pub fn children(&self, dir: InoId) -> impl Iterator<Item = NameId> + use<> {
        let (start, end) = self.child_range(dir);
        (start as u32..end as u32).map(NameId)
    }

    fn child_range(&self, dir: InoId) -> (usize, usize) {
        let rows = self.section(Section::Names);
        let parent = |i: usize| u32_at(rows, i * NAME_ROW);
        let start = partition_point(self.layout.names, |i| parent(i) < dir.0);
        (
            start,
            start + partition_point(self.layout.names - start, |i| parent(start + i) <= dir.0),
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
        let name = u32_at(self.section(Section::DirNames), dir.0 as usize * 4);
        (name != NONE).then_some(NameId(name))
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
        let name = self.name(name);
        self.dir_path(name.parent, out);
        push_component(out, name.bytes);
    }

    /// Appends a directory's path. Needs [`Section::Roots`].
    pub fn dir_path(&self, dir: InoId, out: &mut Vec<u8>) {
        let mut up = Vec::new();
        let mut at = dir;
        while let Some(name) = self.dir_name(at) {
            up.push(name);
            // Decoding checked that the parent's id is lower, so this ends.
            at = self.name(name).parent;
        }
        out.extend_from_slice(self.root_path(at).unwrap_or_default());
        for &name in up.iter().rev() {
            push_component(out, self.name(name).bytes);
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

    /// One inode row. Needs [`Section::Inodes`], which loads
    /// [`Section::States`].
    pub fn inode(&self, id: InoId) -> Inode {
        let row = &self.section(Section::Inodes)[id.0 as usize * INODE_ROW..][..INODE_ROW];
        self.decode_inode(id, row)
    }

    /// One inode row, read alone from the file when [`Section::Inodes`] is
    /// not loaded: a query that reports a few rows pays a few positional
    /// reads rather than the whole section (64 B per inode). Loads
    /// [`Section::States`], which is 2 bits per inode. An inode row indexes
    /// nothing, so it needs no validation.
    pub fn read_inode(&self, id: InoId) -> Result<Inode, OpenError> {
        self.load(&[Section::States])?;
        let Source::File { file, read, .. } = &self.source else {
            return Ok(self.inode(id));
        };
        if self.is_loaded(Section::Inodes) {
            return Ok(self.inode(id));
        }
        assert!(id.0 < self.inode_count(), "inode {id:?} out of range");
        let mut row = [0; INODE_ROW];
        let at = self.layout.range(Section::Inodes).0 + id.0 as usize * INODE_ROW;
        file.read_exact_at(&mut row, at as u64)
            .map_err(OpenError::Io)?;
        read.fetch_add(INODE_ROW as u64, Ordering::Relaxed);
        Ok(self.decode_inode(id, &row))
    }

    fn decode_inode(&self, id: InoId, row: &[u8]) -> Inode {
        let i64_at = |at| u64_at(row, at) as i64;
        let stat = Stat {
            dev: u64_at(row, 0),
            ino: u64_at(row, 8),
            size: u64_at(row, 16),
            mtime_sec: i64_at(24),
            ctime_sec: i64_at(32),
            mtime_nsec: u32_at(row, 40),
            ctime_nsec: u32_at(row, 44),
            mode: u32_at(row, 48),
            uid: u32_at(row, 52),
            gid: u32_at(row, 56),
        };
        let doc = u32_at(row, 60);
        let state_byte = self.section(Section::States)[id.0 as usize / 4];
        let state = ContentState::from_bits(state_byte >> (id.0 % 4 * 2));
        Inode {
            stat,
            state,
            doc: (doc != NONE).then_some(DocId(doc)),
        }
    }

    /// Whether an inode is a directory, file or symlink. Needs
    /// [`Section::Links`] for any inode that is not a directory.
    pub fn kind(&self, id: InoId) -> Kind {
        if id.0 < self.dir_count() {
            Kind::Dir
        } else if self.link_target(id).is_some() {
            Kind::Symlink
        } else {
            Kind::File
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
        self.section(Section::Docs)
            .chunks_exact(DOC_ROW)
            .map(doc_row)
    }

    /// A live document's hash; `None` if the id is dead or never assigned.
    /// Needs [`Section::Docs`].
    pub fn doc_hash(&self, doc: DocId) -> Option<Hash> {
        let rows = self.section(Section::Docs);
        let n = rows.len() / DOC_ROW;
        let at = partition_point(n, |i| u32_at(rows, i * DOC_ROW) < doc.0);
        (at < n)
            .then(|| doc_row(&rows[at * DOC_ROW..][..DOC_ROW]))
            .filter(|&(id, _)| id == doc)
            .map(|(_, h)| h)
    }
}

fn doc_row(row: &[u8]) -> (DocId, Hash) {
    let mut hash = [0; 16];
    hash.copy_from_slice(&row[4..20]);
    (DocId(u32_at(row, 0)), hash)
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
