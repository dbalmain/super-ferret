//! `ferret-bench terms <catalog-dir>`: a full-tree term census over every
//! live `Hashed` document's real file content, for M2's dictionary-format
//! decision (docs/S2.md § M1 "Measured", § Term dictionary).
//!
//! `tokenize` sampled 36.75 MB and gave 1.19e-2 distinct terms per content
//! byte; at that rate the dictionary would dominate the index. Distinct
//! terms grow sublinearly with text (Heaps' law), so the sample's rate
//! overstates the full tree. This command counts the real thing: every
//! `Hashed` document's file, read whole and tokenized, over the 4.03 GB
//! tree, without holding a token per occurrence or a set per term.
//!
//! A term's document frequency is counted from "last DocId seen", the same
//! trick `tokenize.rs` uses for postings pairs: because [`hashed_documents`]
//! walks DocIds in increasing order, a new occurrence is a new document only
//! when it differs from the term's last.
//!
//! Three subsets repeat every count: `jsonl` documents, documents with no
//! extension, and everything else — the three the dictionary-size policy
//! question (§ M1 "Measured") turns on.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::Instant;

use ferret_catalog::{Catalog, DocId};
use ferret_text::{Scratch, cap, tokenize};

use crate::census::{HashedDoc, hashed_documents};
use crate::tokenize::is_hash_like;

/// One distinct term's running state while streaming.
#[derive(Default)]
struct Term {
    occurrences: u64,
    doc_freq: u32,
    /// `None` until the term's first occurrence.
    last_doc: Option<DocId>,
}

/// The document-frequency buckets, in order: df = 1, 2–3, 4–15, 16–127,
/// 128–1023, and ≥ 1024.
const DF_BUCKETS: usize = 6;

fn df_bucket(df: u32) -> usize {
    match df {
        1 => 0,
        2..=3 => 1,
        4..=15 => 2,
        16..=127 => 3,
        128..=1023 => 4,
        _ => 5,
    }
}

/// A group's finished counts: the whole tree, or one of the three subsets.
#[derive(Debug, Default, PartialEq)]
struct Counts {
    documents: u64,
    occurrences: u64,
    distinct: u64,
    hapax: u64,
    /// Distinct (term, doc) pairs: the postings count.
    pairs: u64,
    df: [u64; DF_BUCKETS],
    /// Sum of distinct term lengths in bytes.
    distinct_bytes: u64,
    hash_like: u64,
}

/// One term map per group, plus its document count.
#[derive(Default)]
struct Group {
    documents: u64,
    terms: HashMap<Vec<u8>, Term>,
}

impl Group {
    fn record(&mut self, key: &[u8], doc: DocId) {
        let term = self.terms.entry(key.to_vec()).or_default();
        term.occurrences += 1;
        if term.last_doc != Some(doc) {
            term.doc_freq += 1;
            term.last_doc = Some(doc);
        }
    }

    fn finish(&self) -> Counts {
        let mut counts = Counts {
            documents: self.documents,
            ..Counts::default()
        };
        for (bytes, term) in &self.terms {
            counts.occurrences += term.occurrences;
            counts.distinct += 1;
            counts.hapax += u64::from(term.occurrences == 1);
            counts.pairs += u64::from(term.doc_freq);
            counts.distinct_bytes += bytes.len() as u64;
            counts.hash_like += u64::from(is_hash_like(bytes));
            counts.df[df_bucket(term.doc_freq)] += 1;
        }
        counts
    }
}

/// Which of the three reported subsets a document's extension falls in.
#[derive(Clone, Copy)]
enum Subset {
    Jsonl,
    NoExtension,
    Other,
}

fn subset(extension: &[u8]) -> Subset {
    match extension {
        b"jsonl" => Subset::Jsonl,
        b"" => Subset::NoExtension,
        _ => Subset::Other,
    }
}

/// Streams every live `Hashed` document's file through the tokenizer,
/// counting into `total` and the one matching subset group. Skips and counts
/// a document whose file is unreadable or whose size no longer matches the
/// catalog's record.
fn count(catalog: &Catalog, docs: &[HashedDoc]) -> (Group, Group, Group, Group, u64) {
    let mut total = Group::default();
    let mut jsonl = Group::default();
    let mut no_extension = Group::default();
    let mut other = Group::default();
    let mut skipped = 0u64;
    let mut scratch = Scratch::default();
    let mut path = Vec::new();
    for doc in docs {
        path.clear();
        catalog.path(doc.name, &mut path);
        let bytes = match fs::read(Path::new(std::ffi::OsStr::from_bytes(&path))) {
            Ok(bytes) if bytes.len() as u64 == doc.size => bytes,
            _ => {
                skipped += 1;
                continue;
            }
        };
        let group = match subset(&doc.extension) {
            Subset::Jsonl => &mut jsonl,
            Subset::NoExtension => &mut no_extension,
            Subset::Other => &mut other,
        };
        total.documents += 1;
        group.documents += 1;
        tokenize(&bytes, &mut scratch, |token| {
            let key = cap(token.bytes);
            total.record(key, doc.doc);
            group.record(key, doc.doc);
        });
    }
    (total, jsonl, no_extension, other, skipped)
}

/// Appends `prefix`'s counts as `key: value` lines.
fn block(out: &mut String, prefix: &str, counts: &Counts) {
    let _ = writeln!(out, "{prefix}documents: {}", counts.documents);
    let _ = writeln!(out, "{prefix}occurrences: {}", counts.occurrences);
    let _ = writeln!(out, "{prefix}distinct_terms: {}", counts.distinct);
    let _ = writeln!(out, "{prefix}hapax_terms: {}", counts.hapax);
    let _ = writeln!(out, "{prefix}df_1: {}", counts.df[0]);
    let _ = writeln!(out, "{prefix}df_2_3: {}", counts.df[1]);
    let _ = writeln!(out, "{prefix}df_4_15: {}", counts.df[2]);
    let _ = writeln!(out, "{prefix}df_16_127: {}", counts.df[3]);
    let _ = writeln!(out, "{prefix}df_128_1023: {}", counts.df[4]);
    let _ = writeln!(out, "{prefix}df_ge_1024: {}", counts.df[5]);
    let _ = writeln!(out, "{prefix}pairs: {}", counts.pairs);
    let _ = writeln!(
        out,
        "{prefix}distinct_term_bytes: {}",
        counts.distinct_bytes
    );
    let _ = writeln!(out, "{prefix}hash_like_distinct: {}", counts.hash_like);
    let hash_like_share = if counts.distinct == 0 {
        "-".to_string()
    } else {
        format!("{:.4e}", counts.hash_like as f64 / counts.distinct as f64)
    };
    let _ = writeln!(out, "{prefix}hash_like_share: {hash_like_share}");
}

fn report(
    skipped: u64,
    total: &Counts,
    jsonl: &Counts,
    no_extension: &Counts,
    other: &Counts,
    rss_kib: &str,
    wall_ms: f64,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "skipped_unreadable_or_changed: {skipped}");
    block(&mut out, "", total);
    out.push('\n');
    block(&mut out, "jsonl_", jsonl);
    out.push('\n');
    block(&mut out, "no_extension_", no_extension);
    out.push('\n');
    block(&mut out, "other_", other);
    out.push('\n');
    let _ = writeln!(out, "rss_kib: {rss_kib}");
    let _ = writeln!(out, "wall_ms: {wall_ms:.1}");
    out
}

/// `ferret-bench terms <catalog-dir>`.
pub(crate) fn run(dir: &Path) -> crate::Result<()> {
    let start = Instant::now();
    let catalog = crate::open_catalog(dir)?;
    let docs = hashed_documents(&catalog)?;
    let (total, jsonl, no_extension, other, skipped) = count(&catalog, &docs);
    let wall_ms = start.elapsed().as_secs_f64() * 1e3;
    let status = fs::read_to_string("/proc/self/status")?;
    let rss_kib = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .ok_or("no VmHWM")?
        .trim()
        .to_string();
    print!(
        "{}",
        report(
            skipped,
            &total.finish(),
            &jsonl.finish(),
            &no_extension.finish(),
            &other.finish(),
            &rss_kib,
            wall_ms,
        )
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::Fixture;

    #[test]
    fn counts_df_hapax_pairs_and_subsets_by_hand() {
        let fixture = Fixture::new("terms");
        let catalog = fixture.commit(
            &[
                // jsonl subset: "fn" twice, one document.
                ("a.jsonl", b"fn fn\n"),
                // everything-else subset: "fn" and "main" once each.
                ("b.txt", b"fn main\n"),
                // no-extension subset: "main" once.
                ("c", b"main\n"),
                // Will be mutated below so its catalogued size is stale.
                ("d.bin", b"ignored"),
            ],
            &[],
        );
        let docs = hashed_documents(&catalog).unwrap();
        assert_eq!(docs.len(), 4);

        // Find d.bin's path through the catalog and grow it past the size
        // the catalog recorded, simulating a tree that moved on.
        let mut path = Vec::new();
        let stale = docs.iter().find(|doc| doc.extension == b"bin").unwrap();
        catalog.path(stale.name, &mut path);
        fs::write(
            Path::new(std::ffi::OsStr::from_bytes(&path)),
            b"ignored, now longer",
        )
        .unwrap();

        let (total, jsonl, no_extension, other, skipped) = count(&catalog, &docs);
        assert_eq!(skipped, 1, "d.bin's size no longer matches the catalog");

        let total = total.finish();
        assert_eq!(total.documents, 3);
        assert_eq!(total.occurrences, 5, "fn,fn,fn,main,main");
        assert_eq!(total.distinct, 2);
        assert_eq!(
            total.hapax, 0,
            "fn:2 docs, main:2 docs, neither occurs once"
        );
        assert_eq!(total.pairs, 4, "fn df=2, main df=2");
        assert_eq!(total.df, [0, 2, 0, 0, 0, 0]);
        assert_eq!(total.distinct_bytes, 2 + 4, "fn + main");
        assert_eq!(total.hash_like, 0);

        let jsonl = jsonl.finish();
        assert_eq!(
            jsonl,
            Counts {
                documents: 1,
                occurrences: 2,
                distinct: 1,
                hapax: 0,
                pairs: 1,
                df: [1, 0, 0, 0, 0, 0],
                distinct_bytes: 2,
                hash_like: 0,
            }
        );

        let no_extension = no_extension.finish();
        assert_eq!(
            no_extension,
            Counts {
                documents: 1,
                occurrences: 1,
                distinct: 1,
                hapax: 1,
                pairs: 1,
                df: [1, 0, 0, 0, 0, 0],
                distinct_bytes: 4,
                hash_like: 0,
            }
        );

        let other = other.finish();
        assert_eq!(
            other,
            Counts {
                documents: 1,
                occurrences: 2,
                distinct: 2,
                hapax: 2,
                pairs: 2,
                df: [2, 0, 0, 0, 0, 0],
                distinct_bytes: 2 + 4,
                hash_like: 0,
            }
        );

        let report = report(skipped, &total, &jsonl, &no_extension, &other, "0 kB", 0.0);
        assert_eq!(
            report,
            "skipped_unreadable_or_changed: 1\n\
             documents: 3\n\
             occurrences: 5\n\
             distinct_terms: 2\n\
             hapax_terms: 0\n\
             df_1: 0\n\
             df_2_3: 2\n\
             df_4_15: 0\n\
             df_16_127: 0\n\
             df_128_1023: 0\n\
             df_ge_1024: 0\n\
             pairs: 4\n\
             distinct_term_bytes: 6\n\
             hash_like_distinct: 0\n\
             hash_like_share: 0.0000e0\n\
             \n\
             jsonl_documents: 1\n\
             jsonl_occurrences: 2\n\
             jsonl_distinct_terms: 1\n\
             jsonl_hapax_terms: 0\n\
             jsonl_df_1: 1\n\
             jsonl_df_2_3: 0\n\
             jsonl_df_4_15: 0\n\
             jsonl_df_16_127: 0\n\
             jsonl_df_128_1023: 0\n\
             jsonl_df_ge_1024: 0\n\
             jsonl_pairs: 1\n\
             jsonl_distinct_term_bytes: 2\n\
             jsonl_hash_like_distinct: 0\n\
             jsonl_hash_like_share: 0.0000e0\n\
             \n\
             no_extension_documents: 1\n\
             no_extension_occurrences: 1\n\
             no_extension_distinct_terms: 1\n\
             no_extension_hapax_terms: 1\n\
             no_extension_df_1: 1\n\
             no_extension_df_2_3: 0\n\
             no_extension_df_4_15: 0\n\
             no_extension_df_16_127: 0\n\
             no_extension_df_128_1023: 0\n\
             no_extension_df_ge_1024: 0\n\
             no_extension_pairs: 1\n\
             no_extension_distinct_term_bytes: 4\n\
             no_extension_hash_like_distinct: 0\n\
             no_extension_hash_like_share: 0.0000e0\n\
             \n\
             other_documents: 1\n\
             other_occurrences: 2\n\
             other_distinct_terms: 2\n\
             other_hapax_terms: 2\n\
             other_df_1: 2\n\
             other_df_2_3: 0\n\
             other_df_4_15: 0\n\
             other_df_16_127: 0\n\
             other_df_128_1023: 0\n\
             other_df_ge_1024: 0\n\
             other_pairs: 2\n\
             other_distinct_term_bytes: 6\n\
             other_hash_like_distinct: 0\n\
             other_hash_like_share: 0.0000e0\n\
             \n\
             rss_kib: 0 kB\n\
             wall_ms: 0.0\n"
        );
    }
}
