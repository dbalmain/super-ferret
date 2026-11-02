//! Decoding a snapshot head: the header, the section table and the column
//! descriptors, checked into a [`Layout`] that places every section and
//! column, and the [`View`] that reads them.

use super::*;
use crate::seek::{partition_point, seek};

/// The NUL bytes in `bytes`. Summed as `u8` per 255-byte chunk, which the
/// compiler vectorises; a plain `filter().count()` measured about 4 ms over
/// `$HOME`'s 10.6 MB heap, a third of the whole open.
pub(super) fn count_nuls(bytes: &[u8]) -> usize {
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

    /// Finds a live sequence id, including sparse historical DocIds. The
    /// zero-width representation permits subtraction instead of searching.
    pub(crate) fn sequence_row(self, id: u64, count: usize) -> Option<usize> {
        self.find_sequence(id, count, |below| partition_point(count, below))
    }

    /// As [`View::sequence_row`], searching from row `near`, the answer to a
    /// nearby earlier lookup ([`seek`]).
    pub(crate) fn sequence_row_near(self, id: u64, count: usize, near: usize) -> Option<usize> {
        self.find_sequence(id, count, |below| seek(near.min(count), count, below))
    }

    fn find_sequence(
        self,
        id: u64,
        count: usize,
        search: impl FnOnce(&dyn Fn(usize) -> bool) -> usize,
    ) -> Option<usize> {
        let at = match self.dense() {
            Some(first) => id
                .checked_sub(first)
                .map_or(count, |row| row.min(count as u64) as usize),
            None => search(&|row| self.sequence(row) < id),
        };
        (at < count && self.sequence(at) == id).then_some(at)
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
    pub(crate) generation: Generation,
    pub(crate) sniffer: u32,
    pub(crate) next_doc: u32,
    pub(crate) sections: [(usize, usize); SECTIONS.len()],
    pub(crate) checksums: [[u8; 16]; SECTIONS.len()],
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

    pub(super) fn len(&self, section: Section) -> usize {
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
    let generation = decode_header(head)?;
    let (sections, checksums) = decode_sections(head, file_len)?;
    let counts = Counts {
        dirs: u32_at(head, 24),
        inodes: u32_at(head, 28),
        names: u32_at(head, 32),
        docs: u32_at(head, 36),
    };
    if counts.inodes > NONE - 16 || counts.names == NONE || counts.dirs > counts.inodes {
        return Err(DecodeError::Corrupt("counts"));
    }
    let len = |s: Section| (sections[s as usize].1 - sections[s as usize].0) as u64;
    check_lengths(&counts, len)?;
    let (columns, hashes) = place_columns(head, &counts, len)?;
    Ok(Layout {
        generation,
        checksums,
        sniffer: u32_at(head, 12),
        next_doc: u32_at(head, 16),
        sections,
        columns,
        dirs: counts.dirs as usize,
        inodes: counts.inodes as usize,
        names: counts.names as usize,
        docs: counts.docs as usize,
        hashes,
    })
}

/// The row counts a catalog's header declares.
struct Counts {
    dirs: u32,
    inodes: u32,
    names: u32,
    docs: u32,
}

impl Counts {
    fn rows(&self, rows: Rows) -> u32 {
        match rows {
            Rows::Names => self.names,
            Rows::Dirs => self.dirs,
            Rows::Inodes => self.inodes,
            Rows::Docs => self.docs,
        }
    }
}

/// Validates the header's magic, version, checksum and checkpoint, and
/// returns its generation.
fn decode_header(head: &[u8]) -> Result<Generation, DecodeError> {
    if head.len() < 12 || head[..8] != MAGIC {
        return Err(DecodeError::NotACatalog);
    }
    let version = u32_at(head, 8);
    if version != VERSION {
        return Err(DecodeError::Version(version));
    }
    if head.len() < TABLE_END || u32_at(head, 20) as usize != SECTIONS.len() {
        return Err(DecodeError::Layout);
    }
    if checksum(&head[..TABLE_END - 16]) != head[TABLE_END - 16..TABLE_END] {
        return Err(DecodeError::Corrupt("head"));
    }
    let mut incarnation = [0; 16];
    incarnation.copy_from_slice(&head[40..56]);
    let generation = Generation {
        incarnation,
        checkpoint: u64_at(head, 56),
        sequence: u64_at(head, 64),
    };
    if incarnation == [0; 16]
        || generation.checkpoint == u64::MAX
        || generation.sequence == u64::MAX
        || head[80..96] != [0; 16]
        || u32_at(head, 72) != u32_at(head, 28)
        || u32_at(head, 76) != u32_at(head, 32)
    {
        return Err(DecodeError::Corrupt("checkpoint header"));
    }
    Ok(generation)
}

/// Each section's byte range in the file, and each section's checksum.
type SectionTable = ([(usize, usize); SECTIONS.len()], [[u8; 16]; SECTIONS.len()]);

/// The section table: each section's byte range, tiling the file from the
/// table's end, and its checksum.
fn decode_sections(head: &[u8], file_len: u64) -> Result<SectionTable, DecodeError> {
    let mut sections = [(0, 0); SECTIONS.len()];
    let mut checksums = [[0; 16]; SECTIONS.len()];
    let mut expect = TABLE_END as u64;
    for (i, slot) in sections.iter_mut().enumerate() {
        let offset = u64_at(head, HEADER + i * 32);
        let len = u64_at(head, HEADER + i * 32 + 8);
        let end = offset.checked_add(len).ok_or(DecodeError::Layout)?;
        if offset != expect || end > file_len {
            return Err(DecodeError::Layout);
        }
        if end > usize::MAX as u64 {
            return Err(DecodeError::Layout);
        }
        checksums[i].copy_from_slice(&head[HEADER + i * 32 + 16..HEADER + i * 32 + 32]);
        *slot = (offset as usize, end as usize);
        expect = end;
    }
    if expect != file_len {
        return Err(DecodeError::Layout);
    }
    Ok((sections, checksums))
}

/// Checks the lengths of the sections with fixed rows against the counts.
fn check_lengths(counts: &Counts, len: impl Fn(Section) -> u64) -> Result<(), DecodeError> {
    for (section, row) in [
        (Section::Roots, PAIR_ROW),
        (Section::Links, PAIR_ROW),
        (Section::Specials, PAIR_ROW),
        (Section::WorkTrees, WORK_TREE_ROW),
        (Section::Revealed, 4),
    ] {
        if !len(section).is_multiple_of(row as u64) {
            return Err(DecodeError::Corrupt(section.label()));
        }
    }
    let bits = u64::from(counts.dirs).div_ceil(8);
    if len(Section::Traversed) != bits && len(Section::Traversed) != bits * 2 {
        return Err(DecodeError::Corrupt("traversed"));
    }
    if len(Section::States) != u64::from(counts.inodes).div_ceil(4) {
        return Err(DecodeError::Corrupt("states"));
    }
    if (len(Section::NameHeap) == 0) != (counts.names == 0) {
        return Err(DecodeError::Corrupt("name heap"));
    }

    if len(Section::DocRefs) != u64::from(counts.docs) * 4 || len(Section::Policy) != 16 {
        return Err(DecodeError::Corrupt("writer sections"));
    }
    Ok(())
}

/// Places each column after the columns before it in its section, and
/// checks that a column section holds exactly its columns. Returns the
/// columns and where the docs section's hashes start.
fn place_columns(
    head: &[u8],
    counts: &Counts,
    len: impl Fn(Section) -> u64,
) -> Result<([Placed; COLUMNS.len()], usize), DecodeError> {
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
        let count = counts.rows(column.rows());
        columns[i] = Placed {
            desc,
            start: used[section as usize] as usize,
        };
        used[section as usize] =
            used[section as usize].saturating_add(desc.len(column.coding(), count));
    }
    // The docs section's hashes follow its id column.
    let hashes = used[Section::Docs as usize];
    used[Section::Docs as usize] = hashes.saturating_add(u64::from(counts.docs) * HASH_ROW as u64);
    for column in COLUMNS {
        let section = column.section();
        if used[section as usize] != len(section) {
            return Err(DecodeError::Corrupt(section.label()));
        }
    }
    Ok((columns, hashes as usize))
}
