//! One immutable segment: a front-coded term dictionary and doc-id postings
//! over a contiguous DocId range `[first, last]` (docs/S2.md § Segment
//! file). [`Writer`] takes terms in order; [`Inverter`] builds them from
//! `(DocId, token)` input; [`Segment`] reads one back.
//!
//! ```text
//! head      fixed fields                                            64 B
//!             magic "ferretsg", format version u32, tokenizer version u32,
//!             dictionary codec u32, postings codec u32, first DocId u32,
//!             last DocId u32, terms u64, postings u64, chunk size u32,
//!             section count u32, 8 B zero
//!           section table: (offset u64, length u64, checksum 16 B) for
//!             blocks, postings, index, sums                       128 B
//!           head digest: BLAKE3-128 of every preceding head byte    16 B
//! blocks    front-coded dictionary blocks of BLOCK terms
//! postings  pfor128skip lists, concatenated in term order
//! index     per block: its first term, its byte offset, its postings base
//! sums      BLAKE3-128 per CHUNK bytes of blocks, then of postings
//! ```
//!
//! All integers are little-endian; `vbyte` is LEB128. Sections tile the
//! file from byte [`HEAD`] in that order.
//!
//! **Dictionary codec 1.** Each block holds up to [`BLOCK`] entries. Entry 0
//! stores no term bytes, because its term is the block's first term in the
//! resident index. Every later entry stores `vbyte shared-prefix length,
//! vbyte suffix length, suffix` against the entry before it. Then every
//! entry stores `vbyte df` and, for df = 1, `vbyte (doc - first)`, the
//! inlined singleton with no postings bytes; otherwise `vbyte list length`.
//! A list's offset is the block's postings base plus the lengths before it
//! in the block, so offsets cost nothing beyond the lengths.
//!
//! **Index.** Per block, `vbyte first-term length, first term, vbyte
//! previous block's byte length, vbyte previous block's postings bytes` (both
//! 0 for block 0). It is read whole when the segment opens and stays
//! resident (D64 A), so a lookup binary-searches it and reads one block.
//!
//! **Postings codec 1.** `intpack::pfor128skip` sorted lists of `doc -
//! first`, doc ids only (D61 B). A codec id, not the format version, says
//! so: frequencies would be codec 2 with the same head.
//!
//! **Checksums.** Head, index and sums are read whole at open and checked
//! against the head digest and their table checksums, the catalog's
//! `checkpoint_checksum` convention (BLAKE3 truncated to 128 bits). Blocks
//! and postings are too large to read whole for one lookup (D64 A reads
//! each list with `pread`), so each is checksummed per [`CHUNK`] bytes in
//! `sums`, and its table checksum is the BLAKE3-128 of its own chunk sums.
//! Every read verifies the chunks it touches before decoding a byte of them.
//! So a torn or flipped segment is an error from `open` or from the read
//! that reaches the damage, never a panic: intpack's decoders index their
//! input unchecked, and only checksummed bytes reach them. A file crafted
//! with valid checksums and an inconsistent list could still panic inside
//! intpack; that is outside the contract, as for the catalog's packed
//! columns.

use std::fmt;
use std::io;

mod invert;
mod read;
mod write;

#[cfg(test)]
mod tests;

pub use invert::Inverter;
pub use read::{Hit, Info, PostingCursor, Postings, ReadAt, Segment, TermEntry, Terms};
pub use write::{Sizes, Writer};

const MAGIC: [u8; 8] = *b"ferretsg";

/// Bumped when the container (head, sections, checksums) changes.
pub const FORMAT_VERSION: u32 = 1;

/// Front-coded blocks of [`BLOCK`] terms with singletons inlined.
pub const DICTIONARY_CODEC: u32 = 1;

/// `pfor128skip` doc ids relative to the segment's first DocId, no
/// frequencies (D61 B).
pub const POSTINGS_CODEC: u32 = 1;

/// Terms per dictionary block: a lookup decodes at most this many entries.
pub const BLOCK: usize = 32;

/// Bytes per checksummed chunk of the blocks and postings sections: one
/// page, so a rare term's read costs one page plus its hash.
pub const CHUNK: usize = 4096;

/// Sections, in file order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Blocks = 0,
    Postings = 1,
    Index = 2,
    Sums = 3,
}

const SECTIONS: [Section; 4] = [
    Section::Blocks,
    Section::Postings,
    Section::Index,
    Section::Sums,
];

const FIELDS: usize = 64;
const TABLE_ENTRY: usize = 32;

/// Bytes of the head: fixed fields, section table and head digest.
pub const HEAD: usize = FIELDS + SECTIONS.len() * TABLE_ENTRY + 16;

/// Why a segment could not be read.
#[derive(Debug)]
pub enum ReadError {
    /// The caller stopped a cooperative candidate read.
    Cancelled,
    /// The byte source failed.
    Io(io::Error),
    /// Too short for a head, or the wrong magic.
    NotASegment,
    /// The head digest fails, or its fields are inconsistent.
    Head,
    /// A format version this build does not read.
    Version(u32),
    /// A dictionary or postings codec this build does not read.
    Codec { dictionary: u32, postings: u32 },
    /// The section table does not tile the file: truncated, extended or
    /// corrupt.
    Layout,
    /// Bytes fail their checksum, or decode inconsistently. Names the
    /// section.
    Corrupt(Section),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => write!(f, "candidate read cancelled"),
            Self::Io(error) => write!(f, "segment read failed: {error}"),
            Self::NotASegment => write!(f, "not a segment file"),
            Self::Version(v) => write!(f, "segment format version {v}, expected {FORMAT_VERSION}"),
            Self::Codec {
                dictionary,
                postings,
            } => write!(
                f,
                "segment codecs {dictionary}/{postings}, expected {DICTIONARY_CODEC}/{POSTINGS_CODEC}"
            ),
            Self::Head => write!(f, "segment head corrupt"),
            Self::Layout => write!(f, "segment truncated or corrupt: bad section table"),
            Self::Corrupt(section) => write!(f, "segment corrupt: {section:?}"),
        }
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ReadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// A caller broke the writer's input contract. Nothing has been written.
#[derive(Debug, PartialEq, Eq)]
pub enum WriteError {
    /// `first > last`.
    Range { first: u32, last: u32 },
    /// A term not strictly after the one before it.
    TermOrder,
    /// A term with no documents.
    NoDocuments,
    /// Documents not strictly increasing, or a document added below one
    /// already added.
    DocOrder,
    /// A document outside `[first, last]`.
    DocRange(u32),
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Range { first, last } => write!(f, "segment range [{first}, {last}] is empty"),
            Self::TermOrder => write!(f, "terms not in strictly increasing order"),
            Self::NoDocuments => write!(f, "a term with no documents"),
            Self::DocOrder => write!(f, "documents not in strictly increasing order"),
            Self::DocRange(doc) => write!(f, "document {doc} outside the segment range"),
        }
    }
}

impl std::error::Error for WriteError {}

/// BLAKE3-128, the catalog's `checkpoint_checksum` convention.
fn checksum(bytes: &[u8]) -> [u8; 16] {
    let mut digest = [0; 16];
    digest.copy_from_slice(&blake3::hash(bytes).as_bytes()[..16]);
    digest
}

/// Chunk sums of `bytes`, appended to `out`.
fn chunk_sums(bytes: &[u8], out: &mut Vec<u8>) {
    for chunk in bytes.chunks(CHUNK) {
        out.extend_from_slice(&checksum(chunk));
    }
}

/// Chunks covering `len` bytes.
fn chunks(len: u64) -> u64 {
    len.div_ceil(CHUNK as u64)
}

/// Appends one LEB128 value.
fn put(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Reads one LEB128 value at `*at`, advancing it; `None` past the end or
/// beyond 64 bits.
fn take(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at)?;
        *at += 1;
        let low = u64::from(byte & 0x7f);
        if shift == 63 && low > 1 {
            return None;
        }
        value |= low << shift;
        if byte < 0x80 {
            return Some(value);
        }
    }
    None
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}
