//! The snapshot file: one generation of the catalog.
//!
//! ```text
//! header       magic "FERRETCT" | version u32 | sniffer u32 | next_doc u32 |
//!              section count u32 | dirs u32 | inodes u32 | names u32 |
//!              docs u32                                               (40 B)
//! table        (offset u64, length u64) per section, in SECTIONS order (352 B)
//! descriptors  (base u64, width u32, dictionary length u32) per column, in
//!              COLUMNS order                                         (272 B)
//! sections     contiguous from byte 664 to the end of the file, in order
//! ```
//!
//! All integers are little-endian. The sections a name query reads come
//! first, and a reader loads each on first use (D38 B).
//!
//! ```text
//! names       3 blocked columns: parent InoId, child InoId, offset in
//!             name heap
//! name heap   names, each NUL-terminated, in NameId order (D28)
//! dir names   column, per directory InoId: its NameId, none for a root
//! entries     column, per directory: the entries `getdents` returned, minus
//!             `.` and `..`, before ignore rules; none if unknown (D47)
//! traversed   1 bit per directory: a structural row (D29), LSB first
//! roots       8 B rows: root InoId, offset of its path in strings
//! strings     root paths, link targets, work-tree paths; NUL-terminated
//! dev         column per inode, dictionary
//! ino         column per inode
//! size        column per inode (blocked)
//! mtime       column per inode: whole seconds, signed (blocked)
//! mtime ns    column per inode
//! ctime       column per inode: whole seconds, signed (blocked)
//! ctime ns    column per inode
//! mode        column per inode, dictionary
//! owner       column per inode, dictionary of uid << 32 | gid
//! nlink       column per inode (blocked)
//! doc         column per inode: DocId, none when it has no document
//! states      2 bits per inode: ContentState (D37), LSB first
//! links       8 B rows: symlink InoId, offset of its target in strings
//! work trees  32 B rows: top InoId, offset of common dir in strings, common
//!             dev, common ino, kind u8, 7 B zero
//! docs        column per document: its DocId (sequence), then 16 B rows:
//!             its hash; sorted by id, live documents only (D36 B)
//! ```
//!
//! A column is its dictionary (`u64` values, present only in a dictionary
//! column) and then one value per row of its table, bit-packed at the
//! descriptor's width (`packed`), then 8 bytes of padding (written as zeros,
//! not checked: reads never depend on it). Each column is sized to this
//! catalog's values (D43), in one of five codings:
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
//! - **Blocked:** a frame of reference per block of 128 rows, each block with
//!   its own base and width, found through a table of 16 B entries at the
//!   column's start (`packed::Blocked`). For a column whose neighbouring rows
//!   are close: name offsets only grow, parents only rise, a directory's
//!   children are numbered together, nlink is nearly always 1, and files
//!   numbered by name sit beside their siblings, which share sizes and times
//!   far more than the whole tree does. The descriptor's base is the bytes of
//!   packed values after the table, and its width the widest block's.
//! - **Sequence:** row `i`'s value is `base + i + packed`, for ids sorted
//!   strictly increasing. Ids without holes are all `packed` 0: width 0, no
//!   bytes but the padding, and a row found from its id by subtraction. A hole
//!   widens the column only to the bits of the holes' total.
//!
//! `base + packed` wraps, so no descriptor can make a read overflow.
//!
//! Decoding validates everything an accessor indexes by, so a corrupt or
//! truncated file is a [`DecodeError`] and never a panic: every width is at
//! most 64 and every column section's length is exactly its columns'
//! dictionaries and padded values (and the docs section's hashes); every
//! blocked column's table places each block exactly after the one before it,
//! and its widest block is exactly the descriptor's width; document ids rise
//! strictly and stay below `next_doc`; every offset, id and dictionary index is
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
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::sync::OnceLock;

use crate::packed::{self, Blocked, Packed, RUN};

pub(crate) const MAGIC: [u8; 8] = *b"FERRETCT";
/// 1: fixed-width rows (S1). 2: bit-packed columns (S1a).
pub(crate) const VERSION: u32 = 2;
/// "No id" in the builder's plan, and in the `u32` ids of fixed-width rows.
pub(crate) const NONE: u32 = u32::MAX;

pub(crate) const PAIR_ROW: usize = 8;
pub(crate) const WORK_TREE_ROW: usize = 32;
/// A document's hash row, after the docs section's id column.
pub(crate) const HASH_ROW: usize = 16;

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
/// name rows' three share one, and the docs' ids share theirs with the
/// hashes.
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
    DocId,
}

pub(crate) const COLUMNS: [Column; 17] = [
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
    Column::DocId,
];

/// How a column's packed values become field values; see the module doc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Coding {
    Frame,
    Nullable,
    Dictionary,
    Blocked,
    /// Blocked, and nullable per block.
    NullableBlocked,
    Sequence,
}

impl Coding {
    /// Whether the column is in blocks, read through [`Layout::blocked`].
    pub(crate) fn is_blocked(self) -> bool {
        matches!(self, Coding::Blocked | Coding::NullableBlocked)
    }
}

/// Which table a column has a row for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rows {
    Names,
    Dirs,
    Inodes,
    Docs,
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
            Column::DocId => Section::Docs,
        }
    }

    pub(crate) fn coding(self) -> Coding {
        match self {
            Column::DirName | Column::Entries | Column::Doc => Coding::NullableBlocked,
            Column::Dev | Column::Mode | Column::Owner => Coding::Dictionary,
            Column::NameParent
            | Column::NameOffset
            | Column::NameChild
            | Column::Ino
            | Column::Size
            | Column::Mtime
            | Column::MtimeNs
            | Column::Ctime
            | Column::CtimeNs
            | Column::Nlink => Coding::Blocked,
            Column::DocId => Coding::Sequence,
        }
    }

    pub(crate) fn rows(self) -> Rows {
        match self {
            Column::NameParent | Column::NameChild | Column::NameOffset => Rows::Names,
            Column::DirName | Column::Entries => Rows::Dirs,
            Column::DocId => Rows::Docs,
            _ => Rows::Inodes,
        }
    }
}

/// The header's bytes, before the section table.
pub(crate) const HEADER: usize = 40;
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

    /// A blocked column whose blocks hold `values` bytes, the widest
    /// `width` bits.
    pub(crate) fn blocked((values, width): (u64, u32)) -> Self {
        Self {
            base: values,
            width,
            dict_len: 0,
        }
    }

    /// The column's bytes for `count` rows: dictionary, then values; for a
    /// blocked column, its table and blocks. Saturates on a blocked
    /// descriptor no file could hold.
    pub(crate) fn len(self, coding: Coding, count: u32) -> u64 {
        match coding {
            Coding::Blocked | Coding::NullableBlocked => packed::blocked_len(count, self.base),
            _ => u64::from(self.dict_len) * 8 + packed::len(count, self.width),
        }
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
    pub(crate) docs: u32,
    /// Column descriptors, in [`COLUMNS`] order.
    pub(crate) columns: [Descriptor; COLUMNS.len()],
    /// Every section's length in [`SECTIONS`] order, less its columns: a
    /// column section's length adds its columns' to this (only the docs
    /// section has bytes of its own, its hashes, after its column).
    pub(crate) lens: [u64; SECTIONS.len()],
}

impl Head {
    pub(crate) fn count(&self, rows: Rows) -> u32 {
        match rows {
            Rows::Names => self.names,
            Rows::Dirs => self.dirs,
            Rows::Inodes => self.inodes,
            Rows::Docs => self.docs,
        }
    }

    fn section_lens(&self) -> [u64; SECTIONS.len()] {
        let mut lens = self.lens;
        for (column, desc) in COLUMNS.into_iter().zip(self.columns) {
            lens[column.section() as usize] += desc.len(column.coding(), self.count(column.rows()));
        }
        lens
    }

    /// The head of the file: header, section table and column descriptors,
    /// and the whole file's length.
    pub(crate) fn encode(&self) -> ([u8; TABLE_END], u64) {
        let mut out = Vec::with_capacity(TABLE_END);
        out.extend_from_slice(&MAGIC);
        for v in [
            VERSION,
            self.sniffer,
            self.next_doc,
            SECTIONS.len() as u32,
            self.dirs,
            self.inodes,
            self.names,
            self.docs,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        let mut offset = TABLE_END as u64;
        for len in self.section_lens() {
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
            offset += len;
        }
        for desc in self.columns {
            out.extend_from_slice(&desc.base.to_le_bytes());
            out.extend_from_slice(&desc.width.to_le_bytes());
            out.extend_from_slice(&desc.dict_len.to_le_bytes());
        }
        let mut head = [0; TABLE_END];
        head.copy_from_slice(&out);
        (head, offset)
    }
}

/// Buffered writes at a fixed place in a file, moving on as they go: each
/// section, and each column's values, writes through its own, so the builder
/// can fill several at once (D40: bounded buffers, never a whole column).
pub(crate) struct At<'f> {
    file: &'f File,
    pos: u64,
    buf: Vec<u8>,
}

/// What an [`At`] holds before writing it out.
const AT_BUFFER: usize = 64 << 10;

impl<'f> At<'f> {
    pub(crate) fn new(file: &'f File, pos: u64) -> Self {
        Self {
            file,
            pos,
            buf: Vec::with_capacity(AT_BUFFER),
        }
    }

    /// Writes out what is buffered; returns where the next byte would go.
    pub(crate) fn finish(mut self) -> io::Result<u64> {
        self.flush()?;
        Ok(self.pos)
    }
}

impl Write for At<'_> {
    /// Holds at most [`AT_BUFFER`] bytes: a write that would pass it fills
    /// the buffer and writes it out, and a remainder as large as the buffer
    /// goes straight to the file, so a whole heap written at once is never
    /// copied.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let room = AT_BUFFER - self.buf.len();
        if bytes.len() < room {
            self.buf.extend_from_slice(bytes);
            return Ok(bytes.len());
        }
        let (fill, rest) = bytes.split_at(room);
        self.buf.extend_from_slice(fill);
        self.flush()?;
        if rest.len() >= AT_BUFFER {
            self.file.write_all_at(rest, self.pos)?;
            self.pos += rest.len() as u64;
        } else {
            self.buf.extend_from_slice(rest);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.write_all_at(&self.buf, self.pos)?;
        self.pos += self.buf.len() as u64;
        self.buf.clear();
        Ok(())
    }
}

/// Streams one column to its place in the file: its dictionary, then its
/// values as they are pushed, each through a bounded buffer.
pub(crate) struct ColumnWriter<'f> {
    desc: Descriptor,
    coding: Coding,
    /// Rows pushed so far, for a sequence column.
    rows: u64,
    out: At<'f>,
    values: Values<'f>,
    end: u64,
}

enum Values<'f> {
    Packed(packed::Writer),
    /// The writer and the table's output; the blocks go to `out`.
    Blocked(Box<packed::BlockedWriter>, At<'f>),
}

impl<'f> ColumnWriter<'f> {
    /// Starts `column` at its place in `file`, as `layout` (decoded from the
    /// head just written) places it, writing its dictionary, which must have
    /// the descriptor's length.
    pub(crate) fn start(
        file: &'f File,
        layout: &Layout,
        column: Column,
        dict: &[u64],
    ) -> io::Result<Self> {
        let Placed { desc, start } = layout.columns[column as usize];
        let (coding, count) = (column.coding(), layout.count(column.rows()));
        debug_assert_eq!(dict.len(), desc.dict_len as usize);
        let at = (layout.range(column.section()).0 + start) as u64;
        let end = at + desc.len(coding, count as u32);
        // A dictionary, or a blocked column's table, comes first.
        let mut first = At::new(file, at);
        for &value in dict {
            first.write_all(&value.to_le_bytes())?;
        }
        let (out, values) = match coding {
            Coding::Blocked | Coding::NullableBlocked => {
                let table = packed::blocks(count as u32) * packed::BLOCK_ENTRY;
                let writer = Box::new(packed::BlockedWriter::new());
                (At::new(file, at + table), Values::Blocked(writer, first))
            }
            _ => (first, Values::Packed(packed::Writer::new(desc.width))),
        };
        Ok(Self {
            desc,
            coding,
            rows: 0,
            out,
            values,
            end,
        })
    }

    fn push(&mut self, raw: u64) -> io::Result<()> {
        self.rows += 1;
        match &mut self.values {
            Values::Packed(writer) => writer.push(&mut self.out, raw),
            Values::Blocked(writer, table) => writer.push(table, &mut self.out, raw),
        }
    }

    /// A frame-of-reference or blocked value, inside the range the
    /// descriptor was made from.
    pub(crate) fn value(&mut self, value: u64) -> io::Result<()> {
        match self.coding {
            Coding::Blocked | Coding::NullableBlocked => self.push(value),
            Coding::Sequence => self.push(value - self.rows - self.desc.base),
            _ => self.push(value - self.desc.base),
        }
    }

    /// A nullable column's value, or none.
    pub(crate) fn nullable(&mut self, value: Option<u64>) -> io::Result<()> {
        match (value, &mut self.values) {
            (Some(value), _) => self.value(value),
            (None, Values::Blocked(writer, table)) => {
                self.rows += 1;
                writer.push_null(table, &mut self.out)
            }
            (None, Values::Packed(_)) => self.push(packed::mask(self.desc.width)),
        }
    }

    /// A dictionary column's index.
    pub(crate) fn index(&mut self, index: usize) -> io::Result<()> {
        self.push(index as u64)
    }

    /// Writes the padding, and any last block, and flushes.
    pub(crate) fn finish(mut self) -> io::Result<()> {
        match self.values {
            Values::Packed(writer) => writer.finish(&mut self.out)?,
            Values::Blocked(writer, mut table) => {
                writer.finish(&mut table, &mut self.out)?;
                table.finish()?;
            }
        }
        let end = self.out.finish()?;
        debug_assert_eq!(end, self.end, "column ends where the head placed its end");
        Ok(())
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

/// A column's bytes, decoded in place per its coding; any but a blocked
/// column, which is a [`Blocked`] ([`Layout::blocked`]). The coding is fixed
/// per column, so no read branches on it: a `View` that also held blocked
/// columns stopped `View::get` being inlined into `Catalog::name`, and a name
/// scan at 10M names took 11% longer.
#[derive(Clone, Copy)]
pub(crate) struct View<'a> {
    base: u64,
    dict: &'a [u8],
    packed: Packed<'a>,
}

impl<'a> View<'a> {
    /// A frame-of-reference value, or a dictionary index.
    pub(crate) fn get(&self, row: usize) -> u64 {
        self.base.wrapping_add(self.packed.get(row))
    }

    /// A nullable value.
    pub(crate) fn nullable(&self, row: usize) -> Option<u64> {
        let raw = self.packed.get(row);
        (raw != packed::mask(self.packed.width())).then(|| self.base.wrapping_add(raw))
    }

    /// A sequence column's value at `row`.
    pub(crate) fn sequence(&self, row: usize) -> u64 {
        self.get(row).wrapping_add(row as u64)
    }

    /// For a sequence column that is `base + row` throughout, `base`: its
    /// row for a value is found by subtraction.
    pub(crate) fn dense(&self) -> Option<u64> {
        (self.packed.width() == 0).then_some(self.base)
    }

    /// A dictionary value. Decoding checked every index.
    pub(crate) fn lookup(&self, row: usize) -> u64 {
        u64_at(self.dict, self.get(row) as usize * 8)
    }

    /// [`View::get`] of rows `first..first + out.len()`, into `out`; `first`
    /// is a multiple of 8. See [`Packed::decode`].
    pub(crate) fn decode(&self, first: usize, out: &mut [u64]) {
        self.packed.decode(first, out);
        for value in out {
            *value = self.base.wrapping_add(*value);
        }
    }

    /// [`View::get`] of rows `0..count`, in order: a pass over one column,
    /// decoded a run at a time.
    pub(crate) fn values(self, count: usize) -> impl Iterator<Item = u64> + 'a {
        packed::runs(count, move |first, out| self.decode(first, out))
    }

    /// [`View::nullable`] of rows `0..count`, in order.
    pub(crate) fn nullables(self, count: usize) -> impl Iterator<Item = Option<u64>> + 'a {
        let none = self.base.wrapping_add(packed::mask(self.packed.width()));
        self.values(count)
            .map(move |value| (value != none).then_some(value))
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
    pub(crate) docs: usize,
    /// Where the hashes start in the docs section: after its id column.
    pub(crate) hashes: usize,
}

impl Layout {
    pub(crate) fn count(&self, rows: Rows) -> usize {
        match rows {
            Rows::Names => self.names,
            Rows::Dirs => self.dirs,
            Rows::Inodes => self.inodes,
            Rows::Docs => self.docs,
        }
    }

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

    /// `column`, given the bytes of its section. Not a blocked column.
    pub(crate) fn view<'a>(&self, column: Column, section: &'a [u8]) -> View<'a> {
        debug_assert!(!column.coding().is_blocked(), "{column:?}");
        let Placed { desc, start } = self.columns[column as usize];
        let (dict, values) = section[start..].split_at(desc.dict_len as usize * 8);
        View {
            base: desc.base,
            dict,
            packed: Packed::new(values, desc.width),
        }
    }

    /// Blocked `column`, given the bytes of its section.
    pub(crate) fn blocked<'a>(&self, column: Column, section: &'a [u8]) -> Blocked<'a> {
        debug_assert!(column.coding().is_blocked(), "{column:?}");
        let start = self.columns[column as usize].start;
        Blocked::new(&section[start..], self.count(column.rows()) as u32)
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
    let docs = u32_at(head, 36);
    if inodes == NONE || names == NONE || dirs > inodes {
        return Err(DecodeError::Corrupt("counts"));
    }
    let len = |s: Section| (sections[s as usize].1 - sections[s as usize].0) as u64;
    for (section, row) in [
        (Section::Roots, PAIR_ROW),
        (Section::Links, PAIR_ROW),
        (Section::WorkTrees, WORK_TREE_ROW),
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
            Rows::Docs => docs,
        };
        columns[i] = Placed {
            desc,
            start: used[section as usize] as usize,
        };
        used[section as usize] =
            used[section as usize].saturating_add(desc.len(column.coding(), count));
    }
    // The docs section's hashes follow its id column.
    let hashes = used[Section::Docs as usize];
    used[Section::Docs as usize] = hashes.saturating_add(u64::from(docs) * HASH_ROW as u64);
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
        docs: docs as usize,
        hashes: hashes as usize,
    })
}

/// Validates a whole file held in memory: the table, then every section.
pub(crate) fn decode(bytes: &[u8]) -> Result<Layout, DecodeError> {
    let layout = decode_table(bytes, bytes.len() as u64)?;
    let facts = Facts::default();
    for section in CHECK_ORDER {
        check(section, &layout, &facts, |s| layout.section(bytes, s))?;
    }
    Ok(layout)
}

/// What one section's check found that a later check reuses, so that no
/// check decodes a column a check before it already decoded. Kept by the
/// reader for as long as it loads sections.
#[derive(Debug, Default)]
pub(crate) struct Facts {
    /// Names whose child is a directory, counted by the name-rows check.
    dir_children: OnceLock<usize>,
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
    facts: &Facts,
    get: impl Fn(Section) -> &'a [u8],
) -> Result<(), DecodeError> {
    let terminated = |heap: &[u8]| heap.last().is_none_or(|&b| b == 0);
    let strings_len = l.len(Section::Strings);
    for column in COLUMNS {
        let coding = column.coding();
        if column.section() == section && coding.is_blocked() {
            let desc = l.columns[column as usize].desc;
            let nullable = coding == Coding::NullableBlocked;
            if !l.blocked(column, get(section)).check(desc.width, desc.base, nullable) {
                return Err(DecodeError::Corrupt(section.label()));
            }
        }
    }
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
        Section::Names => {
            let dir_children = check_names(l, get(Section::Names), get(Section::NameHeap))?;
            // A racing loader may have set it first; its count is the same.
            let _ = facts.dir_children.set(dir_children);
        }
        Section::DirNames => {
            let dir_children = *facts.dir_children.get().unwrap_or_else(|| {
                panic!("dir names checked before the names they need");
            });
            check_dir_names(l, get(Section::DirNames), get(Section::Names), dir_children)?;
        }
        Section::Dev => check_dictionary(l, Column::Dev, get(Section::Dev))?,
        Section::Mode => check_dictionary(l, Column::Mode, get(Section::Mode))?,
        Section::Owner => check_dictionary(l, Column::Owner, get(Section::Owner))?,
        Section::Roots => {
            let roots = get(Section::Roots);
            let dir_names = l.blocked(Column::DirName, get(Section::DirNames));
            let unnamed = dir_names.nullables().filter(Option::is_none).count();
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
            // Each id as the reader decodes it: sorted, and assigned.
            let ids = l.view(Column::DocId, get(Section::Docs));
            let mut last = None;
            for (row, id) in ids.values(l.docs).enumerate() {
                let id = id.wrapping_add(row as u64);
                if id >= u64::from(l.next_doc) || last.is_some_and(|last| id <= last) {
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
    if l.view(column, bytes).values(l.inodes).any(|i| i >= len) {
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
///
/// One pass decodes each column once: name `i - 1`'s span is checked when
/// row `i`'s offset, its end, has been. Returns the names whose child is a
/// directory, for [`check_dir_names`].
fn check_names(l: &Layout, rows: &[u8], heap: &[u8]) -> Result<usize, DecodeError> {
    if count_nuls(heap) != l.names {
        return Err(DecodeError::Corrupt("name order"));
    }
    let parents = l.blocked(Column::NameParent, rows);
    let children = l.blocked(Column::NameChild, rows);
    let offsets = l.blocked(Column::NameOffset, rows);
    // The name before this row: its parent and where its bytes start; and
    // the one before that, for sibling order.
    let mut open: Option<(u64, usize)> = None;
    let mut previous: Option<(u64, &[u8])> = None;
    let mut close = |end: usize, open: Option<(u64, usize)>| {
        let Some((parent, start)) = open else {
            return Ok(());
        };
        let (name, nul) = (&heap[start..end - 1], heap[end - 1]);
        let ordered = previous.is_none_or(|(p, prev)| p != parent || prev < name);
        if name.is_empty() || nul != 0 || !ordered {
            return Err(DecodeError::Corrupt("name order"));
        }
        previous = Some((parent, name));
        Ok(())
    };
    let (mut last_parent, mut next_offset, mut dir_children) = (0, 0, 0);
    let mut block = [[0; RUN]; 3];
    for first in (0..l.names).step_by(RUN) {
        let n = RUN.min(l.names - first);
        let [p, c, o] = &mut block;
        parents.decode(first, &mut p[..n]);
        children.decode(first, &mut c[..n]);
        offsets.decode(first, &mut o[..n]);
        for j in 0..n {
            let (parent, child, offset) = (p[j], c[j], o[j]);
            let ok = parent < l.dirs as u64
                && child < l.inodes as u64
                && parent >= last_parent
                && offset >= next_offset
                && (next_offset > 0 || offset == 0)
                && offset < heap.len() as u64;
            if !ok {
                return Err(DecodeError::Corrupt("names"));
            }
            close(offset as usize, open)?;
            open = Some((parent, offset as usize));
            last_parent = parent;
            next_offset = offset + 1;
            dir_children += usize::from(child < l.dirs as u64);
        }
    }
    close(heap.len(), open)?;
    Ok(dir_children)
}

/// Every directory's name edge names it, from a lower-numbered parent, so a
/// walk upwards strictly descends and ends at a root. And the converse: a
/// name whose child is a directory must be that directory's recorded edge,
/// so each directory has exactly one name and a root has none. Without it a
/// name could make a root its own child, and a walk down (retention copying
/// a kept root) would never end.
///
/// The converse is a count. The first loop proves each named directory's
/// edge names it, so those edges are distinct names with directory children;
/// if no other name has a directory child, every such name is some
/// directory's edge. The names with a directory child, `dir_children`, were
/// counted as [`check_names`] decoded the child column, where a check per
/// name looked up its child's edge at random.
fn check_dir_names(
    l: &Layout,
    dir_names: &[u8],
    rows: &[u8],
    dir_children: usize,
) -> Result<(), DecodeError> {
    let dir_names = l.blocked(Column::DirName, dir_names);
    let parents = l.blocked(Column::NameParent, rows);
    let children = l.blocked(Column::NameChild, rows);
    let mut named = 0;
    for (dir, name) in dir_names.nullables().enumerate() {
        let Some(name) = name else {
            continue;
        };
        let ok = name < l.names as u64 && {
            let name = name as usize;
            children.get(name) == dir as u64 && parents.get(name) < dir as u64
        };
        if !ok {
            return Err(DecodeError::Corrupt("dir names"));
        }
        named += 1;
    }
    if dir_children != named {
        return Err(DecodeError::Corrupt("dir names"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Write;

    use super::{AT_BUFFER, At};

    #[test]
    fn a_positional_writer_never_holds_more_than_its_buffer() {
        let path = std::env::temp_dir().join(format!("ferret-at-{}", std::process::id()));
        let file = std::fs::File::options()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        // After a 3-byte gap: a small write; one that exactly fills the
        // buffer; one that fills it and leaves a remainder larger than the
        // buffer, written directly; two halves, the second overflowing into a
        // small buffered remainder; a tail. The bytes land in order.
        let pattern = |n: usize, seed: u8| -> Vec<u8> {
            (0..n).map(|i| (i as u8).wrapping_mul(31) ^ seed).collect()
        };
        let parts = [
            pattern(10, 1),
            pattern(AT_BUFFER - 10, 2),
            pattern(3 * AT_BUFFER + 7, 3),
            pattern(AT_BUFFER / 2, 4),
            pattern(AT_BUFFER / 2 + 5, 5),
            pattern(5, 6),
        ];
        let mut at = At::new(&file, 3);
        for part in &parts {
            at.write_all(part).unwrap();
            assert!(at.buf.len() < AT_BUFFER, "{} buffered", at.buf.len());
            assert_eq!(at.buf.capacity(), AT_BUFFER, "the buffer grew");
        }
        let end = at.finish().unwrap();
        let expected: Vec<u8> = [&[0u8; 3][..]]
            .into_iter()
            .chain(parts.iter().map(Vec::as_slice))
            .flatten()
            .copied()
            .collect();
        assert_eq!(end, expected.len() as u64);
        assert_eq!(std::fs::read(&path).unwrap(), expected);
        let _ = std::fs::remove_file(&path);
    }
}
