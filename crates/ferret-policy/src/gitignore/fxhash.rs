//! A small multiplicative hasher for the matcher's bucket maps.
//!
//! The keys are pattern literals from ignore files and the lookups are file
//! names, so std's SipHash buys resistance to collision attacks the matcher
//! does not need, at several times the cost per lookup: it was the largest
//! hashing cost in a profile of the walk. A colliding name only costs extra
//! comparisons against the few keys in one map, never a wrong answer.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

pub(crate) type FxHashMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

/// The rustc hasher's mixing step, eight bytes at a time.
#[derive(Clone, Copy, Default)]
pub(crate) struct FxHasher {
    hash: u64,
}

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            let mut word = [0; 8];
            word.copy_from_slice(chunk);
            self.add(u64::from_le_bytes(word));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut word = [0; 8];
            word[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(word));
        }
    }

    fn write_u8(&mut self, value: u8) {
        self.add(u64::from(value));
    }

    fn write_u16(&mut self, value: u16) {
        self.add(u64::from(value));
    }

    fn write_usize(&mut self, value: usize) {
        self.add(value as u64);
    }

    fn finish(&self) -> u64 {
        self.hash
    }
}
