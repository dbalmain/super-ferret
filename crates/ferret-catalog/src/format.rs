//! The snapshot file: one generation of the catalog.
//!
//! ```text
//! header       magic "FERRETCT" | version u32 | sniffer u32 | next_doc u32 |
//!              section count u32 | dirs u32 | inodes u32 | names u32  (36 B)
//! table        (offset u64, length u64) per section, in SECTIONS order (352 B)
//! descriptors  (base u64, width u32, dictionary length u32) per column, in
//!              COLUMNS order                                         (256 B)
//! sections     contiguous from byte 644 to the end of the file, in order
//! ```
//!
//! All integers are little-endian. The sections a name query reads come
//! first, and a reader loads each on first use (D38 B).
//!
//! ```text
//! names       3 columns: parent InoId, child InoId, offset in name heap
//! name heap   names, each NUL-terminated, in NameId order (D28)
//! dir names   column, per directory InoId: its NameId, none for a root
//! entries     column, per directory: the entries `getdents` returned, minus
//!             `.` and `..`, before ignore rules; none if unknown (D47)
//! traversed   1 bit per directory: a structural row (D29), LSB first
//! roots       8 B rows: root InoId, offset of its path in strings
//! strings     root paths, link targets, work-tree paths; NUL-terminated
//! dev         column per inode, dictionary
//! ino         column per inode
//! size        column per inode
//! mtime       column per inode: whole seconds, signed
//! mtime ns    column per inode
//! ctime       column per inode: whole seconds, signed
//! ctime ns    column per inode
//! mode        column per inode, dictionary
//! owner       column per inode, dictionary of uid << 32 | gid
//! nlink       column per inode
//! doc         column per inode: DocId, none when it has no document
//! states      2 bits per inode: ContentState (D37), LSB first
//! links       8 B rows: symlink InoId, offset of its target in strings
//! work trees  32 B rows: top InoId, offset of common dir in strings, common
//!             dev, common ino, kind u8, 7 B zero
//! docs        20 B rows: DocId, hash; sorted by id, live documents only
//!             (D36 B)
//! ```
//!
//! A column is its dictionary (`u64` values, present only in a dictionary
//! column) and then one value per row of its table, bit-packed at the
//! descriptor's width (`packed`). Each column is sized to this catalog's
//! values (D43), in one of three codings:
//!
//! - **Frame of reference:** the value is `base + packed`, and the width is
//!   that of the largest value less the smallest, which is the base. Signed
//!   times are stored with the sign bit flipped ([`order`]), which maps `i64`
//!   onto `u64` in order, so any two times' difference fits.
//! - **Nullable:** frame of reference, except that the all-ones value of the
//!   width means none. The writer makes the width wide enough that no real
//!   value is all ones.
//! - **Dictionary:** the packed value is `base + index` into the column's
//!   dictionary. The writer stores base 0 and the sorted distinct values.
//!
//! `base + packed` wraps, so no descriptor can make a read overflow.
//!
//! Decoding validates everything an accessor indexes by, so a corrupt or
//! truncated file is a [`DecodeError`] and never a panic: every width is at
//! most 64 and every column section's length is exactly its columns'
//! dictionaries and padded values; every offset, id and dictionary index is
//! in range, every heap ends in NUL, and each directory's name edge points at
//! a lower-numbered parent, so walking up from any name ends at a root. Each
//! directory is the child of exactly its recorded name edge, so walking down
//! from a root ends too. Field values that index nothing (times, sizes,
//! nlink, entry counts, dictionary values, an inode's `DocId`) are not
//! checked; a flipped bit there reads back as a different value, truncated to
//! the field's type.
//!
//! Validation is split so that it can run per section: [`decode_table`]
//! checks the header, the table, the descriptors and every count and length
//! at open, and [`check`] validates one section when it is loaded. A check
//! that spans sections (a directory's name edge against the name rows) runs
//! when the later one loads, after the sections it [needs](Section::needs).

use std::fmt;
use std::io::{self, Write};

use crate::packed::{self, Packed};

pub(crate) const MAGIC: [u8; 8] = *b"FERRETCT";
/// 1: fixed-width rows (S1). 2: bit-packed columns (S1a).
pub(crate) const VERSION: u32 = 2;
/// "No id" in the builder's plan, and in the `u32` ids of fixed-width rows.
pub(crate) const NONE: u32 = u32::MAX;

pub(crate) const PAIR_ROW: usize = 8;
pub(crate) const WORK_TREE_ROW: usize = 32;
pub(crate) const DOC_ROW: usize = 20;

/// One section of the snapshot file, in file order. A reader loads sections
/// one at a time ([`Catalog::load`](crate::Catalog::load)); the ones a name
/// query needs come first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    /// Name rows: parent, child, heap offset.
    Names,
    /// The name bytes a filename scan reads (D14, D28).
    NameHeap,
    /// Each directory's own name edge (D30).
    DirNames,
    /// Each directory's raw entry count, when known (D47).
    Entries,
    /// The traversed-directory bitset (D29).
    Traversed,
    /// Root directories and their paths.
    Roots,
    /// Root paths, link targets and work-tree paths.
    Strings,
    /// Each inode's `st_dev`.
    Dev,
    /// Each inode's `st_ino`.
    Ino,
    /// Each inode's `st_size`.
    Size,
    /// Each inode's mtime, whole seconds.
    Mtime,
    /// Each inode's mtime, nanoseconds.
    MtimeNs,
    /// Each inode's ctime, whole seconds.
    Ctime,
    /// Each inode's ctime, nanoseconds.
    CtimeNs,
    /// Each inode's `st_mode`.
    Mode,
    /// Each inode's `(st_uid, st_gid)`.
    Owner,
    /// Each inode's `st_nlink`.
    Nlink,
    /// Each inode's document.
    Doc,
    /// Each inode's content state (D37).
    States,
    /// Symlink targets.
    Links,
    /// Work-tree rows (D23).
    WorkTrees,
    /// Live documents and their hashes.
    Docs,
}

pub(crate) const SECTIONS: [Section; 22] = [
    Section::Names,
    Section::NameHeap,
    Section::DirNames,
    Section::Entries,
    Section::Traversed,
    Section::Roots,
    Section::Strings,
    Section::Dev,
    Section::Ino,
    Section::Size,
    Section::Mtime,
    Section::MtimeNs,
    Section::Ctime,
    Section::CtimeNs,
    Section::Mode,
    Section::Owner,
    Section::Nlink,
    Section::Doc,
    Section::States,
    Section::Links,
    Section::WorkTrees,
    Section::Docs,
];

/// One bit-packed column. Each lies in one section, in this order; only the
/// name rows' three share one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Column {
    NameParent,
    NameChild,
    NameOffset,
    DirName,
    Entries,
    Dev,
    Ino,
    Size,
    Mtime,
    MtimeNs,
    Ctime,
    CtimeNs,
    Mode,
    Owner,
    Nlink,
    Doc,
}

pub(crate) const COLUMNS: [Column; 16] = [
    Column::NameParent,
    Column::NameChild,
    Column::NameOffset,
    Column::DirName,
    Column::Entries,
    Column::Dev,
    Column::Ino,
    Column::Size,
    Column::Mtime,
    Column::MtimeNs,
    Column::Ctime,
    Column::CtimeNs,
    Column::Mode,
    Column::Owner,
    Column::Nlink,
    Column::Doc,
];

/// How a column's packed values become field values; see the module doc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Coding {
    Frame,
    Nullable,
    Dictionary,
}

/// Which table a column has a row for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rows {
    Names,
    Dirs,
    Inodes,
}

impl Column {
    pub(crate) fn section(self) -> Section {
        match self {
            Column::NameParent | Column::NameChild | Column::NameOffset => Section::Names,
            Column::DirName => Section::DirNames,
            Column::Entries => Section::Entries,
            Column::Dev => Section::Dev,
            Column::Ino => Section::Ino,
            Column::Size => Section::Size,
            Column::Mtime => Section::Mtime,
            Column::MtimeNs => Section::MtimeNs,
            Column::Ctime => Section::Ctime,
            Column::CtimeNs => Section::CtimeNs,
            Column::Mode => Section::Mode,
            Column::Owner => Section::Owner,
            Column::Nlink => Section::Nlink,
            Column::Doc => Section::Doc,
        }
    }

    pub(crate) fn coding(self) -> Coding {
        match self {
            Column::DirName | Column::Entries | Column::Doc => Coding::Nullable,
            Column::Dev | Column::Mode | Column::Owner => Coding::Dictionary,
            Column::NameParent
            | Column::NameChild
            | Column::NameOffset
            | Column::Ino
            | Column::Size
            | Column::Mtime
            | Column::MtimeNs
            | Column::Ctime
            | Column::CtimeNs
            | Column::Nlink => Coding::Frame,
        }
    }

    pub(crate) fn rows(self) -> Rows {
        match self {
            Column::NameParent | Column::NameChild | Column::NameOffset => Rows::Names,
            Column::DirName | Column::Entries => Rows::Dirs,
            _ => Rows::Inodes,
        }
    }
}

const HEADER: usize = 36;
const TABLE: usize = SECTIONS.len() * 16;
const DESCRIPTORS: usize = COLUMNS.len() * 16;

/// The bytes of the header, section table and column descriptors: what
/// opening a file reads.
pub(crate) const TABLE_END: usize = HEADER + TABLE + DESCRIPTORS;

/// Why a snapshot file could not be decoded.
#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Not a catalog file, or shorter than its header.
    NotACatalog,
    /// Written by a format version this build does not read. A writer treats
    /// such a file as no previous generation and replaces it; a reader
    /// cannot use it until then.
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

/// Maps `i64` onto `u64` in order: the sign bit flipped.
pub(crate) fn order(value: i64) -> u64 {
    (value as u64) ^ (1 << 63)
}

/// The inverse of [`order`].
pub(crate) fn unorder(value: u64) -> i64 {
    (value ^ (1 << 63)) as i64
}

/// How one column is packed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Descriptor {
    pub(crate) base: u64,
    pub(crate) width: u32,
    pub(crate) dict_len: u32,
}

impl Descriptor {
    /// A frame-of-reference column holding values from `range`.
    pub(crate) fn frame(range: Range) -> Self {
        let (min, max) = range.0.unwrap_or((0, 0));
        Self {
            base: min,
            width: packed::width(max - min),
            dict_len: 0,
        }
    }

    /// A nullable column whose real values lie in `range`, which must span
    /// less than `u64::MAX` (the builder's are `u32`s): wide enough that all
    /// ones is past every real value.
    pub(crate) fn nullable(range: Range) -> Self {
        match range.0 {
            // Every row is none: zero bits, whose all-ones value is 0.
            None => Self::default(),
            Some((min, max)) => Self {
                base: min,
                width: packed::width(max - min + 1),
                dict_len: 0,
            },
        }
    }

    /// A dictionary column of `dict_len` distinct values.
    pub(crate) fn dictionary(dict_len: u32) -> Self {
        Self {
            base: 0,
            width: packed::width(u64::from(dict_len.saturating_sub(1))),
            dict_len,
        }
    }

    /// The column's bytes for `count` rows: dictionary, then values.
    pub(crate) fn len(self, count: u32) -> u64 {
        u64::from(self.dict_len) * 8 + packed::len(count, self.width)
    }
}

/// The smallest and largest of the values seen; `None` before any.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Range(pub(crate) Option<(u64, u64)>);

impl Range {
    pub(crate) fn add(&mut self, value: u64) {
        self.0 = Some(match self.0 {
            None => (value, value),
            Some((min, max)) => (min.min(value), max.max(value)),
        });
    }
}

// ── encoding: the builder streams sections through these ──

/// Everything the head of the file records, all known before any section is
/// written.
pub(crate) struct Head {
    pub(crate) sniffer: u32,
    pub(crate) next_doc: u32,
    pub(crate) dirs: u32,
    pub(crate) inodes: u32,
    pub(crate) names: u32,
    /// Column descriptors, in [`COLUMNS`] order.
    pub(crate) columns: [Descriptor; COLUMNS.len()],
    /// Every section's length in [`SECTIONS`] order. A column section's entry
    /// is ignored: its length follows from its descriptors.
    pub(crate) lens: [u64; SECTIONS.len()],
}

impl Head {
    pub(crate) fn count(&self, rows: Rows) -> u32 {
        match rows {
            Rows::Names => self.names,
            Rows::Dirs => self.dirs,
            Rows::Inodes => self.inodes,
        }
    }

    fn section_lens(&self) -> [u64; SECTIONS.len()] {
        let mut lens = self.lens;
        for column in COLUMNS {
            lens[column.section() as usize] = 0;
        }
        for (column, desc) in COLUMNS.into_iter().zip(self.columns) {
            lens[column.section() as usize] += desc.len(self.count(column.rows()));
        }
        lens
    }
}

/// Writes the header, the section table and the column descriptors.
pub(crate) fn write_head(out: &mut impl Write, head: &Head) -> io::Result<()> {
    out.write_all(&MAGIC)?;
    for v in [
        VERSION,
        head.sniffer,
        head.next_doc,
        SECTIONS.len() as u32,
        head.dirs,
        head.inodes,
        head.names,
    ] {
        put_u32(out, v)?;
    }
    let mut offset = TABLE_END as u64;
    for len in head.section_lens() {
        out.write_all(&offset.to_le_bytes())?;
        out.write_all(&len.to_le_bytes())?;
        offset += len;
    }
    for desc in head.columns {
        out.write_all(&desc.base.to_le_bytes())?;
        put_pair(out, desc.width, desc.dict_len)?;
    }
    Ok(())
}

/// Streams one column: its dictionary, then its values as they are pushed.
pub(crate) struct ColumnWriter {
    desc: Descriptor,
    packed: packed::Writer,
}

impl ColumnWriter {
    /// Starts a column, writing its dictionary, which must have the
    /// descriptor's length.
    pub(crate) fn start(out: &mut impl Write, desc: Descriptor, dict: &[u64]) -> io::Result<Self> {
        debug_assert_eq!(dict.len(), desc.dict_len as usize);
        for &value in dict {
            out.write_all(&value.to_le_bytes())?;
        }
        Ok(Self {
            desc,
            packed: packed::Writer::new(desc.width),
        })
    }

    /// A frame-of-reference value, inside the range the descriptor was made
    /// from.
    pub(crate) fn value(&mut self, out: &mut impl Write, value: u64) -> io::Result<()> {
        self.packed.push(out, value - self.desc.base)
    }

    /// A nullable column's value, or none.
    pub(crate) fn nullable(&mut self, out: &mut impl Write, value: Option<u64>) -> io::Result<()> {
        match value {
            Some(value) => self.value(out, value),
            None => self.packed.push(out, packed::mask(self.desc.width)),
        }
    }

    /// A dictionary column's index.
    pub(crate) fn index(&mut self, out: &mut impl Write, index: usize) -> io::Result<()> {
        self.packed.push(out, index as u64)
    }

    pub(crate) fn finish(self, out: &mut impl Write) -> io::Result<()> {
        self.packed.finish(out)
    }
}

pub(crate) fn put_u32(out: &mut impl Write, v: u32) -> io::Result<()> {
    out.write_all(&v.to_le_bytes())
}

pub(crate) fn put_pair(out: &mut impl Write, a: u32, b: u32) -> io::Result<()> {
    put_u32(out, a)?;
    put_u32(out, b)
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

/// One column's descriptor, and where its bytes start in its section.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Placed {
    pub(crate) desc: Descriptor,
    pub(crate) start: usize,
}

/// A column's bytes, decoded in place per its coding.
#[derive(Clone, Copy)]
pub(crate) struct View<'a> {
    base: u64,
    dict: &'a [u8],
    packed: Packed<'a>,
}

impl View<'_> {
    /// A frame-of-reference value, or a dictionary index.
    pub(crate) fn get(&self, row: usize) -> u64 {
        self.base.wrapping_add(self.packed.get(row))
    }

    /// A nullable value.
    pub(crate) fn nullable(&self, row: usize) -> Option<u64> {
        let raw = self.packed.get(row);
        (raw != packed::mask(self.packed.width())).then(|| self.base.wrapping_add(raw))
    }

    /// A dictionary value. Decoding checked every index.
    pub(crate) fn lookup(&self, row: usize) -> u64 {
        u64_at(self.dict, self.get(row) as usize * 8)
    }
}

/// Where each section lies, the counts, and how each column is packed.
/// Built from the head of the file alone, so every check below that needs
/// only a count, a length or a descriptor runs at open, before any section
/// is read.
#[derive(Clone, Debug)]
pub(crate) struct Layout {
    pub(crate) sniffer: u32,
    pub(crate) next_doc: u32,
    pub(crate) sections: [(usize, usize); SECTIONS.len()],
    pub(crate) columns: [Placed; COLUMNS.len()],
    pub(crate) dirs: usize,
    pub(crate) inodes: usize,
    pub(crate) names: usize,
}

impl Layout {
    pub(crate) fn section<'a>(&self, bytes: &'a [u8], section: Section) -> &'a [u8] {
        let (start, end) = self.sections[section as usize];
        &bytes[start..end]
    }

    pub(crate) fn range(&self, section: Section) -> (usize, usize) {
        self.sections[section as usize]
    }

    fn len(&self, section: Section) -> usize {
        let (start, end) = self.range(section);
        end - start
    }

    /// `column`, given the bytes of its section.
    pub(crate) fn view<'a>(&self, column: Column, section: &'a [u8]) -> View<'a> {
        let Placed { desc, start } = self.columns[column as usize];
        let (dict, values) = section[start..].split_at(desc.dict_len as usize * 8);
        View {
            base: desc.base,
            dict,
            packed: Packed::new(values, desc.width),
        }
    }
}

/// Validates the header, section table and column descriptors. `head` is
/// the file's first [`TABLE_END`] bytes, or all of it if shorter; `file_len`
/// is the whole file's length, which the sections must tile exactly.
pub(crate) fn decode_table(head: &[u8], file_len: u64) -> Result<Layout, DecodeError> {
    if head.len() < HEADER || head[..8] != MAGIC {
        return Err(DecodeError::NotACatalog);
    }
    let version = u32_at(head, 8);
    if version != VERSION {
        return Err(DecodeError::Version(version));
    }
    if head.len() < TABLE_END || u32_at(head, 20) as usize != SECTIONS.len() {
        return Err(DecodeError::Layout);
    }
    let mut sections = [(0, 0); SECTIONS.len()];
    let mut expect = TABLE_END as u64;
    for (i, slot) in sections.iter_mut().enumerate() {
        let offset = u64_at(head, HEADER + i * 16);
        let len = u64_at(head, HEADER + i * 16 + 8);
        let end = offset.checked_add(len).ok_or(DecodeError::Layout)?;
        if offset != expect || end > file_len {
            return Err(DecodeError::Layout);
        }
        *slot = (offset as usize, end as usize);
        expect = end;
    }
    if expect != file_len {
        return Err(DecodeError::Layout);
    }

    let (dirs, inodes, names) = (u32_at(head, 24), u32_at(head, 28), u32_at(head, 32));
    if inodes == NONE || names == NONE || dirs > inodes {
        return Err(DecodeError::Corrupt("counts"));
    }
    let len = |s: Section| (sections[s as usize].1 - sections[s as usize].0) as u64;
    for (section, row) in [
        (Section::Roots, PAIR_ROW),
        (Section::Links, PAIR_ROW),
        (Section::WorkTrees, WORK_TREE_ROW),
        (Section::Docs, DOC_ROW),
    ] {
        if !len(section).is_multiple_of(row as u64) {
            return Err(DecodeError::Corrupt(section.label()));
        }
    }
    if len(Section::Traversed) != u64::from(dirs).div_ceil(8) {
        return Err(DecodeError::Corrupt("traversed"));
    }
    if len(Section::States) != u64::from(inodes).div_ceil(4) {
        return Err(DecodeError::Corrupt("states"));
    }
    if (len(Section::NameHeap) == 0) != (names == 0) {
        return Err(DecodeError::Corrupt("name heap"));
    }

    // Each column starts after the columns before it in its section, and a
    // column section holds exactly its columns.
    let mut columns = [Placed::default(); COLUMNS.len()];
    let mut used = [0u64; SECTIONS.len()];
    for (i, column) in COLUMNS.into_iter().enumerate() {
        let at = HEADER + TABLE + i * 16;
        let desc = Descriptor {
            base: u64_at(head, at),
            width: u32_at(head, at + 8),
            dict_len: u32_at(head, at + 12),
        };
        let section = column.section();
        if desc.width > packed::MAX_WIDTH
            || (desc.dict_len != 0 && column.coding() != Coding::Dictionary)
        {
            return Err(DecodeError::Corrupt(section.label()));
        }
        let count = match column.rows() {
            Rows::Names => names,
            Rows::Dirs => dirs,
            Rows::Inodes => inodes,
        };
        columns[i] = Placed {
            desc,
            start: used[section as usize] as usize,
        };
        used[section as usize] += desc.len(count);
    }
    for column in COLUMNS {
        let section = column.section();
        if used[section as usize] != len(section) {
            return Err(DecodeError::Corrupt(section.label()));
        }
    }

    Ok(Layout {
        sniffer: u32_at(head, 12),
        next_doc: u32_at(head, 16),
        sections,
        columns,
        dirs: dirs as usize,
        inodes: inodes as usize,
        names: names as usize,
    })
}

/// Validates a whole file held in memory: the table, then every section.
pub(crate) fn decode(bytes: &[u8]) -> Result<Layout, DecodeError> {
    let layout = decode_table(bytes, bytes.len() as u64)?;
    for section in CHECK_ORDER {
        check(section, &layout, |s| layout.section(bytes, s))?;
    }
    Ok(layout)
}

/// Every section, each after those it [needs](Section::needs).
const CHECK_ORDER: [Section; SECTIONS.len()] = [
    Section::NameHeap,
    Section::Names,
    Section::DirNames,
    Section::Entries,
    Section::Traversed,
    Section::Strings,
    Section::Roots,
    Section::Dev,
    Section::Ino,
    Section::Size,
    Section::Mtime,
    Section::MtimeNs,
    Section::Ctime,
    Section::CtimeNs,
    Section::Mode,
    Section::Owner,
    Section::Nlink,
    Section::Doc,
    Section::States,
    Section::Links,
    Section::WorkTrees,
    Section::Docs,
];

impl Section {
    /// Every section an [`Inode`](crate::Inode) is read from: what
    /// [`Catalog::inode`](crate::Catalog::inode) needs loaded.
    pub const INODE: [Section; 12] = [
        Section::Dev,
        Section::Ino,
        Section::Size,
        Section::Mtime,
        Section::MtimeNs,
        Section::Ctime,
        Section::CtimeNs,
        Section::Mode,
        Section::Owner,
        Section::Nlink,
        Section::Doc,
        Section::States,
    ];

    /// The sections loaded with this one: those whose bytes [`check`] reads
    /// to validate it, which must be loaded first, and those an accessor
    /// documented against this section also reads. Everything else a check
    /// needs is a count, a length or a descriptor from the head of the file.
    /// `tests::decode` holds every public accessor to this: loading its
    /// documented sections alone is enough.
    pub(crate) fn needs(self) -> &'static [Section] {
        match self {
            Section::Names => &[Section::NameHeap],
            Section::DirNames => &[Section::Names],
            Section::Roots => &[Section::DirNames, Section::Strings],
            Section::Links | Section::WorkTrees => &[Section::Strings],
            _ => &[],
        }
    }

    /// What a [`DecodeError::Corrupt`] calls it.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Section::Names => "names",
            Section::NameHeap => "name heap",
            Section::DirNames => "dir names",
            Section::Entries => "entries",
            Section::Traversed => "traversed",
            Section::Roots => "roots",
            Section::Strings => "strings",
            Section::Dev => "dev",
            Section::Ino => "ino",
            Section::Size => "size",
            Section::Mtime => "mtime",
            Section::MtimeNs => "mtime ns",
            Section::Ctime => "ctime",
            Section::CtimeNs => "ctime ns",
            Section::Mode => "mode",
            Section::Owner => "owner",
            Section::Nlink => "nlink",
            Section::Doc => "doc",
            Section::States => "states",
            Section::Links => "links",
            Section::WorkTrees => "work trees",
            Section::Docs => "docs",
        }
    }
}

/// Validates one section, given its bytes and those of every section it
/// [needs](Section::needs), through `get`. See the module doc for what is
/// checked. The sections in the last arm have nothing beyond their lengths,
/// which [`decode_table`] checked.
pub(crate) fn check<'a>(
    section: Section,
    l: &Layout,
    get: impl Fn(Section) -> &'a [u8],
) -> Result<(), DecodeError> {
    let terminated = |heap: &[u8]| heap.last().is_none_or(|&b| b == 0);
    let strings_len = l.len(Section::Strings);
    match section {
        Section::NameHeap => {
            if !terminated(get(Section::NameHeap)) {
                return Err(DecodeError::Corrupt("name heap"));
            }
        }
        Section::Strings => {
            if !terminated(get(Section::Strings)) {
                return Err(DecodeError::Corrupt("strings"));
            }
        }
        Section::Names => check_names(l, get(Section::Names), get(Section::NameHeap))?,
        Section::DirNames => check_dir_names(l, get(Section::DirNames), get(Section::Names))?,
        Section::Dev => check_dictionary(l, Column::Dev, get(Section::Dev))?,
        Section::Mode => check_dictionary(l, Column::Mode, get(Section::Mode))?,
        Section::Owner => check_dictionary(l, Column::Owner, get(Section::Owner))?,
        Section::Roots => {
            let roots = get(Section::Roots);
            let dir_names = l.view(Column::DirName, get(Section::DirNames));
            let unnamed = (0..l.dirs)
                .filter(|&d| dir_names.nullable(d).is_none())
                .count();
            let mut last = None;
            for pair in roots.chunks_exact(PAIR_ROW) {
                let (dir, offset) = (u32_at(pair, 0), u32_at(pair, 4) as usize);
                let ok = (dir as usize) < l.dirs
                    && dir_names.nullable(dir as usize).is_none()
                    && last.is_none_or(|last| dir > last)
                    && offset < strings_len;
                if !ok {
                    return Err(DecodeError::Corrupt("roots"));
                }
                last = Some(dir);
            }
            if roots.len() / PAIR_ROW != unnamed {
                return Err(DecodeError::Corrupt("roots"));
            }
        }
        Section::Links => {
            let mut last = None;
            for pair in get(Section::Links).chunks_exact(PAIR_ROW) {
                let (ino, offset) = (u32_at(pair, 0), u32_at(pair, 4) as usize);
                let ok = (l.dirs..l.inodes).contains(&(ino as usize))
                    && last.is_none_or(|last| ino > last)
                    && offset < strings_len;
                if !ok {
                    return Err(DecodeError::Corrupt("links"));
                }
                last = Some(ino);
            }
        }
        Section::WorkTrees => {
            let mut last = None;
            for row in get(Section::WorkTrees).chunks_exact(WORK_TREE_ROW) {
                let (dir, offset) = (u32_at(row, 0), u32_at(row, 4) as usize);
                let ok = (dir as usize) < l.dirs
                    && last.is_none_or(|last| dir > last)
                    && offset < strings_len
                    && crate::WorkTreeKind::from_byte(row[24]).is_some();
                if !ok {
                    return Err(DecodeError::Corrupt("work trees"));
                }
                last = Some(dir);
            }
        }
        Section::Docs => {
            let mut last = None;
            for row in get(Section::Docs).chunks_exact(DOC_ROW) {
                let id = u32_at(row, 0);
                if id >= l.next_doc || last.is_some_and(|last| id <= last) {
                    return Err(DecodeError::Corrupt("docs"));
                }
                last = Some(id);
            }
        }
        Section::Entries
        | Section::Traversed
        | Section::Ino
        | Section::Size
        | Section::Mtime
        | Section::MtimeNs
        | Section::Ctime
        | Section::CtimeNs
        | Section::Nlink
        | Section::Doc
        | Section::States => {}
    }
    Ok(())
}

/// Every inode's dictionary index is inside the dictionary. Nothing is read
/// when the base and width cannot reach past its end, as when the writer's
/// dictionary length is a power of two.
fn check_dictionary(l: &Layout, column: Column, bytes: &[u8]) -> Result<(), DecodeError> {
    let desc = l.columns[column as usize].desc;
    let len = u64::from(desc.dict_len);
    let reach = desc.base.checked_add(packed::mask(desc.width));
    if reach.is_some_and(|reach| reach < len) {
        return Ok(());
    }
    let view = l.view(column, bytes);
    if (0..l.inodes).any(|i| view.get(i) >= len) {
        return Err(DecodeError::Corrupt(column.section().label()));
    }
    Ok(())
}

/// Name rows: ids in range, offsets strictly increasing inside the heap from
/// 0, parents in order. Offsets and parents must be sorted because readers
/// binary-search them, and the first name must start the heap so every heap
/// byte belongs to a name.
///
/// Each name is exactly the bytes up to the next name's offset: non-empty,
/// its NUL last. The spans tile the heap from 0, so one NUL per name in the
/// whole heap means none inside a name; one vectorised count is far cheaper
/// than a search per name. Siblings are in strictly increasing byte order,
/// because `lookup` binary-searches them by name.
fn check_names(l: &Layout, rows: &[u8], heap: &[u8]) -> Result<(), DecodeError> {
    let parents = l.view(Column::NameParent, rows);
    let children = l.view(Column::NameChild, rows);
    let offsets = l.view(Column::NameOffset, rows);
    let (mut last_parent, mut next_offset) = (0, 0);
    for i in 0..l.names {
        let (parent, child, offset) = (parents.get(i), children.get(i), offsets.get(i));
        let ok = parent < l.dirs as u64
            && child < l.inodes as u64
            && parent >= last_parent
            && offset >= next_offset
            && (next_offset > 0 || offset == 0)
            && offset < heap.len() as u64;
        if !ok {
            return Err(DecodeError::Corrupt("names"));
        }
        last_parent = parent;
        next_offset = offset + 1;
    }

    if count_nuls(heap) != l.names {
        return Err(DecodeError::Corrupt("name order"));
    }
    let mut previous: Option<(u64, &[u8])> = None;
    for i in 0..l.names {
        let start = offsets.get(i) as usize;
        let end = match i + 1 < l.names {
            true => offsets.get(i + 1) as usize,
            false => heap.len(),
        };
        let parent = parents.get(i);
        let (name, nul) = (&heap[start..end - 1], heap[end - 1]);
        let ordered = previous.is_none_or(|(p, prev)| p != parent || prev < name);
        if name.is_empty() || nul != 0 || !ordered {
            return Err(DecodeError::Corrupt("name order"));
        }
        previous = Some((parent, name));
    }
    Ok(())
}

/// Every directory's name edge names it, from a lower-numbered parent, so a
/// walk upwards strictly descends and ends at a root. And the converse: a
/// name whose child is a directory must be that directory's recorded edge,
/// so each directory has exactly one name and a root has none. Without it a
/// name could make a root its own child, and a walk down (retention copying
/// a kept root) would never end.
fn check_dir_names(l: &Layout, dir_names: &[u8], rows: &[u8]) -> Result<(), DecodeError> {
    let dir_names = l.view(Column::DirName, dir_names);
    let parents = l.view(Column::NameParent, rows);
    let children = l.view(Column::NameChild, rows);
    for dir in 0..l.dirs {
        let Some(name) = dir_names.nullable(dir) else {
            continue;
        };
        let ok = name < l.names as u64 && {
            let name = name as usize;
            children.get(name) == dir as u64 && parents.get(name) < dir as u64
        };
        if !ok {
            return Err(DecodeError::Corrupt("dir names"));
        }
    }
    for name in 0..l.names {
        let child = children.get(name);
        if child < l.dirs as u64 && dir_names.nullable(child as usize) != Some(name as u64) {
            return Err(DecodeError::Corrupt("dir names"));
        }
    }
    Ok(())
}
