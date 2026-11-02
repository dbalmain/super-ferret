//! Raw-byte trigram inversion: one bitmap deduplicates each document;
//! stable radix sorting groups keys while preserving ascending DocIds.
//! The segment writer and merge stream treat these keys as opaque bytes.

use ferret_text::{TRIGRAM_VERSION, trigram_key};

use super::{WriteError, Writer};

/// The distinct folded keys of one document at a time: a bit per key,
/// cleared through the keys found, so a document costs its own length.
#[derive(Debug)]
pub struct KeySet {
    /// A bit per 24-bit key, sized so that indexing by a key needs no check.
    seen: Box<[u64; WORDS]>,
}

const WORDS: usize = (1 << 24) / 64;

impl Default for KeySet {
    fn default() -> Self {
        Self::new()
    }
}

impl KeySet {
    pub fn new() -> Self {
        // Built on the heap: two megabytes would not fit a test thread's
        // stack on the way.
        let Ok(seen) = vec![0; WORDS].into_boxed_slice().try_into() else {
            unreachable!("a slice of WORDS words");
        };
        Self { seen }
    }

    /// Replaces `out` with the distinct folded keys of `bytes`, each as its
    /// big-endian number, in order of first occurrence.
    pub fn distinct(&mut self, bytes: &[u8], out: &mut Vec<u32>) {
        out.clear();
        let [first, second, rest @ ..] = bytes else {
            return;
        };
        // Each byte is folded once, into a key that rolls along the windows:
        // the number of `trigram_key` of each.
        let fold = |byte: u8| u32::from(trigram_key([0, 0, byte])[2]);
        let mut number = fold(*first) << 8 | fold(*second);
        for &byte in rest {
            number = (number << 8 | fold(byte)) & 0xff_ffff;
            let word = &mut self.seen[number as usize / 64];
            let bit = 1u64 << (number % 64);
            if *word & bit == 0 {
                *word |= bit;
                out.push(number);
            }
        }
        for &number in out.iter() {
            self.seen[number as usize / 64] &= !(1u64 << (number % 64));
        }
    }

    fn allocated_bytes(&self) -> usize {
        size_of::<[u64; WORDS]>()
    }
}

/// Pair-buffer inverter for ASCII-folded three-byte keys. Add each document
/// once, in strictly ascending DocId order, including empty documents.
#[derive(Debug)]
pub struct TrigramInverter {
    pairs: Vec<u64>,
    scratch: Vec<u64>,
    keys: KeySet,
    found: Vec<u32>,
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
            keys: KeySet::new(),
            found: Vec::new(),
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

    /// Counts this document's distinct folded keys, without adding postings
    /// to the segment.
    pub fn distinct_count(&mut self, bytes: &[u8]) -> usize {
        self.keys.distinct(bytes, &mut self.found);
        self.found.len()
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
    #[cfg(test)]
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
        emit: impl FnMut([u8; 3]),
    ) -> Result<Option<usize>, WriteError> {
        let mut found = std::mem::take(&mut self.found);
        self.keys.distinct(bytes, &mut found);
        let added = self.add_keys(doc, &found, limit, emit);
        self.found = found;
        added
    }

    /// `add_limited` for keys already found by a [`KeySet`].
    pub(crate) fn add_keys(
        &mut self,
        doc: u32,
        keys: &[u32],
        limit: usize,
        mut emit: impl FnMut([u8; 3]),
    ) -> Result<Option<usize>, WriteError> {
        if self.last.is_some_and(|last| doc <= last) {
            return Err(WriteError::DocOrder);
        }
        if keys.len() > limit {
            return Ok(None);
        }
        self.pairs.reserve(keys.len());
        for &number in keys {
            self.pairs.push(u64::from(number) << 32 | u64::from(doc));
            let [_, a, b, c] = number.to_be_bytes();
            emit([a, b, c]);
        }
        self.last = Some(doc);
        Ok(Some(keys.len()))
    }

    /// Buffer capacities, including bitmap, radix and document-list scratch.
    pub fn allocated_bytes(&self) -> usize {
        (self.pairs.capacity() + self.scratch.capacity()) * size_of::<u64>()
            + self.keys.allocated_bytes()
            + (self.documents.capacity() + self.found.capacity()) * size_of::<u32>()
    }

    /// Capacity charge at the next flush, reserving radix scratch for every
    /// allocated pair slot and a document list bounded by buffered pairs.
    pub fn flush_bytes(&self) -> usize {
        let docs = self.pairs.len();
        (self.pairs.capacity() + self.scratch.capacity().max(self.pairs.capacity()))
            * size_of::<u64>()
            + self.keys.allocated_bytes()
            + (self.documents.capacity().max(docs) + self.found.capacity()) * size_of::<u32>()
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
        // Two stable passes of twelve bits sort the 24-bit keys, documents
        // already rising within each. Each pass scatters into the other
        // buffer, so the second leaves the pairs where they started.
        const DIGIT: u32 = 12;
        let mut offsets = [[0usize; 1 << DIGIT]; 2];
        for &pair in &self.pairs {
            offsets[0][(pair >> 32) as usize & 0xfff] += 1;
            offsets[1][(pair >> 44) as usize & 0xfff] += 1;
        }
        for (offsets, shift) in offsets.iter_mut().zip([32, 44]) {
            let mut sum = 0;
            for offset in offsets.iter_mut() {
                (*offset, sum) = (sum, sum + *offset);
            }
            for &pair in &self.pairs {
                let bucket = (pair >> shift) as usize & 0xfff;
                self.scratch[offsets[bucket]] = pair;
                offsets[bucket] += 1;
            }
            std::mem::swap(&mut self.pairs, &mut self.scratch);
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
        // The found-key list keeps its capacity across documents.
        let baseline = baseline + inverter.found.capacity() * size_of::<u32>();
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
        assert!(inverter.keys.seen.iter().all(|word| *word == 0));
        assert_eq!(inverter.add_limited(8, b"yza", 1).unwrap(), Some(1));
        let pairs = inverter.pairs.clone();
        assert_eq!(inverter.add_limited(9, b"xyz", 0).unwrap(), None);
        assert_eq!(inverter.allocated_bytes(), capacity);
        assert_eq!(inverter.pairs.capacity(), 4);
        assert_eq!(inverter.pairs, pairs);
        assert_eq!(inverter.last, Some(8));
        assert!(inverter.keys.seen.iter().all(|word| *word == 0));
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
