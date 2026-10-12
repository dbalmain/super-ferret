//! Hashed fixed n-grams (Dave's request): every folded `n`-byte window of a
//! document, hashed to `bits` bits. Postings are per hashed key, one entry
//! per document, written through the real segment writer as the trigram
//! postings are. A collision only adds documents to a key, so a document
//! holding a literal holds every hashed key of the literal's windows: the
//! screen stays sound at any width.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use ferret_index::segment::Writer;

use super::super::grams::fold;
use super::super::latency::{OwnedIndex, Output};
use crate::content_snapshot::Snapshot;
use crate::segment_build::cpu_seconds;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Hashed {
    pub(super) n: usize,
    pub(super) bits: u32,
}

impl Hashed {
    pub(super) const ALL: [Self; 6] = [
        Self { n: 4, bits: 15 },
        Self { n: 4, bits: 16 },
        Self { n: 4, bits: 17 },
        Self { n: 5, bits: 15 },
        Self { n: 5, bits: 16 },
        Self { n: 5, bits: 17 },
    ];

    pub(super) fn label(self) -> String {
        format!("hash{}-{}", self.n, self.bits)
    }

    /// SplitMix64 of the window, high `bits` bits. Folded input, so the
    /// key is the same for every ASCII case spelling.
    pub(super) fn key(self, window: &[u8]) -> u32 {
        let mut x = window
            .iter()
            .fold(self.n as u64, |acc, &b| (acc << 8) | u64::from(b));
        x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^= x >> 31;
        (x >> (64 - self.bits)) as u32
    }

    /// The segment's key bytes: three bytes, big-endian.
    pub(super) fn encode(key: u32) -> Vec<u8> {
        key.to_be_bytes()[1..].to_vec()
    }

    /// The hashed keys of a literal fact's folded windows; empty when the
    /// literal is shorter than `n`.
    pub(super) fn literal_keys(self, literal: &[u8]) -> Vec<u32> {
        let mut keys: Vec<_> = fold(literal)
            .windows(self.n)
            .map(|w| self.key(w))
            .collect();
        keys.sort_unstable();
        keys.dedup();
        keys
    }
}

pub(super) struct Built {
    pub(super) owned: OwnedIndex,
    pub(super) report: String,
}

/// Builds the variant over every readable document and writes its segment
/// to `dir`. Reports sizes, key use and build cost.
pub(super) fn build(
    variant: Hashed,
    snapshot: &Snapshot,
    live: &[u32],
    bound: u32,
    dir: &Path,
) -> crate::Result<Built> {
    let (cpu, wall) = (cpu_seconds()?, Instant::now());
    let keys = 1usize << variant.bits;
    let mut lists: Vec<Vec<u32>> = vec![Vec::new(); keys];
    // The last document each key was pushed for: deduplicates per document
    // without clearing a set between documents.
    let mut stamp = vec![u32::MAX; keys];
    for (ordinal, &doc) in live.iter().enumerate() {
        if ordinal.is_multiple_of(1024) {
            crate::trigram::check_disk()?;
        }
        if !snapshot.contains(doc) {
            continue;
        }
        let bytes = fold(&fs::read(snapshot.path(doc))?);
        for window in bytes.windows(variant.n) {
            let key = variant.key(window) as usize;
            if stamp[key] != doc {
                stamp[key] = doc;
                lists[key].push(doc);
            }
        }
    }
    let invert_cpu = cpu_seconds()? - cpu;
    let (owned, file) = OwnedIndex::create(dir, &variant.label())?;
    let mut writer = Writer::keyed(0, bound.saturating_sub(1), 1)?;
    for (key, docs) in lists.iter().enumerate() {
        if !docs.is_empty() {
            writer.push(&Hashed::encode(key as u32), docs)?;
        }
    }
    let mut out = Output(file);
    let sizes = writer.finish(&mut out)?;
    out.flush()?;
    out.0.sync_all()?;
    let build_cpu = cpu_seconds()? - cpu;
    let mut dfs: Vec<usize> = lists
        .iter()
        .map(Vec::len)
        .filter(|&len| len > 0)
        .collect();
    dfs.sort_unstable();
    let quantile = |q: f64| dfs[((dfs.len() as f64 * q).ceil() as usize).clamp(1, dfs.len()) - 1];
    let report = format!(
        "hashed mode={} n={} bits={} key_space={keys} keys_used={} pairs={} docs_per_key_median={} docs_per_key_p90={} docs_per_key_p99={} docs_per_key_max={} postings_bytes={} singleton_bytes={} dictionary_bytes={} total_bytes={} invert_cpu_s={invert_cpu:.3} build_cpu_s={build_cpu:.3} build_wall_s={:.3}",
        variant.label(),
        variant.n,
        variant.bits,
        dfs.len(),
        sizes.pairs,
        dfs[(dfs.len() - 1) / 2],
        quantile(0.9),
        quantile(0.99),
        dfs[dfs.len() - 1],
        sizes.postings,
        sizes.singleton_bytes,
        sizes.dictionary(),
        sizes.total(),
        wall.elapsed().as_secs_f64(),
    );
    Ok(Built { owned, report })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_fold_ascii_case_and_fit_the_width() {
        for variant in Hashed::ALL {
            let a = variant.literal_keys(b"Deserialize_LEDGER");
            assert_eq!(a, variant.literal_keys(b"deserialize_ledger"));
            assert!(a.iter().all(|&k| k < 1 << variant.bits));
            assert_eq!(Hashed::encode(a[0]).len(), 3);
        }
        assert!(Hashed::ALL[0].literal_keys(b"abc").is_empty());
        assert_eq!(Hashed::ALL[3].literal_keys(b"abcde").len(), 1);
    }

    /// The screen's soundness: a document containing the literal anywhere,
    /// in any ASCII case, holds every key the query asks for.
    #[test]
    fn a_containing_document_holds_every_query_key() {
        let literal = b"ledger_entry";
        for variant in Hashed::ALL {
            for doc in [&b"xxLEDGER_Entryyy"[..], b"ledger_entry", b"a ledger_entryb"] {
                let held: Vec<_> = fold(doc).windows(variant.n).map(|w| variant.key(w)).collect();
                for key in variant.literal_keys(literal) {
                    assert!(held.contains(&key), "{} {key}", variant.label());
                }
            }
        }
    }
}
