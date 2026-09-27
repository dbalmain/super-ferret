//! Ledger of code whose value was measured against one compiler and must be
//! re-measured when the compiler changes (CLAUDE.md, D11).
//!
//! The AVX2 arm of the name scanner exists only because a benchmark showed
//! it faster than the safe SWAR arm on this compiler. It is correct either
//! way, and `scan`'s tests run every arm the host can execute against a
//! reference; what a newer rustc can change is whether it still pays, since
//! LLVM may vectorise the SWAR loop better or schedule the AVX2 loop worse.
//! [`toolchain_moved_recheck_the_ledger`] fails when the compiling rustc is
//! not [`RECHECKED_WITH`], so a toolchain bump cannot land without someone
//! reading this list.
//!
//! # How to recheck
//!
//! Build `ferret-bench` in release and run its `scan` row on the `$HOME`
//! catalog, which times every arm and `memchr::memmem` on the same heap. Keep
//! the AVX2 arm if it still beats SWAR by a clear margin; otherwise delete it
//! and its row here. Then set [`RECHECKED_WITH`] to the new version.
//!
//! ```sh
//! cargo build --release -p ferret-bench
//! target/release/ferret-bench scan <catalog-dir>
//! ```
//!
//! # The ledger
//!
//! | item | where | measured effect if removed |
//! |---|---|---|
//! | AVX2 arm of the candidate filter | [`scan`](crate::scan) `mod avx2` | 5–9× slower scans: on the synthetic 10M heap (244 MB, warm) SWAR runs 3.3–3.6 GB/s against AVX2's 16–33 GB/s; `flamegraph` folded goes from 10.3 ms to 68.6 ms (rustc 1.95.0, 2026-09-28) |

/// The rustc the ledger above was last measured with.
pub const RECHECKED_WITH: &str = "1.95.0";

#[cfg(test)]
mod tests {
    use super::RECHECKED_WITH;

    #[test]
    fn toolchain_moved_recheck_the_ledger() {
        let full = env!("FERRET_VERIFY_RUSTC");
        let version = full.split_whitespace().nth(1).unwrap_or(full);
        assert_eq!(
            version, RECHECKED_WITH,
            "the compiler is now `{full}` and the AVX2 scanner arm in \
             src/toolchain.rs was measured with {RECHECKED_WITH}; re-measure \
             the ledger's row and update RECHECKED_WITH"
        );
    }
}
