//! Checkpoint identity and publication framing. `format` embeds this identity;
//! `transaction` publishes it and `read` checks it before exposing epoch ids.

use std::fs::File;
use std::io::{self, Read};

use crate::format::{DecodeError, Layout, NONE, u32_at, u64_at};

/// A pinned view. Sequence alone cannot identify a checkpoint after compaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Generation {
    /// Random namespace assigned when an index is created or explicitly reset.
    pub incarnation: [u8; 16],
    /// Inode/name id epoch, increased by checkpoint publication.
    pub checkpoint: u64,
    /// Logical transaction sequence incorporated in this view.
    pub sequence: u64,
}

/// A reference to an inode or name in one checkpoint's namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Handle<T> {
    /// The namespace that assigned the id.
    pub generation: Generation,
    /// Numeric id; interpreted only after checking the generation.
    pub id: T,
}

/// A stale request must re-resolve its locator in the current view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryFromCurrent {
    /// View the caller should use for re-resolution.
    pub current: Generation,
}

impl Generation {
    pub(crate) fn fresh() -> io::Result<Self> {
        let mut incarnation = [0; 16];
        File::open("/dev/urandom")?.read_exact(&mut incarnation)?;
        if incarnation == [0; 16] {
            return Err(io::Error::other("zero catalog incarnation"));
        }
        Ok(Self {
            incarnation,
            checkpoint: 0,
            sequence: 0,
        })
    }

    pub(crate) fn successor(self) -> Result<Self, DecodeError> {
        let checkpoint = self
            .checkpoint
            .checked_add(1)
            .filter(|&n| n != u64::MAX)
            .ok_or(DecodeError::Corrupt("checkpoint exhausted"))?;
        let sequence = self
            .sequence
            .checked_add(1)
            .filter(|&n| n != u64::MAX)
            .ok_or(DecodeError::Corrupt("sequence exhausted"))?;
        Ok(Self {
            checkpoint,
            sequence,
            ..self
        })
    }

    /// Checks the entire expected view before a request may interpret ids.
    pub fn check(self, expected: Self) -> Result<(), RetryFromCurrent> {
        if self == expected {
            Ok(())
        } else {
            Err(RetryFromCurrent { current: self })
        }
    }
}

pub(crate) const MANIFEST_LEN: usize = 128;
const MAGIC: &[u8; 8] = b"FERRETCR";

pub(crate) fn checksum(bytes: &[u8]) -> [u8; 16] {
    let mut digest = [0; 16];
    digest.copy_from_slice(&blake3::hash(bytes).as_bytes()[..16]);
    digest
}

/// Fixed little-endian `current` manifest.
#[derive(Clone)]
pub(crate) struct Manifest {
    pub(crate) generation: Generation,
    pub(crate) log_end: u64,
    pub(crate) checkpoint_sequence: u64,
    pub(crate) sniffer: u32,
    pub(crate) counters: [u32; 3],
    pub(crate) counts: [u32; 4],
}

impl Manifest {
    pub(crate) fn from_layout(l: &Layout) -> Self {
        Self {
            generation: l.generation,
            log_end: 0,
            checkpoint_sequence: l.generation.sequence,
            sniffer: l.sniffer,
            counters: [l.inodes as u32, l.names as u32, l.next_doc],
            counts: [
                l.inodes as u32,
                l.names as u32,
                l.dirs as u32,
                l.docs as u32,
            ],
        }
    }

    pub(crate) fn encode(&self) -> [u8; MANIFEST_LEN] {
        let mut out = [0; MANIFEST_LEN];
        out[..8].copy_from_slice(MAGIC);
        out[8..12].copy_from_slice(&crate::format::VERSION.to_le_bytes());
        out[16..32].copy_from_slice(&self.generation.incarnation);
        for (at, n) in [
            (32, self.generation.checkpoint),
            (40, self.generation.sequence),
            (48, self.log_end),
            (56, self.checkpoint_sequence),
        ] {
            out[at..at + 8].copy_from_slice(&n.to_le_bytes());
        }
        for (at, n) in [(64, self.sniffer)]
            .into_iter()
            .chain(
                self.counters
                    .into_iter()
                    .enumerate()
                    .map(|(i, n)| (68 + i * 4, n)),
            )
            .chain(
                self.counts
                    .into_iter()
                    .enumerate()
                    .map(|(i, n)| (80 + i * 4, n)),
            )
        {
            out[at..at + 4].copy_from_slice(&n.to_le_bytes());
        }
        let digest = checksum(&out[..112]);
        out[112..].copy_from_slice(&digest);
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() != MANIFEST_LEN
            || &bytes[..8] != MAGIC
            || checksum(&bytes[..112]) != bytes[112..]
        {
            return Err(DecodeError::Corrupt("manifest"));
        }
        if u32_at(bytes, 8) != crate::format::VERSION {
            return Err(DecodeError::Version(u32_at(bytes, 8)));
        }
        let mut incarnation = [0; 16];
        incarnation.copy_from_slice(&bytes[16..32]);
        let generation = Generation {
            incarnation,
            checkpoint: u64_at(bytes, 32),
            sequence: u64_at(bytes, 40),
        };
        let counters = std::array::from_fn(|i| u32_at(bytes, 68 + i * 4));
        let counts = std::array::from_fn(|i| u32_at(bytes, 80 + i * 4));
        if bytes[12..16] != [0; 4]
            || bytes[96..112] != [0; 16]
            || incarnation == [0; 16]
            || generation.checkpoint == u64::MAX
            || generation.sequence == u64::MAX
            || counters[0] > NONE - 16
            || counters[1] == NONE
            || counts[0] > counters[0]
            || counts[1] > counters[1]
            || counts[2] > counts[0]
            || counts[3] > counters[2]
            || u64_at(bytes, 56) > generation.sequence
        {
            return Err(DecodeError::Corrupt("manifest"));
        }
        Ok(Self {
            generation,
            log_end: u64_at(bytes, 48),
            checkpoint_sequence: u64_at(bytes, 56),
            sniffer: u32_at(bytes, 64),
            counters,
            counts,
        })
    }

    pub(crate) fn check(&self, l: &Layout) -> Result<(), DecodeError> {
        let expected = Self::from_layout(l);
        if self.generation.incarnation != l.generation.incarnation
            || self.generation.checkpoint != l.generation.checkpoint
            || self.checkpoint_sequence != l.generation.sequence
            || self.sniffer != l.sniffer
            || self
                .counters
                .iter()
                .zip(expected.counters)
                .any(|(a, b)| *a < b)
            || (self.generation.sequence == self.checkpoint_sequence
                && (self.counters != expected.counters || self.counts != expected.counts))
            || (self.log_end == 0 && self.generation.sequence != self.checkpoint_sequence)
        {
            return Err(DecodeError::Corrupt("checkpoint identity"));
        }
        Ok(())
    }
}
