//! File-backed execution of the volume experiment's selected gram trees.
//! Ranking stays outside timing; selected dictionary reads, checksum-aligned
//! posting reads and production cursors stay inside. Cold uses advisory file
//! eviction; verification size bands attribute work from one full query.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use ferret_index::segment::{ReadAt, Segment, Writer};
use ferret_index::{Certainty, Cursor, DocSet, Pinned, postings};
use ferret_query::{NameIndex, Query};
use ferret_verify::{Grep, Matcher, Trigrams};

use super::grams::Variant;
use super::query::{Lists, Need};
use super::{BANDS, Corpus, Sample, append_partition, build, counts, prepare, report, stats};

const MODES: [&str; 3] = ["shipped-four", "word-fixed-4", "word-fixed-4+5"];

/// Logical positional-read bytes, not device sectors. Duplicate checksum
/// chunks are counted each time the reader requests them.
pub(super) struct Meter {
    file: File,
    pub(super) bytes: Cell<u64>,
}

impl ReadAt for Meter {
    fn size(&self) -> io::Result<u64> {
        self.file.size()
    }

    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.file.read_exact_at(buf, offset)?;
        self.bytes.set(self.bytes.get() + buf.len() as u64);
        Ok(())
    }
}

impl Meter {
    pub(super) fn open(path: &Path) -> crate::Result<Segment<Self>> {
        Ok(Segment::open(Self {
            file: File::open(path)?,
            bytes: Cell::new(0),
        })?)
    }

    pub(super) fn evict(&self) -> crate::Result<()> {
        rustix::fs::fadvise(&self.file, 0, None, rustix::fs::Advice::DontNeed)?;
        Ok(())
    }
}

/// Creation must succeed before this guard owns anything. An existing file
/// can never be removed on a failed create; cleanup touches only this path.
pub(super) struct OwnedIndex(pub(super) PathBuf);

impl OwnedIndex {
    pub(super) fn create(dir: &Path, label: &str) -> crate::Result<(Self, File)> {
        fs::create_dir_all(dir)?;
        let path = dir.join(format!("{label}.{}.segment", std::process::id()));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok((Self(path), file))
    }

    pub(super) fn remove(self) -> crate::Result<()> {
        fs::remove_file(&self.0)?;
        Ok(())
    }
}

impl Drop for OwnedIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Check headroom before every bounded output write, so our own index cannot
/// take the filesystem through the 5 GB stop threshold while being finished.
pub(super) struct Output(pub(super) File);

impl io::Write for Output {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len().min(8 << 20);
        crate::trigram::check_disk_for_bytes(n as u64)
            .map_err(|e| io::Error::other(e.to_string()))?;
        self.0.write(&buf[..n])
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

pub(super) fn write_index(
    corpus: &Corpus,
    variant: Variant,
    dir: &Path,
) -> crate::Result<(OwnedIndex, Lists)> {
    let (owned, file) = OwnedIndex::create(dir, &variant.label())?;
    let live: Vec<_> = corpus.live.range(0, corpus.live.bound()).collect();
    let needs: Vec<_> = corpus
        .samples
        .iter()
        .map(|sample| Need::literals(&sample.facts, variant, &[]))
        .collect();
    let mut required = BTreeSet::new();
    for need in &needs {
        need.keys(&mut required);
    }
    let mut query_lists = Lists::new();
    let mut writer = Writer::keyed(0, corpus.live.bound().saturating_sub(1), 1)?;
    let parts = if matches!(variant, Variant::Word4) {
        1
    } else {
        32
    };
    for part in 0..parts {
        let lists = build(
            &corpus.snapshot,
            &live,
            variant,
            &[],
            (parts != 1).then_some(part),
        )?;
        let n = lists.len();
        append_partition(&mut writer, lists, &required, &mut query_lists)?;
        println!(
            "latency_build mode={} completed={} keys={n}",
            variant.label(),
            part + 1
        );
    }
    let mut out = Output(file);
    let sizes = writer.finish(&mut out)?;
    out.0.sync_all()?;
    if out.0.metadata()?.len() != sizes.total() {
        return Err("latency segment length differs from encoded sizes".into());
    }
    println!(
        "latency_index mode={} distinct_keys={} postings_bytes={} total_bytes={}",
        variant.label(),
        sizes.terms,
        sizes.postings,
        sizes.total()
    );
    Ok((owned, query_lists))
}

fn from_trigrams(tree: &Trigrams) -> Need {
    match tree {
        Trigrams::All => Need::All,
        Trigrams::None => Need::None,
        Trigrams::Key(key) => Need::Key(key.to_vec()),
        Trigrams::And(parts) => Need::And(parts.iter().map(from_trigrams).collect()),
        Trigrams::Or(parts) => Need::Or(parts.iter().map(from_trigrams).collect()),
    }
}

fn compile<'a>(
    tree: &Need,
    lists: &'a BTreeMap<Vec<u8>, postings::Term>,
    live: &'a DocSet,
) -> Cursor<'a> {
    match tree {
        Need::All => Cursor::bits(live, Certainty::Maybe),
        Need::None => Cursor::Empty,
        Need::Key(key) => crate::screen::term_cursor(&lists[key]),
        Need::And(parts) => Cursor::and(parts.iter().map(|p| compile(p, lists, live)).collect()),
        Need::Or(parts) => Cursor::or(
            parts.iter().map(|p| compile(p, lists, live)).collect(),
            live.bound(),
        ),
    }
}

struct PostingRun {
    docs: Vec<u32>,
    time: Duration,
    postings_bytes: u64,
    index_bytes: u64,
}

fn read_postings(
    segment: &Segment<Meter>,
    tree: &Need,
    live: &DocSet,
    uncovered: &[u32],
) -> crate::Result<PostingRun> {
    segment.source().bytes.set(0);
    let start = Instant::now();
    let mut keys = BTreeSet::new();
    tree.keys(&mut keys);
    let mut lists = BTreeMap::new();
    let mut postings_bytes = 0;
    for key in keys {
        let term = postings::Term::read([segment], &key)?;
        postings_bytes += term.bytes();
        lists.insert(key, term);
    }
    let cursor = compile(tree, &lists, live);
    let mut docs = crate::screen::drain(Cursor::and(vec![
        cursor,
        Cursor::bits(live, Certainty::Maybe),
    ]));
    docs.extend_from_slice(uncovered);
    docs.sort_unstable();
    docs.dedup();
    let time = start.elapsed();
    Ok(PostingRun {
        docs,
        time,
        postings_bytes,
        index_bytes: segment.source().bytes.get(),
    })
}

#[derive(Clone, Copy, Default)]
pub(super) struct Timing {
    pub(super) postings: Duration,
    pub(super) verify: Duration,
    pub(super) total: Duration,
    pub(super) postings_bytes: u64,
    pub(super) index_bytes: u64,
    pub(super) verified_bytes: u64,
}

struct Trial {
    bands: [Timing; 4],
    candidates: Vec<u32>,
    matches: Vec<u32>,
}

fn band(size: u64) -> usize {
    if size < 1 << 20 {
        1
    } else if size < 16 << 20 {
        2
    } else {
        3
    }
}

pub(super) fn verify(
    grep: &Grep,
    docs: &[u32],
    sizes: &[u64],
    readable: &[bool],
    path: &impl Fn(u32) -> PathBuf,
) -> crate::Result<([Timing; 4], Vec<u32>)> {
    let start = Instant::now();
    let mut times = [Timing::default(); 4];
    let mut matches = Vec::new();
    for &doc in docs {
        if !readable[doc as usize] {
            continue;
        }
        let document_start = Instant::now();
        let bytes = fs::read(path(doc))?;
        if bytes.len() as u64 != sizes[doc as usize] {
            return Err("latency frozen document size changed".into());
        }
        if grep.regex().is_match(&bytes) {
            matches.push(doc);
        }
        let n = bytes.len() as u64;
        drop(bytes);
        let elapsed = document_start.elapsed();
        let b = band(n);
        times[b].verify += elapsed;
        times[b].verified_bytes += n;
        times[0].verified_bytes += n;
    }
    times[0].verify = start.elapsed();
    Ok((times, matches))
}

fn trial(
    corpus: &Corpus,
    sample: &Sample,
    segment: &Segment<Meter>,
    tree: &Need,
    uncovered: &[u32],
) -> crate::Result<Trial> {
    let start = Instant::now();
    let posting = read_postings(segment, tree, &corpus.live, uncovered)?;
    let (mut bands, matches) = verify(
        &sample.grep,
        &posting.docs,
        &corpus.sizes,
        &corpus.readable,
        &|doc| corpus.snapshot.path(doc),
    )?;
    let total = start.elapsed();
    for timing in &mut bands {
        timing.postings = posting.time;
        timing.postings_bytes = posting.postings_bytes;
        timing.index_bytes = posting.index_bytes;
        timing.total = posting.time + timing.verify;
    }
    bands[0].total = total;
    Ok(Trial {
        bands,
        candidates: posting.docs,
        matches,
    })
}

fn evict(corpus: &Corpus, segment: &Segment<Meter>, docs: &[u32]) -> crate::Result<()> {
    // Planning/discovery runs finish first. No corpus bytes are read here;
    // the immutable files are clean and all advisory failures are errors.
    for &doc in docs {
        if corpus.readable[doc as usize] {
            let file = File::open(corpus.snapshot.path(doc))?;
            rustix::fs::fadvise(&file, 0, None, rustix::fs::Advice::DontNeed)?;
        }
    }
    segment.source().evict()
}

fn check_trial(trial: &Trial, expected: &[u32], oracle: &[u32]) -> crate::Result<()> {
    if trial.candidates != expected || trial.matches != oracle {
        return Err("latency candidate or oracle identities differ".into());
    }
    Ok(())
}

pub(super) fn ns(d: Duration) -> u64 {
    d.as_nanos().try_into().unwrap_or(u64::MAX)
}
fn ms_pair(values: Vec<u64>) -> String {
    let (median, p90, _) = stats(values);
    format!("{:.3}/{:.3}", median as f64 / 1e6, p90 as f64 / 1e6)
}
fn byte_pair(values: Vec<u64>) -> String {
    let (median, p90, _) = stats(values);
    format!("{median}/{p90}")
}

fn report_latency(mode: &str, corpus: &Corpus, cold: &[Trial], warm: &[Trial]) {
    println!(
        "| latency mode | class | cache | band | queries | postings ms median/p90 | verify ms median/p90 | total ms median/p90 | postings B median/p90 | index B median/p90 | candidates median/p90 | candidate B median/p90 | verified B median/p90 | oracle docs median/p90 | oracle B median/p90 | postings ns sum | verify ns sum | total ns sum | postings B sum | index B sum | verified B sum |"
    );
    for class in [
        "rare literal",
        "identifier pattern",
        "alternation",
        "broad",
        "code class",
    ] {
        for (cache, trials) in [("cold", cold), ("warm", warm)] {
            let rows: Vec<_> = corpus
                .samples
                .iter()
                .zip(trials)
                .filter(|(s, _)| s.class == class)
                .collect();
            for (b, label) in BANDS.iter().enumerate() {
                let timings: Vec<_> = rows.iter().map(|(_, t)| t.bands[b]).collect();
                let candidate: Vec<_> = rows
                    .iter()
                    .map(|(_, t)| counts(&t.candidates, &corpus.sizes, &corpus.readable)[b])
                    .collect();
                let oracle: Vec<_> = rows
                    .iter()
                    .map(|(s, _)| counts(&s.oracle, &corpus.sizes, &corpus.readable)[b])
                    .collect();
                println!(
                    "| {mode} | {class} | {cache} | {label} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    rows.len(),
                    ms_pair(timings.iter().map(|t| ns(t.postings)).collect()),
                    ms_pair(timings.iter().map(|t| ns(t.verify)).collect()),
                    ms_pair(timings.iter().map(|t| ns(t.total)).collect()),
                    byte_pair(timings.iter().map(|t| t.postings_bytes).collect()),
                    byte_pair(timings.iter().map(|t| t.index_bytes).collect()),
                    byte_pair(candidate.iter().map(|c| c.docs).collect()),
                    byte_pair(candidate.iter().map(|c| c.bytes).collect()),
                    byte_pair(timings.iter().map(|t| t.verified_bytes).collect()),
                    byte_pair(oracle.iter().map(|c| c.docs).collect()),
                    byte_pair(oracle.iter().map(|c| c.bytes).collect()),
                    timings.iter().map(|t| ns(t.postings)).sum::<u64>(),
                    timings.iter().map(|t| ns(t.verify)).sum::<u64>(),
                    timings.iter().map(|t| ns(t.total)).sum::<u64>(),
                    timings.iter().map(|t| t.postings_bytes).sum::<u64>(),
                    timings.iter().map(|t| t.index_bytes).sum::<u64>(),
                    timings.iter().map(|t| t.verified_bytes).sum::<u64>()
                );
            }
        }
    }
}

pub(crate) fn run(
    catalog: &Path,
    index: &Path,
    snapshot: &Path,
    scratch: &Path,
    only: Option<&str>,
) -> crate::Result<()> {
    if only.is_some_and(|labels| labels.split(',').any(|label| !MODES.contains(&label))) {
        return Err("unknown sparse latency mode".into());
    }
    let corpus = prepare(catalog, index, snapshot)?;
    let pinned = Pinned::new(Some(&corpus.view), &corpus.live);
    let names = NameIndex::new(&corpus.catalog);
    let missing: Vec<_> = corpus
        .live
        .range(0, corpus.live.bound())
        .filter(|&doc| !corpus.snapshot.contains(doc))
        .collect();
    let uncovered: Vec<_> = pinned.uncovered().range(0, corpus.live.bound()).collect();
    println!(
        "latency_protocol cache=fadvise-dontneed metadata=resident pairs=1 queries={} layouts=3 band_total=shared-postings-plus-band-verify",
        corpus.samples.len()
    );
    for mode in MODES
        .into_iter()
        .filter(|mode| only.is_none_or(|labels| labels.split(',').any(|l| l == *mode)))
    {
        crate::trigram::check_disk()?;
        let variant = match mode {
            "word-fixed-4" => Some(Variant::Word4),
            "word-fixed-4+5" => Some(Variant::Word45),
            _ => None,
        };
        let (owned, query_lists, path) = if let Some(variant) = variant {
            let (owned, lists) = write_index(&corpus, variant, scratch)?;
            let path = owned.0.clone();
            (Some(owned), lists, path)
        } else {
            let [entry] = corpus.view.manifest().segments.as_slice() else {
                return Err("latency requires full merge".into());
            };
            (None, Lists::new(), index.join(entry.trigram_file_name()))
        };
        let segment = Meter::open(&path)?;
        if segment.codec() != 1 {
            return Err("latency requires codec-1 keys".into());
        }
        let extra = if variant.is_none() {
            &uncovered
        } else {
            &missing
        };
        let mut plans = Vec::new();
        let mut expected = Vec::new();
        for sample in &corpus.samples {
            if let Some(variant) = variant {
                let need = Need::literals(&sample.facts, variant, &[]);
                let mut keys = BTreeSet::new();
                need.keys(&mut keys);
                let df: BTreeMap<_, _> = keys
                    .into_iter()
                    .map(|key| {
                        let df = segment.entry(&key)?.map_or(0, |entry| entry.df as usize);
                        Ok((key, df))
                    })
                    .collect::<crate::Result<_>>()?;
                plans.push(need.selected(&|key| df[key], corpus.live.len() as usize));
                let ids: Vec<_> = corpus.live.range(0, corpus.live.bound()).collect();
                let mut docs = need.candidates(&query_lists, &ids);
                docs.extend_from_slice(extra);
                docs.sort_unstable();
                docs.dedup();
                expected.push(docs);
            } else {
                let query = Query::from_args(
                    &[format!("grep:{}", sample.grep.pattern())],
                    SystemTime::now(),
                )?;
                let explained = query.explain_content(&corpus.catalog, &names, &pinned)?;
                let [plan] = explained.greps.as_slice() else {
                    return Err("latency requires one regex plan".into());
                };
                plans.push(from_trigrams(&plan.selected));
                expected.push(sample.shipped.clone());
            }
        }
        // File candidates must match the established volume experiment, not
        // just pass a looser oracle check after changing the execution engine.
        report(
            mode,
            &corpus.samples,
            &expected,
            &corpus.sizes,
            &corpus.readable,
        )?;
        drop(query_lists);
        let (mut cold, mut warm) = (Vec::new(), Vec::new());
        for (q, ((sample, plan), expected)) in
            corpus.samples.iter().zip(&plans).zip(&expected).enumerate()
        {
            crate::trigram::check_disk()?;
            let discovery = read_postings(&segment, plan, &corpus.live, extra)?;
            if discovery.docs != *expected {
                return Err("disk selection differs from volume selection".into());
            }
            evict(&corpus, &segment, expected)?;
            let c = trial(&corpus, sample, &segment, plan, extra)?;
            let w = trial(&corpus, sample, &segment, plan, extra)?;
            check_trial(&c, expected, &sample.oracle)?;
            check_trial(&w, expected, &sample.oracle)?;
            if c.bands[0].postings_bytes != w.bands[0].postings_bytes
                || c.bands[0].index_bytes != w.bands[0].index_bytes
            {
                return Err("cold and warm index reads differ".into());
            }
            cold.push(c);
            warm.push(w);
            if (q + 1).is_multiple_of(10) {
                println!("latency_progress mode={mode} completed={}", q + 1);
            }
        }
        report_latency(mode, &corpus, &cold, &warm);
        println!(
            "latency_correctness mode={mode} queries={} temperatures=2 missed_matches=0 identity_differences=0",
            corpus.samples.len()
        );
        drop(segment);
        if let Some(owned) = owned {
            owned.remove()?;
            println!("latency_cleanup mode={mode} remaining_intermediate_bytes=0");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
