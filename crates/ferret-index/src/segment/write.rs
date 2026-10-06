//! [`Writer`]: terms in order in, one segment's bytes out. Everything is
//! buffered in memory and written once by [`Writer::finish`], because the
//! head's checksums cover every section.

use std::io::{self, Write};

use intpack::pfor128skip;

use super::{
    BLOCK, CHUNK, DICTIONARY_CODEC, FIELDS, FORMAT_VERSION, HEAD, MAGIC, POSTINGS_CODEC, SECTIONS,
    Section, WriteError, checksum, chunk_sums, put,
};

/// What a finished segment holds, in bytes and counts, for measurement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sizes {
    pub head: u64,
    pub blocks: u64,
    pub postings: u64,
    pub index: u64,
    pub sums: u64,
    pub terms: u64,
    /// Postings, the sum of every term's document frequency.
    pub pairs: u64,
    /// Terms with document frequency 1, inlined in the dictionary.
    pub singletons: u64,
    /// Bytes the singletons' dictionary entries take in `blocks`.
    pub singleton_bytes: u64,
    /// Term bytes stored: suffixes in `blocks` plus first terms in `index`.
    /// The rest of the dictionary is lengths, frequencies and pointers.
    pub term_bytes: u64,
}

impl Sizes {
    /// The whole file.
    pub fn total(&self) -> u64 {
        self.head + self.blocks + self.postings + self.index + self.sums
    }

    /// The term dictionary: its blocks and its block index.
    pub fn dictionary(&self) -> u64 {
        self.blocks + self.index
    }

    /// Adds `other`'s bytes and counts to these.
    pub fn add(&mut self, other: &Sizes) {
        self.head += other.head;
        self.blocks += other.blocks;
        self.postings += other.postings;
        self.index += other.index;
        self.sums += other.sums;
        self.terms += other.terms;
        self.pairs += other.pairs;
        self.singletons += other.singletons;
        self.singleton_bytes += other.singleton_bytes;
        self.term_bytes += other.term_bytes;
    }
}

/// Builds one segment over `[first, last]` from `(term, DocIds)` pushed in
/// strictly increasing term order.
#[derive(Debug)]
pub struct Writer {
    first: u32,
    last: u32,
    tokenizer: u32,
    blocks: Vec<u8>,
    postings: Vec<u8>,
    index: Vec<u8>,
    previous: Vec<u8>,
    in_block: usize,
    /// Where the current block began in `blocks` and `postings`.
    block_start: usize,
    postings_base: usize,
    relative: Vec<u32>,
    sizes: Sizes,
}

impl Writer {
    /// A writer for documents `first..=last`, stamped with the current
    /// tokenizer version.
    pub fn new(first: u32, last: u32) -> Result<Self, WriteError> {
        if first > last {
            return Err(WriteError::Range { first, last });
        }
        Ok(Self {
            first,
            last,
            tokenizer: ferret_text::TOKENIZER_VERSION,
            blocks: Vec::new(),
            postings: Vec::new(),
            index: Vec::new(),
            previous: Vec::new(),
            in_block: 0,
            block_start: 0,
            postings_base: 0,
            relative: Vec::new(),
            sizes: Sizes::default(),
        })
    }

    /// Adds `term`, which must sort strictly after the previous term, with
    /// its documents, strictly increasing and inside `[first, last]`. On an
    /// error nothing is added. Returns the entry's real encoded size: the
    /// bytes this term's entry added to the blocks section (shared/suffix,
    /// document frequency, and either the inlined doc or the list length).
    pub fn push(&mut self, term: &[u8], docs: &[u32]) -> Result<u64, WriteError> {
        if self.sizes.terms > 0 && term <= self.previous.as_slice() {
            return Err(WriteError::TermOrder);
        }
        let (Some(&low), Some(&high)) = (docs.first(), docs.last()) else {
            return Err(WriteError::NoDocuments);
        };
        if docs.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(WriteError::DocOrder);
        }
        for doc in [low, high] {
            if !(self.first..=self.last).contains(&doc) {
                return Err(WriteError::DocRange(doc));
            }
        }

        if self.sizes.terms == 0 || self.in_block == BLOCK {
            put(term.len() as u64, &mut self.index);
            self.index.extend_from_slice(term);
            put(
                (self.blocks.len() - self.block_start) as u64,
                &mut self.index,
            );
            put(
                (self.postings.len() - self.postings_base) as u64,
                &mut self.index,
            );
            self.block_start = self.blocks.len();
            self.postings_base = self.postings.len();
            self.in_block = 0;
            self.sizes.term_bytes += term.len() as u64;
        }

        let entry = self.blocks.len();
        if self.in_block > 0 {
            let shared = term
                .iter()
                .zip(&self.previous)
                .take_while(|(a, b)| a == b)
                .count();
            put(shared as u64, &mut self.blocks);
            put((term.len() - shared) as u64, &mut self.blocks);
            self.blocks.extend_from_slice(&term[shared..]);
            self.sizes.term_bytes += (term.len() - shared) as u64;
        }
        put(docs.len() as u64, &mut self.blocks);
        if let [doc] = docs {
            put(u64::from(doc - self.first), &mut self.blocks);
            self.sizes.singletons += 1;
            self.sizes.singleton_bytes += (self.blocks.len() - entry) as u64;
        } else {
            self.relative.clear();
            self.relative
                .extend(docs.iter().map(|&doc| doc - self.first));
            let start = self.postings.len();
            pfor128skip::encode_sorted(&self.relative, &mut self.postings);
            put((self.postings.len() - start) as u64, &mut self.blocks);
        }

        self.previous.clear();
        self.previous.extend_from_slice(term);
        self.in_block += 1;
        self.sizes.terms += 1;
        self.sizes.pairs += docs.len() as u64;
        Ok((self.blocks.len() - entry) as u64)
    }

    /// Writes the segment to `out` and returns its sizes.
    pub fn finish(self, out: &mut impl Write) -> io::Result<Sizes> {
        let mut sums = Vec::new();
        chunk_sums(&self.blocks, &mut sums);
        let blocks_sums = sums.len();
        chunk_sums(&self.postings, &mut sums);

        let sections: [(&[u8], [u8; 16]); 4] = [
            (&self.blocks, checksum(&sums[..blocks_sums])),
            (&self.postings, checksum(&sums[blocks_sums..])),
            (&self.index, checksum(&self.index)),
            (&sums, checksum(&sums)),
        ];
        let mut head = Vec::with_capacity(HEAD);
        head.extend_from_slice(&MAGIC);
        for word in [
            FORMAT_VERSION,
            self.tokenizer,
            DICTIONARY_CODEC,
            POSTINGS_CODEC,
            self.first,
            self.last,
        ] {
            head.extend_from_slice(&word.to_le_bytes());
        }
        head.extend_from_slice(&self.sizes.terms.to_le_bytes());
        head.extend_from_slice(&self.sizes.pairs.to_le_bytes());
        head.extend_from_slice(&(CHUNK as u32).to_le_bytes());
        head.extend_from_slice(&(SECTIONS.len() as u32).to_le_bytes());
        head.resize(FIELDS, 0);
        let mut offset = HEAD as u64;
        for (section, (bytes, sum)) in SECTIONS.iter().zip(&sections) {
            debug_assert_eq!(head.len(), FIELDS + *section as usize * 32);
            head.extend_from_slice(&offset.to_le_bytes());
            head.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            head.extend_from_slice(sum);
            offset += bytes.len() as u64;
        }
        let digest = checksum(&head);
        head.extend_from_slice(&digest);
        debug_assert_eq!(head.len(), HEAD);

        out.write_all(&head)?;
        for (bytes, _) in &sections {
            out.write_all(bytes)?;
        }
        let section = |s: Section| sections[s as usize].0.len() as u64;
        Ok(Sizes {
            head: HEAD as u64,
            blocks: section(Section::Blocks),
            postings: section(Section::Postings),
            index: section(Section::Index),
            sums: section(Section::Sums),
            ..self.sizes
        })
    }
}
