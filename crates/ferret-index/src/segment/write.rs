//! [`Writer`]: terms in order in, one segment's bytes out. By default
//! everything is buffered in memory and written once by [`Writer::finish`],
//! because the head's checksums cover every section. A merge's output can be
//! as large as the whole index, so [`Writer::spilling`] instead streams whole
//! chunks of the blocks section to their final place in the segment file and
//! of the postings section to a scratch file, taking each chunk's sum as it
//! goes; [`Writer::finish_spilled`] copies the postings after the blocks and
//! writes the small sections and the head last.
//!
//! A codec 2 writer ([`Writer::with_codec`]) sends each single-document term
//! to the fingerprint section rather than the dictionary. Those records
//! arrive in term order, so they wait in memory, sorted once, until the
//! segment finishes or a merge starts pushing the records it carries
//! ([`Writer::push_record`]), which it does after its last term. From then
//! on the section's offset is final, so a spilling writer streams it
//! straight to its place in the file.

use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::FileExt;

use intpack::pfor128skip;

use super::fingerprint::{Encoder, fingerprint};
use super::{
    ALL, BLOCK, CHUNK, DICTIONARY_CODEC, FIELDS, FINGERPRINT_CODEC, FORMAT_VERSION, MAGIC,
    POSTINGS_CODEC, Section, WriteError, checksum, chunk_sums, head_len, put, sections,
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
    /// Codec 2: the fingerprint records section and its block index.
    pub fingerprints: u64,
    pub fingerprint_index: u64,
    /// Codec 2: records written. Each is also counted in `pairs`.
    pub records: u64,
}

impl Sizes {
    /// The whole file.
    pub fn total(&self) -> u64 {
        self.head
            + self.blocks
            + self.postings
            + self.index
            + self.sums
            + self.fingerprints
            + self.fingerprint_index
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
        self.fingerprints += other.fingerprints;
        self.fingerprint_index += other.fingerprint_index;
        self.records += other.records;
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
    codec: u32,
    /// Terms pushed, in the dictionary or not.
    pushed: u64,
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
    /// Codec 2's fingerprint records.
    singles: Option<Singles>,
}

/// A codec 2 writer's fingerprint records.
#[derive(Debug, Default)]
struct Singles {
    /// `(fingerprint, doc - first)` of each single-document term pushed,
    /// in term order until sealed, then sorted and deduplicated.
    pending: Vec<(u32, u32)>,
    /// Whether the dictionary is closed and `pending` sorted.
    sealed: bool,
    /// The first record of `pending` not yet encoded.
    next: usize,
    encoder: Encoder,
    /// The last record [`Writer::push_record`] took.
    carried: Option<(u32, u32)>,
    /// Chunk sums of the section's spilled bytes.
    sums: Vec<u8>,
}

impl Singles {
    /// Closes the dictionary: sorts the pending records, once.
    fn seal(&mut self) {
        if !self.sealed {
            self.pending.sort_unstable();
            self.pending.dedup();
            self.sealed = true;
        }
    }

    /// Encodes pending records below `bound`, or all of them.
    fn flush_below(&mut self, bound: Option<(u32, u32)>) {
        while let Some(&record) = self.pending.get(self.next)
            && bound.is_none_or(|bound| record < bound)
        {
            // An equal record is a collision of two terms in one document:
            // one record already says the document may hold both.
            self.encoder.push(record.0, record.1);
            self.next += 1;
        }
        if self.next == self.pending.len() {
            self.pending = Vec::new();
            self.next = 0;
        }
    }
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
        Self::keyed(first, last, ferret_text::TOKENIZER_VERSION)
    }

    /// A writer for opaque byte keys, stamped with their extractor version.
    pub fn keyed(first: u32, last: u32, version: u32) -> Result<Self, WriteError> {
        Self::build(first, last, version, DICTIONARY_CODEC)
    }

    /// A writer for documents `first..=last` with dictionary codec `codec`:
    /// [`DICTIONARY_CODEC`] or [`FINGERPRINT_CODEC`].
    pub fn with_codec(first: u32, last: u32, codec: u32) -> Result<Self, WriteError> {
        Self::build(first, last, ferret_text::TOKENIZER_VERSION, codec)
    }

    fn build(first: u32, last: u32, version: u32, codec: u32) -> Result<Self, WriteError> {
        if first > last {
            return Err(WriteError::Range { first, last });
        }
        if sections(codec).is_none() {
            return Err(WriteError::Codec(codec));
        }
        Ok(Self {
            first,
            last,
            tokenizer: version,
            codec,
            pushed: 0,
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
            singles: (codec == FINGERPRINT_CODEC).then(Singles::default),
        })
    }

    /// The dictionary codec this writer writes.
    pub fn codec(&self) -> u32 {
        self.codec
    }

    /// Bytes of this writer's head.
    fn head_len(&self) -> u64 {
        head_len(self.sections().len()) as u64
    }

    fn sections(&self) -> &'static [Section] {
        sections(self.codec).unwrap_or(&[])
    }

    /// A writer whose sections leave memory through [`Writer::spill`]:
    /// blocks into `out` at their final offsets, postings into the scratch
    /// file `postings`. Both must be empty, writable files.
    /// [`Writer::finish_spilled`] completes `out`.
    pub fn spilling(first: u32, last: u32, out: File, postings: File) -> Result<Self, WriteError> {
        Self::keyed_spilling(first, last, ferret_text::TOKENIZER_VERSION, out, postings)
    }

    /// [`Writer::spilling`] with an explicit key extractor version.
    pub fn keyed_spilling(
        first: u32,
        last: u32,
        version: u32,
        out: File,
        postings: File,
    ) -> Result<Self, WriteError> {
        Self::spilling_from(Self::keyed(first, last, version)?, out, postings)
    }

    /// [`Writer::spilling`] with dictionary codec `codec`.
    pub fn spilling_with_codec(
        first: u32,
        last: u32,
        codec: u32,
        out: File,
        postings: File,
    ) -> Result<Self, WriteError> {
        Self::spilling_from(Self::with_codec(first, last, codec)?, out, postings)
    }

    fn spilling_from(mut writer: Self, out: File, postings: File) -> Result<Self, WriteError> {
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

    /// Allocated buffer capacities, including encoding scratch and spilled
    /// checksums. Excludes file handles and allocator metadata.
    pub fn allocated_bytes(&self) -> usize {
        self.blocks.capacity()
            + self.postings.capacity()
            + self.index.capacity()
            + self.previous.capacity()
            + self.relative.capacity() * size_of::<u32>()
            + self.spill.as_ref().map_or(0, |spill| {
                spill.blocks_sums.capacity() + spill.postings_sums.capacity()
            })
            + self.singles.as_ref().map_or(0, |singles| {
                singles.pending.capacity() * size_of::<(u32, u32)>()
                    + singles.encoder.allocated_bytes()
                    + singles.sums.capacity()
            })
    }

    /// Capacity needed while finishing an in-memory writer, including the
    /// exactly reserved chunk checksums and the head buffer.
    pub fn finishing_bytes(&self) -> usize {
        let records = self
            .singles
            .as_ref()
            .map_or(0, |s| s.encoder.bytes().len() + s.pending.len() * 10);
        self.allocated_bytes()
            + self.head_len() as usize
            + (self.blocks.len().div_ceil(CHUNK)
                + self.postings.len().div_ceil(CHUNK)
                + records.div_ceil(CHUNK))
                * 16
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
        let head = self.head_len();
        let (blocks_len, postings_len) = self.lengths();
        let Some(spill) = self.spill.as_mut() else {
            return Ok(());
        };
        if let Some(singles) = self.singles.as_mut().filter(|s| s.sealed)
            && singles.encoder.bytes().len() >= SPILL
        {
            // Sealed: no term follows, so the section's offset is final.
            let whole = singles.encoder.bytes().len() / CHUNK * CHUNK;
            let bytes = &singles.encoder.bytes()[..whole];
            chunk_sums(bytes, &mut singles.sums);
            let at = head + blocks_len + postings_len + singles.encoder.len()
                - singles.encoder.bytes().len() as u64;
            write_paced(&spill.out, bytes, at, pace)?;
            singles.encoder.drain(whole);
        }
        if self.blocks.len() >= SPILL {
            let whole = self.blocks.len() / CHUNK * CHUNK;
            chunk_sums(&self.blocks[..whole], &mut spill.blocks_sums);
            write_paced(
                &spill.out,
                &self.blocks[..whole],
                head + spill.blocks_written,
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
    ///
    /// On a codec 2 writer a term with one document becomes a fingerprint
    /// record instead, and the entry's size is 0. Terms may not follow
    /// [`Writer::push_record`].
    pub fn push(&mut self, term: &[u8], docs: &[u32]) -> Result<u64, WriteError> {
        if self.pushed > 0 && term <= self.previous.as_slice() {
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
        if let Some(singles) = self.singles.as_mut() {
            if singles.sealed {
                return Err(WriteError::RecordOrder);
            }
            if let [doc] = docs {
                singles.pending.push((fingerprint(term), doc - self.first));
                self.sizes.singletons += 1;
                self.previous.clear();
                self.previous.extend_from_slice(term);
                self.pushed += 1;
                return Ok(0);
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
        self.pushed += 1;
        self.in_block += 1;
        self.sizes.terms += 1;
        self.sizes.pairs += docs.len() as u64;
        Ok((self.blocks.len() - entry) as u64)
    }

    /// Adds a fingerprint record a merge carries from an input: document
    /// `doc`, inside `[first, last]`, may hold a term whose fingerprint is
    /// `fp`. Records must come in strictly increasing `(fp, doc)` order,
    /// after the last term: the first one closes the dictionary. A codec 1
    /// writer has no records and refuses it.
    pub fn push_record(&mut self, fp: u32, doc: u32) -> Result<(), WriteError> {
        let Some(singles) = self.singles.as_mut() else {
            return Err(WriteError::RecordOrder);
        };
        if !(self.first..=self.last).contains(&doc) {
            return Err(WriteError::DocRange(doc));
        }
        let record = (fp, doc - self.first);
        if singles.carried.is_some_and(|carried| record <= carried) {
            return Err(WriteError::RecordOrder);
        }
        singles.seal();
        singles.flush_below(Some(record));
        singles.encoder.push(record.0, record.1);
        singles.carried = Some(record);
        Ok(())
    }

    /// Closes the dictionary and encodes every pending record.
    fn seal_records(&mut self) {
        if let Some(singles) = self.singles.as_mut() {
            singles.seal();
            singles.flush_below(None);
        }
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
        let mut sums = Vec::with_capacity(
            (self.blocks.len().div_ceil(CHUNK) + self.postings.len().div_ceil(CHUNK)) * 16,
        );
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
