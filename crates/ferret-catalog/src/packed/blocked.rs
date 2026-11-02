//! [`Blocked`] columns: a frame per block of [`BLOCK_ROWS`] rows, each
//! block packed at its own width, with the table that locates them; and the
//! writer and sizer that build one.

use super::*;

/// Rows per block of a [`Blocked`] column. A power of two and a multiple of
/// 64, so a validation run ([`Packed::decode`]'s) never spans two blocks and
/// every full block ends on a byte. Measured on the synthetic 10M catalog's
/// columns, from 64 to 4096: 128 is within 2% of the best size for name
/// offsets, sizes and times, and 256 or more for nlink, whose blocks are
/// nearly all width 0; 128 was the smallest total over those columns.
pub(crate) const BLOCK_ROWS: usize = 128;

/// Bytes per block in a blocked column's table: the block's base, then its
/// values' byte offset shifted left 8 with its width in the low byte.
pub(crate) const BLOCK_ENTRY: u64 = 16;

/// Set beside the width in a nullable blocked column's table entry when the
/// block holds a none: the width's all-ones value is then none, and a block
/// of nothing else is width 0. A block with no none keeps its plain width.
const NULLS: u64 = 0x80;

/// The blocks a blocked column of `count` rows has.
pub(crate) fn blocks(count: u32) -> u64 {
    u64::from(count).div_ceil(BLOCK_ROWS as u64)
}

/// A blocked column's exact length in bytes: its block table, `values` bytes
/// of packed blocks, and the padding. Saturates rather than overflow, since
/// `values` comes from the file: no section is that long.
pub(crate) fn blocked_len(count: u32, values: u64) -> u64 {
    (blocks(count) * BLOCK_ENTRY)
        .saturating_add(values)
        .saturating_add(PAD)
}

/// The bytes of block `k`'s values for its `rows` rows at `width` bits.
fn block_bytes(rows: usize, width: u32) -> u64 {
    (rows as u64 * u64::from(width)).div_ceil(8)
}

/// A column in blocks of [`BLOCK_ROWS`] rows, each with its own frame of
/// reference: value `i` is its block's base plus a packed value at the
/// block's own width. A column whose neighbouring rows are close (offsets,
/// which only grow; nlink, which is nearly always 1) is narrower per block
/// than over the whole catalog. A read is two table loads more than
/// [`Packed`]'s, and O(1).
///
/// A nullable blocked column frames each block by its real values alone,
/// and marks a block that holds a none with [`NULLS`].
///
/// ```text
/// table    per block: base u64, (offset << 8 | nulls | width) u64
///                                                          (BLOCK_ENTRY)
/// values   each block's values, packed LSB first from a byte boundary
/// padding  PAD bytes
/// ```
#[derive(Clone, Copy)]
pub(crate) struct Blocked<'a> {
    table: &'a [u8],
    values: &'a [u8],
    count: usize,
}

impl<'a> Blocked<'a> {
    /// `bytes` must hold at least the table of a `count`-row column; the
    /// decoder checked its exact length, and [`Blocked::check`] every entry,
    /// before any read.
    pub(crate) fn new(bytes: &'a [u8], count: u32) -> Self {
        let (table, values) = bytes.split_at((blocks(count) * BLOCK_ENTRY) as usize);
        Self {
            table,
            values,
            count: count as usize,
        }
    }

    /// Block `k`'s base, values offset and width.
    pub(super) fn entry(&self, k: usize) -> (u64, usize, u32) {
        let (base, packed) = self.words(k);
        (base, (packed >> 8) as usize, (packed & 0x7F) as u32)
    }

    /// Block `k`'s two table words.
    fn words(&self, k: usize) -> (u64, u64) {
        let at = k * BLOCK_ENTRY as usize;
        let word = |at: usize| {
            let mut b = [0; 8];
            b.copy_from_slice(&self.table[at..at + 8]);
            u64::from_le_bytes(b)
        };
        (word(at), word(at + 8))
    }

    /// Whether every entry is one a read can trust: each offset exactly
    /// where the blocks before it end, the last block ending at `values`
    /// bytes (the length the descriptor gave the values), and the widest
    /// block exactly `max_width`, which is at most [`MAX_WIDTH`]: so every
    /// width is too, and the descriptor is exact. [`NULLS`] is set only in
    /// a `nullable` column.
    pub(crate) fn check(&self, max_width: u32, values: u64, nullable: bool) -> bool {
        let (mut end, mut widest) = (0, 0);
        for k in 0..self.table.len() / BLOCK_ENTRY as usize {
            let (_, offset, width) = self.entry(k);
            if offset as u64 != end || (!nullable && self.words(k).1 & NULLS != 0) {
                return false;
            }
            end += block_bytes(BLOCK_ROWS.min(self.count - k * BLOCK_ROWS), width);
            widest = widest.max(width);
        }
        end == values && widest == max_width
    }

    /// Value `i`. Panics past the column's end, as indexing a slice does.
    pub(crate) fn get(&self, i: usize) -> u64 {
        let (base, offset, width) = self.entry(i / BLOCK_ROWS);
        base.wrapping_add(Packed::new(&self.values[offset..], width).get(i % BLOCK_ROWS))
    }

    /// Value `i` of a nullable column, or `None` where it holds none.
    pub(crate) fn nullable(&self, i: usize) -> Option<u64> {
        let (base, packed) = self.words(i / BLOCK_ROWS);
        let (offset, width) = ((packed >> 8) as usize, (packed & 0x7F) as u32);
        let raw = Packed::new(&self.values[offset..], width).get(i % BLOCK_ROWS);
        (packed & NULLS == 0 || raw != mask(width)).then(|| base.wrapping_add(raw))
    }

    /// Every value of a nullable column in order, `None` where it holds none:
    /// a pass over the column that decodes a block at a time, where
    /// [`Blocked::nullable`] re-derives each row's block and position.
    pub(crate) fn nullable_values(&self) -> impl Iterator<Item = Option<u64>> + '_ {
        (0..self.count.div_ceil(BLOCK_ROWS)).flat_map(move |k| {
            let (base, packed) = self.words(k);
            let (offset, width) = ((packed >> 8) as usize, (packed & 0x7F) as u32);
            let rows = BLOCK_ROWS.min(self.count - k * BLOCK_ROWS);
            let mut raw = [0; BLOCK_ROWS];
            Packed::new(&self.values[offset..], width).decode(0, &mut raw[..rows]);
            let none = (packed & NULLS != 0).then(|| mask(width));
            raw.into_iter()
                .take(rows)
                .map(move |raw| (none != Some(raw)).then(|| base.wrapping_add(raw)))
        })
    }

    /// Values `first..first + out.len()` into `out`, a run of a pass over the
    /// column; `first` is a multiple of 8. See [`Packed::decode`].
    pub(crate) fn decode(&self, first: usize, out: &mut [u64]) {
        let mut done = 0;
        while done < out.len() {
            let row = first + done;
            let (base, offset, width) = self.entry(row / BLOCK_ROWS);
            let n = (BLOCK_ROWS - row % BLOCK_ROWS).min(out.len() - done);
            let run = &mut out[done..done + n];
            Packed::new(&self.values[offset..], width).decode(row % BLOCK_ROWS, run);
            for value in run {
                *value = base.wrapping_add(*value);
            }
            done += n;
        }
    }
}

/// Finds a blocked column's values length and widest block from its values
/// in order, holding one block's range.
#[derive(Default)]
pub(crate) struct BlockSizer {
    range: Option<(u64, u64)>,
    nulls: bool,
    rows: usize,
    values: u64,
    width: u32,
}

impl BlockSizer {
    pub(crate) fn push(&mut self, value: u64) {
        let (min, max) = self.range.unwrap_or((value, value));
        self.range = Some((min.min(value), max.max(value)));
        self.next();
    }

    /// A nullable column's none.
    pub(crate) fn push_null(&mut self) {
        self.nulls = true;
        self.next();
    }

    fn next(&mut self) {
        self.rows += 1;
        if self.rows == BLOCK_ROWS {
            self.close();
        }
    }

    fn close(&mut self) {
        if let Some((min, max)) = self.range.take() {
            let w = block_width(min, max, self.nulls);
            self.values += block_bytes(self.rows, w);
            self.width = self.width.max(w);
        }
        (self.rows, self.nulls) = (0, false);
    }

    /// The bytes of packed values, and the widest block.
    pub(crate) fn finish(mut self) -> (u64, u32) {
        self.close();
        (self.values, self.width)
    }
}

/// A block's width for real values from `min` to `max`: one more value's
/// worth when the block holds a none, whose all-ones must be past them all.
/// A nullable column's values span less than `u64::MAX` (the builder's are
/// `u32`s).
fn block_width(min: u64, max: u64, nulls: bool) -> u32 {
    width(max - min + u64::from(nulls))
}

/// Streams a blocked column: holds one block of values, then writes its
/// table entry to one output and its packed values to another, so neither
/// the table nor the values are ever held whole.
pub(crate) struct BlockedWriter {
    block: [u64; BLOCK_ROWS],
    /// Bit `i` set: row `i` of the block is none.
    nulls: u128,
    rows: usize,
    offset: u64,
}

impl BlockedWriter {
    pub(crate) fn new() -> Self {
        Self {
            block: [0; BLOCK_ROWS],
            nulls: 0,
            rows: 0,
            offset: 0,
        }
    }

    pub(crate) fn push(
        &mut self,
        table: &mut impl Write,
        values: &mut impl Write,
        value: u64,
    ) -> io::Result<()> {
        self.block[self.rows] = value;
        self.next(table, values)
    }

    /// A nullable column's none.
    pub(crate) fn push_null(
        &mut self,
        table: &mut impl Write,
        values: &mut impl Write,
    ) -> io::Result<()> {
        self.nulls |= 1 << self.rows;
        self.next(table, values)
    }

    fn next(&mut self, table: &mut impl Write, values: &mut impl Write) -> io::Result<()> {
        self.rows += 1;
        if self.rows == BLOCK_ROWS {
            self.close(table, values)?;
        }
        Ok(())
    }

    fn close(&mut self, table: &mut impl Write, values: &mut impl Write) -> io::Result<()> {
        if self.rows == 0 {
            return Ok(());
        }
        let nulls = self.nulls;
        let is_null = |i: usize| nulls >> i & 1 == 1;
        let block = &self.block[..self.rows];
        let real = || (0..block.len()).filter(|&i| !is_null(i)).map(|i| block[i]);
        // A block of nothing but nones has base 0 and width 0.
        let (min, w) = match (real().min(), real().max()) {
            (Some(min), Some(max)) => (min, block_width(min, max, nulls != 0)),
            _ => (0, 0),
        };
        let flag = if nulls == 0 { 0 } else { NULLS };
        table.write_all(&min.to_le_bytes())?;
        table.write_all(&(self.offset << 8 | flag | u64::from(w)).to_le_bytes())?;
        let mut writer = Writer::new(w);
        for (i, &value) in block.iter().enumerate() {
            let raw = if is_null(i) { mask(w) } else { value - min };
            writer.push(values, raw)?;
        }
        writer.flush(values)?;
        self.offset += block_bytes(self.rows, w);
        (self.rows, self.nulls) = (0, 0);
        Ok(())
    }

    /// Writes the last partial block and the padding.
    pub(crate) fn finish(
        mut self,
        table: &mut impl Write,
        values: &mut impl Write,
    ) -> io::Result<()> {
        self.close(table, values)?;
        values.write_all(&[0; PAD as usize])
    }
}
