//! Raw-byte trigram inversion: one bitmap deduplicates each document;
//! stable radix sorting groups keys while preserving ascending DocIds.
//! The segment writer and merge stream treat these keys as opaque bytes.

use ferret_text::{TRIGRAM_VERSION, trigram_key};

use super::{WriteError, Writer};

/// Pair-buffer inverter for ASCII-folded three-byte keys. Add each document
/// once, in strictly ascending DocId order, including empty documents.
#[derive(Debug)]
pub struct TrigramInverter {
    pairs: Vec<u64>,
    scratch: Vec<u64>,
    seen: Vec<u64>,
    dirty_words: Vec<u64>,
    documents: Vec<u32>,
    last: Option<u32>,
}

impl Default for TrigramInverter {
    fn default() -> Self {
        Self::new()
    }
}

impl TrigramInverter {
    pub fn new() -> Self {
        Self {
            pairs: Vec::new(),
            scratch: Vec::new(),
            seen: vec![0; (1 << 24) / 64],
            dirty_words: vec![0; (1 << 24) / 64 / 64],
            documents: Vec::new(),
            last: None,
        }
    }

    /// Reserves pair slots once; admission must bound the next document.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            pairs: Vec::with_capacity(capacity),
            ..Self::new()
        }
    }

    pub(crate) fn capacity(&self) -> usize {
        self.pairs.capacity().max(self.scratch.capacity())
    }

    /// Buffered distinct key/document pairs.
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// Counts this document's distinct folded keys using the inverter's
    /// bitmap, without adding postings to the segment.
    pub fn distinct_count(&mut self, bytes: &[u8]) -> usize {
        let mut count = 0;
        for window in bytes.windows(3) {
            let key = trigram_key([window[0], window[1], window[2]]);
            let number = u32::from_be_bytes([0, key[0], key[1], key[2]]) as usize;
            let word = &mut self.seen[number / 64];
            let bit = 1u64 << (number % 64);
            if *word & bit == 0 {
                if *word == 0 {
                    let dirty = number / 64;
                    self.dirty_words[dirty / 64] |= 1u64 << (dirty % 64);
                }
                *word |= bit;
                count += 1;
            }
        }
        for (dirty_index, dirty) in self.dirty_words.iter_mut().enumerate() {
            while *dirty != 0 {
                let bit = dirty.trailing_zeros() as usize;
                let index = dirty_index * 64 + bit;
                let word = &mut self.seen[index];
                while *word != 0 {
                    *word &= *word - 1;
                }
                *dirty &= *dirty - 1;
            }
        }
        count
    }

    /// Adds raw document bytes and returns its distinct-key count.
    pub fn add(&mut self, doc: u32, bytes: &[u8]) -> Result<usize, WriteError> {
        self.add_with(doc, bytes, |_| {})
    }

    /// As [`Self::add`], also visiting each distinct folded key once.
    pub fn add_with(
        &mut self,
        doc: u32,
        bytes: &[u8],
        emit: impl FnMut([u8; 3]),
    ) -> Result<usize, WriteError> {
        self.add_inner(doc, bytes, usize::MAX, emit)?
            .ok_or(WriteError::DocOrder)
    }

    /// Adds a document if its distinct keys fit the caller's remaining
    /// capacity. A rejected document leaves ordering and postings unchanged.
    pub(crate) fn add_limited(
        &mut self,
        doc: u32,
        bytes: &[u8],
        limit: usize,
    ) -> Result<Option<usize>, WriteError> {
        self.add_inner(doc, bytes, limit, |_| {})
    }

    fn add_inner(
        &mut self,
        doc: u32,
        bytes: &[u8],
        limit: usize,
        mut emit: impl FnMut([u8; 3]),
    ) -> Result<Option<usize>, WriteError> {
        if self.last.is_some_and(|last| doc <= last) {
            return Err(WriteError::DocOrder);
        }
        let start = self.pairs.len();
        let mut count = 0;
        let mut rejected = false;
        for window in bytes.windows(3) {
            let key = trigram_key([window[0], window[1], window[2]]);
            let number = u32::from_be_bytes([0, key[0], key[1], key[2]]) as usize;
            let bit = 1u64 << (number % 64);
            if self.seen[number / 64] & bit == 0 {
                if count == limit {
                    rejected = true;
                    break;
                }
                self.seen[number / 64] |= bit;
                self.pairs.push((number as u64) << 32 | u64::from(doc));
                emit(key);
                count += 1;
            }
        }
        for pair in &self.pairs[start..] {
            let number = (pair >> 32) as usize;
            self.seen[number / 64] &= !(1u64 << (number % 64));
        }
        if rejected {
            self.pairs.truncate(start);
            return Ok(None);
        }
        self.last = Some(doc);
        Ok(Some(count))
    }

    /// Buffer capacities, including bitmap, radix and document-list scratch.
    pub fn allocated_bytes(&self) -> usize {
        (self.pairs.capacity() + self.scratch.capacity() + self.seen.capacity()) * size_of::<u64>()
            + self.documents.capacity() * size_of::<u32>()
            + self.dirty_words.capacity() * size_of::<u64>()
    }

    /// Capacity charge at the next flush, reserving radix scratch for every
    /// allocated pair slot and a document list bounded by buffered pairs.
    pub fn flush_bytes(&self) -> usize {
        let docs = self.pairs.len();
        (self.pairs.capacity()
            + self.scratch.capacity().max(self.pairs.capacity())
            + self.seen.capacity())
            * size_of::<u64>()
            + self.documents.capacity().max(docs) * size_of::<u32>()
            + self.dirty_words.capacity() * size_of::<u64>()
    }

    /// Discards pairs and ordering state, retaining allocated buffers.
    pub fn clear(&mut self) {
        self.pairs.clear();
        self.last = None;
    }

    /// Drains into an in-memory keyed segment writer. `observe` receives
    /// simultaneous inverter and writer capacities after every encoded key.
    pub fn drain_into_with(
        &mut self,
        first: u32,
        last: u32,
        mut observe: impl FnMut(usize),
    ) -> Result<Writer, WriteError> {
        let mut writer = Writer::keyed(first, last, TRIGRAM_VERSION)?;
        let count = self.pairs.len();
        if self.scratch.capacity() < count {
            self.scratch.reserve_exact(count - self.scratch.len());
        }
        self.scratch.resize(count, 0);
        for shift in [32, 40, 48] {
            let mut offsets = [0usize; 256];
            for pair in &self.pairs {
                offsets[((pair >> shift) & 255) as usize] += 1;
            }
            let mut sum = 0;
            for offset in &mut offsets {
                let frequency = *offset;
                *offset = sum;
                sum += frequency;
            }
            for &pair in &self.pairs {
                let bucket = ((pair >> shift) & 255) as usize;
                self.scratch[offsets[bucket]] = pair;
                offsets[bucket] += 1;
            }
            self.pairs.copy_from_slice(&self.scratch[..count]);
        }
        observe(self.allocated_bytes() + writer.allocated_bytes());
        // D70 charges radix scratch by this flush's length. Retaining its
        // capacity would instead charge the largest previous segment.
        self.scratch = Vec::new();
        let mut start = 0;
        while start < count {
            let number = (self.pairs[start] >> 32) as u32;
            let mut end = start + 1;
            while end < count && self.pairs[end] >> 32 == u64::from(number) {
                end += 1;
            }
            self.documents.clear();
            self.documents.reserve_exact(end - start);
            self.documents
                .extend(self.pairs[start..end].iter().map(|pair| *pair as u32));
            writer.push(&number.to_be_bytes()[1..], &self.documents)?;
            observe(self.allocated_bytes() + writer.allocated_bytes());
            start = end;
        }
        self.clear();
        Ok(writer)
    }

    /// Drains into an in-memory segment with the trigram extractor version.
    pub fn drain_into(&mut self, first: u32, last: u32) -> Result<Writer, WriteError> {
        self.drain_into_with(first, last, |_| {})
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet, HashSet};

    use super::*;
    use crate::segment::{Hit, Segment};

    #[test]
    fn distinct_count_matches_a_brute_force_set_on_random_bytes() {
        let mut inverter = TrigramInverter::new();
        let mut seed = 0x8f37_2a91_6c04_d5ebu64;
        for len in 0..512 {
            let bytes: Vec<_> = (0..len * 17)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed as u8
                })
                .collect();
            let expected = bytes
                .windows(3)
                .map(|window| trigram_key([window[0], window[1], window[2]]))
                .collect::<HashSet<_>>()
                .len();
            assert_eq!(inverter.distinct_count(&bytes), expected, "length {len}");
            assert!(inverter.is_empty());
        }
    }

    #[test]
    fn drains_keep_the_reservation_and_release_length_charged_radix_scratch() {
        let mut inverter = TrigramInverter::with_capacity(1000);
        for (doc, bytes) in [(0, b"abcxyz".as_slice()), (1, b"aaa".as_slice())] {
            inverter.add(doc, bytes).unwrap();
            inverter.drain_into(doc, doc).unwrap();
            assert_eq!(inverter.pairs.capacity(), 1000);
            assert_eq!(inverter.scratch.capacity(), 0);
            assert!(inverter.is_empty());
        }
    }

    #[test]
    fn sparse_doc_ids_reserve_only_the_real_posting_group() {
        // Historical DocId gaps must not turn two postings into GB of scratch.
        let mut inverter = TrigramInverter::with_capacity(2);
        let baseline = inverter.allocated_bytes();
        inverter.add(0, b"abc").unwrap();
        inverter.add(1_000_000_000, b"abc").unwrap();
        assert_eq!(inverter.flush_bytes(), baseline + 2 * (8 + 4));
        let mut peak = 0;
        let writer = inverter
            .drain_into_with(0, 1_000_000_000, |bytes| {
                peak = peak.max(bytes);
            })
            .unwrap();
        assert_eq!(inverter.documents.capacity(), 2);
        assert_eq!(inverter.allocated_bytes(), baseline + 8);
        assert!(peak < baseline + 4096);
        let mut bytes = Vec::new();
        writer.finish(&mut bytes).unwrap();
        let segment = Segment::open(bytes).unwrap();
        let Some(Hit::Postings(list)) = segment.lookup(b"abc").unwrap() else {
            panic!("two real postings");
        };
        let mut docs = Vec::new();
        list.decode(&mut docs);
        assert_eq!(docs, [0, 1_000_000_000]);
    }

    #[test]
    fn rejected_admission_preserves_capacity_order_and_contents() {
        // Rejection at capacity used to grow the fixed pair reservation.
        let mut inverter = TrigramInverter::with_capacity(4);
        inverter.add(7, b"abcde").unwrap();
        let pairs = inverter.pairs.clone();
        let capacity = inverter.allocated_bytes();
        assert_eq!(inverter.add_limited(9, b"xyzab", 1).unwrap(), None);
        assert_eq!(inverter.allocated_bytes(), capacity);
        assert_eq!(inverter.pairs.capacity(), 4);
        assert_eq!(inverter.pairs, pairs);
        assert_eq!(inverter.last, Some(7));
        assert!(inverter.seen.iter().all(|word| *word == 0));
        assert_eq!(inverter.add_limited(8, b"yza", 1).unwrap(), Some(1));
        let pairs = inverter.pairs.clone();
        assert_eq!(inverter.add_limited(9, b"xyz", 0).unwrap(), None);
        assert_eq!(inverter.allocated_bytes(), capacity);
        assert_eq!(inverter.pairs.capacity(), 4);
        assert_eq!(inverter.pairs, pairs);
        assert_eq!(inverter.last, Some(8));
        assert!(inverter.seen.iter().all(|word| *word == 0));
    }

    #[test]
    fn raw_bytes_match_per_document_sets_and_round_trip() {
        let mut inverter = TrigramInverter::new();
        let mut truth: BTreeMap<[u8; 3], Vec<u32>> = BTreeMap::new();
        let mut seed = 0xabc123u64;
        for doc in 0..256 {
            let bytes = match doc {
                0 => vec![],
                1 => vec![b'A'],
                2 => vec![0xff, b'a'],
                3 => b"AAAaaaAAA\xff\x80\xfeAaA".to_vec(),
                _ => (0..doc * 7)
                    .map(|_| {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        seed as u8
                    })
                    .collect(),
            };
            // Independent extraction and folding oracle, not a restatement
            // of the bitmap or radix implementation.
            let keys: BTreeSet<_> = bytes
                .windows(3)
                .map(|w| {
                    [w[0], w[1], w[2]].map(|b| if b.is_ascii_uppercase() { b + 32 } else { b })
                })
                .collect();
            assert_eq!(inverter.add(doc, &bytes).unwrap(), keys.len());
            for key in keys {
                truth.entry(key).or_default().push(doc);
            }
        }
        let mut encoded = Vec::new();
        inverter
            .drain_into(0, 255)
            .unwrap()
            .finish(&mut encoded)
            .unwrap();
        let segment = Segment::open(encoded.as_slice()).unwrap();
        let mut terms = segment.terms();
        for (key, docs) in truth {
            let (actual, entry) = terms.next_term().unwrap().unwrap();
            assert_eq!(actual, key);
            let actual_docs = match segment.read(&entry).unwrap() {
                Hit::Single(doc) => vec![doc],
                Hit::Postings(postings) => {
                    let mut out = Vec::new();
                    postings.decode(&mut out);
                    out
                }
            };
            assert_eq!(actual_docs, docs);
        }
        assert!(terms.next_term().unwrap().is_none());
        assert_eq!(segment.info().tokenizer_version, TRIGRAM_VERSION);
        // Clearing per-document bits and retaining buffers must not hide
        // keys in the next segment, even when DocIds restart.
        assert_eq!(inverter.add(0, b"AAAaaa").unwrap(), 1);
        assert_eq!(inverter.add(0, b"abc"), Err(WriteError::DocOrder));
    }
}
