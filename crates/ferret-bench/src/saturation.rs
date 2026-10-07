//! S3 M1b, D69's sweep (docs/S3.md § Bench plan): what marking a document
//! "saturated" above a distinct-key threshold would save and cost.
//!
//! `saturation <catalog-dir> <term-segment> <tri-dir> <out-dir>`, for each
//! threshold (strictly more distinct keys than it):
//!
//! - **Documents and pairs above it**, from the `.tri` postings themselves
//!   (each posting is one distinct key of one document).
//! - **Bytes saved**: the `.tri` segments stream-merged with the saturated
//!   documents dropped from the live set, against the files as they are.
//! - **Candidates added** per regex class: the saturated documents the query's
//!   postings candidates do not already hold. Queries are `filter-screen`'s,
//!   from M3's derivation, unnarrowed.
//! - **Verification** of the added documents with the compiled verifier: read
//!   each and run `is_match`. The documents are copied once into `<out-dir>`
//!   and read from the copies, so that evicting them
//!   (`posix_fadvise(DONTNEED)`) touches only files the bench wrote. A copy is
//!   freshly and contiguously laid out, so its evicted read is a lower bound on
//!   the original's.
//!
//! Nothing here prints a term, a key, a path or a byte of content.

use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ferret_index::segment::Writer;
use ferret_index::{DocSet, merge};
use ferret_text::TRIGRAM_VERSION;
use ferret_verify::{Grep, Matcher};

use crate::census::hashed_documents;
use crate::content_query::quantile;
use crate::screen::{
    drain, evict, key_counts, open_segments, read_terms, regex_classes, term_classes,
    trigram_cursor,
};

/// Thresholds and query counts for one sweep.
pub(crate) struct Config {
    pub(crate) thresholds: Vec<u64>,
    pub(crate) per_class: usize,
    /// Queries per class whose added documents are verified.
    pub(crate) verified_per_class: usize,
}

pub(crate) fn config() -> Config {
    Config {
        thresholds: vec![16 << 10, 32 << 10, 64 << 10, 128 << 10, 256 << 10],
        per_class: 20,
        verified_per_class: 5,
    }
}

/// One threshold's figures, for tests and the done-note.
#[derive(Debug, Default)]
pub(crate) struct Level {
    pub(crate) threshold: u64,
    pub(crate) documents: u64,
    pub(crate) content_bytes: u64,
    pub(crate) pairs: u64,
    pub(crate) tri_bytes_after: u64,
    /// Per class: (class, added documents per query).
    pub(crate) added: Vec<(&'static str, Vec<u64>)>,
    /// Per class: the verified queries.
    pub(crate) verified: Vec<(&'static str, Vec<Verified>)>,
}

/// Verifying one query's added documents.
#[derive(Debug, Default)]
pub(crate) struct Verified {
    pub(crate) warm_ms: f64,
    pub(crate) evicted_ms: f64,
    pub(crate) bytes: u64,
    pub(crate) matches: u64,
}

/// One query and its postings candidates, unnarrowed.
struct Query<'a> {
    grep: &'a Grep,
    candidates: Vec<u32>,
}

/// The sweep's result.
#[derive(Debug, Default)]
pub(crate) struct Sweep {
    pub(crate) total_pairs: u64,
    pub(crate) tri_bytes: u64,
    pub(crate) levels: Vec<Level>,
}

fn ms(time: Duration) -> f64 {
    time.as_secs_f64() * 1e3
}

/// Reads and searches `docs`' copies; returns time, bytes and matches.
fn verify(copies: &Path, docs: &[u32], grep: &Grep) -> crate::Result<(Duration, u64, u64)> {
    let start = Instant::now();
    let (mut bytes, mut matches) = (0u64, 0u64);
    for doc in docs {
        let content = fs::read(copies.join(doc.to_string()))?;
        bytes += content.len() as u64;
        matches += u64::from(grep.regex().is_match(&content));
    }
    Ok((start.elapsed(), bytes, matches))
}

pub(crate) fn run(
    dir: &Path,
    term_segment: &Path,
    tri_dir: &Path,
    out: &Path,
    config: &Config,
) -> crate::Result<Sweep> {
    if out.exists() {
        return Err("saturation wants a fresh output directory".into());
    }
    fs::create_dir(out)?;
    let catalog = crate::open_catalog(dir)?;
    let bound = catalog.next_doc().0;
    let docs = hashed_documents(&catalog)?;
    let live = DocSet::new(bound, docs.iter().map(|d| d.doc.0));
    let (tri_paths, trigrams) = open_segments(tri_dir, "tri")?;
    let terms = ferret_index::segment::Segment::open(File::open(term_segment)?)?;
    let counts = key_counts(&trigrams, bound)?;
    let mut sweep = Sweep {
        total_pairs: counts.iter().sum(),
        tri_bytes: tri_paths
            .iter()
            .map(|p| fs::metadata(p).map(|m| m.len()))
            .sum::<Result<_, _>>()?,
        levels: Vec::new(),
    };
    println!(
        "saturation live_documents={} total_pairs={} tri_bytes={}",
        live.len(),
        sweep.total_pairs,
        sweep.tri_bytes
    );

    // Queries and their postings candidates, unnarrowed.
    let classes = term_classes(&terms, config.per_class)?;
    let regexes = regex_classes(&classes, &trigrams, config.per_class)?;
    let mut candidates: Vec<(&'static str, Vec<Query<'_>>)> = Vec::new();
    for (class, greps) in &regexes {
        let mut per = Vec::new();
        for grep in greps {
            let keys = grep.trigrams().keys().into_iter().map(|k| k.to_vec());
            let lists = read_terms(&trigrams, keys)?;
            let docs = drain(ferret_index::Cursor::and(vec![
                trigram_cursor(grep.trigrams(), &lists, &live),
                ferret_index::Cursor::bits(&live, ferret_index::Certainty::Yes),
            ]));
            per.push(Query {
                grep,
                candidates: docs,
            });
        }
        candidates.push((class, per));
    }

    // Copies of the documents above the lowest threshold, read the way
    // `trigram-build` reads them: by catalogued path, size-checked.
    let lowest = config.thresholds.iter().copied().min().unwrap_or(u64::MAX);
    let copies = out.join("docs");
    fs::create_dir(&copies)?;
    let mut copied = Vec::new();
    let mut size = vec![0u64; bound as usize];
    let mut path = Vec::new();
    for doc in &docs {
        if !live.contains(doc.doc.0) || counts[doc.doc.0 as usize] <= lowest {
            continue;
        }
        size[doc.doc.0 as usize] = doc.size;
        if copied.len() % 100 == 0 {
            crate::trigram::check_disk()?;
        }
        path.clear();
        catalog.path(doc.name, &mut path);
        let Ok(bytes) = fs::read(Path::new(std::ffi::OsStr::from_bytes(&path))) else {
            continue;
        };
        if bytes.len() as u64 != doc.size {
            continue;
        }
        fs::write(copies.join(doc.doc.0.to_string()), &bytes)?;
        copied.push(doc.doc.0);
    }
    drop(catalog);
    let copy_paths = |docs: &[u32]| -> Vec<PathBuf> {
        docs.iter().map(|d| copies.join(d.to_string())).collect()
    };
    println!("copied_documents={}", copied.len());

    println!();
    println!(
        "| threshold | documents | content bytes | pairs | share of pairs | .tri bytes after | .tri bytes saved | saved share |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- |");
    let mut added_rows = Vec::new();
    for &threshold in &config.thresholds {
        let saturated: Vec<u32> = live
            .range(0, bound)
            .filter(|&doc| counts[doc as usize] > threshold)
            .collect();
        let mut level = Level {
            threshold,
            documents: saturated.len() as u64,
            content_bytes: saturated.iter().map(|&d| size[d as usize]).sum(),
            pairs: saturated.iter().map(|&d| counts[d as usize]).sum(),
            ..Level::default()
        };
        // Bytes saved: re-merge with the saturated documents dead.
        let kept = DocSet::new(
            bound,
            live.range(0, bound)
                .filter(|&doc| counts[doc as usize] <= threshold),
        );
        let merged = out.join("saturated.tri");
        let scratch = out.join("postings.scratch");
        let first = trigrams[0].info().first;
        let last = trigrams[trigrams.len() - 1].info().last;
        let writer = Writer::keyed_spilling(
            first,
            last,
            TRIGRAM_VERSION,
            File::create(&merged)?,
            File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&scratch)?,
        )?;
        merge::stream(&trigrams, &kept, writer, &|_| Ok(()), &|| false)?;
        level.tri_bytes_after = fs::metadata(&merged)?.len();
        fs::remove_file(&merged)?;
        fs::remove_file(&scratch)?;
        println!(
            "| {threshold} | {} | {} | {} | {:.4} | {} | {} | {:.4} |",
            level.documents,
            level.content_bytes,
            level.pairs,
            level.pairs as f64 / sweep.total_pairs as f64,
            level.tri_bytes_after,
            sweep.tri_bytes - level.tri_bytes_after,
            (sweep.tri_bytes - level.tri_bytes_after) as f64 / sweep.tri_bytes as f64,
        );

        // Added candidates, and verifying them.
        for (class, per) in &candidates {
            let mut added_counts = Vec::new();
            let mut verified = Vec::new();
            for (i, query) in per.iter().enumerate() {
                let added: Vec<u32> = saturated
                    .iter()
                    .copied()
                    .filter(|doc| query.candidates.binary_search(doc).is_err())
                    .collect();
                added_counts.push(added.len() as u64);
                if i < config.verified_per_class {
                    let present: Vec<u32> = added
                        .iter()
                        .copied()
                        .filter(|doc| copied.binary_search(doc).is_ok())
                        .collect();
                    evict(&copy_paths(&present))?;
                    let (cold, _, _) = verify(&copies, &present, query.grep)?;
                    let (warm, bytes, matches) = verify(&copies, &present, query.grep)?;
                    verified.push(Verified {
                        warm_ms: ms(warm),
                        evicted_ms: ms(cold),
                        bytes,
                        matches,
                    });
                }
            }
            level.added.push((class, added_counts));
            level.verified.push((class, verified));
        }
        added_rows.push(level);
    }

    println!();
    println!(
        "| threshold | class | queries | candidates median | added median | added p90 | verified queries | verified MiB median | warm ms median | warm ms max | evicted ms median | evicted ms max | matches median |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    for level in &added_rows {
        for ((class, added), (_, verified)) in level.added.iter().zip(&level.verified) {
            let mut base: Vec<f64> = candidates
                .iter()
                .find(|(c, _)| c == class)
                .map(|(_, per)| per.iter().map(|q| q.candidates.len() as f64).collect())
                .unwrap_or_default();
            let mut added: Vec<f64> = added.iter().map(|&n| n as f64).collect();
            let mut warm: Vec<f64> = verified.iter().map(|v| v.warm_ms).collect();
            let mut cold: Vec<f64> = verified.iter().map(|v| v.evicted_ms).collect();
            let mut mib: Vec<f64> = verified
                .iter()
                .map(|v| v.bytes as f64 / 1048576.0)
                .collect();
            let mut matches: Vec<f64> = verified.iter().map(|v| v.matches as f64).collect();
            println!(
                "| {} | {class} | {} | {:.0} | {:.0} | {:.0} | {} | {:.1} | {:.2} | {:.2} | {:.2} | {:.2} | {:.0} |",
                level.threshold,
                added.len(),
                quantile(&mut base, 0.5),
                quantile(&mut added, 0.5),
                quantile(&mut added, 0.9),
                verified.len(),
                quantile(&mut mib, 0.5),
                quantile(&mut warm, 0.5),
                quantile(&mut warm, 1.0),
                quantile(&mut cold, 0.5),
                quantile(&mut cold, 1.0),
                quantile(&mut matches, 0.5),
            );
        }
    }
    fs::remove_dir_all(&copies)?;
    sweep.levels = added_rows;
    Ok(sweep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::tests::{build, files};
    use crate::support::Fixture;
    use ferret_index::segment::TrigramInverter;

    /// End to end over the fixture, at thresholds that saturate exactly the
    /// long document, and none. The saturated count is checked against the
    /// inverter's own count over the files' bytes.
    #[test]
    fn saturation_runs_end_to_end() {
        let fixture = Fixture::new("saturation");
        let files = files();
        let (term_path, tri_dir) = build(&fixture, &files);
        let counts: Vec<u64> = files
            .iter()
            .map(|(_, bytes)| TrigramInverter::new().add(0, bytes).unwrap() as u64)
            .collect();
        let short = counts[1..].iter().copied().max().unwrap();
        assert!(counts[0] > short * 4, "the long document dominates");
        let config = Config {
            thresholds: vec![short, counts[0]],
            per_class: 5,
            verified_per_class: 2,
        };
        let out = fixture.out("saturation-out");
        let sweep = run(&fixture.catalog_dir(), &term_path, &tri_dir, &out, &config).unwrap();
        assert_eq!(sweep.total_pairs, counts.iter().sum::<u64>());
        let [low, high] = &sweep.levels[..] else {
            panic!("two levels");
        };
        assert_eq!((low.documents, low.pairs), (1, counts[0]));
        assert_eq!(high.documents, 0);
        assert!(low.tri_bytes_after < sweep.tri_bytes);
        assert_eq!(low.content_bytes, files[0].1.len() as u64);
        // A rare literal from a short document never selects the long one,
        // so saturating it adds exactly that one candidate, and verifying
        // it reads its bytes and finds no match.
        let (_, added) = low
            .added
            .iter()
            .find(|(class, _)| *class == "rare literal")
            .unwrap();
        assert!(added.iter().all(|&n| n == 1));
        let (_, verified) = low
            .verified
            .iter()
            .find(|(class, _)| *class == "rare literal")
            .unwrap();
        assert!(
            verified
                .iter()
                .all(|v| v.bytes == files[0].1.len() as u64 && v.matches == 0)
        );
        assert!(high.added.iter().all(|(_, a)| a.iter().all(|&n| n == 0)));
        assert!(!out.join("docs").exists());
    }
}
