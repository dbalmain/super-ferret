//! Codec 2's fingerprint records (S3 D65 H): the 32-bit [`fingerprint`] of
//! a term, the [`Encoder`] that lays sorted `(fingerprint, doc)` records
//! out in restart blocks with their block index, and [`decode_block`],
//! which reads one block back. The run lookup and the record stream that
//! use them are on [`super::Segment`].

use super::{FINGERPRINT_CODEC, put, stores_offsets, take, u32_at};

/// A fingerprint occurrence. The byte offset is a hint, never evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Record {
    pub fingerprint: u32,
    pub doc: u32,
    pub offset: Option<u32>,
}

impl Record {
    pub(crate) fn key(self) -> (u32, u32) {
        (self.fingerprint, self.doc)
    }
}

/// Records per restart block. A run of one fingerprint can start anywhere
/// in a block and span any number of them.
pub const RECORD_BLOCK: usize = 128;

/// 32 bits of BLAKE3 over `term`: the first four bytes, little-endian.
/// Fixed by the segment format: a writer and every later reader must agree.
pub fn fingerprint(term: &[u8]) -> u32 {
    #[cfg(test)]
    if let Some(forced) = FORCED.with(std::cell::Cell::get) {
        return forced(term);
    }
    u32_at(blake3::hash(term).as_bytes(), 0)
}

#[cfg(test)]
type ForcedFingerprint = fn(&[u8]) -> u32;

#[cfg(test)]
thread_local! {
    /// Production fingerprint restart-block reads on this thread.
    pub(crate) static BLOCK_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };

    /// A test's fingerprint function in place of BLAKE3's, on this thread:
    /// it forces collisions through the real writer, merge and lookup.
    pub(crate) static FORCED: std::cell::Cell<Option<ForcedFingerprint>> =
        const { std::cell::Cell::new(None) };
}

/// Lays out records pushed in strictly increasing `(fingerprint, doc)`
/// order. The section's bytes leave through [`Encoder::bytes`] and
/// [`Encoder::drain`]; its block index is built beside them.
#[derive(Debug, Default)]
pub(super) struct Encoder {
    /// The unwritten tail of the section.
    bytes: Vec<u8>,
    /// Bytes drained from the front of `bytes` so far.
    drained: u64,
    /// The block index without its leading record count.
    index: Vec<u8>,
    records: u64,
    in_block: usize,
    last: Option<(u32, u32)>,
    codec: u32,
    block: Vec<Record>,
}

impl Encoder {
    pub(super) fn new(codec: u32) -> Self {
        Self {
            codec,
            ..Self::default()
        }
    }

    /// Flushes the final partial restart block before writing its sections.
    pub(super) fn finish(&mut self) {
        self.flush_block();
    }

    fn flush_block(&mut self) {
        if self.block.is_empty() {
            return;
        }
        self.index
            .extend_from_slice(&self.block[0].fingerprint.to_le_bytes());
        self.index
            .extend_from_slice(&(self.len() as u32).to_le_bytes());
        let mut at = 0;
        let mut previous = 0;
        while at < self.block.len() {
            let first = self.block[at];
            let end = at + self.block[at..].partition_point(|r| r.fingerprint == first.fingerprint);
            if at == 0 {
                self.bytes
                    .extend_from_slice(&first.fingerprint.to_le_bytes());
                self.bytes.extend_from_slice(&first.doc.to_le_bytes());
            } else {
                put(u64::from(first.fingerprint - previous), &mut self.bytes);
                put(u64::from(first.doc), &mut self.bytes);
            }
            put((end - at) as u64, &mut self.bytes);
            let mut doc = first.doc;
            for (i, record) in self.block[at..end].iter().enumerate() {
                if i != 0 {
                    put(u64::from(record.doc - doc), &mut self.bytes);
                }
                if stores_offsets(self.codec) {
                    put(
                        record.offset.map_or(0, |o| u64::from(o) + 1),
                        &mut self.bytes,
                    );
                }
                doc = record.doc;
            }
            previous = first.fingerprint;
            at = end;
        }
        self.block.clear();
    }

    /// Appends record `(fp, offset)`, where `offset` is `doc - first`.
    /// Returns false, adding nothing, when it does not sort strictly after
    /// the previous record.
    pub(super) fn push(&mut self, record: Record) -> bool {
        let (fp, offset) = record.key();
        if self.codec != FINGERPRINT_CODEC {
            if self.last.is_some_and(|last| (fp, offset) <= last) {
                return false;
            }
            self.block.push(record);
            self.last = Some((fp, offset));
            self.records += 1;
            if self.block.len() == RECORD_BLOCK {
                self.flush_block();
            }
            return true;
        }
        if self.last.is_some_and(|last| (fp, offset) <= last) {
            return false;
        }
        if self.records == 0 || self.in_block == RECORD_BLOCK {
            let start = self.len();
            self.index.extend_from_slice(&fp.to_le_bytes());
            self.index.extend_from_slice(&(start as u32).to_le_bytes());
            self.in_block = 0;
            self.bytes.extend_from_slice(&fp.to_le_bytes());
            self.bytes.extend_from_slice(&offset.to_le_bytes());
        } else {
            let previous = self.last.map_or(0, |(fp, _)| fp);
            put(u64::from(fp - previous), &mut self.bytes);
            put(u64::from(offset), &mut self.bytes);
        }
        self.last = Some((fp, offset));
        self.in_block += 1;
        self.records += 1;
        true
    }

    /// Bytes of the section so far, drained or not.
    pub(super) fn len(&self) -> u64 {
        self.drained + self.bytes.len() as u64
    }

    pub(super) fn records(&self) -> u64 {
        self.records
    }

    /// The undrained tail.
    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Drops the first `n` undrained bytes, which the caller has written.
    pub(super) fn drain(&mut self, n: usize) {
        self.bytes.drain(..n);
        self.drained += n as u64;
    }

    /// The fingerprint index section: the record count, then the blocks.
    pub(super) fn index(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.index.len() + 10);
        put(self.records, &mut out);
        out.extend_from_slice(&self.index);
        out
    }

    pub(super) fn allocated_bytes(&self) -> usize {
        self.bytes.capacity() + self.index.capacity() + self.block.capacity() * size_of::<Record>()
    }
}

/// The parsed fingerprint index: each block's first fingerprint and start.
#[derive(Debug, Default)]
pub(super) struct Index {
    pub(super) records: u64,
    pub(super) firsts: Vec<u32>,
    pub(super) starts: Vec<u32>,
}

impl Index {
    /// Parses the section; `None` when it is inconsistent with itself or a
    /// records section of `len` bytes.
    pub(super) fn parse(bytes: &[u8], len: u64) -> Option<Self> {
        let mut at = 0;
        let records = take(bytes, &mut at)?;
        let blocks = records.div_ceil(RECORD_BLOCK as u64);
        // Each block has exactly eight index bytes after the record count.
        if blocks > bytes.len().saturating_sub(at) as u64 / 8 {
            return None;
        }
        let mut index = Self {
            records,
            firsts: Vec::with_capacity(blocks as usize),
            starts: Vec::with_capacity(blocks as usize),
        };
        let mut start = 0u64;
        for b in 0..blocks as usize {
            let fp = u32_at(bytes.get(at..at + 4)?, 0);
            at += 4;
            let next_start = u64::from(u32_at(bytes.get(at..at + 4)?, 0));
            at += 4;
            let step = next_start.checked_sub(start)?;
            // A block holds at least its 8-byte first record.
            if b == 0 && step != 0 || b > 0 && step < 8 {
                return None;
            }
            if index.firsts.last().is_some_and(|&last| fp < last) {
                return None;
            }
            start = start.checked_add(step)?;
            if start >= len {
                return None;
            }
            index.firsts.push(fp);
            index.starts.push(u32::try_from(start).ok()?);
        }
        (at == bytes.len() && (blocks > 0 || len == 0)).then_some(index)
    }

    /// Records in block `b`.
    pub(super) fn count(&self, b: usize) -> usize {
        let before = (b * RECORD_BLOCK) as u64;
        (self.records - before).min(RECORD_BLOCK as u64) as usize
    }

    pub(super) fn allocated_bytes(&self) -> usize {
        self.firsts.capacity() * size_of::<u32>() + self.starts.capacity() * size_of::<u32>()
    }
}

/// Decodes one block of `count` records whose first fingerprint the index
/// says is `first_fp`, calling `each(fp, offset)` in order until it
/// returns false. `None` when the bytes are inconsistent: a short or long
/// block, a first fingerprint other than the index's, a record not after
/// the one before it, or an offset past `span`.
pub(super) fn decode_block(
    bytes: &[u8],
    first_fp: u32,
    count: usize,
    span: u32,
    codec: u32,
    mut each: impl FnMut(Record) -> bool,
) -> Option<()> {
    if codec != FINGERPRINT_CODEC {
        return decode_runs(bytes, first_fp, count, span, codec, each);
    }
    if bytes.len() < 8 {
        return None;
    }
    let (mut fp, mut offset) = (u32_at(bytes, 0), u32_at(bytes, 4));
    if fp != first_fp || offset > span {
        return None;
    }
    let mut at = 8;
    for i in 0..count {
        if i > 0 {
            let delta = u32::try_from(take(bytes, &mut at)?).ok()?;
            let next = u32::try_from(take(bytes, &mut at)?).ok()?;
            if next > span || delta == 0 && next <= offset {
                return None;
            }
            fp = fp.checked_add(delta)?;
            offset = next;
        }
        if !each(Record {
            fingerprint: fp,
            doc: offset,
            offset: None,
        }) {
            return Some(());
        }
    }
    (at == bytes.len()).then_some(())
}

/// Each restart block splits runs at its boundary: no decoder needs an
/// earlier block's fingerprint or document base.
fn decode_runs(
    bytes: &[u8],
    first_fp: u32,
    count: usize,
    span: u32,
    codec: u32,
    mut each: impl FnMut(Record) -> bool,
) -> Option<()> {
    if bytes.len() < 9 {
        return None;
    }
    let (mut fp, mut doc) = (u32_at(bytes, 0), u32_at(bytes, 4));
    if fp != first_fp || doc > span {
        return None;
    }
    let (mut at, mut seen) = (8, 0);
    while seen < count {
        if seen > 0 {
            let delta = u32::try_from(take(bytes, &mut at)?).ok()?;
            if delta == 0 {
                return None;
            }
            fp = fp.checked_add(delta)?;
            doc = u32::try_from(take(bytes, &mut at)?).ok()?;
            if doc > span {
                return None;
            }
        }
        let run = usize::try_from(take(bytes, &mut at)?).ok()?;
        if run == 0 || run > count - seen {
            return None;
        }
        for i in 0..run {
            if i > 0 {
                let delta = u32::try_from(take(bytes, &mut at)?).ok()?;
                if delta == 0 {
                    return None;
                }
                doc = doc.checked_add(delta)?;
                if doc > span {
                    return None;
                }
            }
            let offset = if stores_offsets(codec) {
                let value = take(bytes, &mut at)?;
                if value == 0 {
                    None
                } else {
                    Some(u32::try_from(value - 1).ok()?)
                }
            } else {
                None
            };
            if !each(Record {
                fingerprint: fp,
                doc,
                offset,
            }) {
                return Some(());
            }
        }
        seen += run;
    }
    (at == bytes.len()).then_some(())
}
