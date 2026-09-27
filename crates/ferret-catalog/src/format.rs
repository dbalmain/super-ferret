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
use std::io::{self, Write};

use crate::batch::Stat;

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

// ── encoding: the builder streams sections through these ──

/// Writes the header and the section table. `lens` are the sections'
/// lengths in [`SECTIONS`] order, all known before any section is written.
pub(crate) fn write_header(
    out: &mut impl Write,
    sniffer: u32,
    next_doc: u32,
    lens: &[usize; SECTIONS.len()],
) -> io::Result<()> {
    out.write_all(&MAGIC)?;
    out.write_all(&VERSION.to_le_bytes())?;
    out.write_all(&sniffer.to_le_bytes())?;
    out.write_all(&next_doc.to_le_bytes())?;
    out.write_all(&(SECTIONS.len() as u32).to_le_bytes())?;
    let mut offset = DATA_START as u64;
    for &len in lens {
        out.write_all(&offset.to_le_bytes())?;
        out.write_all(&(len as u64).to_le_bytes())?;
        offset += len as u64;
    }
    Ok(())
}

pub(crate) fn put_u32(out: &mut impl Write, v: u32) -> io::Result<()> {
    out.write_all(&v.to_le_bytes())
}

pub(crate) fn put_pair(out: &mut impl Write, a: u32, b: u32) -> io::Result<()> {
    put_u32(out, a)?;
    put_u32(out, b)
}

/// One inode row: dev, ino, size, mtime s, ctime s, mtime ns, ctime ns,
/// mode, uid, gid, DocId (or NONE).
pub(crate) fn put_inode(out: &mut impl Write, stat: &Stat, doc: u32) -> io::Result<()> {
    let mut row = [0u8; INODE_ROW];
    row[0..8].copy_from_slice(&stat.dev.to_le_bytes());
    row[8..16].copy_from_slice(&stat.ino.to_le_bytes());
    row[16..24].copy_from_slice(&stat.size.to_le_bytes());
    row[24..32].copy_from_slice(&stat.mtime_sec.to_le_bytes());
    row[32..40].copy_from_slice(&stat.ctime_sec.to_le_bytes());
    row[40..44].copy_from_slice(&stat.mtime_nsec.to_le_bytes());
    row[44..48].copy_from_slice(&stat.ctime_nsec.to_le_bytes());
    row[48..52].copy_from_slice(&stat.mode.to_le_bytes());
    row[52..56].copy_from_slice(&stat.uid.to_le_bytes());
    row[56..60].copy_from_slice(&stat.gid.to_le_bytes());
    row[60..64].copy_from_slice(&doc.to_le_bytes());
    out.write_all(&row)
}

/// One work-tree row.
pub(crate) fn put_work_tree(
    out: &mut impl Write,
    dir: u32,
    offset: u32,
    common_id: (u64, u64),
    kind: u8,
) -> io::Result<()> {
    put_pair(out, dir, offset)?;
    out.write_all(&common_id.0.to_le_bytes())?;
    out.write_all(&common_id.1.to_le_bytes())?;
    out.write_all(&[kind, 0, 0, 0, 0, 0, 0, 0])
}

/// Packs `width`-bit values (1 or 2) LSB first as they are pushed.
pub(crate) struct Bits {
    width: usize,
    byte: u8,
    count: usize,
}

impl Bits {
    pub(crate) fn new(width: usize) -> Self {
        Self {
            width,
            byte: 0,
            count: 0,
        }
    }

    pub(crate) fn push(&mut self, out: &mut impl Write, value: u8) -> io::Result<()> {
        let per_byte = 8 / self.width;
        self.byte |= value << ((self.count % per_byte) * self.width);
        self.count += 1;
        if self.count.is_multiple_of(per_byte) {
            out.write_all(&[self.byte])?;
            self.byte = 0;
        }
        Ok(())
    }

    /// Writes a final partial byte.
    pub(crate) fn finish(self, out: &mut impl Write) -> io::Result<()> {
        if !self.count.is_multiple_of(8 / self.width) {
            out.write_all(&[self.byte])?;
        }
        Ok(())
    }
}

// ── decoding ──

/// The NUL bytes in `bytes`. Summed as `u8` per 255-byte chunk, which the
/// compiler vectorises; a plain `filter().count()` measured about 4 ms over
/// `$HOME`'s 10.6 MB heap, a third of the whole open.
fn count_nuls(bytes: &[u8]) -> usize {
    bytes
        .chunks(255)
        .map(|chunk| usize::from(chunk.iter().map(|&b| u8::from(b == 0)).sum::<u8>()))
        .sum()
}

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

    // Names: ids in range, offsets strictly increasing inside the heap from
    // 0, parents in order. Offsets and parents must be sorted because readers
    // binary-search them, and the first name must start the heap so every
    // heap byte belongs to a name.
    let rows = section(Section::Names);
    let (mut last_parent, mut next_offset) = (0, 0u64);
    for row in rows.chunks_exact(NAME_ROW) {
        let (parent, child, offset) = (u32_at(row, 0), u32_at(row, 4), u32_at(row, 8));
        let ok = (parent as usize) < l.dirs
            && (child as usize) < l.inodes
            && parent >= last_parent
            && u64::from(offset) >= next_offset
            && (next_offset > 0 || offset == 0)
            && (offset as usize) < heap.len();
        if !ok {
            return Err(DecodeError::Corrupt("names"));
        }
        last_parent = parent;
        next_offset = u64::from(offset) + 1;
    }

    // Each name is exactly the bytes up to the next name's offset: non-empty,
    // its NUL last. The spans tile the heap from 0, so one NUL per name in
    // the whole heap means none inside a name; one vectorised count is far
    // cheaper than a search per name. Siblings are in strictly increasing
    // byte order, because `lookup` binary-searches them by name.
    if count_nuls(heap) != l.names {
        return Err(DecodeError::Corrupt("name order"));
    }
    let mut previous: Option<(u32, &[u8])> = None;
    for (i, row) in rows.chunks_exact(NAME_ROW).enumerate() {
        let start = u32_at(row, 8) as usize;
        let end = match rows.get((i + 1) * NAME_ROW..) {
            Some(next) if !next.is_empty() => u32_at(next, 8) as usize,
            _ => heap.len(),
        };
        let parent = u32_at(row, 0);
        let (name, nul) = (&heap[start..end - 1], heap[end - 1]);
        let ordered = previous.is_none_or(|(p, prev)| p != parent || prev < name);
        if name.is_empty() || nul != 0 || !ordered {
            return Err(DecodeError::Corrupt("name order"));
        }
        previous = Some((parent, name));
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
