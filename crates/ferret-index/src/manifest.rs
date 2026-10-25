//! The index manifest: which segments make up the index, how far it has
//! covered the catalog, and which documents it could not read (docs/S2.md §
//! Manifest and commit, § Coverage). Replaced whole by write, fsync, rename
//! and directory fsync, as the catalog replaces `current`.
//!
//! ```text
//! head        magic "ferretix", format version u32, tokenizer version u32,
//!             catalog incarnation 16 B, high water u32, frontier u32,
//!             sequence u64, next segment number u64, segment count u32,
//!             unreadable count u32, last follow u64, last merge u64,
//!             trigram version u32, 4 B zero                         88 B
//! segments    per pair, in DocId order, 104 B: number u64, first u32,
//!             last u32, docs u32, 4 B zero, terms u64, pairs u64,
//!             bytes u64, the term head digest 16 B, trigram bytes u64,
//!             keys u64, pairs u64, trigram head digest 16 B
//! unreadable  u32 per document, strictly ascending
//! digest      BLAKE3-128 of every preceding byte
//! ```
//!
//! The head's tokenizer version is [`ferret_text::TERM_VERSION`], what the
//! term segments hold (the tokenizer less T1's blobs), which each term segment
//! head records too. A manifest recording another value, such as 1 from an
//! index built before T1, is discarded and rebuilt through coverage.
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

use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::regular;
use crate::segment::Info;

const MAGIC: [u8; 8] = *b"ferretix";

/// Bumped when this layout changes.
pub const FORMAT_VERSION: u32 = 3;

const HEAD: usize = 88;
const ENTRY: usize = 104;

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
    pub trigram_bytes: u64,
    pub trigram_keys: u64,
    pub trigram_pairs: u64,
    pub trigram_digest: [u8; 16],
}

impl SegmentEntry {
    /// `seg-<first>-<last>-<number>.seg` (docs/S2.md § Segment file).
    pub fn file_name(&self) -> String {
        format!("seg-{}-{}-{}.seg", self.first, self.last, self.number)
    }

    /// The trigram file of this pair.
    pub fn trigram_file_name(&self) -> String {
        format!("seg-{}-{}-{}.tri", self.first, self.last, self.number)
    }

    /// Combined bytes used by resource accounting and merge selection.
    pub fn total_bytes(&self) -> u64 {
        self.bytes + self.trigram_bytes
    }

    /// Whether the trigram head agrees with its own manifest facts.
    pub fn matches_trigrams(
        &self,
        info: &Info,
        digest: &[u8; 16],
        bytes: u64,
        version: u32,
    ) -> bool {
        (
            info.first,
            info.last,
            info.terms,
            info.pairs,
            info.tokenizer_version,
        ) == (
            self.first,
            self.last,
            self.trigram_keys,
            self.trigram_pairs,
            version,
        ) && *digest == self.trigram_digest
            && bytes == self.trigram_bytes
    }

    /// Whether an opened segment's head is the one this entry names.
    pub fn matches(&self, info: &Info, digest: &[u8; 16], bytes: u64, tokenizer: u32) -> bool {
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
    pub trigram_version: u32,
    /// The catalog incarnation the index belongs to.
    pub incarnation: [u8; 16],
    pub high_water: u32,
    pub frontier: u32,
    /// Increased by every publication.
    pub sequence: u64,
    pub next_number: u64,
    /// Unix seconds of the last committed follow/merge, absent until run.
    pub last_follow: Option<u64>,
    pub last_merge: Option<u64>,
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
            tokenizer_version: ferret_text::TERM_VERSION,
            trigram_version: ferret_text::TRIGRAM_VERSION,
            incarnation,
            high_water: 0,
            frontier: 0,
            sequence: 0,
            next_number: 0,
            last_follow: None,
            last_merge: None,
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
        out.extend_from_slice(&self.last_follow.unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&self.last_merge.unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&self.trigram_version.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
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
            out.extend_from_slice(&s.trigram_bytes.to_le_bytes());
            out.extend_from_slice(&s.trigram_keys.to_le_bytes());
            out.extend_from_slice(&s.trigram_pairs.to_le_bytes());
            out.extend_from_slice(&s.trigram_digest);
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
        // `declared_len` checks the head's fixed fields and is at least
        // HEAD + 16, so equal lengths leave a whole head and digest.
        if declared_len(bytes)? != bytes.len() as u64 {
            return None;
        }
        let body = bytes.len() - 16;
        if checksum(&bytes[..body]) != bytes[body..] {
            return None;
        }
        let mut incarnation = [0; 16];
        incarnation.copy_from_slice(&bytes[16..32]);
        let (count, unreadable) = (u32_at(bytes, 56) as usize, u32_at(bytes, 60) as usize);
        let head = HEAD;
        let mut manifest = Self {
            tokenizer_version: u32_at(bytes, 12),
            trigram_version: u32_at(bytes, 80),
            incarnation,
            high_water: u32_at(bytes, 32),
            frontier: u32_at(bytes, 36),
            sequence: u64_at(bytes, 40),
            next_number: u64_at(bytes, 48),
            last_follow: (head == HEAD)
                .then(|| u64_at(bytes, 64))
                .filter(|&n| n != 0),
            last_merge: (head == HEAD)
                .then(|| u64_at(bytes, 72))
                .filter(|&n| n != 0),
            segments: Vec::with_capacity(count),
            unreadable: Vec::with_capacity(unreadable),
        };
        for i in 0..count {
            let at = head + i * ENTRY;
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
                trigram_bytes: u64_at(bytes, at + 64),
                trigram_keys: u64_at(bytes, at + 72),
                trigram_pairs: u64_at(bytes, at + 80),
                trigram_digest: bytes[at + 88..at + 104].try_into().ok()?,
            });
        }
        let at = head + count * ENTRY;
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
    /// decode. Something other than a regular file at the name does not
    /// decode, and is never waited on (`crate::regular`).
    pub fn read(dir: &Path) -> io::Result<Option<Self>> {
        let file = match regular::open(&dir.join(FILE)) {
            Ok(file) => file,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound || regular::is_not_regular(&error) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let len = file.metadata()?.len();
        Self::read_from(&file, len)
    }

    /// Reads a manifest that was `len` bytes when it was opened: the fixed
    /// head first, then the rest only when the head's own counts declare
    /// exactly `len` bytes, so a malformed file costs one head read and no
    /// allocation of its size. A file that shrank or grew since is
    /// `Ok(None)`, as one that does not decode.
    fn read_from(source: &impl FileExt, len: u64) -> io::Result<Option<Self>> {
        let mut head = [0; HEAD];
        if len < head.len() as u64 || !read_exact(source, &mut head, 0)? {
            return Ok(None);
        }
        if declared_len(&head) != Some(len) {
            return Ok(None);
        }
        let size = usize::try_from(len).map_err(io::Error::other)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e))?;
        bytes.extend_from_slice(&head);
        bytes.resize(size, 0);
        if !read_exact(source, &mut bytes[HEAD..], HEAD as u64)?
            || source.read_at(&mut [0], len)? != 0
        {
            return Ok(None);
        }
        Ok(Self::decode(&bytes))
    }

    /// Replaces `dir`'s manifest: write a temporary, fsync it, rename it
    /// over the old one, fsync the directory.
    pub(crate) fn publish(&self, dir: &Path) -> io::Result<()> {
        let temp = dir.join(TEMP);
        let mut file = regular::replace(&temp)?;
        file.write_all(&self.encode())?;
        file.sync_all()?;
        crate::store::hit(crate::store::Point::ManifestSynced)?;
        fs::rename(&temp, dir.join(FILE))?;
        crate::store::hit(crate::store::Point::ManifestRenamed)?;
        File::open(dir)?.sync_all()
    }
}

/// The whole manifest's length that the head at the start of `bytes`
/// declares; `None` for a short head or a wrong magic, format version or
/// reserved field.
fn declared_len(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < HEAD
        || bytes[..8] != MAGIC
        || u32_at(bytes, 8) != FORMAT_VERSION
        || bytes[84..88] != [0; 4]
    {
        return None;
    }
    let count = u64::from(u32_at(bytes, 56));
    let unreadable = u64::from(u32_at(bytes, 60));
    (HEAD as u64 + 16)
        .checked_add(count.checked_mul(ENTRY as u64)?)?
        .checked_add(unreadable.checked_mul(4)?)
}

/// Fills `buf` from `source` at `offset`; `false` when the file ends first.
fn read_exact(source: &impl FileExt, buf: &mut [u8], offset: u64) -> io::Result<bool> {
    match source.read_exact_at(buf, offset) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
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

    #[test]
    fn legacy_manifests_require_rebuild() {
        for version in [1u32, 2] {
            let mut bytes = sample().encode();
            bytes[8..12].copy_from_slice(&version.to_le_bytes());
            let body = bytes.len() - 16;
            let sum = checksum(&bytes[..body]);
            bytes[body..].copy_from_slice(&sum);
            assert_eq!(Manifest::decode(&bytes), None);
        }
    }

    fn sample() -> Manifest {
        Manifest {
            tokenizer_version: 1,
            trigram_version: 2,
            incarnation: [7; 16],
            high_water: 100,
            frontier: 90,
            sequence: 4,
            next_number: 3,
            last_follow: Some(100),
            last_merge: Some(90),
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
                    trigram_bytes: 100,
                    trigram_keys: 10,
                    trigram_pairs: 20,
                    trigram_digest: [3; 16],
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
                    trigram_bytes: 200,
                    trigram_keys: 20,
                    trigram_pairs: 40,
                    trigram_digest: [4; 16],
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

    // ── reading from a file ──

    /// A scratch directory, removed on drop.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "ferret-index-manifest-{}-{name}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        /// Writes `bytes` as the manifest and opens it.
        fn manifest(&self, bytes: &[u8]) -> Counting {
            fs::write(self.0.join(FILE), bytes).unwrap();
            Counting {
                file: File::open(self.0.join(FILE)).unwrap(),
                read: std::cell::Cell::new(0),
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A file that counts the bytes read through it.
    struct Counting {
        file: File,
        read: std::cell::Cell<u64>,
    }

    impl Counting {
        fn len(&self) -> u64 {
            self.file.metadata().unwrap().len()
        }

        /// What [`Manifest::read`] does with the file, and the bytes it read.
        fn read(&self) -> (Option<Manifest>, u64) {
            self.read_as(self.len())
        }

        /// As `read`, with `len` as the length `fstat` reported.
        fn read_as(&self, len: u64) -> (Option<Manifest>, u64) {
            self.read.set(0);
            let manifest = Manifest::read_from(self, len).unwrap();
            (manifest, self.read.get())
        }
    }

    impl FileExt for Counting {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
            let n = self.file.read_at(buf, offset)?;
            self.read.set(self.read.get() + n as u64);
            Ok(n)
        }

        fn write_at(&self, _: &[u8], _: u64) -> io::Result<usize> {
            unreachable!("a manifest read never writes")
        }
    }

    /// A manifest with `segments` single-document segments and as many
    /// unreadable documents.
    fn large(segments: u32) -> Manifest {
        let entry = sample().segments[0];
        Manifest {
            frontier: segments * 2,
            high_water: segments * 2,
            next_number: u64::from(segments),
            segments: (0..segments)
                .map(|i| SegmentEntry {
                    number: u64::from(i),
                    first: i * 2,
                    last: i * 2 + 1,
                    ..entry
                })
                .collect(),
            unreadable: (0..segments).map(|i| i * 2).collect(),
            ..sample()
        }
    }

    /// Writes the head's counts and reseals the digest, so only the framing
    /// is wrong.
    fn with_counts(mut bytes: Vec<u8>, segments: u32, unreadable: u32) -> Vec<u8> {
        bytes[56..60].copy_from_slice(&segments.to_le_bytes());
        bytes[60..64].copy_from_slice(&unreadable.to_le_bytes());
        let body = bytes.len() - 16;
        let sum = checksum(&bytes[..body]);
        bytes[body..].copy_from_slice(&sum);
        bytes
    }

    #[test]
    fn declared_length_is_the_encoders_length() {
        for manifest in [Manifest::empty([1; 16]), sample(), large(1000)] {
            let bytes = manifest.encode();
            assert_eq!(
                bytes.len() as u64,
                HEAD as u64
                    + manifest.segments.len() as u64 * ENTRY as u64
                    + manifest.unreadable.len() as u64 * 4
                    + 16
            );
            assert_eq!(declared_len(&bytes), Some(bytes.len() as u64));
        }
    }

    #[test]
    fn valid_manifests_round_trip_through_a_file() {
        let dir = Scratch::new("round-trip");
        for manifest in [Manifest::empty([1; 16]), sample(), large(20_000)] {
            let bytes = manifest.encode();
            let file = dir.manifest(&bytes);
            assert_eq!(file.read(), (Some(manifest.clone()), bytes.len() as u64));
            assert_eq!(Manifest::read(&dir.0).unwrap(), Some(manifest));
        }
    }

    /// The reviewer's probe: 32 MiB of zeros used to be read whole before
    /// the magic was checked.
    #[test]
    fn a_large_zero_filled_manifest_is_refused_after_its_head() {
        let dir = Scratch::new("zeros");
        let file = dir.manifest(&vec![0; 32 << 20]);
        let (manifest, read) = file.read();
        assert_eq!(manifest, None);
        assert!(read <= HEAD as u64, "read {read} bytes");
        assert_eq!(Manifest::read(&dir.0).unwrap(), None);
    }

    #[test]
    fn a_bad_head_is_refused_after_reading_only_the_head() {
        let dir = Scratch::new("bad-head");
        let good = large(100).encode();
        let mut padded = good.clone();
        padded.push(0);
        let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
        for (name, at) in [("magic", 0), ("version", 8), ("reserved", 85)] {
            let mut bytes = good.clone();
            bytes[at] ^= 0x40;
            cases.push((name, bytes));
        }
        cases.push((
            "segment count overflow",
            with_counts(good.clone(), u32::MAX, 100),
        ));
        cases.push((
            "unreadable count overflow",
            with_counts(good.clone(), 100, u32::MAX),
        ));
        cases.push(("one segment short", with_counts(good.clone(), 99, 100)));
        cases.push(("one byte long", padded));
        for (name, bytes) in cases {
            let (manifest, read) = dir.manifest(&bytes).read();
            assert_eq!(manifest, None, "{name}");
            assert!(read <= HEAD as u64, "{name}: read {read} bytes");
        }
        for len in [0, 1, HEAD - 1, HEAD, HEAD + 15] {
            let (manifest, read) = dir.manifest(&good[..len]).read();
            assert_eq!(manifest, None, "{len} bytes");
            assert!(read <= HEAD as u64, "{len} bytes: read {read}");
        }
    }

    #[test]
    fn a_manifest_that_changed_length_after_fstat_is_refused() {
        let dir = Scratch::new("changed");
        let bytes = sample().encode();
        let len = bytes.len() as u64;
        let file = dir.manifest(&bytes);
        fs::OpenOptions::new()
            .write(true)
            .open(dir.0.join(FILE))
            .unwrap()
            .set_len(len - 10)
            .unwrap();
        assert_eq!(file.read_as(len).0, None, "truncated");
        let file = dir.manifest(&bytes);
        fs::OpenOptions::new()
            .append(true)
            .open(dir.0.join(FILE))
            .unwrap()
            .write_all(&[0])
            .unwrap();
        assert_eq!(file.read_as(len).0, None, "grown");
    }

    #[test]
    fn a_failed_checksum_is_still_refused() {
        let dir = Scratch::new("checksum");
        let mut bytes = large(100).encode();
        let at = HEAD + 3 * ENTRY + 5;
        bytes[at] ^= 1;
        let (manifest, read) = dir.manifest(&bytes).read();
        assert_eq!(manifest, None);
        assert_eq!(read, bytes.len() as u64);
    }
}
