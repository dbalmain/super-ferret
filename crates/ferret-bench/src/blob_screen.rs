//! T1's open question (docs/TOKENS.md § Screening routed blob `text:`
//! queries): how well the trigram screen of a routed blob `text:` atom holds
//! up.
//!
//! `blob-screen sample <catalog-dir> <index-dir> <snapshot-dir> <out-file>`
//! draws blob arguments from the frozen corpus: every run
//! [`ferret_text::base64_runs`] reports that is itself routed
//! ([`ferret_query::answered_by_trigrams`]), one per document per length band
//! (so a file of ten thousand blobs counts once), [`PER_BAND`] per band by
//! reservoir; and [`WORDS`] df = 1 terms of the fully merged index as a
//! comparison. The file holds the arguments (hex) and their source DocIds;
//! keep it out of the repository.
//!
//! `blob-screen run <catalog-dir> <index-dir> <snapshot-dir> <queries-file>
//! <out-tsv>` runs each sampled argument as `text:ARG` through
//! [`Query::run_content`], once unmeasured and once warm, verifying against the
//! snapshot's bytes. Per query it writes the screen's size, the planner's
//! Maybe bound and whether it crosses the quarter-of-live refusal, candidate
//! documents, documents and bytes verified, rows, warm wall time, and whether
//! the argument's own document is among the rows (the positive control). The
//! TSV holds no argument bytes. Whichever screen the product's
//! [`Text::trigrams`] builds is the one measured.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use ferret_catalog::{NameId, Target};
use ferret_index::{CatalogView, DocSet, Pinned, View};
use ferret_query::{Content, DocNames, NameIndex, Query};
use ferret_text::base64_runs;
use ferret_verify::{Text, Trigrams};

use crate::content_query::{Rng, once, quantile};

/// Blob arguments per length band.
const PER_BAND: usize = 75;
/// df = 1 comparison words.
const WORDS: usize = 50;
/// Longest argument sampled: a pasted blob, not a whole embedded file.
const MAX_LEN: usize = 4096;
/// Length bands, by the run's bytes including padding: (label, first, last).
/// The detector's floor is 20 characters before padding.
pub(crate) const BANDS: [(&str, usize, usize); 4] = [
    ("20-31", 0, 31),
    ("32-63", 32, 63),
    ("64-255", 64, 255),
    ("256-4096", 256, MAX_LEN),
];

pub(crate) fn run(args: &[String]) -> crate::Result<()> {
    match args {
        [mode, dir, index, snapshot, file] if mode == "sample" => sample(
            Path::new(dir),
            Path::new(index),
            Path::new(snapshot),
            Path::new(file),
        ),
        [mode, dir, index, snapshot, queries, out] if mode == "run" => measure(
            Path::new(dir),
            Path::new(index),
            Path::new(snapshot),
            Path::new(queries),
            Path::new(out),
        ),
        _ => Err("blob-screen sample|run: see the module docs".into()),
    }
}

fn band(len: usize) -> Option<usize> {
    BANDS
        .iter()
        .position(|&(_, first, last)| (first..=last).contains(&len))
}

fn routed(arg: &[u8]) -> bool {
    Text::new(arg, false).is_some_and(|text| ferret_query::answered_by_trigrams(&text))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn unhex(s: &str) -> crate::Result<Vec<u8>> {
    (0..s.len())
        .step_by(2)
        .map(|i| Ok(u8::from_str_radix(s.get(i..i + 2).ok_or("odd hex")?, 16)?))
        .collect()
}

fn sample(dir: &Path, index: &Path, snapshot_dir: &Path, file: &Path) -> crate::Result<()> {
    let catalog = crate::open_catalog(dir)?;
    catalog.load_all()?;
    let catalog = catalog.into_resident()?;
    let snapshot = crate::content_snapshot::Snapshot::open(snapshot_dir, &catalog)?;
    let live = DocSet::new(catalog.next_doc().0, catalog.docs().map(|(doc, _)| doc.0));
    let view = View::open(
        index,
        &CatalogView {
            incarnation: catalog.generation().incarnation,
            live: &live,
        },
    )?
    .ok_or("no content index that fits the catalog")?;
    let mut rng = Rng(0x5eed_0b10);
    // Per band: documents offered, and the reservoir of (doc, argument).
    let mut seen = [0u64; BANDS.len()];
    let mut chosen: [Vec<(u32, Vec<u8>)>; BANDS.len()] = Default::default();
    let (mut runs, mut unrouted, mut long, mut docs_with) = (0u64, 0u64, 0u64, 0u64);
    for doc in live.range(0, live.bound()) {
        if !snapshot.contains(doc) {
            continue;
        }
        let bytes = fs::read(snapshot.path(doc))?;
        // This document's pick per band, by reservoir over its runs.
        let mut mine: [(u64, Option<std::ops::Range<usize>>); BANDS.len()] = Default::default();
        base64_runs(&bytes, |run| {
            runs += 1;
            if run.len() > MAX_LEN {
                long += 1;
                return;
            }
            if !routed(&bytes[run.clone()]) {
                unrouted += 1;
                return;
            }
            let Some(b) = band(run.len()) else { return };
            mine[b].0 += 1;
            if rng.below(mine[b].0) == 0 {
                mine[b].1 = Some(run);
            }
        });
        docs_with += u64::from(mine.iter().any(|(n, _)| *n > 0));
        for (b, (_, run)) in mine.into_iter().enumerate() {
            let Some(run) = run else { continue };
            seen[b] += 1;
            let item = (doc, bytes[run].to_vec());
            if chosen[b].len() < PER_BAND {
                chosen[b].push(item);
            } else {
                let at = rng.below(seen[b]) as usize;
                if at < PER_BAND {
                    chosen[b][at] = item;
                }
            }
        }
    }
    let words = crate::content_query::pick_singletons(&view)?;
    let mut out = String::new();
    for (b, items) in chosen.iter().enumerate() {
        println!(
            "band {}: {} sampled of {} documents",
            BANDS[b].0,
            items.len(),
            seen[b]
        );
        for (doc, arg) in items {
            let _ = writeln!(out, "{}\t{doc}\t{}", BANDS[b].0, hex(arg));
        }
    }
    let words: Vec<_> = words
        .into_iter()
        .filter(|w| !routed(w))
        .take(WORDS)
        .collect();
    println!("df1 words: {}", words.len());
    for word in words {
        let _ = writeln!(out, "df1\t-\t{}", hex(&word));
    }
    println!(
        "runs: {runs}; over {MAX_LEN} bytes: {long}; not routed on their own: {unrouted}; documents with a sampled-length routed run: {docs_with}"
    );
    fs::write(file, out)?;
    Ok(())
}

/// One sampled query: its band, the DocId it came from, the argument.
type Sampled = (String, Option<u32>, Vec<u8>);

fn read_queries(file: &Path) -> crate::Result<Vec<Sampled>> {
    fs::read_to_string(file)?
        .lines()
        .map(|line| {
            let mut fields = line.split('\t');
            let (Some(band), Some(doc), Some(arg)) = (fields.next(), fields.next(), fields.next())
            else {
                return Err("malformed query line".into());
            };
            let doc = (doc != "-").then(|| doc.parse()).transpose()?;
            Ok((band.to_owned(), doc, unhex(arg)?))
        })
        .collect()
}

fn keys(tree: &Trigrams) -> usize {
    tree.keys().len()
}

fn measure(
    dir: &Path,
    index: &Path,
    snapshot_dir: &Path,
    queries: &Path,
    out: &Path,
) -> crate::Result<()> {
    let catalog = crate::open_catalog(dir)?;
    catalog.load_all()?;
    let catalog = catalog.into_resident()?;
    let snapshot = crate::content_snapshot::Snapshot::open(snapshot_dir, &catalog)?;
    let live = DocSet::new(catalog.next_doc().0, catalog.docs().map(|(doc, _)| doc.0));
    let view = View::open(
        index,
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
    let quarter = u64::from(live.len()) / 4;
    println!(
        "live_documents: {} segments: {} uncovered: {}",
        live.len(),
        view.segments().len(),
        pinned.uncovered().len()
    );
    let mut tsv = String::from(
        "band\tlen\trouted\tscreen_keys\tscreen_all\tmaybe_bound\trefused\tcandidates\tverified\tverified_bytes\trows\twarm_ms\town_doc\n",
    );
    for (band, doc, arg) in read_queries(queries)? {
        crate::trigram::check_disk()?;
        let text = Text::new(&arg, false).ok_or("argument holds no token")?;
        let screen = text.trigrams();
        let mut query_arg = b"text:".to_vec();
        query_arg.extend_from_slice(&arg);
        let query = Query::from_args([query_arg.as_slice()], std::time::SystemTime::now())?;
        once(&catalog, &names, &content, &query, Some(&snapshot))?;
        let warm = once(&catalog, &names, &content, &query, Some(&snapshot))?;
        let own = doc.map(|doc| {
            warm.identities.iter().any(|&name| {
                matches!(catalog.name(NameId(name)).target(),
                    Target::Inode(inode) if catalog.doc(inode).map(|d| d.0) == Some(doc))
            })
        });
        let maybe = warm.report.bounds.maybe;
        eprintln!(
            "{band} {}: {} candidates, {:.0} ms",
            arg.len(),
            warm.report.documents,
            warm.wall.as_secs_f64() * 1e3
        );
        let _ = writeln!(
            tsv,
            "{band}\t{}\t{}\t{}\t{}\t{maybe}\t{}\t{}\t{}\t{}\t{}\t{:.3}\t{}",
            arg.len(),
            u8::from(ferret_query::answered_by_trigrams(&text)),
            keys(&screen),
            u8::from(screen == Trigrams::All),
            u8::from(maybe > quarter),
            warm.report.documents,
            warm.report.verified,
            warm.read_bytes,
            warm.rows,
            warm.wall.as_secs_f64() * 1e3,
            own.map_or("-", |found| if found { "1" } else { "0" }),
        );
    }
    fs::write(out, &tsv)?;
    summarize(&tsv);
    Ok(())
}

/// Per band: medians and p90/p99 of the TSV's figures.
fn summarize(tsv: &str) {
    let rows: Vec<Vec<&str>> = tsv
        .lines()
        .skip(1)
        .map(|l| l.split('\t').collect())
        .collect();
    println!(
        "| band | n | screen all | refused | own doc missing | candidates p50/p90/p99 | verified MiB p50/p90/p99 | rows p50/p90/p99 | warm ms p50/p90/p99 |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    let labels = BANDS.iter().map(|b| b.0).chain(["df1"]);
    for label in labels {
        let band: Vec<_> = rows.iter().filter(|r| r[0] == label).collect();
        let column = |i: usize, scale: f64| -> String {
            let mut v: Vec<f64> = band
                .iter()
                .map(|r| r[i].parse::<f64>().unwrap_or(f64::NAN) / scale)
                .collect();
            format!(
                "{:.1}/{:.1}/{:.1}",
                quantile(&mut v, 0.5),
                quantile(&mut v, 0.9),
                quantile(&mut v, 0.99)
            )
        };
        let count = |i: usize, value: &str| band.iter().filter(|r| r[i] == value).count();
        println!(
            "| {label} | {} | {} | {} | {} | {} | {} | {} | {} |",
            band.len(),
            count(4, "1"),
            count(6, "1"),
            count(12, "0"),
            column(7, 1.0),
            column(9, 1048576.0),
            column(10, 1.0),
            column(11, 1.0),
        );
    }
}
