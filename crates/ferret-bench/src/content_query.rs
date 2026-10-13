//! S2 M4b's measurements (docs/S2.md § M4): `text:` query latency by term
//! class, warm and evicted, and phrase candidate counts and verification
//! time, D7's deciding fact.
//!
//! `content-query <catalog-dir> <index-dir>` opens the catalog and the
//! content index built beside it (`index-build`), and runs [`PER_CLASS`]
//! queries in each class through [`Query::run_content`], the planner
//! `ferret search` runs:
//!
//! - **df1**, **df2-100**, **mid** (df 1,000–100,000), **common** (top 50), and
//!   **absent** (one-character mutations checked absent in the control). Seeded
//!   terms come from a fully merged codec-1 reference (`--terms`), so every
//!   lifecycle state and codec measures the same single-unit queries.
//! - **phrase**: two adjacent units of a sampled document, as one argument.
//! - **not**: `NOT text:t` for a rare term: every name but a few.
//!
//! Each query runs once after its index files were dropped from the page
//! cache (`posix_fadvise` `DONTNEED`: **evicted**), then again (**warm**).
//! The catalog stays warm, and documents a phrase verifies are read by path
//! with a bracketed version check, as `index-build` reads them (`ferret-bench`
//! has no `ferret-crawl` edge). Rows are counted, not printed.
//!
//! `--corpus` verifies against the same frozen DocId bytes both codecs
//! indexed. `--covered` limits the live set to the captured readable DocIds
//! for a diagnostic without the historical catalog's unreadable fallback.
//! `--compare` checks every warm query's actual row identities.
//!
//! Nothing here prints a term or a byte of content: only classes, counts
//! and times.
//!
//! S3 M5's `regex-query <catalog-dir> <index-dir> <snapshot-dir>` uses the
//! same real planner and frozen reader for the five regex classes. Cold
//! runs also evict candidate documents; warm runs reuse the compiled query.
//! The reports split verification and engine time and compare the planner's
//! M bound with its actual snapshot-Maybe count.

use std::fs::{self, File};
use std::io::Read;
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
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
pub(crate) struct Rng(pub(crate) u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub(crate) fn below(&mut self, n: u64) -> u64 {
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
    records: u64,
}

/// `only`, when not empty, names the classes to run.
pub(crate) fn run(dir: &Path, index_dir: &Path, only: &[String]) -> crate::Result<()> {
    let mut classes_only = Vec::new();
    let (mut terms_dir, mut compare_dir, mut corpus_dir) = (None, None, None);
    let mut covered = false;
    let mut options = only.iter();
    while let Some(option) = options.next() {
        match option.as_str() {
            "--covered" => covered = true,
            "--corpus" => corpus_dir = Some(options.next().ok_or("missing corpus snapshot")?),
            "--terms" => terms_dir = Some(options.next().ok_or("missing terms index")?),
            "--compare" => compare_dir = Some(options.next().ok_or("missing comparison index")?),
            "rare" => classes_only.push("df2-100".to_owned()),
            _ => classes_only.push(option.clone()),
        }
    }
    let only = &classes_only;
    // Resident, as the engine holds it.
    let catalog = crate::open_catalog(dir)?;
    catalog.load_all()?;
    let catalog = catalog.into_resident()?;
    let snapshot = corpus_dir
        .map(|dir| crate::content_snapshot::Snapshot::open(Path::new(dir), &catalog))
        .transpose()?;
    if covered && snapshot.is_none() {
        return Err("--covered requires --corpus".into());
    }
    let live = DocSet::new(
        catalog.next_doc().0,
        catalog.docs().map(|(doc, _)| doc.0).filter(|&doc| {
            !covered
                || snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.contains(doc))
        }),
    );
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

    let open = |dir: &Path| -> crate::Result<View> {
        View::open(
            dir,
            &CatalogView {
                incarnation: catalog.generation().incarnation,
                live: &live,
            },
        )?
        .ok_or_else(|| "no fitting comparison index".into())
    };
    let terms_view = terms_dir.map(|dir| open(Path::new(dir))).transpose()?;
    let classes = pick_terms(terms_view.as_ref().unwrap_or(&view))?;
    let compare_view = compare_dir.map(|dir| open(Path::new(dir))).transpose()?;
    let compare_pinned = compare_view
        .as_ref()
        .map(|view| Pinned::new(Some(view), &live));
    let compare = compare_pinned.as_ref().map(|pinned| Content {
        pinned,
        docs: &docs,
        bound: None,
    });
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
        "| class | queries | df/candidates median | warm median ms | warm p90 ms | warm p95 ms | evicted median ms | evicted p90 ms | evicted p95 ms | rows median | content-driven |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    let mut phrase_samples = Vec::new();
    let mut runs: Vec<(&str, Vec<Vec<String>>)> = Vec::new();
    for (class, terms) in [
        ("df1", &classes.singleton),
        ("df2-100", &classes.rare),
        ("absent", &classes.absent),
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
            let evicted = once(&catalog, &names, &content, &query, snapshot.as_ref())?.wall;
            let warm = once(&catalog, &names, &content, &query, snapshot.as_ref())?;
            if let Some(compare) = &compare {
                let control = once(&catalog, &names, compare, &query, snapshot.as_ref())?;
                if warm.identities != control.identities {
                    return Err("verified rows differ from control".into());
                }
            }
            let records = if matches!(class, "df1" | "df2-100" | "absent" | "mid" | "common") {
                let term = args[0].strip_prefix("text:").ok_or("missing term prefix")?;
                ferret_index::postings::Term::read(
                    view.segments().iter().map(|s| &**s),
                    term.as_bytes(),
                )?
                .records()
            } else {
                0
            };
            samples.push(Sample {
                evicted,
                records,
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
            "| {class} | {} | {:.0} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.0} | {driven} |",
            samples.len(),
            quantile(&mut estimate, 0.5),
            quantile(&mut warm, 0.5),
            quantile(&mut warm, 0.9),
            quantile(&mut warm, 0.95),
            quantile(&mut evicted, 0.5),
            quantile(&mut evicted, 0.9),
            quantile(&mut evicted, 0.95),
            quantile(&mut rows, 0.5),
        );
        let mut records: Vec<f64> = samples.iter().map(|s| s.records as f64).collect();
        let mut verified: Vec<f64> = samples.iter().map(|s| s.report.verified as f64).collect();
        let mut bytes: Vec<f64> = samples.iter().map(|s| s.read_bytes as f64).collect();
        println!(
            "{class}_records_per_term: median {:.0} p90 {:.0} max {:.0}; verified_documents: median {:.0} p90 {:.0}; read_bytes: median {:.0} p90 {:.0}",
            quantile(&mut records, 0.5),
            quantile(&mut records, 0.9),
            quantile(&mut records, 1.0),
            quantile(&mut verified, 0.5),
            quantile(&mut verified, 0.9),
            quantile(&mut bytes, 0.5),
            quantile(&mut bytes, 0.9)
        );
        if class == "phrase" {
            phrase_samples = samples;
        }
    }

    if phrase_samples.is_empty() {
        return Ok(());
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
        ("tokenized whole (documents)", |s| {
            s.report.matching.whole as f64
        }),
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

pub(crate) fn arg(prefix: &str, bytes: &[u8]) -> String {
    format!("{prefix}{}", String::from_utf8_lossy(bytes))
}

/// One run of a query.
pub(crate) struct Once {
    pub(crate) identities: Vec<u32>,
    pub(crate) wall: Duration,
    pub(crate) rows: u64,
    pub(crate) report: ContentReport,
    pub(crate) read: Duration,
    pub(crate) read_bytes: u64,
}

/// One run of `query`, rows counted.
pub(crate) fn once(
    catalog: &Catalog,
    names: &NameIndex,
    content: &Content<'_>,
    query: &Query,
    snapshot: Option<&crate::content_snapshot::Snapshot>,
) -> crate::Result<Once> {
    let (mut read, mut read_bytes) = (Duration::ZERO, 0u64);
    let mut path = Vec::new();
    let mut reader = |name: NameId, request: ferret_query::ReadRequest<'_>| {
        let start = Instant::now();
        if let Some(snapshot) = snapshot {
            let doc = match catalog.name(name).target() {
                Target::Inode(inode) => catalog.doc(inode)?,
                _ => return None,
            };
            // A stat still costs a filesystem operation, as in the ordinary
            // reader. These bytes are exactly the version both arms indexed.
            let path = snapshot.path(doc.0);
            let version = match request {
                ferret_query::ReadRequest::Stat => fs::metadata(path)
                    .ok()
                    .map(|_| ferret_query::ReadVersion::Catalogued),
                ferret_query::ReadRequest::Bytes(out, _) => fs::read(path).ok().map(|bytes| {
                    read_bytes += bytes.len() as u64;
                    *out = bytes;
                    ferret_query::ReadVersion::Catalogued
                }),
            };
            read += start.elapsed();
            return version;
        }
        let version = match request {
            ferret_query::ReadRequest::Stat => {
                path.clear();
                catalog.path(name, &mut path);
                fs::symlink_metadata(Path::new(std::ffi::OsStr::from_bytes(&path)))
                    .ok()
                    .filter(fs::Metadata::is_file)
                    .map(|meta| version(catalog, name, &meta))
            }
            ferret_query::ReadRequest::Bytes(out, _) => {
                let version = read_current_name(catalog, name, &mut path, out);
                if version.is_some() {
                    read_bytes += out.len() as u64;
                }
                version
            }
        };
        read += start.elapsed();
        version
    };
    let mut rows = 0u64;
    let mut identities = Vec::new();
    let start = Instant::now();
    let stats = query.run_content(catalog, names, content, &mut reader, None, |row| {
        identities.push(row.name.0);
        rows += 1;
        ControlFlow::Continue(())
    })?;
    let wall = start.elapsed();
    let report = stats
        .content
        .ok_or("a content query reported no content plan")?;
    identities.sort_unstable();
    Ok(Once {
        identities,
        wall,
        rows,
        report,
        read,
        read_bytes,
    })
}

fn same_version(catalog: &Catalog, name: NameId, meta: &fs::Metadata) -> bool {
    let Target::Inode(inode) = catalog.name(name).target() else {
        return false;
    };
    let stat = catalog.inode(inode).stat;
    meta.is_file()
        && (meta.dev(), meta.ino(), meta.len()) == (stat.dev, stat.ino, stat.size)
        && (meta.mtime(), meta.mtime_nsec()) == (stat.mtime_sec, i64::from(stat.mtime_nsec))
        && (meta.ctime(), meta.ctime_nsec()) == (stat.ctime_sec, i64::from(stat.ctime_nsec))
}

fn version(catalog: &Catalog, name: NameId, meta: &fs::Metadata) -> ferret_query::ReadVersion {
    if same_version(catalog, name, meta) {
        ferret_query::ReadVersion::Catalogued
    } else {
        ferret_query::ReadVersion::Current
    }
}

/// The benchmark uses fixed trees and a path reader, without crawl's checked
/// directory walk. Bracket the file descriptor against its current key.
pub(crate) fn read_current_name(
    catalog: &Catalog,
    name: NameId,
    path: &mut Vec<u8>,
    out: &mut Vec<u8>,
) -> Option<ferret_query::ReadVersion> {
    path.clear();
    catalog.path(name, path);
    let mut file = File::open(Path::new(std::ffi::OsStr::from_bytes(path))).ok()?;
    let before = file.metadata().ok()?;
    if !before.is_file() {
        return None;
    }
    out.clear();
    file.read_to_end(out).ok()?;
    let after = file.metadata().ok()?;
    if (
        before.dev(),
        before.ino(),
        before.len(),
        before.mtime(),
        before.mtime_nsec(),
        before.ctime(),
        before.ctime_nsec(),
    ) != (
        after.dev(),
        after.ino(),
        after.len(),
        after.mtime(),
        after.mtime_nsec(),
        after.ctime(),
        after.ctime_nsec(),
    ) {
        return None;
    }
    Some(version(catalog, name, &after))
}

/// Phrase sampling still uses only bytes from the catalogued version.
pub(crate) fn read_name(
    catalog: &Catalog,
    name: NameId,
    path: &mut Vec<u8>,
    out: &mut Vec<u8>,
) -> bool {
    read_current_name(catalog, name, path, out) == Some(ferret_query::ReadVersion::Catalogued)
}

struct Classes {
    singleton: Vec<Vec<u8>>,
    absent: Vec<Vec<u8>>,
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
    if segment.codec() != ferret_index::segment::DICTIONARY_CODEC {
        return Err("term classes require a codec-1 control; use --terms".into());
    }
    let mut rng = Rng(0x5eed_0001);
    let mut singleton = Reservoir::new();
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
            1 => singleton.offer(&mut rng, term),
            2..=100 => rare.offer(&mut rng, term),
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
    let common: Vec<Vec<u8>> = top.into_iter().map(|(_, t)| t).collect();
    let mut absent = Vec::new();
    for original in singleton.items.iter().chain(&rare.items) {
        let original = std::str::from_utf8(original)?;
        let suffix = &original[original.chars().next().ok_or("empty term")?.len_utf8()..];
        for replacement in 'a'..='z' {
            let mutation = format!("{replacement}{suffix}").into_bytes();
            if term_query(&mutation)
                && !absent.contains(&mutation)
                && view.lookup(&mutation)?.is_empty()
            {
                absent.push(mutation);
                break;
            }
        }
        if absent.len() == PER_CLASS {
            break;
        }
    }
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
        singleton: singleton.items,
        absent,
        rare: rare.items,
        mid: mid.items,
        common,
    })
}

/// [`PER_CLASS`] df = 1 terms of a fully merged codec-1 index, as the df1
/// class draws them.
pub(crate) fn pick_singletons(view: &View) -> crate::Result<Vec<Vec<u8>>> {
    Ok(pick_terms(view)?.singleton)
}

/// Whether `term`, as `text:term`, is a single-unit term query: under the
/// cap's ambiguity, and one run with no parts.
fn term_query(term: &[u8]) -> bool {
    term.len() < 61
        && Text::new(term, false)
            .is_some_and(|text| text.whole() == Some(term) && text.units().len() == 1)
}

/// One live name per live document (the first one `name_reader` visits).
/// Shared by phrase sampling and the chunk census's global map pass (S3 M6),
/// which both need every live document exactly once.
pub(crate) fn live_doc_names(catalog: &Catalog, live: &DocSet) -> Vec<(u32, NameId)> {
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
}

/// Phrase arguments: two adjacent units of a randomly chosen live
/// document, each a term query on its own.
pub(crate) fn pick_phrases(catalog: &Catalog, live: &DocSet) -> crate::Result<Vec<Vec<u8>>> {
    let mut rng = Rng(0x5eed_0002);
    let docs = live_doc_names(catalog, live);
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
pub(crate) fn quantile(values: &mut [f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    let rank = ((values.len() - 1) as f64 * q).round() as usize;
    values[rank]
}

/// S3 M5 through the real planner and verifier. Definitions remain in memory;
/// output contains only class labels, counts, bytes and timings.
pub(crate) fn run_regex(dir: &Path, index_dir: &Path, snapshot_dir: &Path) -> crate::Result<()> {
    let catalog = crate::open_catalog(dir)?;
    catalog.load_all()?;
    let catalog = catalog.into_resident()?;
    let snapshot = crate::content_snapshot::Snapshot::open(snapshot_dir, &catalog)?;
    let live = DocSet::new(catalog.next_doc().0, catalog.docs().map(|(doc, _)| doc.0));
    let view = View::open(
        index_dir,
        &CatalogView {
            incarnation: catalog.generation().incarnation,
            live: &live,
        },
    )?
    .ok_or("no paired index")?;
    let names = NameIndex::new(&catalog);
    let docs = DocNames::new(&catalog)?;
    let pinned = Pinned::new(Some(&view), &live);
    let [segment] = view.segments() else {
        return Err("regex-query requires full merge".into());
    };
    let term_classes = crate::screen::term_classes(segment, PER_CLASS)?;
    // The shared class generator takes owned segment handles; open a second
    // read-only set rather than teaching it about the paired store.
    let trigrams = view
        .manifest()
        .segments
        .iter()
        .map(|entry| {
            ferret_index::segment::Segment::open(File::open(
                index_dir.join(entry.trigram_file_name()),
            )?)
        })
        .collect::<Result<Vec<_>, ferret_index::ReadError>>()?;
    let classes = crate::screen::regex_classes(&term_classes, &trigrams, PER_CLASS)?;
    let prefix = ferret_query::GREP_PREFIX;
    let mut queries: Vec<(&str, Vec<Vec<String>>)> = classes
        .into_iter()
        .filter(|(class, _)| *class != "code class")
        .map(|(class, greps)| {
            (
                class,
                greps
                    .into_iter()
                    .map(|grep| {
                        let mut args = vec![format!("{prefix}{}", grep.pattern())];
                        if class == "broad" {
                            args.insert(0, "ext:rs".to_owned());
                        }
                        args
                    })
                    .collect(),
            )
        })
        .collect();
    let mut refused = Vec::new();
    let common = term_classes.common.first().ok_or("no common seed")?;
    for i in 0..PER_CLASS {
        refused.push(match i % 3 {
            0 => vec![format!(r"{prefix}\d{{4}}-\d{{2}}")],
            1 => vec![
                "NOT".into(),
                format!("{prefix}{}", String::from_utf8_lossy(common)),
            ],
            _ => vec!["NOT".into(), arg("text:", common), format!(r"{prefix}\d+")],
        });
    }
    for (class, queries) in &mut queries {
        if *class == "identifier pattern" {
            *queries = regex_identifiers(&snapshot, &live)?
                .into_iter()
                .map(|pattern| vec![format!("{prefix}{pattern}")])
                .collect();
        }
    }
    queries.push(("refused", refused));
    println!(
        "regex_live={} readable={} uncovered={} paired_segments={}",
        live.len(),
        catalog
            .docs()
            .filter(|(d, _)| snapshot.contains(d.0))
            .count(),
        pinned.uncovered().len(),
        view.segments().len()
    );
    println!(
        "| class | n | refused | cold median/p90 ms | warm median/p90 ms | candidates median/p90 | verified B median/p90 | cold verification share median/p90 % | warm verification share median/p90 % | engine/verification median/p90 % | M over-estimates | M median/p90 | actual Maybe median/p90 |"
    );
    for (class, args) in queries {
        let (
            mut cold,
            mut warm,
            mut candidates,
            mut bytes,
            mut cold_share,
            mut warm_share,
            mut engine_share,
            mut estimates,
            mut actual,
        ) = (
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let (mut over, mut refusals) = (0, 0);
        let mut p90_samples = Vec::new();
        let mut refusal_ms = Vec::new();
        for args in &args {
            crate::trigram::check_disk()?;
            let query = Query::from_args(args, std::time::SystemTime::now())?;
            let refusal_start = Instant::now();
            let checked = query.run_content(
                &catalog,
                &names,
                &Content {
                    pinned: &pinned,
                    docs: &docs,
                    bound: Some(ferret_query::UNCOVERED_BOUND),
                },
                &mut |_, _| None,
                None,
                |_| ControlFlow::Continue(()),
            );
            match checked {
                Err(ferret_query::RunError::ScanRequired { .. }) => {
                    refusals += 1;
                    refusal_ms.push(ms(refusal_start.elapsed()));
                }
                Err(error) => return Err(error.into()),
                Ok(_) => {}
            }
            let content = Content {
                pinned: &pinned,
                docs: &docs,
                bound: None,
            };
            // Drive the real cursor/evaluator to discover Maybe read paths,
            // then evict those files before the timed run. This pass never
            // reads content and does not warm its bytes.
            let mut evict_docs = std::collections::BTreeSet::new();
            query.run_content(
                &catalog,
                &names,
                &content,
                &mut |name, request| {
                    if matches!(request, ferret_query::ReadRequest::Bytes(..))
                        && let Target::Inode(inode) = catalog.name(name).target()
                        && let Some(doc) = catalog.doc(inode)
                    {
                        evict_docs.insert(doc.0);
                    }
                    None
                },
                None,
                |_| ControlFlow::Continue(()),
            )?;
            for doc in evict_docs {
                if let Ok(file) = File::open(snapshot.path(doc)) {
                    rustix::fs::fadvise(&file, 0, None, rustix::fs::Advice::DontNeed)?;
                }
            }
            for entry in fs::read_dir(index_dir)? {
                let file = File::open(entry?.path())?;
                rustix::fs::fadvise(&file, 0, None, rustix::fs::Advice::DontNeed)?;
            }
            let c = once(&catalog, &names, &content, &query, Some(&snapshot))?;
            let w = once(&catalog, &names, &content, &query, Some(&snapshot))?;
            if c.identities != w.identities {
                return Err("cold/warm rows differ".into());
            }
            if w.report.bounds.maybe < w.report.snapshot_maybe {
                return Err("M bound violated".into());
            }
            over += usize::from(w.report.bounds.maybe > w.report.snapshot_maybe);
            p90_samples.push((
                c.wall,
                c.report.verification_time,
                c.report.regex_time,
                w.wall,
                w.report.verification_time,
                w.report.regex_time,
            ));
            cold.push(ms(c.wall));
            warm.push(ms(w.wall));
            candidates.push(w.report.documents as f64);
            bytes.push(w.read_bytes as f64);
            cold_share
                .push(c.report.verification_time.as_secs_f64() / c.wall.as_secs_f64() * 100.0);
            warm_share
                .push(w.report.verification_time.as_secs_f64() / w.wall.as_secs_f64() * 100.0);
            engine_share.push(if w.report.verification_time.is_zero() {
                0.0
            } else {
                w.report.regex_time.as_secs_f64() / w.report.verification_time.as_secs_f64() * 100.0
            });
            estimates.push(w.report.bounds.maybe as f64);
            actual.push(w.report.snapshot_maybe as f64);
        }
        if !p90_samples.is_empty() {
            let at = ((p90_samples.len() as f64 * 0.9).ceil() as usize).saturating_sub(1);
            p90_samples.sort_by_key(|sample| sample.0);
            let c = p90_samples[at];
            p90_samples.sort_by_key(|sample| sample.3);
            let w = p90_samples[at];
            let share = |part: Duration, whole: Duration| {
                if whole.is_zero() {
                    0.0
                } else {
                    part.as_secs_f64() / whole.as_secs_f64() * 100.0
                }
            };
            println!(
                "d17 class={class} cold_p90_verify_pct={:.2} cold_p90_engine_of_verify_pct={:.2} warm_p90_verify_pct={:.2} warm_p90_engine_of_verify_pct={:.2}",
                share(c.1, c.0),
                share(c.2, c.1),
                share(w.4, w.3),
                share(w.5, w.4)
            );
        }
        if !refusal_ms.is_empty() {
            println!(
                "refusal class={class} median_ms={:.2} p90_ms={:.2}",
                quantile(&mut refusal_ms, 0.5),
                quantile(&mut refusal_ms, 0.9)
            );
        }
        let pair = |values: &mut Vec<f64>| {
            format!("{:.2}/{:.2}", quantile(values, 0.5), quantile(values, 0.9))
        };
        println!(
            "| {class} | {} | {refusals} | {} | {} | {} | {} | {} | {} | {} | {over}/{} | {} | {} |",
            args.len(),
            pair(&mut cold),
            pair(&mut warm),
            pair(&mut candidates),
            pair(&mut bytes),
            pair(&mut cold_share),
            pair(&mut warm_share),
            pair(&mut engine_share),
            args.len(),
            pair(&mut estimates),
            pair(&mut actual)
        );
    }
    Ok(())
}

/// Real identifier runs, with two tokenizer parts and at least one byte
/// between their chosen ends. The regex uses raw ASCII spans, not normalized
/// term bytes: its meaning is a substring of the captured document.
pub(crate) fn regex_identifiers(
    snapshot: &crate::content_snapshot::Snapshot,
    live: &DocSet,
) -> crate::Result<Vec<String>> {
    let ids: Vec<_> = live
        .range(0, live.bound())
        .filter(|&doc| snapshot.contains(doc))
        .collect();
    if ids.is_empty() {
        return Err("no readable identifier population".into());
    }
    let mut rng = Rng(0x5eed_0505);
    let mut reservoir = Reservoir::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut scratch = Scratch::default();
    for attempt in 0..PER_CLASS * PHRASE_TRIES {
        let bytes = fs::read(snapshot.path(ids[rng.below(ids.len() as u64) as usize]))?;
        let mut runs: Vec<(std::ops::Range<usize>, Vec<std::ops::Range<usize>>)> = Vec::new();
        tokenize(&bytes, &mut scratch, |token| match token.kind {
            Kind::Whole => runs.push((token.run, Vec::new())),
            Kind::Part => {
                if let Some((_, parts)) = runs.last_mut() {
                    parts.push(token.run);
                }
            }
        });
        for (run, parts) in runs {
            if !(10..=40).contains(&run.len())
                || !bytes[run.clone()]
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
                || parts.len() < 2
            {
                continue;
            }
            let (first, last) = (&parts[0], &parts[parts.len() - 1]);
            if first.len() < 3 || last.len() < 3 {
                continue;
            }
            let head = &bytes[first.start..(first.start + 4).min(first.end)];
            let tail = &bytes[(last.end - 4.min(last.len()))..last.end];
            if head.len() + tail.len() >= run.len() {
                continue;
            }
            let pattern = format!(
                r"{}\w+{}",
                std::str::from_utf8(head)?,
                std::str::from_utf8(tail)?
            );
            if seen.insert(pattern.clone()) {
                reservoir.offer(&mut rng, pattern.as_bytes());
            }
        }
        if attempt >= 99 && reservoir.items.len() == PER_CLASS {
            break;
        }
    }
    if reservoir.items.len() != PER_CLASS {
        return Err("insufficient real identifier seeds".into());
    }
    reservoir
        .items
        .into_iter()
        .map(|bytes| String::from_utf8(bytes).map_err(Into::into))
        .collect()
}
