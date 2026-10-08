//! S3 M6: the D66 heuristic chunk census (docs/S3.md § Experiment: D66 C,
//! reduced to a census). Every figure here is a **heuristic estimate**, not
//! a bound: it assumes a lazy reader would visit only the anchor's marked
//! chunks, while the real matcher ([`ferret_verify::TextMatcher`]) always
//! runs a whole-document İ/Kelvin check first and can find a substring
//! anchor inside an unrelated token. The result can **start** a D66 C design
//! task; it can never **close** the experiment (S3.md).
//!
//! `chunk-census <catalog-dir> <index-dir>` regenerates M4c's 50 phrase
//! queries through the same code path
//! ([`content_query::pick_phrases`]), not a copy of its output, and for
//! each one:
//!
//! 1. **Map selectivity.** Tokenizes each 1 MiB+ candidate document once with
//!    the real tokenizer, marks every chunk each token overlaps at 1 MiB, 256
//!    KiB and 64 KiB ([`chunk_span`]), and counts the chunks holding the
//!    query's anchor term plus one neighbour each side
//!    ([`widen_to_neighbours`]).
//! 2. **Forced whole reads.** A candidate whose current stat differs from the
//!    catalogued version, or that fails the real İ/Kelvin check
//!    ([`ferret_verify::forces_whole`]), counts once at its whole size instead
//!    of being chunk-mapped.
//! 3. **Opportunity.** Per query, (1) + (2) is reported against the real bytes
//!    [`content_query::once`] measures reading for that query (M4c's code
//!    path), median and p90.
//! 4. **Map size.** Separately, over every live document of 1 MiB or more (not
//!    only the 50 queries' candidates), counts distinct (term, document, chunk)
//!    triples at each chunk size, from the same whole-document token spans.
//!
//! Nothing here prints a term, a path or document bytes: only classes,
//! counts, byte sizes and the decision-rule verdict.

use std::collections::BTreeSet;
use std::ops::{ControlFlow, Range};
use std::path::Path;

use ferret_catalog::{NameId, Target};
use ferret_index::{CatalogView, DocSet, Pinned, View};
use ferret_query::{Content, DocNames, NameIndex, Query, ReadVersion};
use ferret_text::{Scratch, cap, tokenize};
use ferret_verify::forces_whole;

use crate::content_query::{self, quantile};

/// D66 C only targets documents at or above this size (S3.md).
const LARGE: u64 = 1 << 20;

/// (label, chunk size in bytes), in the order S3.md's spec lists them.
const CHUNK_SIZES: [(&str, usize); 3] = [
    ("1 MiB", 1 << 20),
    ("256 KiB", 256 * 1024),
    ("64 KiB", 64 * 1024),
];

/// M2c's measured merged content index size (S2 M4c done-note), the
/// baseline the map-bytes estimate is compared against for the decision
/// rule's "at most 10% more index bytes".
const M4C_INDEX_BYTES: u64 = 392_012_728;

/// The decision rule (S3.md § Experiment): cut p90 bytes read by at least
/// this fraction...
const CUT_THRESHOLD: f64 = 0.5;
/// ...for at most this much more index bytes.
const MAP_GROWTH_THRESHOLD: f64 = 0.10;

pub(crate) fn run(dir: &Path, index_dir: &Path) -> crate::Result<()> {
    let catalog = crate::open_catalog(dir)?;
    catalog.load_all()?;
    let catalog = catalog.into_resident()?;
    let live = DocSet::new(catalog.next_doc().0, catalog.docs().map(|(doc, _)| doc.0));
    let view = View::open(
        index_dir,
        &CatalogView {
            incarnation: catalog.generation().incarnation,
            live: &live,
        },
    )?
    .ok_or("no content index that fits the catalog")?;
    let names = NameIndex::new(&catalog);
    let docs = DocNames::new(&catalog)?;
    let pinned = Pinned::new(Some(&view), &live);
    let content = Content {
        pinned: &pinned,
        docs: &docs,
        bound: None,
    };

    println!("heuristic estimate: every figure below is a census, not a bound (S3.md)");
    println!("live_documents: {}", live.len());

    let phrases = content_query::pick_phrases(&catalog, &live)?;

    let mut actual_bytes: Vec<f64> = Vec::new();
    let mut actual_large_bytes: Vec<f64> = Vec::new();
    let mut estimate: [Vec<f64>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut estimate_large: [Vec<f64>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut forced_whole_docs = 0u64;
    let mut scratch = Scratch::default();
    let (mut path, mut bytes) = (Vec::new(), Vec::new());

    for phrase in &phrases {
        let space = phrase
            .iter()
            .position(|&b| b == b' ')
            .ok_or("phrase missing separator")?;
        let (a, b) = (&phrase[..space], &phrase[space + 1..]);
        // The longer unit, as the real matcher's anchor is its longest
        // needle (ferret-verify Needles::new).
        let anchor: &[u8] = if a.len() >= b.len() { a } else { b };

        let args = [content_query::arg("text:", phrase)];
        let query = Query::from_args(
            args.iter().map(String::as_bytes),
            std::time::SystemTime::now(),
        )?;

        // Discovery pass: every candidate the verifier would read, without
        // reading a byte (content_query::run_regex's eviction pattern).
        let mut candidates: Vec<NameId> = Vec::new();
        let mut seen = BTreeSet::new();
        query.run_content(
            &catalog,
            &names,
            &content,
            &mut |name, request| {
                if matches!(request, ferret_query::ReadRequest::Bytes(_))
                    && let Target::Inode(inode) = catalog.name(name).target()
                    && let Some(doc) = catalog.doc(inode)
                    && seen.insert(doc.0)
                {
                    candidates.push(name);
                }
                None
            },
            None,
            |_| ControlFlow::Continue(()),
        )?;

        let real = content_query::once(&catalog, &names, &content, &query, None)?;
        actual_bytes.push(real.read_bytes as f64);

        let mut query_estimate = [0u64; 3];
        let mut query_estimate_large = [0u64; 3];
        let mut query_actual_large = 0u64;
        for name in candidates {
            let Target::Inode(_) = catalog.name(name).target() else {
                continue;
            };
            let Some(version) =
                content_query::read_current_name(&catalog, name, &mut path, &mut bytes)
            else {
                continue;
            };
            let doc_len = bytes.len();
            if doc_len < LARGE as usize {
                // Small documents are still read whole in every chunk-map
                // design, so include their unchanged cost in each projection.
                for slot in &mut query_estimate {
                    *slot += doc_len as u64;
                }
                continue;
            }
            query_actual_large += doc_len as u64;
            if version != ReadVersion::Catalogued || forces_whole(&bytes) {
                forced_whole_docs += 1;
                for slot in &mut query_estimate_large {
                    *slot += doc_len as u64;
                }
                continue;
            }
            for (slot, &(_, chunk_size)) in CHUNK_SIZES.iter().enumerate() {
                let mut hits = BTreeSet::new();
                tokenize(&bytes, &mut scratch, |token| {
                    if token.bytes == anchor {
                        let (start, end) = chunk_span(&token.run, chunk_size);
                        hits.insert(start);
                        hits.insert(end);
                    }
                });
                let marked = widen_to_neighbours(&hits, doc_len, chunk_size);
                query_estimate_large[slot] += marked
                    .iter()
                    .map(|&c| chunk_bytes(c, chunk_size, doc_len))
                    .sum::<u64>();
            }
        }
        actual_large_bytes.push(query_actual_large as f64);
        for (slot, &large_value) in query_estimate_large.iter().enumerate() {
            // The "all candidates" projection must cover the same reads as
            // `actual_bytes` (real.read_bytes, which counts every candidate):
            // the unchanged small-document bytes already staged above, plus
            // this chunk size's large-document estimate.
            query_estimate[slot] += large_value;
            estimate[slot].push(query_estimate[slot] as f64);
            estimate_large[slot].push(large_value as f64);
        }
    }

    println!("forced_whole_reads (changed stat or ikelvin, large candidates): {forced_whole_docs}");
    println!();
    let mut actual_p50 = actual_bytes.clone();
    let actual_p50 = quantile(&mut actual_p50, 0.5);
    let mut actual_p90 = actual_bytes.clone();
    let actual_p90 = quantile(&mut actual_p90, 0.9);
    let mut actual_large_p50 = actual_large_bytes.clone();
    let actual_large_p50 = quantile(&mut actual_large_p50, 0.5);
    let mut actual_large_p90 = actual_large_bytes.clone();
    let actual_large_p90 = quantile(&mut actual_large_p90, 0.9);
    println!(
        "heuristic estimate, all candidate documents | chunk size | projected p50 B | projected p90 B | actual p50 B | actual p90 B | p90 cut % |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- |");
    let mut cuts = [0.0; 3];
    for (slot, &(label, _)) in CHUNK_SIZES.iter().enumerate() {
        let mut p50 = estimate[slot].clone();
        let p50 = quantile(&mut p50, 0.5);
        let mut p90 = estimate[slot].clone();
        let p90 = quantile(&mut p90, 0.9);
        let cut = if actual_p90 > 0.0 {
            (1.0 - p90 / actual_p90) * 100.0
        } else {
            0.0
        };
        cuts[slot] = cut;
        println!(
            "| heuristic estimate | {label} | {p50:.0} | {p90:.0} | {actual_p50:.0} | {actual_p90:.0} | {cut:.1} |"
        );
    }
    println!();
    println!(
        "heuristic estimate, large documents only (>= 1 MiB) | chunk size | projected p50 B | projected p90 B | actual p50 B | actual p90 B | p90 cut % |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- |");
    for (slot, &(label, _)) in CHUNK_SIZES.iter().enumerate() {
        let mut p50 = estimate_large[slot].clone();
        let p50 = quantile(&mut p50, 0.5);
        let mut p90 = estimate_large[slot].clone();
        let p90 = quantile(&mut p90, 0.9);
        let cut = if actual_large_p90 > 0.0 {
            (1.0 - p90 / actual_large_p90) * 100.0
        } else {
            0.0
        };
        println!(
            "| heuristic estimate | {label} | {p50:.0} | {p90:.0} | {actual_large_p50:.0} | {actual_large_p90:.0} | {cut:.1} |"
        );
    }

    // Step 4: the map's real size, over every live document of 1 MiB or
    // more (not only the 50 queries' candidates): a single pass.
    let mut triples = [0u64; 3];
    let mut large_docs = 0u64;
    let doc_list = content_query::live_doc_names(&catalog, &live);
    let mut doc_scratch = Scratch::default();
    let (mut dpath, mut dbytes) = (Vec::new(), Vec::new());
    for (_doc, name) in doc_list {
        let Target::Inode(inode) = catalog.name(name).target() else {
            continue;
        };
        if catalog.inode(inode).stat.size < LARGE {
            continue;
        }
        if !content_query::read_name(&catalog, name, &mut dpath, &mut dbytes) {
            continue;
        }
        large_docs += 1;
        let mut sets: [BTreeSet<(Vec<u8>, usize)>; 3] = Default::default();
        tokenize(&dbytes, &mut doc_scratch, |token| {
            for (slot, &(_, chunk_size)) in CHUNK_SIZES.iter().enumerate() {
                insert_term_chunks(&mut sets[slot], token.bytes, &token.run, chunk_size);
            }
        });
        for (slot, set) in sets.iter().enumerate() {
            triples[slot] += set.len() as u64;
        }
    }
    println!();
    println!("map_size_large_documents: {large_docs}");
    println!(
        "heuristic estimate | chunk size | (term,doc,chunk) triples | map bytes (1 B/triple, heuristic) | % of M4c's merged index ({M4C_INDEX_BYTES} B) |"
    );
    println!("| --- | --- | --- | --- |");
    let mut map_pct = [0.0; 3];
    for (slot, &(label, _)) in CHUNK_SIZES.iter().enumerate() {
        let map_bytes = triples[slot];
        let pct = map_bytes as f64 / M4C_INDEX_BYTES as f64 * 100.0;
        map_pct[slot] = pct;
        println!(
            "| heuristic estimate | {label} | {} | {map_bytes} | {pct:.1} |",
            triples[slot]
        );
    }

    println!();
    let mut started = Vec::new();
    for (slot, &(label, _)) in CHUNK_SIZES.iter().enumerate() {
        let passes =
            cuts[slot] >= CUT_THRESHOLD * 100.0 && map_pct[slot] <= MAP_GROWTH_THRESHOLD * 100.0;
        println!(
            "decision_rule chunk_size={label} cut_pct={:.1} map_growth_pct={:.1} passes={passes}",
            cuts[slot], map_pct[slot]
        );
        if passes {
            started.push(label);
        }
    }
    println!();
    if started.is_empty() {
        println!(
            "verdict: D66 C parked, not closed (no chunk size cuts p90 bytes read by >= 50% for <= 10% more index bytes)"
        );
    } else {
        println!(
            "verdict: D66 C can start a design task at: {}",
            started.join(", ")
        );
    }
    Ok(())
}

/// The inclusive chunk range `run` overlaps at `chunk_size`: a token
/// straddling a boundary marks both chunks, and a token ending exactly at
/// one does not spill into the next (no token is created or lost at a
/// boundary).
fn chunk_span(run: &Range<usize>, chunk_size: usize) -> (usize, usize) {
    let start = run.start / chunk_size;
    let end = if run.end == 0 {
        0
    } else {
        (run.end - 1) / chunk_size
    };
    (start, end)
}

/// Insert the stored term key at every chunk overlapped by this token's span.
/// Long tokens may share a capped key, but retain their own independent spans.
fn insert_term_chunks(
    set: &mut BTreeSet<(Vec<u8>, usize)>,
    token: &[u8],
    run: &Range<usize>,
    chunk_size: usize,
) {
    let key = cap(token).to_vec();
    let (start, end) = chunk_span(run, chunk_size);
    for chunk in start..=end {
        set.insert((key.clone(), chunk));
    }
}

/// The last chunk index of a document of `doc_len` bytes at `chunk_size`.
fn last_chunk(doc_len: usize, chunk_size: usize) -> usize {
    if doc_len == 0 {
        0
    } else {
        (doc_len - 1) / chunk_size
    }
}

/// `hits`, each widened by one neighbouring chunk on either side, clamped
/// so expansion stops at the document's ends.
fn widen_to_neighbours(
    hits: &BTreeSet<usize>,
    doc_len: usize,
    chunk_size: usize,
) -> BTreeSet<usize> {
    let last = last_chunk(doc_len, chunk_size);
    let mut out = BTreeSet::new();
    for &c in hits {
        let lo = c.saturating_sub(1);
        let hi = (c + 1).min(last);
        for n in lo..=hi {
            out.insert(n);
        }
    }
    out
}

/// The byte width of chunk `c` at `chunk_size`, clamped to the document's
/// end.
fn chunk_bytes(c: usize, chunk_size: usize, doc_len: usize) -> u64 {
    let start = c * chunk_size;
    let end = ((c + 1) * chunk_size).min(doc_len);
    end.saturating_sub(start) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn straddling_token_marks_both_chunks() {
        assert_eq!(chunk_span(&(90..110), 100), (0, 1));
    }

    #[test]
    fn no_token_created_or_lost_at_a_boundary() {
        // Ends exactly at the boundary: stays in chunk 0, does not spill
        // into chunk 1.
        assert_eq!(chunk_span(&(0..100), 100), (0, 0));
        // Starts exactly at the boundary: chunk 1 only.
        assert_eq!(chunk_span(&(100..200), 100), (1, 1));
    }

    #[test]
    fn neighbour_expansion_stops_at_document_ends() {
        let mut hits = BTreeSet::new();
        hits.insert(0);
        // doc_len 50 at chunk_size 100: one chunk total, no chunk -1.
        assert_eq!(widen_to_neighbours(&hits, 50, 100), BTreeSet::from([0]));

        let mut hits = BTreeSet::new();
        hits.insert(5);
        // doc_len 600 at chunk_size 100: last chunk is 5, no chunk 6.
        assert_eq!(widen_to_neighbours(&hits, 600, 100), BTreeSet::from([4, 5]));
    }

    #[test]
    fn chunk_bytes_clamped_at_document_end() {
        assert_eq!(chunk_bytes(0, 100, 50), 50);
        assert_eq!(chunk_bytes(0, 100, 250), 100);
        assert_eq!(chunk_bytes(2, 100, 250), 50);
    }

    #[test]
    fn shared_capped_prefix_deduplicates_keys_within_a_chunk() {
        // Two distinct tokens (65 bytes each, differing only in their last
        // byte) whose first 64 bytes are identical. `cap` truncates each to
        // that shared 64-byte prefix, so keying on `cap(token)` must collapse
        // them to one triple in the chunk both tokens fall in. Keying on the
        // raw, uncapped bytes instead (the bug fix 5 corrects) would keep
        // them as two triples, so this discriminates the two implementations.
        let mut bytes = vec![b'a'; 64];
        bytes.push(b'x');
        bytes.push(b' ');
        bytes.extend(vec![b'a'; 64]);
        bytes.push(b'y');

        let mut scratch = Scratch::default();
        let mut triples = BTreeSet::new();
        tokenize(&bytes, &mut scratch, |token| {
            // A chunk size larger than the whole buffer keeps both tokens in
            // chunk 0, isolating key deduplication from span placement.
            insert_term_chunks(&mut triples, token.bytes, &token.run, 1000);
        });

        assert_eq!(triples.len(), 1);
        assert!(triples.contains(&(vec![b'a'; 64], 0)));
    }

    #[test]
    fn distinct_capped_tokens_keep_separate_chunks() {
        // Sanity check alongside the dedup test: tokens landing in different
        // chunks are never merged, regardless of their capped key.
        let mut bytes = vec![b'a'; 64];
        bytes.push(b'x');
        bytes.resize(100, b' ');
        bytes.extend(vec![b'a'; 64]);
        bytes.push(b'y');

        let mut scratch = Scratch::default();
        let mut triples = BTreeSet::new();
        tokenize(&bytes, &mut scratch, |token| {
            insert_term_chunks(&mut triples, token.bytes, &token.run, 100);
        });

        assert_eq!(triples.len(), 2);
        assert!(triples.contains(&(vec![b'a'; 64], 0)));
        assert!(triples.contains(&(vec![b'a'; 64], 1)));
    }
}
