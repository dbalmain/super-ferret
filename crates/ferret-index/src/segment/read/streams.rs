//! Streams over a segment: one dictionary block's entries, every term in
//! order, and the fingerprint records. Each reads through a [`Window`] of
//! checked chunks.

use super::*;
use crate::segment::take;

/// Entries of one dictionary block, decoded in order.
pub(super) struct Entries {
    /// The block's bytes end exactly where the block does.
    pub(super) bytes: Vec<u8>,
    pub(super) at: usize,
    /// The current term; the block's first before the first entry.
    pub(super) term: Vec<u8>,
    pub(super) remaining: usize,
    pub(super) seen: usize,
    /// Offset of the next list in the postings section.
    pub(super) postings: u64,
    pub(super) postings_end: u64,
    pub(super) first: u32,
    /// `last - first`.
    pub(super) span: u64,
}

impl Entries {
    pub(super) fn next_entry(&mut self) -> Result<Option<(&[u8], TermEntry)>, ReadError> {
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
    pub(super) segment: &'s Segment<R>,
    /// The postings this stream's lists are read through.
    pub(super) window: Window,
    /// The dictionary blocks this stream reads through.
    pub(super) blocks: Window,
    /// The next block to read and its place; `None` after the last.
    pub(super) next: Option<(usize, Place)>,
    pub(super) entries: Option<Entries>,
}

impl<R: ReadAt> Terms<'_, R> {
    /// The next term and its entry, or `None` after the last.
    pub fn next_term(&mut self) -> Result<Option<(&[u8], TermEntry)>, ReadError> {
        loop {
            let exhausted = self.entries.as_ref().is_none_or(|e| e.remaining == 0);
            if exhausted {
                let Some((block, place)) = self.next else {
                    return Ok(None);
                };
                let (entries, next) = self.segment.block(block, place, Some(&mut self.blocks))?;
                self.entries = Some(entries);
                self.next = next.map(|place| (block + 1, place));
                continue;
            }
            let Some(entries) = self.entries.as_mut() else {
                return Ok(None);
            };
            return entries.next_entry();
        }
    }

    /// [`Segment::read`] for an entry of this stream. Lists lie in term
    /// order, so a stream reading each term's list reads the postings front
    /// to back, and checks each chunk once rather than once a list.
    pub fn read(&mut self, entry: &TermEntry) -> Result<Hit, ReadError> {
        match entry.location {
            Location::Single(doc) => Ok(Hit::Single(doc)),
            Location::List { offset, len } => {
                let bytes = self.segment.read_through(
                    Section::Postings,
                    offset,
                    offset + len,
                    &mut self.window,
                )?;
                Ok(Hit::Postings(Postings {
                    first: self.segment.info.first,
                    df: entry.df,
                    bytes: bytes.to_vec(),
                    start: 0,
                }))
            }
        }
    }
}

/// Checked chunks of one section, held for a stream reading it front to
/// back.
#[derive(Debug, Default)]
pub(super) struct Window {
    pub(super) from: u64,
    pub(super) bytes: Vec<u8>,
}

/// A segment's fingerprint records in `(fingerprint, DocId)` order.
/// Holds at most one restart block at a time.
pub struct Records<'a, R> {
    pub(super) segment: &'a Segment<R>,
    pub(super) window: Window,
    pub(super) block: usize,
    pub(super) records: Vec<(u32, u32)>,
    pub(super) at: usize,
}
impl<R: ReadAt> Records<'_, R> {
    /// Advances the stream, checking the next block before decoding it.
    pub fn next_record(&mut self) -> Result<Option<(u32, u32)>, ReadError> {
        if self.at == self.records.len() {
            if self.block == self.segment.fingerprints.firsts.len() {
                return Ok(None);
            }
            self.records.clear();
            self.segment
                .record_block(self.block, Some(&mut self.window), |fp, doc| {
                    self.records.push((fp, doc));
                    true
                })?;
            self.block += 1;
            self.at = 0;
        }
        let record = self.records[self.at];
        self.at += 1;
        Ok(Some(record))
    }
}
