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
    documents: Vec<u32>,
    first: Option<u32>,
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
            documents: Vec::new(),
            first: None,
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

    /// Adds raw document bytes and returns its distinct-key count.
    pub fn add(&mut self, doc: u32, bytes: &[u8]) -> Result<usize, WriteError> {
        self.add_with(doc, bytes, |_| {})
    }

    /// As [`Self::add`], also visiting each distinct folded key once.
    pub fn add_with(
        &mut self,
        doc: u32,
        bytes: &[u8],
        mut emit: impl FnMut([u8; 3]),
    ) -> Result<usize, WriteError> {
        if self.last.is_some_and(|last| doc <= last) {
            return Err(WriteError::DocOrder);
        }
        self.first.get_or_insert(doc);
        self.last = Some(doc);
        let start = self.pairs.len();
        for window in bytes.windows(3) {
            let key = trigram_key([window[0], window[1], window[2]]);
            let number = u32::from_be_bytes([0, key[0], key[1], key[2]]) as usize;
            let bit = 1u64 << (number % 64);
            if self.seen[number / 64] & bit == 0 {
                self.seen[number / 64] |= bit;
                self.pairs.push((number as u64) << 32 | u64::from(doc));
                emit(key);
            }
        }
        for pair in &self.pairs[start..] {
            let number = (pair >> 32) as usize;
            self.seen[number / 64] &= !(1u64 << (number % 64));
        }
        Ok(self.pairs.len() - start)
    }

    /// Buffer capacities, including bitmap, radix and document-list scratch.
    pub fn allocated_bytes(&self) -> usize {
        (self.pairs.capacity() + self.scratch.capacity() + self.seen.capacity()) * size_of::<u64>()
            + self.documents.capacity() * size_of::<u32>()
    }

    /// Capacity charge at the next flush, reserving radix scratch for every
    /// allocated pair slot and a document list spanning the segment range.
    pub fn flush_bytes(&self) -> usize {
        let docs = self.first.zip(self.last).map_or(0, |(first, last)| {
            (u64::from(last) - u64::from(first) + 1) as usize
        });
        (self.pairs.capacity()
            + self.scratch.capacity().max(self.pairs.capacity())
            + self.seen.capacity())
            * size_of::<u64>()
            + self.documents.capacity().max(docs) * size_of::<u32>()
    }

    /// Discards pairs and ordering state, retaining allocated buffers.
    pub fn clear(&mut self) {
        self.pairs.clear();
        self.first = None;
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
        let docs = self.first.zip(self.last).map_or(0, |(first, last)| {
            (u64::from(last) - u64::from(first) + 1) as usize
        });
        self.documents.clear();
        self.documents.reserve_exact(docs);
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
        let mut start = 0;
        while start < count {
            let number = (self.pairs[start] >> 32) as u32;
            let mut end = start + 1;
            while end < count && self.pairs[end] >> 32 == u64::from(number) {
                end += 1;
            }
            self.documents.clear();
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
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::segment::{Hit, Segment};

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
