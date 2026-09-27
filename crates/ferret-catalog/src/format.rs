//! The snapshot file: one generation of the catalog.
//!
//! ```text
//! header   magic "FERRETCT" | version u32 | sniffer u32 | next_doc u32 |
//!          section count u32                                    (24 B)
//! table    (offset u64, length u64) per section, in SECTIONS order (176 B)
//! sections contiguous from byte 200 to the end of the file, in order
//! ```
//!
//! All integers are little-endian and every row is fixed-width. The sections
//! a name query reads come first, so a reader that ever loads sections lazily
//! can stop early (D30).
//!
//! ```text
//! names       12 B  parent InoId, child InoId, offset in name heap
//! name heap    1 B  names, each NUL-terminated, in NameId order (D28)
//! dir names    4 B  per directory InoId: its NameId, or NONE for a root
//! traversed  1 bit  per directory: a structural row (D29), LSB first
//! roots        8 B  root InoId, offset of its path in strings
//! strings      1 B  root paths, link targets, work-tree paths; NUL-terminated
//! inodes      64 B  dev, ino, size, mtime s, ctime s (u64/i64), mtime ns,
//!                   ctime ns, mode, uid, gid, DocId or NONE (u32)
//! states     2 bit  per inode: ContentState (D37), LSB first
//! links        8 B  symlink InoId, offset of its target in strings
//! work trees  32 B  top InoId, offset of common dir in strings, common dev,
//!                   common ino, kind u8, 7 B zero
//! docs        20 B  DocId, hash; sorted by id, live documents only (D36 B)
//! ```
//!
//! Decoding validates everything an accessor indexes by, so a corrupt or
//! truncated file is a [`DecodeError`] and never a panic: every offset and id
//! is in range, every heap ends in NUL, and each directory's name edge points
//! at a lower-numbered parent, so walking up from any name ends at a root.
//! Field values that index nothing (times, sizes, a `DocId` in an inode row)
//! are not checked; a flipped bit there reads back as a different value.

use std::fmt;

use crate::batch::Stat;
use crate::{ContentState, Hash};

pub(crate) const MAGIC: [u8; 8] = *b"FERRETCT";
pub(crate) const VERSION: u32 = 1;
/// "No id" in any id column.
pub(crate) const NONE: u32 = u32::MAX;

pub(crate) const INODE_ROW: usize = 64;
pub(crate) const NAME_ROW: usize = 12;
pub(crate) const PAIR_ROW: usize = 8;
pub(crate) const WORK_TREE_ROW: usize = 32;
pub(crate) const DOC_ROW: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Section {
    Names,
    NameHeap,
    DirNames,
    Traversed,
    Roots,
    Strings,
    Inodes,
    States,
    Links,
    WorkTrees,
    Docs,
}

pub(crate) const SECTIONS: [Section; 11] = [
    Section::Names,
    Section::NameHeap,
    Section::DirNames,
    Section::Traversed,
    Section::Roots,
    Section::Strings,
    Section::Inodes,
    Section::States,
    Section::Links,
    Section::WorkTrees,
    Section::Docs,
];

const HEADER: usize = 24;
const TABLE: usize = SECTIONS.len() * 16;
const DATA_START: usize = HEADER + TABLE;

/// Why a snapshot file could not be decoded.
#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Not a catalog file, or shorter than its header.
    NotACatalog,
    /// Written by a format version this build does not read.
    Version(u32),
    /// The section table does not tile the file: truncated, extended, or
    /// corrupt.
    Layout,
    /// A section's contents are inconsistent. Names the section.
    Corrupt(&'static str),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotACatalog => write!(f, "not a catalog file"),
            Self::Version(v) => write!(f, "catalog format version {v}, expected {VERSION}"),
            Self::Layout => write!(f, "catalog file truncated or corrupt: bad section table"),
            Self::Corrupt(section) => write!(f, "catalog file corrupt: {section}"),
        }
    }
}

impl std::error::Error for DecodeError {}

// ── tables: what the builder produces and the encoder writes ──

pub(crate) struct NameRow {
    pub(crate) parent: u32,
    pub(crate) child: u32,
    pub(crate) offset: u32,
}

pub(crate) struct WorkTreeRow {
    pub(crate) dir: u32,
    pub(crate) offset: u32,
    pub(crate) common_id: (u64, u64),
    pub(crate) kind: u8,
}

/// One generation, decoded into owned rows. Built by `build`, written by
/// [`encode`].
#[derive(Default)]
pub(crate) struct Tables {
    pub(crate) sniffer: u32,
    pub(crate) next_doc: u32,
    pub(crate) names: Vec<NameRow>,
    pub(crate) name_heap: Vec<u8>,
    pub(crate) dir_names: Vec<u32>,
    pub(crate) traversed: Vec<bool>,
    /// (root InoId, path offset in strings).
    pub(crate) roots: Vec<(u32, u32)>,
    pub(crate) strings: Vec<u8>,
    /// Stat and DocId (or NONE) per inode.
    pub(crate) inodes: Vec<(Stat, u32)>,
    pub(crate) states: Vec<ContentState>,
    /// (symlink InoId, target offset in strings).
    pub(crate) links: Vec<(u32, u32)>,
    pub(crate) work_trees: Vec<WorkTreeRow>,
    pub(crate) docs: Vec<(u32, Hash)>,
}

pub(crate) fn encode(t: &Tables) -> Vec<u8> {
    let mut sections: Vec<Vec<u8>> = Vec::with_capacity(SECTIONS.len());
    for section in SECTIONS {
        let mut out = Vec::new();
        match section {
            Section::Names => {
                out.reserve(t.names.len() * NAME_ROW);
                for row in &t.names {
                    put_u32(&mut out, row.parent);
                    put_u32(&mut out, row.child);
                    put_u32(&mut out, row.offset);
                }
            }
            Section::NameHeap => out.extend_from_slice(&t.name_heap),
            Section::DirNames => t.dir_names.iter().for_each(|&n| put_u32(&mut out, n)),
            Section::Traversed => out = pack_bits(t.traversed.iter().map(|&b| u8::from(b)), 1),
            Section::Roots => t.roots.iter().for_each(|&(a, b)| put_pair(&mut out, a, b)),
            Section::Strings => out.extend_from_slice(&t.strings),
            Section::Inodes => {
                out.reserve(t.inodes.len() * INODE_ROW);
                for (stat, doc) in &t.inodes {
                    put_u64(&mut out, stat.dev);
                    put_u64(&mut out, stat.ino);
                    put_u64(&mut out, stat.size);
                    out.extend_from_slice(&stat.mtime_sec.to_le_bytes());
                    out.extend_from_slice(&stat.ctime_sec.to_le_bytes());
                    put_u32(&mut out, stat.mtime_nsec);
                    put_u32(&mut out, stat.ctime_nsec);
                    put_u32(&mut out, stat.mode);
                    put_u32(&mut out, stat.uid);
                    put_u32(&mut out, stat.gid);
                    put_u32(&mut out, *doc);
                }
            }
            Section::States => out = pack_bits(t.states.iter().map(|&s| s as u8), 2),
            Section::Links => t.links.iter().for_each(|&(a, b)| put_pair(&mut out, a, b)),
            Section::WorkTrees => {
                for row in &t.work_trees {
                    put_u32(&mut out, row.dir);
                    put_u32(&mut out, row.offset);
                    put_u64(&mut out, row.common_id.0);
                    put_u64(&mut out, row.common_id.1);
                    out.push(row.kind);
                    out.extend_from_slice(&[0; 7]);
                }
            }
            Section::Docs => {
                for (id, hash) in &t.docs {
                    put_u32(&mut out, *id);
                    out.extend_from_slice(hash);
                }
            }
        }
        sections.push(out);
    }

    let total = DATA_START + sections.iter().map(Vec::len).sum::<usize>();
    let mut file = Vec::with_capacity(total);
    file.extend_from_slice(&MAGIC);
    put_u32(&mut file, VERSION);
    put_u32(&mut file, t.sniffer);
    put_u32(&mut file, t.next_doc);
    put_u32(&mut file, SECTIONS.len() as u32);
    let mut offset = DATA_START as u64;
    for section in &sections {
        put_u64(&mut file, offset);
        put_u64(&mut file, section.len() as u64);
        offset += section.len() as u64;
    }
    for section in &sections {
        file.extend_from_slice(section);
    }
    file
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_pair(out: &mut Vec<u8>, a: u32, b: u32) {
    put_u32(out, a);
    put_u32(out, b);
}

/// Packs `width`-bit values (1 or 2) LSB first.
fn pack_bits(values: impl Iterator<Item = u8>, width: usize) -> Vec<u8> {
    let per_byte = 8 / width;
    let mut out = Vec::new();
    for (i, v) in values.enumerate() {
        if i % per_byte == 0 {
            out.push(0);
        }
        if let Some(last) = out.last_mut() {
            *last |= v << ((i % per_byte) * width);
        }
    }
    out
}

// ── decoding ──

pub(crate) fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut b = [0; 4];
    b.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(b)
}

pub(crate) fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut b = [0; 8];
    b.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(b)
}

/// Where each section lies in a validated file, and the header's counters.
#[derive(Clone, Debug)]
pub(crate) struct Layout {
    pub(crate) sniffer: u32,
    pub(crate) next_doc: u32,
    pub(crate) sections: [(usize, usize); SECTIONS.len()],
    pub(crate) dirs: usize,
    pub(crate) inodes: usize,
    pub(crate) names: usize,
}

impl Layout {
    pub(crate) fn section<'a>(&self, bytes: &'a [u8], section: Section) -> &'a [u8] {
        let (start, end) = self.sections[section as usize];
        &bytes[start..end]
    }
}

/// Validates `bytes` as a snapshot. See the module doc for what is checked.
pub(crate) fn decode(bytes: &[u8]) -> Result<Layout, DecodeError> {
    if bytes.len() < HEADER || bytes[..8] != MAGIC {
        return Err(DecodeError::NotACatalog);
    }
    let version = u32_at(bytes, 8);
    if version != VERSION {
        return Err(DecodeError::Version(version));
    }
    if bytes.len() < DATA_START || u32_at(bytes, 20) as usize != SECTIONS.len() {
        return Err(DecodeError::Layout);
    }
    let mut sections = [(0, 0); SECTIONS.len()];
    let mut expect = DATA_START as u64;
    for (i, slot) in sections.iter_mut().enumerate() {
        let offset = u64_at(bytes, HEADER + i * 16);
        let len = u64_at(bytes, HEADER + i * 16 + 8);
        let end = offset.checked_add(len).ok_or(DecodeError::Layout)?;
        if offset != expect || end > bytes.len() as u64 {
            return Err(DecodeError::Layout);
        }
        *slot = (offset as usize, end as usize);
        expect = end;
    }
    if expect != bytes.len() as u64 {
        return Err(DecodeError::Layout);
    }

    let len = |s: Section| sections[s as usize].1 - sections[s as usize].0;
    let rows = |s: Section, width: usize, what| {
        if len(s) % width == 0 {
            Ok(len(s) / width)
        } else {
            Err(DecodeError::Corrupt(what))
        }
    };
    let names = rows(Section::Names, NAME_ROW, "names")?;
    let dirs = rows(Section::DirNames, 4, "dir names")?;
    let inodes = rows(Section::Inodes, INODE_ROW, "inodes")?;
    rows(Section::Roots, PAIR_ROW, "roots")?;
    rows(Section::Links, PAIR_ROW, "links")?;
    rows(Section::WorkTrees, WORK_TREE_ROW, "work trees")?;
    rows(Section::Docs, DOC_ROW, "docs")?;
    if inodes >= NONE as usize || names >= NONE as usize || dirs > inodes {
        return Err(DecodeError::Corrupt("inodes"));
    }
    if len(Section::Traversed) != dirs.div_ceil(8) {
        return Err(DecodeError::Corrupt("traversed"));
    }
    if len(Section::States) != inodes.div_ceil(4) {
        return Err(DecodeError::Corrupt("states"));
    }

    let layout = Layout {
        sniffer: u32_at(bytes, 12),
        next_doc: u32_at(bytes, 16),
        sections,
        dirs,
        inodes,
        names,
    };
    validate(bytes, &layout)?;
    Ok(layout)
}

fn validate(bytes: &[u8], l: &Layout) -> Result<(), DecodeError> {
    let section = |s| l.section(bytes, s);
    let terminated = |heap: &[u8]| heap.last().is_none_or(|&b| b == 0);

    let heap = section(Section::NameHeap);
    if !terminated(heap) || heap.is_empty() != (l.names == 0) {
        return Err(DecodeError::Corrupt("name heap"));
    }
    let strings = section(Section::Strings);
    if !terminated(strings) {
        return Err(DecodeError::Corrupt("strings"));
    }

    // Names: ids in range, offsets strictly increasing inside the heap,
    // parents in order. Offsets and parents must be sorted because readers
    // binary-search them.
    let rows = section(Section::Names);
    let (mut last_parent, mut next_offset) = (0, 0u64);
    for row in rows.chunks_exact(NAME_ROW) {
        let (parent, child, offset) = (u32_at(row, 0), u32_at(row, 4), u32_at(row, 8));
        let ok = (parent as usize) < l.dirs
            && (child as usize) < l.inodes
            && parent >= last_parent
            && u64::from(offset) >= next_offset
            && (offset as usize) < heap.len();
        if !ok {
            return Err(DecodeError::Corrupt("names"));
        }
        last_parent = parent;
        next_offset = u64::from(offset) + 1;
    }

    // Every directory's name edge names it, from a lower-numbered parent, so
    // a walk upwards strictly descends and ends at a root.
    let dir_names = section(Section::DirNames);
    let mut unnamed = 0;
    for (dir, at) in (0..l.dirs).zip((0..).step_by(4)) {
        let name = u32_at(dir_names, at);
        if name == NONE {
            unnamed += 1;
            continue;
        }
        let ok = (name as usize) < l.names && {
            let row = name as usize * NAME_ROW;
            u32_at(rows, row + 4) as usize == dir && (u32_at(rows, row) as usize) < dir
        };
        if !ok {
            return Err(DecodeError::Corrupt("dir names"));
        }
    }

    // Roots: exactly the unnamed directories, sorted by InoId.
    let roots = section(Section::Roots);
    let mut last = None;
    for pair in roots.chunks_exact(PAIR_ROW) {
        let (dir, offset) = (u32_at(pair, 0), u32_at(pair, 4) as usize);
        let ok = (dir as usize) < l.dirs
            && u32_at(dir_names, dir as usize * 4) == NONE
            && last.is_none_or(|last| dir > last)
            && offset < strings.len();
        if !ok {
            return Err(DecodeError::Corrupt("roots"));
        }
        last = Some(dir);
    }
    if roots.len() / PAIR_ROW != unnamed {
        return Err(DecodeError::Corrupt("roots"));
    }

    let mut last = None;
    for pair in section(Section::Links).chunks_exact(PAIR_ROW) {
        let (ino, offset) = (u32_at(pair, 0), u32_at(pair, 4) as usize);
        let ok = (l.dirs..l.inodes).contains(&(ino as usize))
            && last.is_none_or(|last| ino > last)
            && offset < strings.len();
        if !ok {
            return Err(DecodeError::Corrupt("links"));
        }
        last = Some(ino);
    }

    let mut last = None;
    for row in section(Section::WorkTrees).chunks_exact(WORK_TREE_ROW) {
        let (dir, offset) = (u32_at(row, 0), u32_at(row, 4) as usize);
        let ok = (dir as usize) < l.dirs
            && last.is_none_or(|last| dir > last)
            && offset < strings.len()
            && crate::WorkTreeKind::from_byte(row[24]).is_some();
        if !ok {
            return Err(DecodeError::Corrupt("work trees"));
        }
        last = Some(dir);
    }

    let mut last = None;
    for row in section(Section::Docs).chunks_exact(DOC_ROW) {
        let id = u32_at(row, 0);
        if id >= l.next_doc || last.is_some_and(|last| id <= last) {
            return Err(DecodeError::Corrupt("docs"));
        }
        last = Some(id);
    }
    Ok(())
}
