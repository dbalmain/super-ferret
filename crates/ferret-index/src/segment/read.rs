//! [`Segment`]: opens one segment with checked reads, looks terms up and
//! iterates them in order. The head, block index and chunk sums are read
//! at open and stay resident; a dictionary block or a postings list is read
//! with one positional read per lookup into an owned buffer (D64 A), and its
//! chunks are verified before anything decodes them.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

use intpack::Cursor as _;
use intpack::pfor128skip::{self, SortedCursor};

use super::{
    BLOCK, CHUNK, DICTIONARY_CODEC, FIELDS, FORMAT_VERSION, HEAD, MAGIC, POSTINGS_CODEC, ReadError,
    SECTIONS, Section, TABLE_ENTRY, checksum, chunks, take, u32_at, u64_at,
};

/// Positional reads from wherever a segment's bytes live. Implemented for
/// an open [`File`] and for bytes in memory; this crate never opens a path.
pub trait ReadAt {
    /// Total bytes.
    fn size(&self) -> io::Result<u64>;
    /// Fills `buf` from `offset`, or fails with `UnexpectedEof`.
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
}

impl ReadAt for File {
    fn size(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }

    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        FileExt::read_exact_at(self, buf, offset)
    }
}

impl ReadAt for [u8] {
    fn size(&self) -> io::Result<u64> {
        Ok(self.len() as u64)
    }

    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let bytes = start
            .checked_add(buf.len())
            .and_then(|end| self.get(start..end))
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        buf.copy_from_slice(bytes);
        Ok(())
    }
}

impl ReadAt for Vec<u8> {
    fn size(&self) -> io::Result<u64> {
        self.as_slice().size()
    }

    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.as_slice().read_exact_at(buf, offset)
    }
}

impl<T: ReadAt + ?Sized> ReadAt for &T {
    fn size(&self) -> io::Result<u64> {
        (**self).size()
    }

    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        (**self).read_exact_at(buf, offset)
    }
}

/// The head's fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    pub tokenizer_version: u32,
    pub first: u32,
    /// Inclusive.
    pub last: u32,
    pub terms: u64,
    /// The sum of every term's document frequency.
    pub pairs: u64,
}

/// One block's place, from the resident index.
#[derive(Clone, Copy, Debug)]
struct Block {
    /// Its first term's bytes in `Segment::firsts`.
    term_start: usize,
    term_end: usize,
    /// Byte offset in the blocks section.
    start: u64,
    /// Offset in the postings section of its first list.
    postings: u64,
}

/// Where a term's documents are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Location {
    /// Inlined in the dictionary: df = 1.
    Single(u32),
    /// A postings list at this offset and length in the postings section.
    List { offset: u64, len: u64 },
}

/// One dictionary entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermEntry {
    /// Document frequency in this segment.
    pub df: u32,
    location: Location,
}

/// A term's documents in this segment.
#[derive(Debug)]
pub enum Hit {
    /// The term's only document, inlined in the dictionary.
    Single(u32),
    /// Two or more documents.
    Postings(Postings),
}

/// One postings list, read and verified, owned.
#[derive(Debug)]
pub struct Postings {
    first: u32,
    df: u32,
    /// The verified chunks; the list is `bytes[start..]`.
    bytes: Vec<u8>,
    start: usize,
}

impl Postings {
    /// Documents in the list.
    pub fn len(&self) -> u32 {
        self.df
    }

    /// Never true: a list holds at least two documents.
    pub fn is_empty(&self) -> bool {
        self.df == 0
    }

    /// A cursor before the first document.
    pub fn cursor(&self) -> PostingCursor<'_> {
        PostingCursor {
            inner: SortedCursor::new(self.df as usize, &self.bytes[self.start..]),
            first: self.first,
        }
    }

    /// Every document, appended to `out`.
    pub fn decode(&self, out: &mut Vec<u32>) {
        let from = out.len();
        pfor128skip::decode_sorted(self.df as usize, &self.bytes[self.start..], out);
        for doc in &mut out[from..] {
            *doc = doc.wrapping_add(self.first);
        }
    }
}

/// Ascending DocIds of one list, with intpack's `next_geq` semantics: the
/// cursor stays on the returned document, and targets never decrease.
pub struct PostingCursor<'a> {
    inner: SortedCursor<'a>,
    first: u32,
}

impl PostingCursor<'_> {
    /// The smallest document `>= target` at or after the current one.
    pub fn next_geq(&mut self, target: u32) -> Option<u32> {
        let relative = target.saturating_sub(self.first);
        // The writer stores `doc - first` for docs in `[first, last]`, and
        // the bytes are checksummed, so the add cannot wrap.
        self.inner
            .next_geq(relative)
            .map(|doc| doc.wrapping_add(self.first))
    }

    /// The document after the current one.
    #[allow(clippy::should_implement_trait)] // intpack's `Cursor` names it so
    pub fn next(&mut self) -> Option<u32> {
        self.inner.next().map(|doc| doc.wrapping_add(self.first))
    }
}

/// One open segment.
pub struct Segment<R> {
    source: R,
    info: Info,
    /// `(offset, length)` of the blocks and postings sections in the file.
    blocks_at: (u64, u64),
    postings_at: (u64, u64),
    /// Chunk sums: the blocks section's, then the postings section's.
    sums: Vec<u8>,
    firsts: Vec<u8>,
    index: Vec<Block>,
}

impl<R: ReadAt> Segment<R> {
    /// Reads and checks the head, the block index and the chunk sums.
    pub fn open(source: R) -> Result<Self, ReadError> {
        let size = source.size()?;
        let mut head = vec![0; size.min(HEAD as u64) as usize];
        source.read_exact_at(&mut head, 0)?;
        if head.len() < 12 || head[..8] != MAGIC {
            return Err(ReadError::NotASegment);
        }
        let version = u32_at(&head, 8);
        if version != FORMAT_VERSION {
            return Err(ReadError::Version(version));
        }
        if head.len() < HEAD {
            return Err(ReadError::Layout);
        }
        if checksum(&head[..HEAD - 16]) != head[HEAD - 16..] {
            return Err(ReadError::Head);
        }
        let (dictionary, postings) = (u32_at(&head, 16), u32_at(&head, 20));
        if (dictionary, postings) != (DICTIONARY_CODEC, POSTINGS_CODEC) {
            return Err(ReadError::Codec {
                dictionary,
                postings,
            });
        }
        let info = Info {
            tokenizer_version: u32_at(&head, 12),
            first: u32_at(&head, 24),
            last: u32_at(&head, 28),
            terms: u64_at(&head, 32),
            pairs: u64_at(&head, 40),
        };
        if info.first > info.last
            || u32_at(&head, 48) as usize != CHUNK
            || u32_at(&head, 52) as usize != SECTIONS.len()
            || head[56..FIELDS] != [0; 8]
        {
            return Err(ReadError::Head);
        }

        let mut table = [(0u64, 0u64, [0u8; 16]); 4];
        let mut expect = HEAD as u64;
        for (i, slot) in table.iter_mut().enumerate() {
            let at = FIELDS + i * TABLE_ENTRY;
            let (offset, len) = (u64_at(&head, at), u64_at(&head, at + 8));
            let end = offset.checked_add(len).ok_or(ReadError::Layout)?;
            if offset != expect || end > size {
                return Err(ReadError::Layout);
            }
            slot.0 = offset;
            slot.1 = len;
            slot.2.copy_from_slice(&head[at + 16..at + 32]);
            expect = end;
        }
        if expect != size {
            return Err(ReadError::Layout);
        }
        let read_whole = |section: Section| -> Result<Vec<u8>, ReadError> {
            let (offset, len, sum) = table[section as usize];
            let mut bytes = vec![0; usize::try_from(len).map_err(|_| ReadError::Layout)?];
            source.read_exact_at(&mut bytes, offset)?;
            if checksum(&bytes) != sum {
                return Err(ReadError::Corrupt(section));
            }
            Ok(bytes)
        };
        let index = read_whole(Section::Index)?;
        let sums = read_whole(Section::Sums)?;
        let (blocks_len, postings_len) = (table[0].1, table[1].1);
        let blocks_sums = chunks(blocks_len) as usize * 16;
        if sums.len() as u64 != (chunks(blocks_len) + chunks(postings_len)) * 16 {
            return Err(ReadError::Corrupt(Section::Sums));
        }
        if checksum(&sums[..blocks_sums]) != table[0].2 {
            return Err(ReadError::Corrupt(Section::Blocks));
        }
        if checksum(&sums[blocks_sums..]) != table[1].2 {
            return Err(ReadError::Corrupt(Section::Postings));
        }

        let (firsts, index) = parse_index(&index, info.terms, blocks_len, postings_len)
            .ok_or(ReadError::Corrupt(Section::Index))?;
        Ok(Self {
            source,
            info,
            blocks_at: (table[0].0, blocks_len),
            postings_at: (table[1].0, postings_len),
            sums,
            firsts,
            index,
        })
    }

    pub fn info(&self) -> Info {
        self.info
    }

    /// Bytes resident for this segment: the block index and chunk sums.
    pub fn resident_bytes(&self) -> usize {
        self.firsts.len() + self.index.len() * size_of::<Block>() + self.sums.len()
    }

    /// `term`'s documents, or `None` when the segment does not hold it.
    pub fn lookup(&self, term: &[u8]) -> Result<Option<Hit>, ReadError> {
        let found = self
            .index
            .partition_point(|b| &self.firsts[b.term_start..b.term_end] <= term);
        let Some(block) = found.checked_sub(1) else {
            return Ok(None);
        };
        let mut entries = self.block(block)?;
        while let Some((candidate, entry)) = entries.next_entry()? {
            match candidate.cmp(term) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => return self.read(&entry).map(Some),
                std::cmp::Ordering::Greater => break,
            }
        }
        Ok(None)
    }

    /// Every term in order, with its entry.
    pub fn terms(&self) -> Terms<'_, R> {
        Terms {
            segment: self,
            next_block: 0,
            entries: None,
        }
    }

    /// The documents an entry from [`Segment::terms`] points at.
    pub fn read(&self, entry: &TermEntry) -> Result<Hit, ReadError> {
        match entry.location {
            Location::Single(doc) => Ok(Hit::Single(doc)),
            Location::List { offset, len } => {
                let (bytes, start) = self.read_checked(Section::Postings, offset, offset + len)?;
                Ok(Hit::Postings(Postings {
                    first: self.info.first,
                    df: entry.df,
                    bytes,
                    start,
                }))
            }
        }
    }

    /// Decodes block `b`'s entries lazily.
    fn block(&self, b: usize) -> Result<Entries, ReadError> {
        let here = self.index[b];
        let next = self.index.get(b + 1);
        let end = next.map_or(self.blocks_at.1, |n| n.start);
        let postings_end = next.map_or(self.postings_at.1, |n| n.postings);
        let (mut bytes, at) = self.read_checked(Section::Blocks, here.start, end)?;
        bytes.truncate(at + (end - here.start) as usize);
        let count = if next.is_some() {
            BLOCK
        } else {
            (self.info.terms - (b * BLOCK) as u64) as usize
        };
        Ok(Entries {
            bytes,
            at,
            term: self.firsts[here.term_start..here.term_end].to_vec(),
            remaining: count,
            seen: 0,
            postings: here.postings,
            postings_end,
            first: self.info.first,
            span: u64::from(self.info.last - self.info.first),
        })
    }

    /// Reads `[start, end)` of a chunked section, verifying every chunk it
    /// touches. Returns the chunks and where `start` is in them.
    fn read_checked(
        &self,
        section: Section,
        start: u64,
        end: u64,
    ) -> Result<(Vec<u8>, usize), ReadError> {
        let ((offset, len), sums) = match section {
            Section::Blocks => (self.blocks_at, &self.sums[..]),
            _ => (
                self.postings_at,
                &self.sums[chunks(self.blocks_at.1) as usize * 16..],
            ),
        };
        if start > end || end > len {
            return Err(ReadError::Corrupt(section));
        }
        let chunk = CHUNK as u64;
        let (low, high) = (start / chunk, end.div_ceil(chunk));
        let from = low * chunk;
        let to = (high * chunk).min(len);
        let mut bytes = vec![0; (to.max(from) - from) as usize];
        self.source.read_exact_at(&mut bytes, offset + from)?;
        for (i, piece) in bytes.chunks(CHUNK).enumerate() {
            let at = (low as usize + i) * 16;
            if checksum(piece) != sums[at..at + 16] {
                return Err(ReadError::Corrupt(section));
            }
        }
        Ok((bytes, (start - from) as usize))
    }
}

/// Parses the block index; `None` when it is inconsistent with the head or
/// the section lengths.
fn parse_index(
    bytes: &[u8],
    terms: u64,
    blocks_len: u64,
    postings_len: u64,
) -> Option<(Vec<u8>, Vec<Block>)> {
    let count = terms.div_ceil(BLOCK as u64);
    // Each block's index entry is at least three bytes.
    if count > bytes.len() as u64 {
        return None;
    }
    let mut firsts = Vec::new();
    let mut index = Vec::with_capacity(count as usize);
    let (mut at, mut start, mut postings) = (0, 0u64, 0u64);
    for b in 0..count as usize {
        let len = usize::try_from(take(bytes, &mut at)?).ok()?;
        let term = bytes.get(at..at.checked_add(len)?)?;
        at += len;
        let (step, postings_step) = (take(bytes, &mut at)?, take(bytes, &mut at)?);
        if b == 0 && (step, postings_step) != (0, 0) || b > 0 && step == 0 {
            return None;
        }
        if let Some(previous) = index.last() {
            let previous: &Block = previous;
            if term <= &firsts[previous.term_start..previous.term_end] {
                return None;
            }
        }
        start = start.checked_add(step)?;
        postings = postings.checked_add(postings_step)?;
        if start >= blocks_len || postings > postings_len {
            return None;
        }
        let term_start = firsts.len();
        firsts.extend_from_slice(term);
        index.push(Block {
            term_start,
            term_end: firsts.len(),
            start,
            postings,
        });
    }
    (at == bytes.len() && (count > 0 || blocks_len == 0 && postings_len == 0))
        .then_some((firsts, index))
}

/// Entries of one dictionary block, decoded in order.
struct Entries {
    /// The block's bytes end exactly where the block does.
    bytes: Vec<u8>,
    at: usize,
    /// The current term; the block's first before the first entry.
    term: Vec<u8>,
    remaining: usize,
    seen: usize,
    /// Offset of the next list in the postings section.
    postings: u64,
    postings_end: u64,
    first: u32,
    /// `last - first`.
    span: u64,
}

impl Entries {
    fn next_entry(&mut self) -> Result<Option<(&[u8], TermEntry)>, ReadError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let entry = self.entry().ok_or(ReadError::Corrupt(Section::Blocks))?;
        Ok(Some((&self.term, entry)))
    }

    /// Decodes one entry, leaving its term in `self.term`. `None` when the
    /// bytes are inconsistent.
    fn entry(&mut self) -> Option<TermEntry> {
        let bytes = &self.bytes;
        if self.seen > 0 {
            let shared = usize::try_from(take(bytes, &mut self.at)?).ok()?;
            let suffix = usize::try_from(take(bytes, &mut self.at)?).ok()?;
            let tail = bytes.get(self.at..self.at.checked_add(suffix)?)?;
            // Strictly after the previous term: a non-empty suffix that
            // extends it, or whose first byte sorts after the one it
            // replaces.
            let increasing = tail.first().is_some_and(|&byte| {
                shared == self.term.len() || shared < self.term.len() && byte > self.term[shared]
            });
            if !increasing {
                return None;
            }
            self.term.truncate(shared);
            self.term.extend_from_slice(tail);
            self.at += suffix;
        }
        let df = take(bytes, &mut self.at)?;
        if df == 0 || df > self.span + 1 {
            return None;
        }
        let location = if df == 1 {
            let doc = take(bytes, &mut self.at)?;
            if doc > self.span {
                return None;
            }
            Location::Single(self.first + doc as u32)
        } else {
            let len = take(bytes, &mut self.at)?;
            let end = self.postings.checked_add(len)?;
            if len < pfor128skip::aux_len(df as usize) as u64 || len == 0 || end > self.postings_end
            {
                return None;
            }
            let offset = self.postings;
            self.postings = end;
            Location::List { offset, len }
        };
        self.remaining -= 1;
        self.seen += 1;
        if self.remaining == 0 && (self.postings != self.postings_end || self.at != bytes.len()) {
            return None;
        }
        Some(TermEntry {
            df: df as u32,
            location,
        })
    }
}

/// Every term of a segment, in order.
pub struct Terms<'s, R> {
    segment: &'s Segment<R>,
    next_block: usize,
    entries: Option<Entries>,
}

impl<R: ReadAt> Terms<'_, R> {
    /// The next term and its entry, or `None` after the last.
    pub fn next_term(&mut self) -> Result<Option<(&[u8], TermEntry)>, ReadError> {
        loop {
            let exhausted = self.entries.as_ref().is_none_or(|e| e.remaining == 0);
            if exhausted {
                if self.next_block == self.segment.index.len() {
                    return Ok(None);
                }
                self.entries = Some(self.segment.block(self.next_block)?);
                self.next_block += 1;
                continue;
            }
            let Some(entries) = self.entries.as_mut() else {
                return Ok(None);
            };
            return entries.next_entry();
        }
    }
}
