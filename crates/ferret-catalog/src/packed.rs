//! Bit-packed columns: `count` values of `width` bits each (0 to 64), least
//! significant bit first, value `i` starting at bit `i * width` of a
//! little-endian byte string.
//!
//! A read is one unaligned 8-byte load, a shift and a mask, plus one more
//! byte when the value straddles the 8-byte window (`shift + width > 64`,
//! which only widths above 57 can). Every column ends in [`PAD`] bytes of
//! padding, so the load never runs off the end: the writer writes zeros, the
//! decoder does not check them (reads never depend on them), and [`len`] is
//! the exact length, which the decoder does hold each column to. Width 0 is
//! legal: every value is 0, and the column is only its padding.
//!
//! The values are raw: frame of reference, dictionaries and the none
//! sentinel are the format's business (see `format`).

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
}

/// Packs values as they are pushed, so a column streams to the output
/// without being held.
pub(crate) struct Writer {
    width: u32,
    /// Bits not yet written, low first; fewer than 64 between pushes.
    pending: u128,
    filled: u32,
}

impl Writer {
    pub(crate) fn new(width: u32) -> Self {
        debug_assert!(width <= MAX_WIDTH);
        Self {
            width,
            pending: 0,
            filled: 0,
        }
    }

    /// Appends `value`, which must fit the width.
    pub(crate) fn push(&mut self, out: &mut impl Write, value: u64) -> io::Result<()> {
        debug_assert!(
            value <= mask(self.width),
            "{value} does not fit {}",
            self.width
        );
        self.pending |= u128::from(value & mask(self.width)) << self.filled;
        self.filled += self.width;
        if self.filled >= u64::BITS {
            out.write_all(&(self.pending as u64).to_le_bytes())?;
            self.pending >>= u64::BITS;
            self.filled -= u64::BITS;
        }
        Ok(())
    }

    /// Writes the last partial word and the padding.
    pub(crate) fn finish(self, out: &mut impl Write) -> io::Result<()> {
        let tail = self.filled.div_ceil(8) as usize;
        out.write_all(&(self.pending as u64).to_le_bytes()[..tail])?;
        out.write_all(&[0; PAD as usize])
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

    #[test]
    fn a_read_past_the_padded_end_panics_rather_than_reading_garbage() {
        let bytes = pack(64, &[u64::MAX]);
        let column = Packed::new(&bytes, 64);
        assert_eq!(column.get(0), u64::MAX);
        assert!(std::panic::catch_unwind(|| column.get(2)).is_err());
    }
}
