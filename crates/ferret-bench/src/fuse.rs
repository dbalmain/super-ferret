//! A binary fuse filter with 8-bit fingerprints and arity 3 (Graf and
//! Lemire, "Binary Fuse Filters: Fast and Smaller Than Xor Filters", 2022),
//! the construction S3.md § Per-document trigram filters sizes. Bench-only:
//! D68 screens per-document filters before any index code exists.
//!
//! The sizing and hashing follow the paper's reference implementation
//! (`xor_singleheader`'s `binary_fuse8`): segment length
//! `2^⌊ln n / ln 3.33 + 2.25⌋` capped at 2^18, size factor
//! `max(1.125, 0.875 + 0.25 ln 10^6 / ln n)`, murmur64's finaliser over
//! `key + seed`, and `h ^ (h >> 32)` as the fingerprint. Construction peels
//! in key order rather than the reference's block-sorted order, which
//! changes speed, not the result's shape.
//!
//! Keys must be distinct: the caller passes a set. Duplicate keys never
//! peel, and construction reports failure after [`ATTEMPTS`] seeds.

/// Seeds tried before construction gives up.
const ATTEMPTS: u32 = 100;

/// The serialized header: seed (8), log2 segment length (1), segment count
/// (4). The array length follows from the last two.
pub(crate) const HEADER: usize = 13;

/// One document's filter: Maybe or No for any key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fuse {
    seed: u64,
    shape: Shape,
    fingerprints: Vec<u8>,
}

/// The geometry a filter's size determines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Shape {
    segment_length_log2: u8,
    segment_count: u32,
}

impl Shape {
    /// The reference implementation's geometry for `size` keys.
    pub(crate) fn for_size(size: usize) -> Self {
        let length = if size == 0 {
            4
        } else {
            let log = ((size as f64).ln() / 3.33f64.ln() + 2.25).floor() as u32;
            (1u32 << log).min(1 << 18)
        };
        let capacity = if size <= 1 {
            0
        } else {
            let factor = (0.875 + 0.25 * 1e6f64.ln() / (size as f64).ln()).max(1.125);
            (size as f64 * factor).round() as u64
        };
        let segments = capacity.div_ceil(u64::from(length));
        let segment_count = if segments <= 2 { 1 } else { segments - 2 };
        Self {
            segment_length_log2: length.trailing_zeros() as u8,
            // A document has far fewer than 2^32 × 4 keys.
            segment_count: u32::try_from(segment_count).unwrap_or(u32::MAX),
        }
    }

    fn segment_length(self) -> u32 {
        1 << self.segment_length_log2
    }

    /// Fingerprint bytes: `(segment_count + 2) × segment_length`.
    pub(crate) fn array_length(self) -> usize {
        (self.segment_count as usize + 2) * self.segment_length() as usize
    }

    /// The three slots `hash` maps to.
    fn slots(self, hash: u64) -> [usize; 3] {
        let length = self.segment_length();
        let mask = u64::from(length - 1);
        let span = u64::from(self.segment_count) * u64::from(length);
        let h0 = ((u128::from(hash) * u128::from(span)) >> 64) as u64;
        let h1 = (h0 + u64::from(length)) ^ ((hash >> 18) & mask);
        let h2 = (h0 + 2 * u64::from(length)) ^ (hash & mask);
        [h0 as usize, h1 as usize, h2 as usize]
    }
}

/// murmur64's finaliser, as the reference hashes `key + seed`.
fn mix(key: u64, seed: u64) -> u64 {
    let mut h = key.wrapping_add(seed);
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

fn fingerprint(hash: u64) -> u8 {
    (hash ^ (hash >> 32)) as u8
}

/// splitmix64, for seeds.
fn next_seed(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl Fuse {
    /// Builds a filter over `keys`, which must be distinct. `None` when no
    /// seed in [`ATTEMPTS`] peels: only duplicates cause that in practice.
    pub(crate) fn build(keys: &[u64], seed_state: u64) -> Option<Self> {
        let shape = Shape::for_size(keys.len());
        if keys.is_empty() {
            // Nothing to hold: no fingerprints, and every probe says No.
            return Some(Self {
                seed: 0,
                shape,
                fingerprints: Vec::new(),
            });
        }
        let length = shape.array_length();
        let mut fingerprints = vec![0u8; length];
        let mut state = seed_state;
        let mut count = vec![0u8; length];
        let mut xor = vec![0u64; length];
        let mut alone = Vec::with_capacity(length);
        let mut order: Vec<(u64, u8)> = Vec::with_capacity(keys.len());
        for _ in 0..ATTEMPTS {
            let seed = next_seed(&mut state);
            count.fill(0);
            xor.fill(0);
            order.clear();
            alone.clear();
            // Each slot counts its keys in the high six bits and xors the
            // key's position (0, 1 or 2) into the low two, as the reference
            // does. A count past 63 wraps below 4 and fails this seed.
            let mut overflow = false;
            for &key in keys {
                let hash = mix(key, seed);
                for (position, slot) in shape.slots(hash).into_iter().enumerate() {
                    count[slot] = count[slot].wrapping_add(4) ^ position as u8;
                    xor[slot] ^= hash;
                    overflow |= count[slot] < 4;
                }
            }
            if overflow {
                continue;
            }
            alone.extend((0..length).filter(|&slot| count[slot] >> 2 == 1));
            while let Some(slot) = alone.pop() {
                if count[slot] >> 2 != 1 {
                    continue;
                }
                let hash = xor[slot];
                let found = count[slot] & 3;
                order.push((hash, found));
                let slots = shape.slots(hash);
                for step in 1..3u8 {
                    let position = (found + step) % 3;
                    let other = slots[position as usize];
                    if count[other] >> 2 == 2 {
                        alone.push(other);
                    }
                    count[other] = count[other].wrapping_sub(4) ^ position;
                    xor[other] ^= hash;
                }
            }
            if order.len() != keys.len() {
                continue;
            }
            fingerprints.fill(0);
            for &(hash, found) in order.iter().rev() {
                let slots = shape.slots(hash);
                let [a, b] = [(found + 1) % 3, (found + 2) % 3].map(|p| slots[p as usize]);
                fingerprints[slots[found as usize]] =
                    fingerprint(hash) ^ fingerprints[a] ^ fingerprints[b];
            }
            return Some(Self {
                seed,
                shape,
                fingerprints,
            });
        }
        None
    }

    /// Whether `key` may be in the set: always for a member, about 1/256 of
    /// the time otherwise. The bench probes serialized filters by [`Header`].
    #[cfg(test)]
    pub(crate) fn contains(&self, key: u64) -> bool {
        if self.is_empty() {
            return false;
        }
        let hash = mix(key, self.seed);
        let [a, b, c] = self.shape.slots(hash);
        fingerprint(hash) ^ self.fingerprints[a] ^ self.fingerprints[b] ^ self.fingerprints[c] == 0
    }

    /// Serialized bytes: [`HEADER`] plus the fingerprints. An empty set
    /// serializes to nothing.
    #[cfg(test)]
    pub(crate) fn serialized_len(&self) -> usize {
        if self.is_empty() {
            0
        } else {
            HEADER + self.fingerprints.len()
        }
    }

    fn is_empty(&self) -> bool {
        self.fingerprints.is_empty()
    }

    /// Appends the serialized filter to `out`.
    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        if self.is_empty() {
            return;
        }
        out.extend_from_slice(&self.seed.to_le_bytes());
        out.push(self.shape.segment_length_log2);
        out.extend_from_slice(&self.shape.segment_count.to_le_bytes());
        out.extend_from_slice(&self.fingerprints);
    }
}

/// A serialized filter's header, read back: what a probe needs before it
/// touches fingerprint bytes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Header {
    seed: u64,
    shape: Shape,
}

impl Header {
    /// Parses [`HEADER`] bytes.
    pub(crate) fn parse(bytes: &[u8; HEADER]) -> Self {
        let mut seed = [0; 8];
        seed.copy_from_slice(&bytes[..8]);
        let mut count = [0; 4];
        count.copy_from_slice(&bytes[9..13]);
        Self {
            seed: u64::from_le_bytes(seed),
            shape: Shape {
                segment_length_log2: bytes[8],
                segment_count: u32::from_le_bytes(count),
            },
        }
    }

    /// The fingerprint length the header implies.
    #[cfg(test)]
    pub(crate) fn array_length(self) -> usize {
        self.shape.array_length()
    }

    /// The key's fingerprint and its three slots, relative to the
    /// fingerprint array: a probe reads those three bytes.
    pub(crate) fn locate(self, key: u64) -> (u8, [usize; 3]) {
        let hash = mix(key, self.seed);
        (fingerprint(hash), self.shape.slots(hash))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(state: &mut u64, n: usize) -> Vec<u64> {
        let mut keys: Vec<u64> = (0..n).map(|_| next_seed(state)).collect();
        keys.sort_unstable();
        keys.dedup();
        keys
    }

    /// No false negatives at any size, including the tiny filters most
    /// documents get; and the serialized header reproduces every probe.
    #[test]
    fn members_always_match_and_the_header_round_trips() {
        let mut state = 7;
        for n in (0..300).chain([1_000, 4_096, 65_537, 300_000]) {
            let set = keys(&mut state, n);
            let filter = Fuse::build(&set, n as u64).expect("distinct keys peel");
            assert!(set.iter().all(|&key| filter.contains(key)), "size {n}");
            let mut bytes = Vec::new();
            filter.write(&mut bytes);
            assert_eq!(bytes.len(), filter.serialized_len());
            if n == 0 {
                assert!(bytes.is_empty());
                assert!(!filter.contains(next_seed(&mut state)));
                continue;
            }
            let header = Header::parse(bytes[..HEADER].try_into().unwrap());
            let fingerprints = &bytes[HEADER..];
            assert_eq!(header.array_length(), fingerprints.len());
            for &key in set.iter().take(50) {
                let (f, [a, b, c]) = header.locate(key);
                assert_eq!(f ^ fingerprints[a] ^ fingerprints[b] ^ fingerprints[c], 0);
            }
        }
    }

    /// The false-positive rate over a million non-members, against 1/256:
    /// within 30% either way. A filter whose fingerprints were mostly zero,
    /// or whose slots collided, misses this band.
    #[test]
    fn false_positive_rate_is_near_one_in_256() {
        let mut state = 11;
        let mut probes = 0u64;
        let mut hits = 0u64;
        for n in [1_000, 10_000, 100_000, 500_000] {
            let set = keys(&mut state, n);
            let filter = Fuse::build(&set, 3).unwrap();
            let members: std::collections::HashSet<_> = set.iter().copied().collect();
            for _ in 0..250_000 {
                let key = next_seed(&mut state);
                if !members.contains(&key) {
                    probes += 1;
                    hits += u64::from(filter.contains(key));
                }
            }
        }
        let rate = hits as f64 / probes as f64;
        println!("fuse8 false-positive rate {rate:.6} over {probes} probes (1/256 = 0.003906)");
        assert!((0.7 / 256.0..1.3 / 256.0).contains(&rate), "rate {rate}");
    }

    /// Large sets cost about 9 bits a key, as S3.md sizes them.
    #[test]
    fn large_sets_cost_about_nine_bits_a_key() {
        let mut state = 13;
        let set = keys(&mut state, 1_000_000);
        let filter = Fuse::build(&set, 5).unwrap();
        let bits = filter.serialized_len() as f64 * 8.0 / set.len() as f64;
        assert!((8.9..9.5).contains(&bits), "{bits} bits per key");
    }
}
