//! S2 M4b's measurements (docs/S2.md § M4): `text:` query latency by term
//! class, warm and evicted, and phrase candidate counts and verification
//! time, D7's deciding fact.
//!
//! `content-query <catalog-dir> <index-dir>` opens the catalog and the
//! content index built beside it (`index-build`), and runs [`PER_CLASS`]
//! queries in each class through [`Query::run_content`], the planner
//! `ferret search` runs:
//!
//! - **rare**: a term with df ≤ 100; **mid**: df 1,000–100,000; **common**: df
//!   ≥ 1,000,000, or the largest dfs present when no term reaches it. Terms are
//!   drawn from the index's own dictionary, seeded, and only single-unit terms
//!   (a term query, certainty Yes) qualify.
//! - **phrase**: two adjacent units of a sampled document, as one argument.
//! - **not**: `NOT text:t` for a rare term: every name but a few.
//!
//! Each query runs once after its index files were dropped from the page
//! cache (`posix_fadvise` `DONTNEED`: **evicted**), then again (**warm**).
//! The catalog stays warm, and documents a phrase verifies are read by path
//! with a size check, as `index-build` reads them (`ferret-bench` has no
//! `ferret-crawl` edge). Rows are counted, not printed.
//!
//! Nothing here prints a term or a byte of content: only classes, counts
//! and times.

use std::fs::{self, File};
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

use ferret_catalog::{Catalog, NameId, Target};
use ferret_index::{CatalogView, DocSet, Pinned, View};
use ferret_query::{Content, ContentReport, DocNames, NameIndex, Query, Side};
use ferret_text::{Kind, Scratch, tokenize};
use ferret_verify::Text;

/// Queries per class.
const PER_CLASS: usize = 50;
/// Documents sampled for phrase pairs, per pair wanted.
const PHRASE_TRIES: usize = 20;

/// Seeded xorshift64*.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A reservoir of `PER_CLASS` items drawn uniformly from a stream.
struct Reservoir {
    seen: u64,
    items: Vec<Vec<u8>>,
}

impl Reservoir {
    fn new() -> Self {
        Self {
            seen: 0,
            items: Vec::new(),
        }
    }

    fn offer(&mut self, rng: &mut Rng, item: &[u8]) {
        self.seen += 1;
        if self.items.len() < PER_CLASS {
            self.items.push(item.to_vec());
        } else {
            let at = rng.below(self.seen) as usize;
            if at < PER_CLASS {
                self.items[at] = item.to_vec();
            }
        }
    }
}

/// One figure of a phrase sample.
type Measure = fn(&Sample) -> f64;

/// One query's measurements.
struct Sample {
    evicted: Duration,
    warm: Duration,
    rows: u64,
    report: ContentReport,
    /// Time inside the verification reader, warm run: reads only; the
    /// matcher runs after each read returns.
    read: Duration,
    /// Bytes the verification reader returned.
    read_bytes: u64,
}

/// `only`, when not empty, names the classes to run.
pub(crate) fn run(dir: &Path, index_dir: &Path, only: &[String]) -> crate::Result<()> {
    // Resident, as the engine holds it.
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
    let segments = view.segments().len();
    let start = Instant::now();
    let names = NameIndex::new(&catalog);
    let docs = DocNames::new(&catalog)?;
    let setup = start.elapsed();
    let pinned = Pinned::new(Some(&view), &live);
    println!("live_documents: {}", live.len());
    println!("names: {}", catalog.name_count());
    println!("segments: {segments}");
    println!("index_bytes: {}", view.bytes());
    println!("uncovered: {}", pinned.uncovered().len());
    println!("docnames_bytes: {}", docs.bytes());
    println!("setup_ms (name index + DocNames): {:.1}", ms(setup));

    let classes = pick_terms(&view)?;
    let phrases = pick_phrases(&catalog, &live)?;
    let files: Vec<_> = fs::read_dir(index_dir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    let evict = || -> crate::Result<()> {
        for file in &files {
            let file = File::open(file)?;
            rustix::fs::fadvise(&file, 0, None, rustix::fs::Advice::DontNeed)?;
        }
        Ok(())
    };
    let content = Content {
        pinned: &pinned,
        docs: &docs,
        bound: None,
    };

    println!();
    println!(
        "| class | queries | df/candidates median | warm median ms | warm p95 ms | evicted median ms | evicted p95 ms | rows median | content-driven |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    let mut phrase_samples = Vec::new();
    let mut runs: Vec<(&str, Vec<Vec<String>>)> = Vec::new();
    for (class, terms) in [
        ("rare", &classes.rare),
        ("mid", &classes.mid),
        ("common", &classes.common),
    ] {
        runs.push((class, terms.iter().map(|t| vec![arg("text:", t)]).collect()));
    }
    runs.push((
        "phrase",
        phrases.iter().map(|p| vec![arg("text:", p)]).collect(),
    ));
    runs.push((
        "not",
        classes
            .rare
            .iter()
            .map(|t| vec!["NOT".to_owned(), arg("text:", t)])
            .collect(),
    ));
    for (class, queries) in runs {
        if !only.is_empty() && !only.iter().any(|c| c == class) {
            continue;
        }
        let mut samples = Vec::new();
        for args in &queries {
            let query = Query::from_args(
                args.iter().map(String::as_bytes),
                std::time::SystemTime::now(),
            )?;
            evict()?;
            let evicted = once(&catalog, &names, &content, &query)?.wall;
            let warm = once(&catalog, &names, &content, &query)?;
            samples.push(Sample {
                evicted,
                warm: warm.wall,
                rows: warm.rows,
                report: warm.report,
                read: warm.read,
                read_bytes: warm.read_bytes,
            });
        }
        let mut warm: Vec<f64> = samples.iter().map(|s| ms(s.warm)).collect();
        let mut evicted: Vec<f64> = samples.iter().map(|s| ms(s.evicted)).collect();
        let mut rows: Vec<f64> = samples.iter().map(|s| s.rows as f64).collect();
        let mut estimate: Vec<f64> = samples
            .iter()
            .map(|s| match class {
                "phrase" => s.report.documents as f64,
                _ => s.report.atoms.first().map_or(0.0, |a| a.estimate as f64),
            })
            .collect();
        let driven = samples
            .iter()
            .filter(|s| s.report.driver == Side::Content)
            .count();
        println!(
            "| {class} | {} | {:.0} | {:.2} | {:.2} | {:.2} | {:.2} | {:.0} | {driven} |",
            samples.len(),
            quantile(&mut estimate, 0.5),
            quantile(&mut warm, 0.5),
            quantile(&mut warm, 0.95),
            quantile(&mut evicted, 0.5),
            quantile(&mut evicted, 0.95),
            quantile(&mut rows, 0.5),
        );
        if class == "phrase" {
            phrase_samples = samples;
        }
    }

    println!();
    println!(
        "phrase candidates and verification (warm), over {} queries:",
        phrase_samples.len()
    );
    println!("| measure | min | median | p90 | p95 | max |");
    println!("| --- | --- | --- | --- | --- | --- |");
    let rows: [(&str, Measure); 10] = [
        ("candidates (documents probed)", |s| {
            s.report.documents as f64
        }),
        ("verified (documents read)", |s| s.report.verified as f64),
        ("verification read ms", |s| ms(s.read)),
        ("verification read MiB", |s| s.read_bytes as f64 / 1048576.0),
        ("wall ms", |s| ms(s.warm)),
        ("derived match ms (wall - read)", |s| {
            ms(s.warm.saturating_sub(s.read))
        }),
        ("rejected by byte search (documents)", |s| {
            s.report.matching.rejected as f64
        }),
        ("tokenized whole (documents)", |s| s.report.matching.whole as f64),
        ("MiB tokenized", |s| {
            s.report.matching.tokenized as f64 / 1048576.0
        }),
        ("rows", |s| s.rows as f64),
    ];
    for (name, f) in rows {
        let mut values: Vec<f64> = phrase_samples.iter().map(f).collect();
        println!(
            "| {name} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} |",
            quantile(&mut values, 0.0),
            quantile(&mut values, 0.5),
            quantile(&mut values, 0.9),
            quantile(&mut values, 0.95),
            quantile(&mut values, 1.0),
        );
    }
    let mut verified: Vec<f64> = phrase_samples
        .iter()
        .map(|s| s.report.verified as f64)
        .collect();
    // Verification is the read plus the matcher's tokenize; the planning
    // and row emission around it are what the mid class costs, a few ms.
    let mut verify_ms: Vec<f64> = phrase_samples.iter().map(|s| ms(s.warm)).collect();
    let (p90_docs, p90_ms) = (quantile(&mut verified, 0.9), quantile(&mut verify_ms, 0.9));
    println!(
        "d7_positions_trigger: {} (p90 verified {p90_docs:.0} vs 10000; p90 wall, nearly all verification, {p90_ms:.1} ms vs 100)",
        if p90_docs > 10_000.0 || p90_ms > 100.0 {
            "fires"
        } else {
            "does not fire"
        }
    );
    let changed: u64 = phrase_samples.iter().map(|s| s.report.changed).sum();
    println!("phrase_changed_documents: {changed}");
    Ok(())
}

fn arg(prefix: &str, bytes: &[u8]) -> String {
    format!("{prefix}{}", String::from_utf8_lossy(bytes))
}

/// One run of a query.
struct Once {
    wall: Duration,
    rows: u64,
    report: ContentReport,
    read: Duration,
    read_bytes: u64,
}

/// One run of `query`, rows counted.
fn once(
    catalog: &Catalog,
    names: &NameIndex,
    content: &Content<'_>,
    query: &Query,
) -> crate::Result<Once> {
    let (mut read, mut read_bytes) = (Duration::ZERO, 0u64);
    let mut path = Vec::new();
    let mut reader = |name: NameId, out: &mut Vec<u8>| {
        let start = Instant::now();
        let ok = read_name(catalog, name, &mut path, out);
        read += start.elapsed();
        if ok {
            read_bytes += out.len() as u64;
        }
        ok
    };
    let mut rows = 0u64;
    let start = Instant::now();
    let stats = query.run_content(catalog, names, content, &mut reader, None, |_| {
        rows += 1;
        ControlFlow::Continue(())
    })?;
    let wall = start.elapsed();
    let report = stats
        .content
        .ok_or("a content query reported no content plan")?;
    Ok(Once {
        wall,
        rows,
        report,
        read,
        read_bytes,
    })
}

/// Reads `name`'s file by path, refusing a size the catalog did not record.
fn read_name(catalog: &Catalog, name: NameId, path: &mut Vec<u8>, out: &mut Vec<u8>) -> bool {
    let Target::Inode(inode) = catalog.name(name).target() else {
        return false;
    };
    let size = catalog.inode(inode).stat.size;
    path.clear();
    catalog.path(name, path);
    match fs::read(Path::new(std::ffi::OsStr::from_bytes(path))) {
        Ok(read) if read.len() as u64 == size => {
            *out = read;
            true
        }
        _ => false,
    }
}

struct Classes {
    rare: Vec<Vec<u8>>,
    mid: Vec<Vec<u8>>,
    common: Vec<Vec<u8>>,
}

/// Term-query terms by df class, from the dictionary of a single-segment
/// index (df is per segment, so a multi-segment index would need a merge).
fn pick_terms(view: &View) -> crate::Result<Classes> {
    let [segment] = view.segments() else {
        return Err("content-query wants a fully merged, single-segment index".into());
    };
    let mut rng = Rng(0x5eed_0001);
    let (mut rare, mut mid, mut million) = (Reservoir::new(), Reservoir::new(), Reservoir::new());
    // The largest dfs, in case no term reaches a million documents.
    let mut top: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut terms = segment.terms();
    while let Some((term, entry)) = terms.next_term()? {
        if !term_query(term) {
            continue;
        }
        let df = entry.df;
        match df {
            0..=100 => rare.offer(&mut rng, term),
            1_000..=100_000 => mid.offer(&mut rng, term),
            1_000_000.. => million.offer(&mut rng, term),
            _ => {}
        }
        if top.len() < PER_CLASS || df > top[top.len() - 1].0 {
            top.push((df, term.to_vec()));
            top.sort_unstable_by_key(|&(df, _)| std::cmp::Reverse(df));
            top.truncate(PER_CLASS);
        }
    }
    let common = if million.items.len() >= PER_CLASS {
        million.items
    } else {
        top.into_iter().map(|(_, t)| t).collect()
    };
    println!(
        "term_classes: rare {} of {} eligible, mid {} of {}, common {} (df >= 1M eligible: {})",
        rare.items.len(),
        rare.seen,
        mid.items.len(),
        mid.seen,
        common.len(),
        million.seen
    );
    Ok(Classes {
        rare: rare.items,
        mid: mid.items,
        common,
    })
}

/// Whether `term`, as `text:term`, is a single-unit term query: under the
/// cap's ambiguity, and one run with no parts.
fn term_query(term: &[u8]) -> bool {
    term.len() < 61
        && Text::new(term, false)
            .is_some_and(|text| text.whole() == Some(term) && text.units().len() == 1)
}

/// Phrase arguments: two adjacent units of a randomly chosen live
/// document, each a term query on its own.
fn pick_phrases(catalog: &Catalog, live: &DocSet) -> crate::Result<Vec<Vec<u8>>> {
    let mut rng = Rng(0x5eed_0002);
    let docs: Vec<(u32, NameId)> = {
        let mut first = vec![None; catalog.next_doc().0 as usize];
        for (id, name) in catalog.name_reader().runs_from(NameId(0)) {
            if let Target::Inode(inode) = name.target()
                && catalog.is_live_name(id)
                && let Some(doc) = catalog.doc(inode)
                && live.contains(doc.0)
                && first[doc.0 as usize].is_none()
            {
                first[doc.0 as usize] = Some(id);
            }
        }
        first
            .into_iter()
            .enumerate()
            .filter_map(|(d, n)| n.map(|n| (d as u32, n)))
            .collect()
    };
    let (mut path, mut bytes, mut scratch) = (Vec::new(), Vec::new(), Scratch::default());
    let mut phrases = Vec::new();
    let mut tries = 0;
    while phrases.len() < PER_CLASS && tries < PER_CLASS * PHRASE_TRIES && !docs.is_empty() {
        tries += 1;
        let (_, name) = docs[rng.below(docs.len() as u64) as usize];
        if !read_name(catalog, name, &mut path, &mut bytes) {
            continue;
        }
        // The document's units: per run, its parts, or the whole run.
        let mut units: Vec<Vec<u8>> = Vec::new();
        let (mut run_start, mut parts) = (0, false);
        tokenize(&bytes, &mut scratch, |token| {
            match token.kind {
                Kind::Whole => (run_start, parts) = (units.len(), false),
                Kind::Part if !parts => {
                    units.truncate(run_start);
                    parts = true;
                }
                Kind::Part => {}
            }
            units.push(token.bytes.to_vec());
        });
        if units.len() < 2 {
            continue;
        }
        let at = rng.below(units.len() as u64 - 1) as usize;
        let (a, b) = (&units[at], &units[at + 1]);
        if !term_query(a) || !term_query(b) {
            continue;
        }
        let mut phrase = a.clone();
        phrase.push(b' ');
        phrase.extend_from_slice(b);
        phrases.push(phrase);
    }
    println!(
        "phrase_pairs: {} from {tries} sampled documents",
        phrases.len()
    );
    Ok(phrases)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// The `q` quantile, nearest rank.
fn quantile(values: &mut [f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    let rank = ((values.len() - 1) as f64 * q).round() as usize;
    values[rank]
}
