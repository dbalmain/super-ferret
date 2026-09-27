//! [`Catalog`]: one opened generation, and everything a query reads from it.
//!
//! Opening reads the snapshot into memory and validates it; nothing else is
//! derived (D30 C). Accessors decode fixed-width rows in place. The catalog
//! applies no matching: `ferret-query` scans [`Catalog::name_heap`] or
//! iterates [`Catalog::names`], and comes back here with the `NameId`s it hit.
//!
//! Ids passed in must come from this generation; an id out of range panics,
//! as indexing a slice does. Anything read from the file cannot panic: that is
//! what decoding validated.

use std::fmt;
use std::io;
use std::path::Path;

use crate::batch::{Stat, WorkTreeKind};
use crate::format::{
    self, DOC_ROW, INODE_ROW, Layout, NAME_ROW, NONE, PAIR_ROW, Section, WORK_TREE_ROW, u32_at,
    u64_at,
};
use crate::{ContentState, DecodeError, DocId, Hash, InoId, NameId};

/// The snapshot's file name inside the catalog directory.
pub(crate) const FILE: &str = "catalog";

/// Why [`Catalog::open`] failed.
#[derive(Debug)]
pub enum OpenError {
    /// Reading the file failed.
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

/// One generation of the catalog, read into memory. It stays valid however
/// many commits follow.
pub struct Catalog {
    bytes: Vec<u8>,
    layout: Layout,
}

impl Catalog {
    /// Reads and validates the catalog in `dir`. `Ok(None)` when nothing has
    /// been committed there yet.
    pub fn open(dir: &Path) -> Result<Option<Catalog>, OpenError> {
        match std::fs::read(dir.join(FILE)) {
            Ok(bytes) => Self::from_bytes(bytes).map(Some).map_err(OpenError::Decode),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(OpenError::Io(e)),
        }
    }

    /// Validates `bytes` as a snapshot file.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Catalog, DecodeError> {
        let layout = format::decode(&bytes)?;
        Ok(Catalog { bytes, layout })
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn section(&self, section: Section) -> &[u8] {
        self.layout.section(&self.bytes, section)
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

    /// Live documents.
    pub fn doc_count(&self) -> u32 {
        (self.section(Section::Docs).len() / DOC_ROW) as u32
    }

    // ── names ──

    /// Every name, NUL-terminated, in `NameId` order: the bytes a filename
    /// scan reads (D14, D28). A NUL never occurs inside a name, so a literal
    /// match cannot straddle two; [`Catalog::name_at`] maps a match's offset
    /// back to its name.
    pub fn name_heap(&self) -> &[u8] {
        self.section(Section::NameHeap)
    }

    /// Every name with its id, in heap order.
    pub fn names(&self) -> impl Iterator<Item = (NameId, &[u8])> + '_ {
        (0..self.name_count()).map(|i| (NameId(i), self.name(NameId(i)).bytes))
    }

    /// One name edge.
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

    /// The name whose bytes (or terminator) hold heap offset `offset`, or
    /// `None` past the end of the heap.
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

    /// The names in directory `dir`, in name order.
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

    /// The entry called `name` in directory `dir`.
    pub fn lookup(&self, dir: InoId, name: &[u8]) -> Option<NameId> {
        let (start, end) = self.child_range(dir);
        let bytes = |i: usize| self.name(NameId((start + i) as u32)).bytes;
        let at = partition_point(end - start, |i| bytes(i) < name);
        (at < end - start && bytes(at) == name).then_some(NameId((start + at) as u32))
    }

    /// A directory's own name edge; `None` for a root (D30).
    pub fn dir_name(&self, dir: InoId) -> Option<NameId> {
        let name = u32_at(self.section(Section::DirNames), dir.0 as usize * 4);
        (name != NONE).then_some(NameId(name))
    }

    /// Whether a directory is a structural row: walked through for
    /// re-included entries, but not catalogued itself (D29). Its name is in
    /// the heap; a query should not report it.
    pub fn is_traversed(&self, dir: InoId) -> bool {
        let bits = self.section(Section::Traversed);
        bits[dir.0 as usize / 8] >> (dir.0 % 8) & 1 == 1
    }

    // ── paths ──

    /// Appends the path of the entry `name` names: its root's path, then each
    /// name below it, joined by `/`.
    pub fn path(&self, name: NameId, out: &mut Vec<u8>) {
        let name = self.name(name);
        self.dir_path(name.parent, out);
        push_component(out, name.bytes);
    }

    /// Appends a directory's path.
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

    /// The configured roots, as `(top directory, path)`.
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

    /// One inode row.
    pub fn inode(&self, id: InoId) -> Inode {
        let row = &self.section(Section::Inodes)[id.0 as usize * INODE_ROW..][..INODE_ROW];
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

    /// Whether an inode is a directory, file or symlink.
    pub fn kind(&self, id: InoId) -> Kind {
        if id.0 < self.dir_count() {
            Kind::Dir
        } else if self.link_target(id).is_some() {
            Kind::Symlink
        } else {
            Kind::File
        }
    }

    /// A symlink's target, as `readlink` returned it.
    pub fn link_target(&self, id: InoId) -> Option<&[u8]> {
        let links = self.section(Section::Links);
        let n = links.len() / PAIR_ROW;
        let at = partition_point(n, |i| u32_at(links, i * PAIR_ROW) < id.0);
        (at < n && u32_at(links, at * PAIR_ROW) == id.0)
            .then(|| self.string(u32_at(links, at * PAIR_ROW + 4)))
    }

    // ── work trees and documents ──

    /// Every work tree whose top is at or below a root, by top directory.
    pub fn work_trees(&self) -> impl Iterator<Item = WorkTree<'_>> + '_ {
        self.section(Section::WorkTrees)
            .chunks_exact(WORK_TREE_ROW)
            .map(|row| self.work_tree_row(row))
    }

    /// The work tree whose top is `dir`, if it is one.
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

    /// Every live document with its hash, by id.
    pub fn docs(&self) -> impl Iterator<Item = (DocId, Hash)> + '_ {
        self.section(Section::Docs)
            .chunks_exact(DOC_ROW)
            .map(doc_row)
    }

    /// A live document's hash; `None` if the id is dead or never assigned.
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
