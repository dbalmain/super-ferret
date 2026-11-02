//! The snapshot file: one generation of the catalog.
//!
//! ```text
//! header       v3's 40 B fields, incarnation 16 B, checkpoint/sequence u64,
//!              next inode/name u32, reserved zeros 16 B                 (96 B)
//! table        (offset u64, length u64, checksum 16 B) per section      (832 B)
//! descriptors  (base u64, width u32, dictionary length u32) per column (288 B)
//! head digest  BLAKE3-128 of every preceding head byte                  (16 B)
//! sections     contiguous from byte 1232 to the end, in SECTIONS order
//! ```
//!
//! All integers are little-endian. The sections a name query reads come
//! first, and a reader loads each on first use (D38 B).
//!
//! ```text
//! names       3 blocked columns: parent InoId, child InoId or ignored type tag, offset in
//!             name heap
//! name heap   names, each NUL-terminated, in NameId order (D28)
//! dir names   column, per directory InoId: its NameId, none for a root
//!             (nullable)
//! entries     column, per directory: the entries `getdents` returned, minus
//!             `.` and `..`, before ignore rules; none if unknown (D47)
//!             (nullable blocked)
//! traversed   structural-directory bitset (D29), LSB first; optionally
//!             followed by an independent search-suppression bitset
//! roots       8 B rows: root InoId, offset of its path in strings
//! strings     root paths, link targets, work-tree paths; NUL-terminated
//! dev         column per inode, dictionary
//! ino         column per inode (blocked)
//! size        column per inode (blocked)
//! mtime       column per inode: whole seconds, signed (blocked)
//! mtime ns    column per inode (blocked)
//! ctime       column per inode: whole seconds, signed (blocked)
//! ctime ns    column per inode (blocked)
//! mode        column per inode, dictionary
//! owner       column per inode, dictionary of uid << 32 | gid
//! nlink       column per inode (blocked)
//! doc         column per inode: DocId, none when it has no document
//!             (nullable blocked)
//! states      2 bits per inode: ContentState (D37), LSB first
//! links       8 B rows: symlink InoId, offset of its target in strings
//! specials    8 B rows: visible FIFO/socket/device InoId and Kind tag
//! work trees  32 B rows: top InoId, offset of common dir in strings, common
//!             dev, common ino, kind u8, 7 B zero
//! docs        column per document: its DocId (sequence), then 16 B rows:
//!             its hash; sorted by id, live documents only (D36 B)
//! doc refs    u32 indexed-inode count per live document, in Docs order
//! retained at nullable blocked u64 last trustworthy sequence per directory
//! policy      BLAKE3-128 fingerprint of global rules and eligibility
//! revealed    u32 per name a repository ignore file's `!` line reveals
//!             although it is hidden (D127): its NameId, strictly rising
//! ```
//!
//! A column is its dictionary (`u64` values, present only in a dictionary
//! column) and then one value per row of its table, bit-packed at the
//! descriptor's width (`packed`), then 8 bytes of padding (written as zeros,
//! checksummed along with all section bytes). Each column is sized to this
//! catalog's values (D43), in one of five codings, each built on a frame of
//! reference: a value is `base + packed`, and the width is that of the largest
//! value less the smallest, which is the base. Signed times are stored with
//! the sign bit flipped ([`order`]), which maps `i64` onto `u64` in order, so
//! any two times' difference fits.
//!
//! - **Nullable:** one frame for the column, except that the all-ones value of
//!   the width means none. The writer makes the width wide enough that no real
//!   value is all ones. Only dir names: blocked, they were 3 MiB smaller at 10M
//!   names and a full-name scan, which reads one per directory it enters, took
//!   1.5–2% longer.
//! - **Dictionary:** the packed value is `base + index` into the column's
//!   dictionary. The writer stores base 0 and the sorted distinct values.
//! - **Blocked:** a frame of reference per block of 128 rows, each block with
//!   its own base and width, found through a table of 16 B entries at the
//!   column's start (`packed::Blocked`). For a column whose neighbouring rows
//!   are close: name offsets only grow, parents only rise, a directory's
//!   children are numbered together, nlink is nearly always 1, and files
//!   numbered by name sit beside their siblings, which share sizes and times
//!   far more than the whole tree does; inode numbers and nanoseconds, of files
//!   made together, share their high bits. The descriptor's base is the bytes
//!   of packed values after the table, and its width the widest block's.
//! - **Nullable blocked:** blocked, with each block framed by its real values
//!   alone. A block that holds a none sets `packed::blocked::NULLS` beside its
//!   width, and its width's all-ones value is none; a block of nothing but
//!   nones is width 0, and a block without one keeps its plain width. For the
//!   inodes' documents, which directories and unhashed files leave out in runs,
//!   and entry counts.
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
//! and its widest block is exactly the descriptor's width, and only a
//! nullable blocked column flags a block as holding a none; document ids rise
//! strictly and stay below `next_doc`; every offset, id and dictionary index is
//! in range, every heap ends in NUL, and each directory's name edge points at
//! a lower-numbered parent, so walking up from any name ends at a root. Each
//! directory is the child of exactly its recorded name edge, so walking down
//! from a root ends too. Field values that index nothing (times, sizes,
//! nlink, entry counts, dictionary values, an inode's `DocId`) are not
//! structurally constrained; every byte is protected by a section checksum
//! verified before any column is interpreted.
//!
//! Validation is split so that it can run per section: [`decode_table`]
//! checks the header, the table, the descriptors and every count and length
//! at open, and [`check`] validates one section when it is loaded. A check
//! that spans sections (a directory's name edge against the name rows) runs
//! when the later one loads, after the sections it [needs](Section::needs).

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::{fmt, io};

use crate::generation::{Generation, checksum};
use crate::packed;

mod check;
mod encode;
mod layout;
use check::CHECK_ORDER;
pub(crate) use check::{Facts, check, document_references};
pub(crate) use encode::{At, Bits, ColumnWriter, Head, Output, put_pair, put_u32, put_work_tree};
use layout::count_nuls;
pub(crate) use layout::{Layout, Placed, View, decode_table, u32_at, u64_at};

pub(crate) const MAGIC: [u8; 8] = *b"FERRETCT";
/// 1: fixed-width rows (S1). 2: bit-packed columns (S1a).
/// 3: ignored type tags, collapsed opaque directories and visible specials.
/// 4: checked checkpoint, incarnation, allocation counters and writer sections.
/// 5: the revealed-names section and the log's `NamePut` flags (D127).
pub(crate) const VERSION: u32 = 5;
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
    /// Visible FIFOs, sockets and devices: (inode, kind), sorted by inode.
    Specials,
    /// Work-tree rows (D23).
    WorkTrees,
    /// Live documents and their hashes.
    Docs,
    /// Indexed inode references per live document, in Docs row order.
    DocRefs,
    /// Last trustworthy subtree sequence per directory, or none.
    RetainedAt,
    /// Writer policy fingerprint (BLAKE3-128).
    Policy,
    /// Hidden names a repository ignore file's `!` line reveals (D127).
    Revealed,
}

pub(crate) const SECTIONS: [Section; 27] = [
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
    Section::Specials,
    Section::WorkTrees,
    Section::Docs,
    Section::DocRefs,
    Section::RetainedAt,
    Section::Policy,
    Section::Revealed,
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
    RetainedAt,
}

pub(crate) const COLUMNS: [Column; 18] = [
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
    Column::RetainedAt,
];

/// How a column's packed values become field values; see the module doc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Coding {
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
            Column::RetainedAt => Section::RetainedAt,
        }
    }

    pub(crate) fn coding(self) -> Coding {
        match self {
            Column::DirName => Coding::Nullable,
            Column::Entries | Column::Doc | Column::RetainedAt => Coding::NullableBlocked,
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
            Column::DirName | Column::Entries | Column::RetainedAt => Rows::Dirs,
            Column::DocId => Rows::Docs,
            _ => Rows::Inodes,
        }
    }
}

/// The header's bytes, before the section table.
pub(crate) const HEADER: usize = 96;
const TABLE: usize = SECTIONS.len() * 32;
const DESCRIPTORS: usize = COLUMNS.len() * 16;

/// The bytes of the header, section table and column descriptors: what
/// opening a file reads.
pub(crate) const TABLE_END: usize = HEADER + TABLE + DESCRIPTORS + 16;

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

/// Validates a whole file held in memory: the table, then every section.
pub(crate) fn decode(bytes: &[u8]) -> Result<Layout, DecodeError> {
    let layout = decode_table(bytes, bytes.len() as u64)?;
    let facts = Facts::default();
    for section in CHECK_ORDER {
        check(section, &layout, &facts, |s| layout.section(bytes, s))?;
    }
    Ok(layout)
}

/// Finalizes checksums after bounded positional section writes. Never holds a
/// section in memory; callers must open the file for both reading and writing.
pub(crate) fn seal(file: &File) -> io::Result<()> {
    let len = file.metadata()?.len();
    let mut head = vec![0; TABLE_END];
    file.read_exact_at(&mut head, 0)?;
    let layout = decode_table(&head, len).map_err(io::Error::other)?;
    let mut buffer = vec![0; 1 << 20];
    for section in SECTIONS {
        let (start, end) = layout.range(section);
        let mut position = start;
        let mut hasher = blake3::Hasher::new();
        while position < end {
            let n = buffer.len().min(end - position);
            file.read_exact_at(&mut buffer[..n], position as u64)?;
            hasher.update(&buffer[..n]);
            position += n;
        }
        let at = HEADER + section as usize * 32 + 16;
        head[at..at + 16].copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    }
    let digest = checksum(&head[..TABLE_END - 16]);
    head[TABLE_END - 16..].copy_from_slice(&digest);
    crate::bulk::write_at(file, &head, 0)
}

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
            Section::Links => &[Section::Strings, Section::Specials],
            Section::WorkTrees => &[Section::Strings],
            Section::DocRefs => &[Section::Docs, Section::Doc],
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
            Section::Specials => "specials",
            Section::WorkTrees => "work trees",
            Section::Docs => "docs",
            Section::DocRefs => "document references",
            Section::RetainedAt => "retained at",
            Section::Policy => "policy",
            Section::Revealed => "revealed",
        }
    }
}
