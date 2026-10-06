//! The index manifest: which segments make up the index, how far it has
//! covered the catalog, and which documents it could not read (docs/S2.md §
//! Manifest and commit, § Coverage). Replaced whole by write, fsync, rename
//! and directory fsync, as the catalog replaces `current`.
//!
//! ```text
//! head        magic "ferretix", format version u32, tokenizer version u32,
//!             catalog incarnation 16 B, high water u32, frontier u32,
//!             sequence u64, next segment number u64, segment count u32,
//!             unreadable count u32                                    64 B
//! segments    per segment, in DocId order, 64 B: number u64, first u32,
//!             last u32, docs u32, 4 B zero, terms u64, pairs u64,
//!             bytes u64, the segment's head digest 16 B
//! unreadable  u32 per document, strictly ascending
//! digest      BLAKE3-128 of every preceding byte
//! ```
//!
//! **Frontier.** Every DocId below `frontier` has been dealt with by some
//! follow pass: it was dead when the pass reached it, or the pass tokenized
//! it into the segment whose range holds it, or it is in `unreadable` (and
//! inside a segment's range too). So the live documents the index has not
//! covered are exactly the live ones at or above the frontier, plus the
//! unreadable set. This is S2.md's "outside every segment range" made
//! cheap: follow only ever appends above the frontier.
//!
//! **High water** is the largest catalog `next_doc` any pass was based on.
//! An index whose high water exceeds the catalog's `next_doc` was built
//! against DocIds the catalog has not published, and is discarded.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use crate::segment::Info;

const MAGIC: [u8; 8] = *b"ferretix";

/// Bumped when this layout changes.
pub const FORMAT_VERSION: u32 = 1;

const HEAD: usize = 64;
const ENTRY: usize = 64;

/// The manifest's file name inside the index directory.
pub const FILE: &str = "manifest";
const TEMP: &str = "manifest.tmp";

/// One segment as the manifest lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentEntry {
    /// Unique within the directory; part of the file name.
    pub number: u64,
    pub first: u32,
    /// Inclusive.
    pub last: u32,
    /// Documents tokenized into it, live when they were: its dead fraction's
    /// denominator.
    pub docs: u32,
    pub terms: u64,
    pub pairs: u64,
    /// The whole file.
    pub bytes: u64,
    /// The segment head's own digest, so a file swapped in under the name
    /// is refused.
    pub digest: [u8; 16],
}

impl SegmentEntry {
    /// `seg-<first>-<last>-<number>.seg` (docs/S2.md § Segment file).
    pub fn file_name(&self) -> String {
        format!("seg-{}-{}-{}.seg", self.first, self.last, self.number)
    }

    /// Whether an opened segment's head is the one this entry names.
    pub(crate) fn matches(
        &self,
        info: &Info,
        digest: &[u8; 16],
        bytes: u64,
        tokenizer: u32,
    ) -> bool {
        (
            info.first,
            info.last,
            info.terms,
            info.pairs,
            info.tokenizer_version,
        ) == (self.first, self.last, self.terms, self.pairs, tokenizer)
            && *digest == self.digest
            && bytes == self.bytes
    }
}

/// One published state of the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub tokenizer_version: u32,
    /// The catalog incarnation the index belongs to.
    pub incarnation: [u8; 16],
    pub high_water: u32,
    pub frontier: u32,
    /// Increased by every publication.
    pub sequence: u64,
    pub next_number: u64,
    /// In DocId order; ranges never overlap.
    pub segments: Vec<SegmentEntry>,
    /// Live documents a follow pass could not read with their catalog key
    /// intact, ascending. Never retried; pruned when the catalog drops them.
    pub unreadable: Vec<u32>,
}

impl Manifest {
    /// An index that covers nothing, for `incarnation`.
    pub fn empty(incarnation: [u8; 16]) -> Self {
        Self {
            tokenizer_version: ferret_text::TOKENIZER_VERSION,
            incarnation,
            high_water: 0,
            frontier: 0,
            sequence: 0,
            next_number: 0,
            segments: Vec::new(),
            unreadable: Vec::new(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(HEAD + self.segments.len() * ENTRY + self.unreadable.len() * 4 + 16);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&self.tokenizer_version.to_le_bytes());
        out.extend_from_slice(&self.incarnation);
        out.extend_from_slice(&self.high_water.to_le_bytes());
        out.extend_from_slice(&self.frontier.to_le_bytes());
        out.extend_from_slice(&self.sequence.to_le_bytes());
        out.extend_from_slice(&self.next_number.to_le_bytes());
        out.extend_from_slice(&(self.segments.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.unreadable.len() as u32).to_le_bytes());
        debug_assert_eq!(out.len(), HEAD);
        for s in &self.segments {
            out.extend_from_slice(&s.number.to_le_bytes());
            out.extend_from_slice(&s.first.to_le_bytes());
            out.extend_from_slice(&s.last.to_le_bytes());
            out.extend_from_slice(&s.docs.to_le_bytes());
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&s.terms.to_le_bytes());
            out.extend_from_slice(&s.pairs.to_le_bytes());
            out.extend_from_slice(&s.bytes.to_le_bytes());
            out.extend_from_slice(&s.digest);
        }
        for doc in &self.unreadable {
            out.extend_from_slice(&doc.to_le_bytes());
        }
        let digest = checksum(&out);
        out.extend_from_slice(&digest);
        out
    }

    /// `None` for anything that is not a well-formed manifest of this
    /// format: the caller discards the index and rebuilds it through
    /// coverage, since every byte of it is derived from the catalog.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let body = bytes.len().checked_sub(16)?;
        if bytes.len() < HEAD + 16
            || bytes[..8] != MAGIC
            || checksum(&bytes[..body]) != bytes[body..]
            || u32_at(bytes, 8) != FORMAT_VERSION
        {
            return None;
        }
        let mut incarnation = [0; 16];
        incarnation.copy_from_slice(&bytes[16..32]);
        let (count, unreadable) = (u32_at(bytes, 56) as usize, u32_at(bytes, 60) as usize);
        if body != HEAD + count.checked_mul(ENTRY)? + unreadable.checked_mul(4)? {
            return None;
        }
        let mut manifest = Self {
            tokenizer_version: u32_at(bytes, 12),
            incarnation,
            high_water: u32_at(bytes, 32),
            frontier: u32_at(bytes, 36),
            sequence: u64_at(bytes, 40),
            next_number: u64_at(bytes, 48),
            segments: Vec::with_capacity(count),
            unreadable: Vec::with_capacity(unreadable),
        };
        for i in 0..count {
            let at = HEAD + i * ENTRY;
            let mut digest = [0; 16];
            digest.copy_from_slice(&bytes[at + 48..at + 64]);
            if bytes[at + 20..at + 24] != [0; 4] {
                return None;
            }
            manifest.segments.push(SegmentEntry {
                number: u64_at(bytes, at),
                first: u32_at(bytes, at + 8),
                last: u32_at(bytes, at + 12),
                docs: u32_at(bytes, at + 16),
                terms: u64_at(bytes, at + 24),
                pairs: u64_at(bytes, at + 32),
                bytes: u64_at(bytes, at + 40),
                digest,
            });
        }
        let at = HEAD + count * ENTRY;
        manifest
            .unreadable
            .extend((0..unreadable).map(|i| u32_at(bytes, at + i * 4)));
        manifest.is_consistent().then_some(manifest)
    }

    /// Ranges ascending and disjoint and below the frontier, numbers below
    /// `next_number`, unreadable ascending and below the frontier, and the
    /// frontier at most the high water.
    fn is_consistent(&self) -> bool {
        let ranges =
            self.segments.iter().all(|s| {
                s.first <= s.last && s.last < self.frontier && s.number < self.next_number
            }) && self.segments.windows(2).all(|p| p[0].last < p[1].first);
        let unreadable = self.unreadable.windows(2).all(|p| p[0] < p[1])
            && self.unreadable.last().is_none_or(|&d| d < self.frontier);
        ranges && unreadable && self.frontier <= self.high_water
    }

    /// Reads `dir`'s manifest: `Ok(None)` when there is none or it does not
    /// decode.
    pub fn read(dir: &Path) -> io::Result<Option<Self>> {
        match fs::read(dir.join(FILE)) {
            Ok(bytes) => Ok(Self::decode(&bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Replaces `dir`'s manifest: write a temporary, fsync it, rename it
    /// over the old one, fsync the directory.
    pub(crate) fn publish(&self, dir: &Path) -> io::Result<()> {
        let temp = dir.join(TEMP);
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp)?;
        file.write_all(&self.encode())?;
        file.sync_all()?;
        fs::rename(&temp, dir.join(FILE))?;
        crate::store::hit(crate::store::Point::ManifestRenamed)?;
        File::open(dir)?.sync_all()
    }
}

fn checksum(bytes: &[u8]) -> [u8; 16] {
    let mut digest = [0; 16];
    digest.copy_from_slice(&blake3::hash(bytes).as_bytes()[..16]);
    digest
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            tokenizer_version: 1,
            incarnation: [7; 16],
            high_water: 100,
            frontier: 90,
            sequence: 4,
            next_number: 3,
            segments: vec![
                SegmentEntry {
                    number: 0,
                    first: 0,
                    last: 40,
                    docs: 30,
                    terms: 500,
                    pairs: 900,
                    bytes: 8000,
                    digest: [1; 16],
                },
                SegmentEntry {
                    number: 2,
                    first: 45,
                    last: 89,
                    docs: 40,
                    terms: 600,
                    pairs: 1000,
                    bytes: 9000,
                    digest: [2; 16],
                },
            ],
            unreadable: vec![3, 50],
        }
    }

    #[test]
    fn a_manifest_round_trips_and_every_flipped_or_cut_byte_is_refused() {
        let manifest = sample();
        let bytes = manifest.encode();
        assert_eq!(Manifest::decode(&bytes), Some(manifest));
        for at in 0..bytes.len() {
            let mut flipped = bytes.clone();
            flipped[at] ^= 0x10;
            assert_eq!(Manifest::decode(&flipped), None, "flip at {at}");
            assert_eq!(Manifest::decode(&bytes[..at]), None, "cut at {at}");
        }
    }

    #[test]
    fn inconsistent_manifests_are_refused_even_with_a_good_digest() {
        let cases: [fn(&mut Manifest); 5] = [
            |m| m.segments[1].first = 40,   // overlapping ranges
            |m| m.segments[1].last = 90,    // a range reaching the frontier
            |m| m.unreadable = vec![50, 3], // unsorted unreadable set
            |m| m.frontier = 101,           // frontier above the high water
            |m| m.segments[1].number = 3,   // a number not yet issued
        ];
        for (i, break_it) in cases.iter().enumerate() {
            let mut manifest = sample();
            break_it(&mut manifest);
            assert_eq!(Manifest::decode(&manifest.encode()), None, "case {i}");
        }
    }
}
