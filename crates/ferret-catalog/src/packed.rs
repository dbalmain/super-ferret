//! Bit-packed columns: `count` values of `width` bits each (0 to 64), least
//! significant bit first, value `i` starting at bit `i * width` of a
//! little-endian byte string.
//!
//! A read is one unaligned 8-byte load, a shift and a mask, plus one more
//! byte when the value straddles the 8-byte window (`shift + width > 64`,
//! which only widths 58 to 63 can: width 64 always has shift 0). Every column
//! ends in [`PAD`] bytes of padding, so the load never runs off the end: the
//! writer writes zeros, the decoder does not check them (reads never depend on
//! them), and [`len`] is the exact length, which the decoder does hold each
//! column to. Width 0 is legal: every value is 0, and the column is only its
//! padding.
//!
//! The values are raw: frame of reference, dictionaries and the none
//! sentinel are the format's business (see `format`), except in a
//! [`Blocked`] column, which carries a frame per block of [`BLOCK_ROWS`] rows.

use std::io::{self, Write};

/// Bytes of padding after every column's last value, written as zeros.
pub(crate) const PAD: u64 = 8;

/// The widest a column can be.
pub(crate) const MAX_WIDTH: u32 = 64;

/// The bits needed to hold every value up to `max`: 0 for 0.
pub(crate) fn width(max: u64) -> u32 {
    u64::BITS - max.leading_zeros()
}

/// The largest value `width` bits hold: all ones. `width` is at most 64.
pub(crate) fn mask(width: u32) -> u64 {
    u64::MAX.checked_shr(u64::BITS - width).unwrap_or(0)
}

/// A column's exact length in bytes, padding included. Cannot overflow for
/// any `u32` count and width up to [`MAX_WIDTH`].
pub(crate) fn len(count: u32, width: u32) -> u64 {
    (u64::from(count) * u64::from(width)).div_ceil(8) + PAD
}

/// A column's bytes, read in place.
#[derive(Clone, Copy)]
pub(crate) struct Packed<'a> {
    bytes: &'a [u8],
    width: u32,
    mask: u64,
}

impl<'a> Packed<'a> {
    /// `bytes` must be at least [`len`] of the column's count and `width`,
    /// and `width` at most [`MAX_WIDTH`]; the decoder checked both.
    pub(crate) fn new(bytes: &'a [u8], width: u32) -> Self {
        Self {
            bytes,
            width,
            mask: mask(width),
        }
    }

    pub(crate) fn width(&self) -> u32 {
        self.width
    }

    /// Value `i`. Panics past the column's end, as indexing a slice does.
    pub(crate) fn get(&self, i: usize) -> u64 {
        let bit = i * self.width as usize;
        let (at, shift) = (bit / 8, (bit % 8) as u32);
        let mut word = [0; 8];
        word.copy_from_slice(&self.bytes[at..at + 8]);
        let mut value = u64::from_le_bytes(word) >> shift;
        if shift + self.width > u64::BITS {
            value |= u64::from(self.bytes[at + 8]) << (u64::BITS - shift);
        }
        value & self.mask
    }

    /// Values `first..first + out.len()` into `out`, for a pass over the
    /// whole column; `first` is a multiple of 8, and the values are inside
    /// the column. Through [`Packed::get`] each value re-derives its byte and
    /// shift and handles the straddling byte; here the run starts on a
    /// byte, and only widths above 57 take a second loop. A name query's user
    /// time at 10M names was 0.18 s validating through `get`, 0.14 s
    /// through this (v1's aligned `u32`s: 0.09 s); a bit-buffer iterator
    /// was no faster than `get`.
    pub(crate) fn decode(&self, first: usize, out: &mut [u64]) {
        debug_assert!(first.is_multiple_of(8));
        let width = self.width as usize;
        let start = first / 8 * width;
        let end = start + (out.len() * width).div_ceil(8) + PAD as usize;
        let bytes = &self.bytes[start..end];
        for (j, value) in out.iter_mut().enumerate() {
            let bit = j * width;
            let mut word = [0; 8];
            word.copy_from_slice(&bytes[bit / 8..bit / 8 + 8]);
            *value = u64::from_le_bytes(word) >> (bit % 8) & self.mask;
        }
        if width > 57 {
            // Only widths 58 to 63 can straddle the 8-byte window (64 has shift
            // 0).
            for (j, value) in out.iter_mut().enumerate() {
                let (bit, shift) = (j * width, (j * width % 8) as u32);
                if shift as usize + width > 64 {
                    *value |= u64::from(bytes[bit / 8 + 8]) << (u64::BITS - shift) & self.mask;
                }
            }
        }
    }
}

/// Packs values as they are pushed, so a column streams to the output
/// without being held.
pub(crate) struct Writer {
    width: u32,
    mask: u64,
    /// Bits not yet written, low first; fewer than 64 between pushes.
    pending: u64,
    filled: u32,
}

impl Writer {
    pub(crate) fn new(width: u32) -> Self {
        debug_assert!(width <= MAX_WIDTH);
        Self {
            width,
            mask: mask(width),
            pending: 0,
            filled: 0,
        }
    }

    /// Appends `value`, which must fit the width.
    pub(crate) fn push(&mut self, out: &mut impl Write, value: u64) -> io::Result<()> {
        debug_assert!(value <= self.mask, "{value} does not fit {}", self.width);
        let value = value & self.mask;
        self.pending |= value << self.filled;
        self.filled += self.width;
        if self.filled >= u64::BITS {
            out.write_all(&self.pending.to_le_bytes())?;
            self.filled -= u64::BITS;
            // The bits that did not fit: none when the word was empty.
            self.pending = value.checked_shr(self.width - self.filled).unwrap_or(0);
        }
        Ok(())
    }

    /// Writes the last partial word and the padding.
    pub(crate) fn finish(self, out: &mut impl Write) -> io::Result<()> {
        self.flush(out)?;
        out.write_all(&[0; PAD as usize])
    }

    /// Writes the last partial word, without padding: the end of one block.
    fn flush(self, out: &mut impl Write) -> io::Result<()> {
        let tail = self.filled.div_ceil(8) as usize;
        out.write_all(&self.pending.to_le_bytes()[..tail])
    }
}

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
/// ```text
/// table    per block: base u64, (offset << 8 | width) u64    (BLOCK_ENTRY)
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
    fn entry(&self, k: usize) -> (u64, usize, u32) {
        let at = k * BLOCK_ENTRY as usize;
        let word = |at: usize| {
            let mut b = [0; 8];
            b.copy_from_slice(&self.table[at..at + 8]);
            u64::from_le_bytes(b)
        };
        let (base, packed) = (word(at), word(at + 8));
        (base, (packed >> 8) as usize, (packed & 0xFF) as u32)
    }

    /// Whether every entry is one a read can trust: each offset exactly
    /// where the blocks before it end, the last block ending at `values`
    /// bytes (the length the descriptor gave the values), and the widest
    /// block exactly `max_width`, which is at most [`MAX_WIDTH`]: so every
    /// width is too, and the descriptor is exact.
    pub(crate) fn check(&self, max_width: u32, values: u64) -> bool {
        let (mut end, mut widest) = (0, 0);
        for k in 0..self.table.len() / BLOCK_ENTRY as usize {
            let (_, offset, width) = self.entry(k);
            if offset as u64 != end {
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
    rows: usize,
    values: u64,
    width: u32,
}

impl BlockSizer {
    pub(crate) fn push(&mut self, value: u64) {
        let (min, max) = self.range.unwrap_or((value, value));
        self.range = Some((min.min(value), max.max(value)));
        self.rows += 1;
        if self.rows == BLOCK_ROWS {
            self.close();
        }
    }

    fn close(&mut self) {
        if let Some((min, max)) = self.range.take() {
            let w = width(max - min);
            self.values += block_bytes(self.rows, w);
            self.width = self.width.max(w);
        }
        self.rows = 0;
    }

    /// The bytes of packed values, and the widest block.
    pub(crate) fn finish(mut self) -> (u64, u32) {
        self.close();
        (self.values, self.width)
    }
}

/// Streams a blocked column: holds one block of values, then writes its
/// table entry to one output and its packed values to another, so neither
/// the table nor the values are ever held whole.
pub(crate) struct BlockedWriter {
    block: [u64; BLOCK_ROWS],
    rows: usize,
    offset: u64,
}

impl BlockedWriter {
    pub(crate) fn new() -> Self {
        Self {
            block: [0; BLOCK_ROWS],
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
        self.rows += 1;
        if self.rows == BLOCK_ROWS {
            self.close(table, values)?;
        }
        Ok(())
    }

    fn close(&mut self, table: &mut impl Write, values: &mut impl Write) -> io::Result<()> {
        let block = &self.block[..self.rows];
        let (Some(&min), Some(&max)) = (block.iter().min(), block.iter().max()) else {
            return Ok(());
        };
        let w = width(max - min);
        table.write_all(&min.to_le_bytes())?;
        table.write_all(&(self.offset << 8 | u64::from(w)).to_le_bytes())?;
        let mut writer = Writer::new(w);
        for &value in block {
            writer.push(values, value - min)?;
        }
        writer.flush(values)?;
        self.offset += block_bytes(self.rows, w);
        self.rows = 0;
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

#[cfg(test)]
mod tests {
    use super::*;

    const WIDTHS: [u32; 10] = [0, 1, 2, 7, 8, 31, 57, 58, 63, 64];

    fn pack(width: u32, values: &[u64]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut writer = Writer::new(width);
        for &v in values {
            writer.push(&mut out, v).unwrap();
        }
        writer.finish(&mut out).unwrap();
        out
    }

    /// Values that exercise every bit of a `width`-bit field: 0, all ones,
    /// alternating patterns, and a pseudo-random spread.
    fn values(width: u32, count: usize) -> Vec<u64> {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        (0..count)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let v = match i % 5 {
                    0 => 0,
                    1 => u64::MAX,
                    2 => 0x5555_5555_5555_5555,
                    3 => 0xAAAA_AAAA_AAAA_AAAA,
                    _ => x,
                };
                v & mask(width)
            })
            .collect()
    }

    #[test]
    fn widths_and_masks_at_the_edges() {
        assert_eq!(
            [0, 1, 2, 3, 255, 256, u64::MAX].map(width),
            [0, 1, 2, 2, 8, 9, 64]
        );
        assert_eq!(mask(0), 0);
        assert_eq!(mask(1), 1);
        assert_eq!(mask(63), u64::MAX >> 1);
        assert_eq!(mask(64), u64::MAX);
        for w in 0..=MAX_WIDTH {
            assert_eq!(width(mask(w)), w);
        }
    }

    #[test]
    fn every_width_round_trips_at_every_count_up_to_130() {
        // 130 values cross a 64-bit word boundary at every width, and every
        // count checks the last value against the padding.
        for w in 0..=MAX_WIDTH {
            for count in 0..130 {
                let values = values(w, count);
                let bytes = pack(w, &values);
                assert_eq!(bytes.len() as u64, len(count as u32, w), "w {w} n {count}");
                assert!(bytes.ends_with(&[0; PAD as usize]), "w {w} n {count}");
                let column = Packed::new(&bytes, w);
                for (i, &v) in values.iter().enumerate() {
                    assert_eq!(column.get(i), v, "w {w} n {count} i {i}");
                }
                // Runs from each multiple of 8: short, a block, and to the
                // column's end, whose loads reach into the padding.
                for first in (0..count).step_by(8) {
                    for len in [0, 1, 7, 8, 9, 64, count - first] {
                        let end = (first + len).min(count);
                        let mut out = vec![0; end - first];
                        column.decode(first, &mut out);
                        assert_eq!(out, values[first..end], "w {w} n {count} {first}..{end}");
                    }
                }
            }
        }
    }

    #[test]
    fn all_ones_and_zero_at_the_named_edges() {
        for w in WIDTHS {
            for count in [1, 2, 7, 8, 9, 64, 65] {
                for fill in [0, mask(w)] {
                    let bytes = pack(w, &vec![fill; count]);
                    let column = Packed::new(&bytes, w);
                    assert!((0..count).all(|i| column.get(i) == fill), "w {w} n {count}");
                }
            }
        }
    }

    #[test]
    fn a_value_does_not_leak_into_its_neighbours() {
        // One all-ones value among zeros reads back alone at every position:
        // the straddling byte for widths above 57 is masked, not or-ed in
        // wholesale.
        for w in WIDTHS {
            for hot in 0..10 {
                let values: Vec<u64> = (0..10)
                    .map(|i| if i == hot { mask(w) } else { 0 })
                    .collect();
                let bytes = pack(w, &values);
                let column = Packed::new(&bytes, w);
                for (i, &v) in values.iter().enumerate() {
                    assert_eq!(column.get(i), v, "w {w} hot {hot} i {i}");
                }
            }
        }
    }

    #[test]
    fn width_zero_and_count_zero_are_only_padding() {
        assert_eq!(pack(0, &[0; 1000]), [0; PAD as usize]);
        assert_eq!(pack(64, &[]), [0; PAD as usize]);
        assert_eq!(Packed::new(&[0; 8], 0).get(999_999), 0);
    }

    /// A blocked column of `values`: its bytes, values length and widest
    /// block, as a sizing pass and the writer find them.
    fn pack_blocked(values: &[u64]) -> (Vec<u8>, u64, u32) {
        let mut sizer = BlockSizer::default();
        values.iter().for_each(|&v| sizer.push(v));
        let (len, widest) = sizer.finish();
        let (mut table, mut packed) = (Vec::new(), Vec::new());
        let mut writer = BlockedWriter::new();
        for &v in values {
            writer.push(&mut table, &mut packed, v).unwrap();
        }
        writer.finish(&mut table, &mut packed).unwrap();
        table.extend(packed);
        (table, len, widest)
    }

    /// Blocks of every width from 0 to 64, each block's values spread over
    /// its width above a base that differs per block; the last block short.
    fn blocked_values(count: usize) -> Vec<u64> {
        (0..count)
            .map(|i| {
                let (block, row) = (i / BLOCK_ROWS, i % BLOCK_ROWS);
                let w = (block * 7 % 65) as u32;
                let spread = values(w, BLOCK_ROWS)[row];
                // Width 64 has no room for a base; the others sit above one.
                let base = if w == 64 { 0 } else { (block as u64) << 40 };
                base.wrapping_add(spread)
            })
            .collect()
    }

    #[test]
    fn a_blocked_column_round_trips_at_every_block_width() {
        let widths: Vec<u32> = (0..65).map(|b| (b * 7 % 65) as u32).collect();
        assert!(widths.contains(&0) && widths.contains(&64));
        for count in [0, 1, 8, 127, 128, 129, 1000, 65 * BLOCK_ROWS + 3] {
            let values = blocked_values(count);
            let (bytes, len, widest) = pack_blocked(&values);
            assert_eq!(bytes.len() as u64, blocked_len(count as u32, len), "n {count}");
            let column = Blocked::new(&bytes, count as u32);
            assert!(column.check(widest, len), "n {count}");
            for (i, &v) in values.iter().enumerate() {
                assert_eq!(column.get(i), v, "n {count} i {i}");
            }
            // Runs from multiples of 8, some across a block boundary.
            for first in (0..count).step_by(8) {
                for len in [1, 8, 64, 200] {
                    let end = (first + len).min(count);
                    let mut out = vec![0; end - first];
                    column.decode(first, &mut out);
                    assert_eq!(out, values[first..end], "n {count} {first}..{end}");
                }
            }
        }
    }

    #[test]
    fn a_blocked_column_of_equal_values_is_only_its_table_and_padding() {
        let (bytes, len, widest) = pack_blocked(&[5; 300]);
        assert_eq!((len, widest), (0, 0));
        assert_eq!(bytes.len() as u64, 3 * BLOCK_ENTRY + PAD);
        let column = Blocked::new(&bytes, 300);
        assert!((0..300).all(|i| column.get(i) == 5));
    }

    #[test]
    fn a_blocked_table_that_misplaces_or_widens_a_block_is_refused() {
        // Three blocks: widths 3, 9 and 3 (the last short), so the second
        // block starts at 48 bytes and the third at 192.
        let values: Vec<u64> = (0..300)
            .map(|i| (i % 8) as u64 * if i / BLOCK_ROWS == 1 { 64 } else { 1 })
            .collect();
        let (bytes, len, widest) = pack_blocked(&values);
        assert_eq!(widest, 9);
        assert!(Blocked::new(&bytes, 300).check(widest, len));
        let set = |k: usize, offset: u64, width: u64| {
            let mut bad = bytes.clone();
            let at = k * BLOCK_ENTRY as usize + 8;
            bad[at..at + 8].copy_from_slice(&(offset << 8 | width).to_le_bytes());
            bad
        };
        assert!(Blocked::new(&set(1, 48, 9), 300).check(widest, len), "unchanged");
        for (what, bad) in [
            ("wider than the column", set(1, 48, 10)),
            ("wider than 64", set(2, 192, 200)),
            ("an offset past its predecessor's end", set(1, 49, 9)),
            ("an offset before it", set(1, 47, 9)),
            // Narrower keeps the next offset wrong: the third block no
            // longer starts where the second ends.
            ("narrower than written", set(1, 48, 8)),
        ] {
            assert!(!Blocked::new(&bad, 300).check(widest, len), "{what}");
        }
        // The blocks agree with each other but not with the values length,
        // or no block is as wide as the descriptor says.
        let column = Blocked::new(&bytes, 300);
        assert!(!column.check(widest, len + 1) && !column.check(widest, len - 1));
        assert!(!column.check(widest + 1, len));
    }

    #[test]
    fn a_read_past_the_padded_end_panics_rather_than_reading_garbage() {
        let bytes = pack(64, &[u64::MAX]);
        let column = Packed::new(&bytes, 64);
        assert_eq!(column.get(0), u64::MAX);
        assert!(std::panic::catch_unwind(|| column.get(2)).is_err());
    }
}
