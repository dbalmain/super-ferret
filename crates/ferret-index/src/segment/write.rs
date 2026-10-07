//! [`Writer`]: terms in order in, one segment's bytes out. By default
//! everything is buffered in memory and written once by [`Writer::finish`],
//! because the head's checksums cover every section. A merge's output can be
//! as large as the whole index, so [`Writer::spilling`] instead streams whole
//! chunks of the blocks section to their final place in the segment file and
//! of the postings section to a scratch file, taking each chunk's sum as it
//! goes; [`Writer::finish_spilled`] copies the postings after the blocks and
//! writes the small sections and the head last.

use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::FileExt;

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

/// Buffered bytes past which [`Writer::spill`] writes whole chunks out.
pub(super) const SPILL: usize = 1 << 20;

/// Builds one segment over `[first, last]` from `(term, DocIds)` pushed in
/// strictly increasing term order.
#[derive(Debug)]
pub struct Writer {
    first: u32,
    last: u32,
    tokenizer: u32,
    /// The unspilled tails of the two chunked sections.
    blocks: Vec<u8>,
    postings: Vec<u8>,
    index: Vec<u8>,
    previous: Vec<u8>,
    in_block: usize,
    /// Where the current block began in the blocks and postings sections.
    block_start: u64,
    postings_base: u64,
    relative: Vec<u32>,
    sizes: Sizes,
    spill: Option<Spill>,
}

/// A spilling writer's files and what has left memory so far.
#[derive(Debug)]
struct Spill {
    /// The segment file: blocks are written at their final offsets.
    out: File,
    /// Scratch for the postings section, copied into `out` at the end.
    postings: File,
    blocks_written: u64,
    postings_written: u64,
    /// Chunk sums of the spilled bytes of each section.
    blocks_sums: Vec<u8>,
    postings_sums: Vec<u8>,
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
            spill: None,
        })
    }

    /// A writer whose sections leave memory through [`Writer::spill`]:
    /// blocks into `out` at their final offsets, postings into the scratch
    /// file `postings`. Both must be empty, writable files.
    /// [`Writer::finish_spilled`] completes `out`.
    pub fn spilling(first: u32, last: u32, out: File, postings: File) -> Result<Self, WriteError> {
        let mut writer = Self::new(first, last)?;
        writer.spill = Some(Spill {
            out,
            postings,
            blocks_written: 0,
            postings_written: 0,
            blocks_sums: Vec::new(),
            postings_sums: Vec::new(),
        });
        Ok(writer)
    }

    /// Bytes of the blocks and postings sections so far, spilled or not.
    fn lengths(&self) -> (u64, u64) {
        let (blocks, postings) = self
            .spill
            .as_ref()
            .map_or((0, 0), |s| (s.blocks_written, s.postings_written));
        (
            blocks + self.blocks.len() as u64,
            postings + self.postings.len() as u64,
        )
    }

    /// Writes whole chunks out of a spilling writer once its buffers pass
    /// a threshold, calling `pace` with each write's size first. Does
    /// nothing for an in-memory writer. Call it between pushes.
    pub fn spill(&mut self, pace: &dyn Fn(usize) -> io::Result<()>) -> io::Result<()> {
        let Some(spill) = self.spill.as_mut() else {
            return Ok(());
        };
        if self.blocks.len() >= SPILL {
            let whole = self.blocks.len() / CHUNK * CHUNK;
            chunk_sums(&self.blocks[..whole], &mut spill.blocks_sums);
            write_paced(
                &spill.out,
                &self.blocks[..whole],
                HEAD as u64 + spill.blocks_written,
                pace,
            )?;
            spill.blocks_written += whole as u64;
            self.blocks.drain(..whole);
        }
        if self.postings.len() >= SPILL {
            let whole = self.postings.len() / CHUNK * CHUNK;
            chunk_sums(&self.postings[..whole], &mut spill.postings_sums);
            write_paced(
                &spill.postings,
                &self.postings[..whole],
                spill.postings_written,
                pace,
            )?;
            spill.postings_written += whole as u64;
            self.postings.drain(..whole);
        }
        Ok(())
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
            let (blocks, postings) = self.lengths();
            put(term.len() as u64, &mut self.index);
            self.index.extend_from_slice(term);
            put(blocks - self.block_start, &mut self.index);
            put(postings - self.postings_base, &mut self.index);
            self.block_start = blocks;
            self.postings_base = postings;
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

    /// Writes an in-memory writer's segment to `out` and returns its sizes.
    /// A spilling writer is refused: use [`Writer::finish_spilled`].
    pub fn finish(self, out: &mut impl Write) -> io::Result<Sizes> {
        if self.spill.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a spilling segment writer finishes with finish_spilled",
            ));
        }
        let mut sums = Vec::new();
        chunk_sums(&self.blocks, &mut sums);
        let blocks_sums = sums.len();
        chunk_sums(&self.postings, &mut sums);
        let lengths = [
            self.blocks.len() as u64,
            self.postings.len() as u64,
            self.index.len() as u64,
            sums.len() as u64,
        ];
        let digests = [
            checksum(&sums[..blocks_sums]),
            checksum(&sums[blocks_sums..]),
            checksum(&self.index),
            checksum(&sums),
        ];
        out.write_all(&self.head(lengths, digests))?;
        for bytes in [&self.blocks, &self.postings, &self.index, &sums] {
            out.write_all(bytes)?;
        }
        Ok(self.sizes_of(lengths))
    }

    /// Completes a spilling writer's segment file: the rest of the blocks,
    /// the postings copied from scratch, then the index, the sums and, last,
    /// the head. Does not sync. `pace` is called before each write.
    pub fn finish_spilled(mut self, pace: &dyn Fn(usize) -> io::Result<()>) -> io::Result<Sizes> {
        let Some(mut spill) = self.spill.take() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "an in-memory segment writer finishes with finish",
            ));
        };
        chunk_sums(&self.blocks, &mut spill.blocks_sums);
        chunk_sums(&self.postings, &mut spill.postings_sums);
        let blocks_len = spill.blocks_written + self.blocks.len() as u64;
        let postings_len = spill.postings_written + self.postings.len() as u64;
        let mut at = HEAD as u64 + spill.blocks_written;
        write_paced(&spill.out, &self.blocks, at, pace)?;
        at += self.blocks.len() as u64;
        let mut copy = vec![0; SPILL];
        let mut from = 0;
        while from < spill.postings_written {
            let n = (spill.postings_written - from).min(SPILL as u64) as usize;
            for (i, chunk) in copy[..n].chunks_mut(64 << 10).enumerate() {
                pace(chunk.len())?;
                spill
                    .postings
                    .read_exact_at(chunk, from + (i * (64 << 10)) as u64)?;
            }
            write_paced(&spill.out, &copy[..n], at, pace)?;
            (from, at) = (from + n as u64, at + n as u64);
        }
        let mut sums = spill.blocks_sums;
        let blocks_sums = sums.len();
        sums.extend_from_slice(&spill.postings_sums);
        let tail: [&[u8]; 3] = [&self.postings, &self.index, &sums];
        for bytes in tail {
            write_paced(&spill.out, bytes, at, pace)?;
            at += bytes.len() as u64;
        }
        let lengths = [
            blocks_len,
            postings_len,
            self.index.len() as u64,
            sums.len() as u64,
        ];
        let digests = [
            checksum(&sums[..blocks_sums]),
            checksum(&sums[blocks_sums..]),
            checksum(&self.index),
            checksum(&sums),
        ];
        spill.out.write_all_at(&self.head(lengths, digests), 0)?;
        Ok(self.sizes_of(lengths))
    }

    /// The head over sections of `lengths` with section checksums `digests`.
    fn head(&self, lengths: [u64; 4], digests: [[u8; 16]; 4]) -> Vec<u8> {
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
        for &section in &SECTIONS {
            debug_assert_eq!(head.len(), FIELDS + section as usize * 32);
            let len = lengths[section as usize];
            head.extend_from_slice(&offset.to_le_bytes());
            head.extend_from_slice(&len.to_le_bytes());
            head.extend_from_slice(&digests[section as usize]);
            offset += len;
        }
        let digest = checksum(&head);
        head.extend_from_slice(&digest);
        debug_assert_eq!(head.len(), HEAD);
        head
    }

    fn sizes_of(&self, lengths: [u64; 4]) -> Sizes {
        let section = |s: Section| lengths[s as usize];
        Sizes {
            head: HEAD as u64,
            blocks: section(Section::Blocks),
            postings: section(Section::Postings),
            index: section(Section::Index),
            sums: section(Section::Sums),
            ..self.sizes
        }
    }
}

// Bound each paced transfer, including a large singleton term or tail section.
fn write_paced(
    file: &std::fs::File,
    bytes: &[u8],
    offset: u64,
    pace: &dyn Fn(usize) -> io::Result<()>,
) -> io::Result<()> {
    for (i, chunk) in bytes.chunks(64 << 10).enumerate() {
        pace(chunk.len())?;
        file.write_all_at(chunk, offset + (i * (64 << 10)) as u64)?;
    }
    Ok(())
}
