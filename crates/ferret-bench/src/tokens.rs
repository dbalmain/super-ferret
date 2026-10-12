//! The token-policy experiment (docs/TOKENS.md): T1 drops base64-like runs
//! from the term index, T2 moves timestamps to a date index.
//!
//! `token-census <catalog-dir> <corpus-dir> [--samples <file>]` tokenizes a
//! frozen corpus once and reports, for no policy, T1, T2 and T1+T2: terms,
//! hapaxes and their bytes, postings each policy drops, false-suppression
//! counts, epoch candidates by range band and key context, and the date
//! index's shape and query costs at second, minute, hour and day keys.
//! `--samples` writes suppressed runs from code and prose files, and their
//! extensions, for review by hand: that file holds content, so it belongs
//! in a gitignored directory. Stdout carries counts only.
//!
//! [`policy`] parses the `--tokens`/`--policy` spellings `index-build` and
//! `content-query` share; [`explain_missing`] is how `content-query` accounts
//! for rows a policy drops.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::hash::{BuildHasher, Hash, Hasher, RandomState};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use ferret_catalog::{Catalog, NameId, Section, Target};
use ferret_index::dates::{DateIndex, pair_layout_bytes};
use ferret_text::{
    EPOCH_RANGE, Form, Policy, Scratch, base64_runs, cap, has_timestamp_in, timestamps, tokenize,
    tokenize_with,
};

/// `none`, `t1`, `t2` or `t1t2`.
pub(crate) fn policy(spec: &str) -> crate::Result<Policy> {
    Ok(match spec {
        "none" => Policy::NONE,
        "t1" => Policy {
            base64: true,
            timestamps: false,
        },
        "t2" => Policy {
            base64: false,
            timestamps: true,
        },
        "t1t2" => Policy {
            base64: true,
            timestamps: true,
        },
        _ => return Err("token policy must be none, t1, t2 or t1t2".into()),
    })
}

/// Checks a policy arm's rows against the control's for one `text:` term:
/// every control row the arm lacks must be a document holding `term` only
/// where `policy` suppressed it, and the arm may hold no row the control
/// lacks. Returns the dropped rows.
pub(crate) fn explain_missing(
    catalog: &Catalog,
    snapshot: Option<&crate::content_snapshot::Snapshot>,
    policy: Policy,
    term: &[u8],
    arm: &[u32],
    control: &[u32],
) -> crate::Result<u64> {
    if arm.iter().any(|name| control.binary_search(name).is_err()) {
        return Err("a policy arm returned a row the control did not".into());
    }
    let snapshot = snapshot.ok_or("explaining a policy difference needs --corpus")?;
    let mut dropped = 0;
    let mut scratch = Scratch::default();
    for &name in control.iter().filter(|n| arm.binary_search(n).is_err()) {
        let Target::Inode(inode) = catalog.name(NameId(name)).target() else {
            return Err("an unexplained row: not a file".into());
        };
        let doc = catalog
            .doc(inode)
            .ok_or("an unexplained row: no document")?;
        let bytes = fs::read(snapshot.path(doc.0))?;
        let (mut plain, mut kept) = (false, false);
        tokenize(&bytes, &mut scratch, |t| plain |= cap(t.bytes) == term);
        tokenize_with(&bytes, &mut scratch, policy, |t| {
            kept |= cap(t.bytes) == term
        });
        if !plain || kept {
            return Err("a dropped row the policy does not explain".into());
        }
        dropped += 1;
    }
    Ok(dropped)
}

// ── The census ──

/// No policy, T1, T2, T1+T2: bit `i` of a term's per-document mask.
const ARMS: [(&str, Policy); 4] = [
    ("none", Policy::NONE),
    (
        "t1",
        Policy {
            base64: true,
            timestamps: false,
        },
    ),
    (
        "t2",
        Policy {
            base64: false,
            timestamps: true,
        },
    ),
    (
        "t1t2",
        Policy {
            base64: true,
            timestamps: true,
        },
    ),
];

const SHARDS: usize = 256;
const GRANULARITIES: [(&str, i64); 4] = [
    ("second", 1),
    ("minute", 60),
    ("hour", 3600),
    ("day", 86_400),
];
/// Seconds east of UTC for the local-day ranges: Sydney in October.
const LOCAL_OFFSET: i64 = 11 * 3600;
const QUERIES: usize = 30;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Code,
    Prose,
    Other,
}

fn kind(ext: &str) -> Kind {
    match ext {
        "rs" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "py" | "go" | "c" | "h" | "cc"
        | "cpp" | "hpp" | "java" | "kt" | "rb" | "sh" | "lua" | "nix" | "swift" | "cs" | "php"
        | "el" | "hs" | "ml" | "scala" | "zig" => Kind::Code,
        "md" | "txt" | "rst" | "org" | "tex" | "adoc" => Kind::Prose,
        _ => Kind::Other,
    }
}

/// An epoch-length number: which band its value is in, and what the key
/// before it says.
#[derive(Default)]
struct EpochTally {
    /// `[digits 10|13][band below|inside|above][context id|time|other]`.
    counts: [[[u64; 3]; 3]; 2],
}

#[derive(Default)]
struct DocTotals {
    docs: u64,
    bytes: u64,
    base64_docs: [u64; 3],
    base64_doc_bytes: u64,
    base64_runs: [u64; 3],
    base64_run_bytes: [u64; 3],
    stamps: [u64; 4],
    stamp_docs: u64,
    epochs: EpochTally,
}

impl DocTotals {
    fn add(&mut self, other: &DocTotals) {
        self.docs += other.docs;
        self.bytes += other.bytes;
        self.base64_doc_bytes += other.base64_doc_bytes;
        self.stamp_docs += other.stamp_docs;
        for i in 0..3 {
            self.base64_docs[i] += other.base64_docs[i];
            self.base64_runs[i] += other.base64_runs[i];
            self.base64_run_bytes[i] += other.base64_run_bytes[i];
        }
        for i in 0..4 {
            self.stamps[i] += other.stamps[i];
        }
        for (a, b) in self.epochs.counts.iter_mut().zip(&other.epochs.counts) {
            for (a, b) in a.iter_mut().zip(b) {
                for (a, b) in a.iter_mut().zip(b) {
                    *a += b;
                }
            }
        }
    }
}

type Shard = HashMap<Box<[u8]>, [u32; 4]>;

pub(crate) fn census(catalog_dir: &Path, corpus: &Path, options: &[String]) -> crate::Result<()> {
    let samples_out = match options {
        [] => None,
        [flag, path] if flag == "--samples" => Some(Path::new(path)),
        _ => return Err("token-census takes only --samples <file>".into()),
    };
    print_examples();
    let ext = extensions(catalog_dir)?;
    let mut docs: Vec<u32> = fs::read_dir(corpus)?
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    docs.sort_unstable();

    let start = Instant::now();
    let hasher = RandomState::new();
    let shards: Vec<Mutex<Shard>> = (0..SHARDS).map(|_| Mutex::default()).collect();
    let totals = Mutex::new(DocTotals::default());
    let pairs = Mutex::new(Vec::<(i64, u32)>::new());
    let occurrences = Mutex::new(Vec::<(i64, u32, u32)>::new());
    let picked = Mutex::new(Vec::<(Kind, String, Vec<u8>)>::new());
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(8, |n| n.get().min(16));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut local = DocTotals::default();
                let mut scratch = Scratch::default();
                let mut terms: HashMap<Vec<u8>, u8> = HashMap::new();
                let mut by_shard: Vec<Vec<(Vec<u8>, u8)>> = vec![Vec::new(); SHARDS];
                let (mut b64, mut stamps) = (Vec::new(), Vec::new());
                let mut doc_pairs = Vec::new();
                let mut doc_occurrences = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&doc) = docs.get(i) else { break };
                    let Ok(bytes) = fs::read(corpus.join(doc.to_string())) else {
                        continue;
                    };
                    let doc_ext = ext.get(&doc).map_or("", String::as_str);
                    let doc_kind = kind(doc_ext) as usize;
                    local.docs += 1;
                    local.bytes += bytes.len() as u64;

                    b64.clear();
                    base64_runs(&bytes, |run| b64.push(run));
                    stamps.clear();
                    doc_pairs.clear();
                    timestamps(&bytes, |stamp| {
                        local.stamps[stamp.form as usize] += 1;
                        doc_pairs.push(stamp.seconds);
                        doc_occurrences.push((stamp.seconds, stamp.nanos, doc));
                        if stamp.time_start < stamp.span.end {
                            stamps.push(stamp.time_start..stamp.span.end);
                        }
                    });
                    epoch_candidates(&bytes, &mut local.epochs);
                    if !b64.is_empty() {
                        local.base64_docs[doc_kind] += 1;
                        local.base64_doc_bytes += bytes.len() as u64;
                        local.base64_runs[doc_kind] += b64.len() as u64;
                        local.base64_run_bytes[doc_kind] +=
                            b64.iter().map(|r| r.len() as u64).sum::<u64>();
                        if samples_out.is_some() && doc_kind != Kind::Other as usize {
                            let run = &b64[b64.len() / 2];
                            let kind = if doc_kind == 0 {
                                Kind::Code
                            } else {
                                Kind::Prose
                            };
                            picked.lock().expect("no panics").push((
                                kind,
                                doc_ext.to_owned(),
                                bytes[run.clone()].to_vec(),
                            ));
                        }
                    }
                    if !doc_pairs.is_empty() {
                        local.stamp_docs += 1;
                        doc_pairs.sort_unstable();
                        doc_pairs.dedup();
                        pairs
                            .lock()
                            .expect("no panics")
                            .extend(doc_pairs.iter().map(|&s| (s, doc)));
                    }
                    if doc_occurrences.len() > 4096 {
                        occurrences
                            .lock()
                            .expect("no panics")
                            .append(&mut doc_occurrences);
                    }

                    terms.clear();
                    let (mut b, mut s) = (0, 0);
                    tokenize(&bytes, &mut scratch, |token| {
                        let overlaps = |spans: &[std::ops::Range<usize>], at: &mut usize| {
                            while spans.get(*at).is_some_and(|r| r.end <= token.run.start) {
                                *at += 1;
                            }
                            spans.get(*at).is_some_and(|r| r.start < token.run.end)
                        };
                        let in_b64 = overlaps(&b64, &mut b);
                        let in_stamp = overlaps(&stamps, &mut s);
                        let mask = 1
                            | u8::from(!in_b64) << 1
                            | u8::from(!in_stamp) << 2
                            | u8::from(!in_b64 && !in_stamp) << 3;
                        let term = cap(token.bytes);
                        match terms.get_mut(term) {
                            Some(m) => *m |= mask,
                            None => {
                                terms.insert(term.to_vec(), mask);
                            }
                        }
                    });
                    for (term, mask) in terms.drain() {
                        let shard = (hasher.hash_one(&term) as usize) % SHARDS;
                        by_shard[shard].push((term, mask));
                    }
                    for (shard, entries) in by_shard.iter_mut().enumerate() {
                        if entries.is_empty() {
                            continue;
                        }
                        let mut map = shards[shard].lock().expect("no panics");
                        for (term, mask) in entries.drain(..) {
                            let df = map.entry(term.into_boxed_slice()).or_default();
                            for (arm, count) in df.iter_mut().enumerate() {
                                *count += u32::from(mask >> arm & 1);
                            }
                        }
                    }
                }
                occurrences
                    .lock()
                    .expect("no panics")
                    .append(&mut doc_occurrences);
                totals.lock().expect("no panics").add(&local);
            });
        }
    });
    let totals = totals.into_inner()?;
    println!("census_seconds: {:.1}", start.elapsed().as_secs_f64());
    println!("documents: {}", totals.docs);
    println!("document_bytes: {}", totals.bytes);
    report_terms(shards)?;
    report_base64(&totals);
    report_stamps(&totals);
    let mut pairs = pairs.into_inner()?;
    let occurrences = occurrences.into_inner()?;
    report_dates(corpus, &mut pairs, &occurrences)?;
    if let Some(out) = samples_out {
        write_samples(out, picked.into_inner()?)?;
    }
    Ok(())
}

/// What today's tokenizer emits for the brief's three forms, and what T2
/// keeps of them.
fn print_examples() {
    for text in [
        "2026-10-12T03:04:05.123Z",
        "2026/10/12 03:04:05",
        "1760238245123",
    ] {
        let (mut all, mut kept) = (Vec::new(), Vec::new());
        tokenize(text.as_bytes(), &mut Scratch::default(), |t| {
            all.push(String::from_utf8_lossy(t.bytes).into_owned());
        });
        tokenize_with(text.as_bytes(), &mut Scratch::default(), ARMS[2].1, |t| {
            kept.push(String::from_utf8_lossy(t.bytes).into_owned());
        });
        println!("example {text:?}: tokens {all:?}; t2 keeps {kept:?}");
    }
}

fn extensions(catalog_dir: &Path) -> crate::Result<HashMap<u32, String>> {
    let catalog = crate::open_catalog(catalog_dir)?;
    catalog.load(&[
        Section::Names,
        Section::Roots,
        Section::Size,
        Section::Doc,
        Section::Docs,
    ])?;
    let mut out = HashMap::new();
    for (_, edge) in catalog.name_reader().runs_from(NameId(0)) {
        let Target::Inode(inode) = edge.target() else {
            continue;
        };
        let Some(doc) = catalog.doc(inode) else {
            continue;
        };
        out.entry(doc.0)
            .or_insert_with(|| match edge.bytes.iter().rposition(|&b| b == b'.') {
                Some(at) if at > 0 => {
                    String::from_utf8_lossy(&edge.bytes[at + 1..].to_ascii_lowercase()).into_owned()
                }
                _ => String::new(),
            });
    }
    Ok(out)
}

/// Standalone 10- and 13-digit numbers, as `timestamps` bounds them but
/// before its range and key checks, tallied by band and key.
fn epoch_candidates(bytes: &[u8], tally: &mut EpochTally) {
    let mut at = 0;
    while at < bytes.len() {
        if !bytes[at].is_ascii_digit() {
            at += 1;
            continue;
        }
        let start = at;
        while at < bytes.len() && bytes[at].is_ascii_digit() {
            at += 1;
        }
        let len = at - start;
        let word = |b: &u8| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'.';
        if !matches!(len, 10 | 13)
            || start > 0 && word(&bytes[start - 1])
            || bytes.get(at).is_some_and(word)
        {
            continue;
        }
        let value: i64 = std::str::from_utf8(&bytes[start..at])
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let seconds = if len == 13 { value / 1000 } else { value };
        // Equal-width sidebands: 20 years on either side of the window.
        let band = if seconds < EPOCH_RANGE.start {
            if seconds >= EPOCH_RANGE.start - (EPOCH_RANGE.end - EPOCH_RANGE.start) {
                0
            } else {
                continue;
            }
        } else if seconds < EPOCH_RANGE.end {
            1
        } else if seconds < EPOCH_RANGE.end + (EPOCH_RANGE.end - EPOCH_RANGE.start) {
            2
        } else {
            continue;
        };
        let context = key_context(bytes, start);
        tally.counts[usize::from(len == 13)][band][context] += 1;
    }
}

/// 0: the key before the number names an identifier; 1: a time; 2: neither
/// or no key.
fn key_context(bytes: &[u8], start: usize) -> usize {
    let before = &bytes[start.saturating_sub(40)..start];
    let gap = before
        .iter()
        .rev()
        .take(3)
        .take_while(|&&b| matches!(b, b'"' | b'\'' | b':' | b'=' | b' '))
        .count();
    if gap == 0 {
        return 2;
    }
    let key = &before[..before.len() - gap];
    let len = key
        .iter()
        .rev()
        .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        .count();
    let key = key[key.len() - len..].to_ascii_lowercase();
    let has = |needle: &[u8]| key.windows(needle.len()).any(|w| w == needle);
    if key.ends_with(b"id") || key.ends_with(b"ids") {
        0
    } else if [
        &b"time"[..],
        b"date",
        b"stamp",
        b"epoch",
        b"created",
        b"updated",
        b"modified",
        b"expire",
        b"when",
        b"start",
        b"end",
        b"ms",
    ]
    .iter()
    .any(|n| has(n))
        || key.ends_with(b"at")
        || key == b"ts"
        || key.ends_with(b"_ts")
    {
        1
    } else {
        2
    }
}

fn report_terms(shards: Vec<Mutex<Shard>>) -> crate::Result<()> {
    // Per arm: terms, bytes, hapaxes, hapax bytes, postings.
    let mut sums = [[0u64; 5]; 4];
    // Per policy arm (t1, t2, t1t2): postings dropped, terms gone entirely,
    // hapaxes gone, df≥2 terms gone, terms partly dropped, postings dropped
    // from terms still at df≥2 ("false suppression" candidates), and their
    // term count.
    let mut lost = [[0u64; 7]; 3];
    // Hapaxes T2 removes, by shape.
    let mut shapes: HashMap<&'static str, (u64, u64)> = HashMap::new();
    for shard in shards {
        for (term, df) in shard.into_inner()? {
            let len = term.len() as u64;
            for arm in 0..4 {
                if df[arm] > 0 {
                    sums[arm][0] += 1;
                    sums[arm][1] += len;
                    sums[arm][4] += u64::from(df[arm]);
                }
                if df[arm] == 1 {
                    sums[arm][2] += 1;
                    sums[arm][3] += len;
                }
            }
            for arm in 1..4 {
                let row = &mut lost[arm - 1];
                let dropped = u64::from(df[0] - df[arm]);
                if dropped == 0 {
                    continue;
                }
                row[0] += dropped;
                if df[arm] == 0 {
                    row[1] += 1;
                    if df[0] == 1 {
                        row[2] += 1;
                    } else {
                        row[3] += 1;
                    }
                } else {
                    row[4] += 1;
                }
                if df[arm] >= 2 {
                    row[5] += dropped;
                    row[6] += 1;
                }
            }
            if df[0] == 1 && df[2] == 0 {
                let entry = shapes.entry(shape(&term)).or_default();
                entry.0 += 1;
                entry.1 += len;
            }
        }
    }
    println!();
    println!("| arm | terms | term bytes | hapaxes | hapax bytes | postings |");
    println!("| --- | --- | --- | --- | --- | --- |");
    for (arm, row) in ARMS.iter().zip(sums) {
        println!(
            "| {} | {} | {} | {} | {} | {} |",
            arm.0, row[0], row[1], row[2], row[3], row[4]
        );
    }
    println!();
    println!(
        "| arm | postings dropped | terms gone | hapaxes gone | df≥2 terms gone | terms partly dropped | postings dropped from terms left at df≥2 | such terms |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- |");
    for (arm, row) in ARMS[1..].iter().zip(lost) {
        let mut line = format!("| {} |", arm.0);
        for value in row {
            let _ = write!(line, " {value} |");
        }
        println!("{line}");
    }
    let mut shapes: Vec<_> = shapes.into_iter().collect();
    shapes.sort_by(|a, b| b.1.0.cmp(&a.1.0));
    println!();
    println!("| hapaxes t2 removes, by shape | count | bytes |");
    println!("| --- | --- | --- |");
    for (shape, (n, b)) in shapes {
        println!("| {shape} | {n} | {b} |");
    }
    Ok(())
}

fn shape(term: &[u8]) -> &'static str {
    let digits = term.iter().filter(|b| b.is_ascii_digit()).count();
    if digits == term.len() {
        match term.len() {
            10 => "10 digits",
            13 => "13 digits",
            1..=4 => "1-4 digits",
            5..=9 => "5-9 digits",
            _ => "other digits",
        }
    } else if digits == 0 {
        "letters"
    } else {
        "digits and letters"
    }
}

fn report_base64(totals: &DocTotals) {
    println!();
    println!("| base64 runs | code | prose | other |");
    println!("| --- | --- | --- | --- |");
    let row = |label: &str, values: [u64; 3]| {
        println!(
            "| {label} | {} | {} | {} |",
            values[0], values[1], values[2]
        );
    };
    row("documents", totals.base64_docs);
    row("runs", totals.base64_runs);
    row("run bytes", totals.base64_run_bytes);
    println!(
        "base64_document_bytes: {} (documents a has-suppressed-runs flag would send to verification)",
        totals.base64_doc_bytes
    );
}

fn report_stamps(totals: &DocTotals) {
    println!();
    println!("timestamp_documents: {}", totals.stamp_docs);
    for (label, form) in [
        ("date_time", Form::DateTime),
        ("date", Form::Date),
        ("epoch_seconds", Form::EpochSeconds),
        ("epoch_millis", Form::EpochMillis),
    ] {
        println!("timestamps_{label}: {}", totals.stamps[form as usize]);
    }
    println!();
    println!(
        "Epoch-length numbers, standalone (not part of a word or decimal). Bands are 20 years each: below 2010, 2010–2030 (accepted), 2030–2050."
    );
    println!("| digits | band | id key | time key | other |");
    println!("| --- | --- | --- | --- | --- |");
    for (digits, by_band) in ["10", "13"].iter().zip(&totals.epochs.counts) {
        for (band, counts) in ["1990–2010", "2010–2030", "2030–2050"].iter().zip(by_band) {
            println!(
                "| {digits} | {band} | {} | {} | {} |",
                counts[0], counts[1], counts[2]
            );
        }
    }
}

fn quantile(sorted: &[u32], q: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() - 1) as f64 * q).round() as usize]
}

fn report_dates(
    corpus: &Path,
    pairs: &mut Vec<(i64, u32)>,
    occurrences: &[(i64, u32, u32)],
) -> crate::Result<()> {
    println!();
    println!("timestamp_occurrences: {}", occurrences.len());
    println!("distinct_second_doc_pairs: {}", pairs.len());
    let sizes: HashMap<u32, u64> = {
        let mut docs: Vec<u32> = pairs.iter().map(|p| p.1).collect();
        docs.sort_unstable();
        docs.dedup();
        docs.into_iter()
            .filter_map(|d| Some((d, fs::metadata(corpus.join(d.to_string())).ok()?.len())))
            .collect()
    };
    // Seeded picks from the occurrences: instants, UTC days, local days,
    // months.
    let mut rng = crate::content_query::Rng(0x7a65_7374);
    let picks: Vec<(i64, u32)> = (0..QUERIES)
        .filter_map(|_| {
            let i = rng.below(occurrences.len() as u64) as usize;
            occurrences.get(i).map(|o| (o.0, o.1))
        })
        .collect();
    println!();
    println!(
        "| key | index bytes | pair-layout bytes | keys | pairs | docs/key median | docs/key p99 | docs/key max |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- |");
    let mut indexes = Vec::new();
    for (label, granularity) in GRANULARITIES {
        let mut copy = pairs.clone();
        let index = DateIndex::build(granularity, &mut copy);
        let pair_bytes = pair_layout_bytes(&copy);
        let mut per_key = index.docs_per_key();
        per_key.sort_unstable();
        println!(
            "| {label} | {} | {pair_bytes} | {} | {} | {} | {} | {} |",
            index.bytes(),
            index.keys(),
            index.pairs(),
            quantile(&per_key, 0.5),
            quantile(&per_key, 0.99),
            per_key.last().copied().unwrap_or(0),
        );
        indexes.push((label, index));
    }
    pairs.clear();
    pairs.shrink_to_fit();

    println!();
    println!(
        "{QUERIES} seeded queries per class, picked from timestamp occurrences. Point: the occurrence's instant to the millisecond. Day: its UTC day, and its local (UTC+11) day. Month: its UTC month. Candidates are documents the index returns; verified are those that must be read, because the query is not on key boundaries (or carries a fraction). Warm verify reads and scans each verified document once untimed, then times it."
    );
    println!(
        "| key | query | candidates median | candidates p90 | verified docs median | verified docs p90 | verify MB median | verify MB p90 | lookup ms median | verify ms median | verify ms p90 | true matches median |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    for (label, index) in &indexes {
        for query in ["point", "utc-day", "local-day", "month"] {
            let mut rows: Vec<[f64; 6]> = Vec::new();
            for &(seconds, nanos) in &picks {
                let ((from, from_n), (to, to_n)) = match query {
                    "point" => {
                        let ms = nanos / 1_000_000 * 1_000_000;
                        ((seconds, ms), (seconds, ms + 1_000_000))
                    }
                    "utc-day" => {
                        let day = seconds - seconds.rem_euclid(86_400);
                        ((day, 0), (day + 86_400, 0))
                    }
                    "local-day" => {
                        let local = seconds + LOCAL_OFFSET;
                        let day = local - local.rem_euclid(86_400) - LOCAL_OFFSET;
                        ((day, 0), (day + 86_400, 0))
                    }
                    _ => {
                        let (from, to) = month_bounds(seconds);
                        ((from, 0), (to, 0))
                    }
                };
                // A range ending on a fraction still needs its last second.
                let to_key = if to_n > 0 { to + 1 } else { to };
                let lookup_start = Instant::now();
                let mut docs = Vec::new();
                index.range(from, to_key, &mut docs);
                let lookup = lookup_start.elapsed();
                let exact = from_n == 0 && to_n == 0 && index.exact(from, to);
                let mut verify_docs = Vec::new();
                if !exact {
                    // Interior keys are exact; only documents of the edge
                    // keys need reading.
                    let g = index.granularity();
                    let mut edges = Vec::new();
                    index.range(from, from - from.rem_euclid(g) + g, &mut edges);
                    let mut tail = Vec::new();
                    let last_key_start = (to_key - 1) - (to_key - 1).rem_euclid(g);
                    index.range(last_key_start, to_key, &mut tail);
                    edges.extend(tail);
                    edges.sort_unstable();
                    edges.dedup();
                    verify_docs = edges;
                }
                let bytes: u64 = verify_docs.iter().filter_map(|d| sizes.get(d)).sum();
                let mut matched = docs.len() - verify_docs.len();
                let mut verify_ms = 0.0;
                if !verify_docs.is_empty() {
                    let mut buffer = Vec::new();
                    for pass in 0..2 {
                        let start = Instant::now();
                        let mut hits = 0;
                        for &doc in &verify_docs {
                            buffer.clear();
                            if let Ok(b) = fs::read(corpus.join(doc.to_string())) {
                                buffer = b;
                            }
                            hits +=
                                usize::from(has_timestamp_in(&buffer, (from, from_n), (to, to_n)));
                        }
                        if pass == 1 {
                            verify_ms = start.elapsed().as_secs_f64() * 1e3;
                            matched += hits;
                        }
                    }
                }
                rows.push([
                    docs.len() as f64,
                    verify_docs.len() as f64,
                    bytes as f64 / 1e6,
                    lookup.as_secs_f64() * 1e3,
                    verify_ms,
                    matched as f64,
                ]);
            }
            let column = |i: usize, q: f64| {
                let mut values: Vec<f64> = rows.iter().map(|r| r[i]).collect();
                values.sort_by(f64::total_cmp);
                values[((values.len() - 1) as f64 * q).round() as usize]
            };
            println!(
                "| {label} | {query} | {:.0} | {:.0} | {:.0} | {:.0} | {:.2} | {:.2} | {:.3} | {:.2} | {:.2} | {:.0} |",
                column(0, 0.5),
                column(0, 0.9),
                column(1, 0.5),
                column(1, 0.9),
                column(2, 0.5),
                column(2, 0.9),
                column(3, 0.5),
                column(4, 0.5),
                column(4, 0.9),
                column(5, 0.5),
            );
        }
    }
    Ok(())
}

/// The UTC month holding `seconds`, as `[first, next first)`.
fn month_bounds(seconds: i64) -> (i64, i64) {
    let days = seconds.div_euclid(86_400);
    // Hinnant's civil_from_days, for the year and month.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let first = (days - (day - 1)) * 86_400;
    let month_len = match mp {
        // Mar, Apr, ... Jan, Feb.
        11 => {
            let year = yoe + era * 400 + 1;
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
        1 | 3 | 6 | 8 => 30,
        _ => 31,
    };
    (first, first + month_len * 86_400)
}

fn write_samples(out: &Path, mut picked: Vec<(Kind, String, Vec<u8>)>) -> crate::Result<()> {
    // Deterministic order, then an even spread of at most 150 per kind.
    picked.sort_by(|a, b| {
        let mut ha = std::hash::DefaultHasher::new();
        a.2.hash(&mut ha);
        let mut hb = std::hash::DefaultHasher::new();
        b.2.hash(&mut hb);
        ha.finish().cmp(&hb.finish())
    });
    let mut text = String::new();
    for wanted in [Kind::Code, Kind::Prose] {
        let label = if wanted == Kind::Code {
            "code"
        } else {
            "prose"
        };
        let all: Vec<_> = picked.iter().filter(|p| p.0 == wanted).collect();
        let _ = writeln!(text, "## {label}: {} documents with runs", all.len());
        for (_, ext, run) in all.iter().take(150) {
            let shown = String::from_utf8_lossy(&run[..run.len().min(100)]);
            let _ = writeln!(text, "{ext}\t{}\t{shown}", run.len());
        }
    }
    fs::write(out, text)?;
    Ok(())
}
