//! The resident block index: one sampled place per [`SAMPLE`] blocks,
//! parsed and checked at open; the places between samples decode on demand.

use super::*;

/// One block's place, decoded from the resident index bytes.
///
/// Offsets stay `u64`/`usize`: the format caps only the fingerprint section
/// at `u32` (`Writer::check_record_length`); blocks, postings, the index and
/// term lengths have no narrower bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Place {
    /// Where its index entry starts in `Segment::index_bytes`: the vbyte
    /// length of its first term, then the term.
    pub(super) at: usize,
    /// Byte offset in the blocks section.
    pub(super) start: u64,
    /// Offset in the postings section of its first list.
    pub(super) postings: u64,
}

/// One index entry's first term at `*at`, advancing past it.
pub(super) fn index_term<'a>(bytes: &'a [u8], at: &mut usize) -> Option<&'a [u8]> {
    let len = usize::try_from(take(bytes, at)?).ok()?;
    let term = bytes.get(*at..at.checked_add(len)?)?;
    *at += len;
    Some(term)
}

/// The place of the block after `place`'s; `None` when the entries do not
/// decode.
pub(super) fn next_place(bytes: &[u8], place: Place) -> Option<Place> {
    let mut at = place.at;
    index_term(bytes, &mut at)?;
    take(bytes, &mut at)?;
    take(bytes, &mut at)?;
    let next = at;
    index_term(bytes, &mut at)?;
    let (step, postings_step) = (take(bytes, &mut at)?, take(bytes, &mut at)?);
    Some(Place {
        at: next,
        start: place.start.checked_add(step)?,
        postings: place.postings.checked_add(postings_step)?,
    })
}

/// Whether `a` sorts strictly before `b`. Inline rather than a `memcmp`
/// call: first terms are short, and `parse_index` compares one per block.
fn before(a: &[u8], b: &[u8]) -> bool {
    for (x, y) in a.iter().zip(b) {
        if x != y {
            return x < y;
        }
    }
    a.len() < b.len()
}

/// Parses the block index; `None` when it is inconsistent with the head or
/// the section lengths. Returns every [`SAMPLE`]th block's place; the caller
/// keeps `bytes`, from which the rest are decoded.
pub(super) fn parse_index(
    bytes: &[u8],
    terms: u64,
    blocks_len: u64,
    postings_len: u64,
) -> Option<Vec<Place>> {
    let count = terms.div_ceil(BLOCK as u64);
    // Each block's index entry is at least three bytes.
    if count > bytes.len() as u64 {
        return None;
    }
    let mut samples = Vec::with_capacity(count.div_ceil(SAMPLE as u64) as usize);
    let (mut at, mut start, mut postings) = (0, 0u64, 0u64);
    let mut previous: &[u8] = &[];
    for b in 0..count as usize {
        let entry = at;
        let term = index_term(bytes, &mut at)?;
        let (step, postings_step) = (take(bytes, &mut at)?, take(bytes, &mut at)?);
        if b == 0 && (step, postings_step) != (0, 0)
            || b > 0 && (step == 0 || !before(previous, term))
        {
            return None;
        }
        start = start.checked_add(step)?;
        postings = postings.checked_add(postings_step)?;
        if start >= blocks_len || postings > postings_len {
            return None;
        }
        previous = term;
        if b % SAMPLE == 0 {
            samples.push(Place {
                at: entry,
                start,
                postings,
            });
        }
    }
    (at == bytes.len() && (count > 0 || blocks_len == 0 && postings_len == 0)).then_some(samples)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::put;

    /// An Index section: per block its first term, byte step and postings
    /// step, as the writer lays them out.
    fn index(entries: &[(&[u8], u64, u64)]) -> Vec<u8> {
        let mut out = Vec::new();
        for &(term, step, postings_step) in entries {
            put(term.len() as u64, &mut out);
            out.extend_from_slice(term);
            put(step, &mut out);
            put(postings_step, &mut out);
        }
        out
    }

    /// Each block's first term, byte offset and postings base, decoded the
    /// way lookups do: from the samples `parse_index` keeps, stepping with
    /// `next_place`. Fails the test if a sample disagrees with the walk.
    fn parsed(
        bytes: &[u8],
        blocks: u64,
        blocks_len: u64,
        postings_len: u64,
    ) -> Option<Vec<(Vec<u8>, u64, u64)>> {
        let samples = parse_index(bytes, blocks * BLOCK as u64, blocks_len, postings_len)?;
        let mut places = Vec::new();
        let mut place = samples.first().copied();
        while let Some(here) = place {
            places.push(here);
            place = (places.len() < blocks as usize)
                .then(|| next_place(bytes, here).expect("parsed entries decode"));
        }
        let every: Vec<_> = places.iter().step_by(SAMPLE).copied().collect();
        assert_eq!(samples, every, "samples are every {SAMPLE}th place");
        Some(
            places
                .iter()
                .map(|p| {
                    let term = index_term(bytes, &mut { p.at }).unwrap_or_default();
                    (term.to_vec(), p.start, p.postings)
                })
                .collect(),
        )
    }

    #[test]
    fn index_decodes_places_across_samples() {
        // Steps alternate one and two bytes wide, and the postings base
        // passes u32 inside the second sample run.
        let past_u32 = u64::from(u32::MAX) + 1;
        let names: Vec<_> = (0..2 * SAMPLE + 3)
            .map(|b| format!("t{b:03}").into_bytes())
            .collect();
        let entries: Vec<(&[u8], u64, u64)> = names
            .iter()
            .enumerate()
            .map(|(b, name)| match b {
                0 => (name.as_slice(), 0, 0),
                b if b == SAMPLE + 3 => (name.as_slice(), 300, past_u32),
                b => (name.as_slice(), 1 + 200 * (b as u64 % 2), b as u64),
            })
            .collect();
        let (mut start, mut postings) = (0, 0);
        let want: Vec<_> = entries
            .iter()
            .map(|&(term, step, postings_step)| {
                start += step;
                postings += postings_step;
                (term.to_vec(), start, postings)
            })
            .collect();
        assert_eq!(
            parsed(&index(&entries), entries.len() as u64, start + 1, postings),
            Some(want)
        );
    }

    #[test]
    fn index_accepts_its_section_boundaries_and_wide_offsets() {
        let past_u32 = u64::from(u32::MAX) + 1;
        // The last block starts on the blocks section's last byte, and the
        // postings base reaches the section's end: both are the widest the
        // checks allow. Offsets past u32 must not wrap.
        let bytes = index(&[(b"ab", 0, 0), (b"abc", past_u32, past_u32), (b"b", 7, 1)]);
        let blocks_len = past_u32 + 8;
        let postings_len = past_u32 + 1;
        assert_eq!(
            parsed(&bytes, 3, blocks_len, postings_len),
            Some(vec![
                (b"ab".to_vec(), 0, 0),
                (b"abc".to_vec(), past_u32, past_u32),
                (b"b".to_vec(), past_u32 + 7, postings_len),
            ])
        );
        // The same at the top of u64.
        let top = index(&[(b"a", 0, 0), (b"b", u64::MAX - 1, u64::MAX)]);
        assert_eq!(
            parsed(&top, 2, u64::MAX, u64::MAX),
            Some(vec![
                (b"a".to_vec(), 0, 0),
                (b"b".to_vec(), u64::MAX - 1, u64::MAX)
            ])
        );
    }

    #[test]
    fn index_rejects_entries_outside_their_bounds() {
        let ok =
            |entries: &[(&[u8], u64, u64)]| parsed(&index(entries), entries.len() as u64, 100, 50);
        assert!(
            ok(&[(b"ab", 0, 0), (b"abc", 99, 50)]).is_some(),
            "the control"
        );
        assert_eq!(
            ok(&[(b"ab", 0, 0), (b"abc", 100, 0)]),
            None,
            "start at the section end"
        );
        assert_eq!(
            ok(&[(b"ab", 0, 0), (b"abc", 1, 51)]),
            None,
            "postings past the end"
        );
        assert_eq!(ok(&[(b"ab", 0, 0), (b"ab", 1, 0)]), None, "a repeated term");
        assert_eq!(
            ok(&[(b"abc", 0, 0), (b"ab", 1, 0)]),
            None,
            "a prefix after its extension"
        );
        assert_eq!(ok(&[(b"ab", 0, 0), (b"aa", 1, 0)]), None, "a smaller term");
        assert_eq!(ok(&[(b"ab", 0, 0), (b"abc", 0, 0)]), None, "an empty block");
        assert_eq!(ok(&[(b"ab", 1, 0)]), None, "block 0 with a step");
        // Each second entry is in bounds, so only a checked sum rejects the
        // third: a wrapping one lands back inside the section.
        let overflow = index(&[(b"a", 0, 0), (b"b", u64::MAX - 1, 0), (b"c", 2, 0)]);
        assert_eq!(parsed(&overflow, 3, u64::MAX, 0), None, "a wrapping start");
        let overflow = index(&[(b"a", 0, 0), (b"b", 1, u64::MAX), (b"c", 1, 1)]);
        assert_eq!(
            parsed(&overflow, 3, 3, u64::MAX),
            None,
            "a wrapping postings base"
        );
        let mut trailing = index(&[(b"ab", 0, 0)]);
        trailing.push(0);
        assert_eq!(parsed(&trailing, 1, 100, 50), None, "a trailing byte");
        assert_eq!(
            parsed(&index(&[(b"ab", 0, 0)]), 2, 100, 50),
            None,
            "a missing block"
        );
    }
}
