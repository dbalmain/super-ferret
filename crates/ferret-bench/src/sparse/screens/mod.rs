//! Regex screens against a full scan (docs/SCREENS.md). One population —
//! the sparse experiment's seeds, compiled case-sensitive as `grep:` is
//! (D67 A2) — and five arms timed cold and warm on it: ripgrep over the
//! frozen corpus, shipped trigrams, a term-index screen ANDed with
//! trigrams, word 4+5 grams, and hashed fixed 4/5-grams ANDed with
//! trigrams. Every screen must hold every oracle match, and rg's file set
//! must equal the oracle's. Aggregates go to stdout; per-query rows, which
//! carry no pattern or path, go to the scratch directory.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use ferret_index::segment::{Hit, Segment};
use ferret_index::{Certainty, Cursor, DocSet, Pinned, postings};
use ferret_query::{NameIndex, Query};
use ferret_verify::trigram::RequiredLiterals;
use ferret_verify::{GrepFlags, Trigrams};

use super::grams::Variant;
use super::latency::{Meter, ns, verify, write_index};
use super::query::Need;
use super::{Corpus, counts, prepare_with, stats};

mod hashed;
mod product;
mod resident;
mod residue;
mod rg;
mod units;

use hashed::Hashed;
use resident::Resident;
use units::Unit;

const CLASSES: [&str; 5] = [
    "rare literal",
    "identifier pattern",
    "alternation",
    "broad",
    "code class",
];

/// A prefix naming more terms than this is dropped from the conjunction,
/// which only weakens the screen; the enumeration so far stays in the time.
const PREFIX_CAP: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Leaf {
    Tri(Vec<u8>),
    Term(Vec<u8>),
    Prefix(Vec<u8>),
    /// Every term containing these lowercase bytes: one dictionary pass.
    Substring(Vec<u8>),
    /// A key of the arm's own segment: word 4+5 or a hashed variant.
    Extra(Vec<u8>),
    /// Documents whose terms are incomplete: T1 suppressed a run, or a
    /// token over the 64-byte cap lost its tail.
    Fallback,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Plan {
    All,
    None,
    Leaf(Leaf),
    And(Vec<Plan>),
    Or(Vec<Plan>),
}

impl Plan {
    fn and(parts: impl IntoIterator<Item = Self>) -> Self {
        let mut flat = Vec::new();
        for part in parts {
            match part {
                Self::All => {}
                Self::None => return Self::None,
                Self::And(inner) => flat.extend(inner),
                other => flat.push(other),
            }
        }
        flat.sort();
        flat.dedup();
        match flat.len() {
            0 => Self::All,
            1 => flat.remove(0),
            _ => Self::And(flat),
        }
    }

    fn or(parts: impl IntoIterator<Item = Self>) -> Self {
        let mut flat = Vec::new();
        for part in parts {
            match part {
                Self::None => {}
                Self::All => return Self::All,
                Self::Or(inner) => flat.extend(inner),
                other => flat.push(other),
            }
        }
        flat.sort();
        flat.dedup();
        match flat.len() {
            0 => Self::None,
            1 => flat.remove(0),
            _ => Self::Or(flat),
        }
    }

    fn leaves(&self, out: &mut BTreeSet<Leaf>) {
        match self {
            Self::Leaf(leaf) => {
                out.insert(leaf.clone());
            }
            Self::And(parts) | Self::Or(parts) => parts.iter().for_each(|p| p.leaves(out)),
            Self::All | Self::None => {}
        }
    }

    fn has(&self, want: &impl Fn(&Leaf) -> bool) -> bool {
        let mut leaves = BTreeSet::new();
        self.leaves(&mut leaves);
        leaves.iter().any(want)
    }
}

fn from_trigrams(tree: &Trigrams) -> Plan {
    match tree {
        Trigrams::All => Plan::All,
        Trigrams::None => Plan::None,
        Trigrams::Key(key) => Plan::Leaf(Leaf::Tri(key.to_vec())),
        Trigrams::And(parts) => Plan::and(parts.iter().map(from_trigrams)),
        Trigrams::Or(parts) => Plan::or(parts.iter().map(from_trigrams)),
    }
}

fn from_need(need: &Need) -> Plan {
    match need {
        Need::All => Plan::All,
        Need::None => Plan::None,
        Need::Key(key) => Plan::Leaf(Leaf::Extra(key.clone())),
        Need::And(parts) => Plan::and(parts.iter().map(from_need)),
        Need::Or(parts) => Plan::or(parts.iter().map(from_need)),
    }
}

/// How the term screen reads a literal: the brief's units (exact and
/// prefix lookups, sound only case-sensitive), substrings of whole terms
/// (sound in either case), or both.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lookups {
    Units,
    Substrings,
    Both,
}

/// The term screen of a regex's facts: each literal's lookups ANDed, `All`
/// where a literal has none. OR branches stay.
fn term_plan(facts: &RequiredLiterals, lookups: Lookups) -> Plan {
    match facts {
        RequiredLiterals::All => Plan::All,
        RequiredLiterals::None => Plan::None,
        RequiredLiterals::Literal(bytes) => {
            let mut leaves = Vec::new();
            if lookups != Lookups::Substrings {
                leaves.extend(units::units(bytes).into_iter().map(|unit| match unit {
                    Unit::Exact(term) => Plan::Leaf(Leaf::Term(term)),
                    Unit::Prefix(prefix) => Plan::Leaf(Leaf::Prefix(prefix)),
                }));
            }
            if lookups != Lookups::Units {
                leaves.extend(
                    units::substrings(bytes)
                        .into_iter()
                        .map(|needle| Plan::Leaf(Leaf::Substring(needle))),
                );
            }
            Plan::and(leaves)
        }
        RequiredLiterals::And(parts) => Plan::and(parts.iter().map(|p| term_plan(p, lookups))),
        RequiredLiterals::Or(parts) => Plan::or(parts.iter().map(|p| term_plan(p, lookups))),
    }
}

/// Hashed keys of each literal, as a gram tree the four-rarest rule can
/// select from.
fn hashed_need(facts: &RequiredLiterals, variant: Hashed) -> Need {
    let and = |parts: Vec<Need>| {
        let mut flat = Vec::new();
        for part in parts {
            match part {
                Need::All => {}
                Need::None => return Need::None,
                Need::And(inner) => flat.extend(inner),
                other => flat.push(other),
            }
        }
        flat.sort();
        flat.dedup();
        if flat.is_empty() {
            Need::All
        } else {
            Need::And(flat)
        }
    };
    match facts {
        RequiredLiterals::All => Need::All,
        RequiredLiterals::None => Need::None,
        RequiredLiterals::Literal(bytes) => and(variant
            .literal_keys(bytes)
            .into_iter()
            .map(|key| Need::Key(Hashed::encode(key)))
            .collect()),
        RequiredLiterals::And(parts) => {
            and(parts.iter().map(|p| hashed_need(p, variant)).collect())
        }
        RequiredLiterals::Or(parts) => {
            Need::Or(parts.iter().map(|p| hashed_need(p, variant)).collect())
        }
    }
}

struct Sources<'a> {
    tri: &'a Segment<Meter>,
    term: Option<&'a Segment<Meter>>,
    extra: Option<&'a Segment<Meter>>,
    fallback: &'a DocSet,
    live: &'a DocSet,
    /// Substring lookups and the residue fallback from flat files, in
    /// place of the dictionary pass and the blanket fallback.
    resident: Option<&'a Resident>,
}

impl Sources<'_> {
    fn segments(&self) -> Vec<&Segment<Meter>> {
        [Some(self.tri), self.term, self.extra]
            .into_iter()
            .flatten()
            .collect()
    }
}

struct Read {
    docs: Vec<u32>,
    time: Duration,
    postings_bytes: u64,
    index_bytes: u64,
    prefix_terms: u64,
    capped_prefixes: u64,
}

fn compile<'a>(
    plan: &Plan,
    lists: &'a BTreeMap<Leaf, postings::Term>,
    sets: &'a BTreeMap<Leaf, Option<DocSet>>,
    fallback: &'a DocSet,
    src: &Sources<'a>,
) -> Cursor<'a> {
    match plan {
        Plan::All => Cursor::bits(src.live, Certainty::Maybe),
        Plan::None => Cursor::Empty,
        Plan::Leaf(Leaf::Fallback) => Cursor::bits(fallback, Certainty::Maybe),
        Plan::Leaf(leaf @ (Leaf::Prefix(_) | Leaf::Substring(_))) => match &sets[leaf] {
            Some(set) => Cursor::bits(set, Certainty::Maybe),
            None => Cursor::bits(src.live, Certainty::Maybe),
        },
        Plan::Leaf(leaf) => crate::screen::term_cursor(&lists[leaf]),
        Plan::And(parts) => Cursor::and(
            parts
                .iter()
                .map(|p| compile(p, lists, sets, fallback, src))
                .collect(),
        ),
        Plan::Or(parts) => Cursor::or(
            parts
                .iter()
                .map(|p| compile(p, lists, sets, fallback, src))
                .collect(),
            src.live.bound(),
        ),
    }
}

/// Dictionary and posting reads, then the cursor drain: the screen's whole
/// cost, all inside the timing.
fn read(src: &Sources, plan: &Plan, extra_docs: &[u32]) -> crate::Result<Read> {
    for segment in src.segments() {
        segment.source().bytes.set(0);
    }
    let start = Instant::now();
    let mut leaves = BTreeSet::new();
    plan.leaves(&mut leaves);
    let (mut lists, mut sets) = (BTreeMap::new(), BTreeMap::new());
    let (mut postings_bytes, mut prefix_terms, mut capped_prefixes) = (0, 0, 0);
    let mut needles = Vec::new();
    for leaf in leaves {
        let segment = match &leaf {
            Leaf::Tri(_) => src.tri,
            Leaf::Term(_) | Leaf::Prefix(_) => src.term.ok_or("term leaf without terms")?,
            Leaf::Extra(_) => src.extra.ok_or("extra leaf without segment")?,
            Leaf::Substring(needle) => {
                needles.push(needle.clone());
                continue;
            }
            Leaf::Fallback => continue,
        };
        match &leaf {
            Leaf::Tri(key) | Leaf::Term(key) | Leaf::Extra(key) => {
                let term = postings::Term::read([segment], key)?;
                postings_bytes += term.bytes();
                lists.insert(leaf, term);
            }
            Leaf::Prefix(prefix) => {
                let mut terms = segment.terms_from(prefix);
                let mut docs = Vec::new();
                let mut named = 0;
                while let Some((term, entry)) = terms.next_term()? {
                    if term < prefix.as_slice() {
                        continue;
                    }
                    if !term.starts_with(prefix) || named > PREFIX_CAP {
                        break;
                    }
                    named += 1;
                    match segment.read(&entry)? {
                        Hit::Single(doc) => docs.push(doc),
                        Hit::Postings(list) => {
                            postings_bytes += list.bytes();
                            list.decode(&mut docs);
                        }
                    }
                }
                prefix_terms += named as u64;
                let set = if named > PREFIX_CAP {
                    capped_prefixes += 1;
                    None
                } else {
                    Some(DocSet::new(src.live.bound(), docs))
                };
                sets.insert(leaf, set);
            }
            Leaf::Substring(_) | Leaf::Fallback => {}
        }
    }
    if let Some(resident) = src.resident {
        resident.bytes.set(0);
    }
    let mut residue = None;
    if !needles.is_empty() {
        let segment = src.term.ok_or("substring leaf without terms")?;
        let found = match src.resident {
            Some(resident) => {
                residue = Some(resident.residue_docs(&needles, src.live.bound())?);
                resident.substrings(segment, &needles, src.live.bound(), &mut postings_bytes)?
            }
            None => substring_scan(segment, &needles, src.live.bound(), &mut postings_bytes)?,
        };
        for (needle, (named, set)) in needles.into_iter().zip(found) {
            prefix_terms += named;
            capped_prefixes += u64::from(set.is_none());
            sets.insert(Leaf::Substring(needle), set);
        }
    }
    let empty = DocSet::new(src.live.bound(), []);
    let fallback = match (src.resident, &residue) {
        (Some(_), Some(set)) => set,
        (Some(_), None) => &empty,
        (None, _) => src.fallback,
    };
    let cursor = compile(plan, &lists, &sets, fallback, src);
    let mut docs = crate::screen::drain(Cursor::and(vec![
        cursor,
        Cursor::bits(src.live, Certainty::Maybe),
    ]));
    docs.extend_from_slice(extra_docs);
    docs.sort_unstable();
    docs.dedup();
    let time = start.elapsed();
    Ok(Read {
        docs,
        time,
        postings_bytes,
        index_bytes: src
            .segments()
            .iter()
            .map(|s| s.source().bytes.get())
            .sum::<u64>()
            + src.resident.map_or(0, |r| r.bytes.get()),
        prefix_terms,
        capped_prefixes,
    })
}

/// One pass over the dictionary for every needle at once: each needle's
/// matching terms and the union of their documents, or `None` past
/// [`PREFIX_CAP`] terms, which drops the needle from the conjunction.
fn substring_scan(
    segment: &Segment<Meter>,
    needles: &[Vec<u8>],
    bound: u32,
    postings_bytes: &mut u64,
) -> crate::Result<Vec<(u64, Option<DocSet>)>> {
    let finders: Vec<_> = needles.iter().map(memchr::memmem::Finder::new).collect();
    let mut named = vec![0u64; needles.len()];
    let mut docs: Vec<Vec<u32>> = vec![Vec::new(); needles.len()];
    let mut terms = segment.terms();
    while let Some((term, entry)) = terms.next_term()? {
        let mut hit = None;
        for (i, finder) in finders.iter().enumerate() {
            if term.len() >= needles[i].len() && finder.find(term).is_some() {
                named[i] += 1;
                if named[i] as usize <= PREFIX_CAP {
                    hit.get_or_insert_with(Vec::new).push(i);
                }
            }
        }
        let Some(hit) = hit else { continue };
        let mut list = Vec::new();
        match segment.read(&entry)? {
            Hit::Single(doc) => list.push(doc),
            Hit::Postings(postings) => {
                *postings_bytes += postings.bytes();
                postings.decode(&mut list);
            }
        }
        for i in hit {
            docs[i].extend_from_slice(&list);
        }
    }
    Ok(named
        .into_iter()
        .zip(docs)
        .map(|(n, d)| (n, (n as usize <= PREFIX_CAP).then(|| DocSet::new(bound, d))))
        .collect())
}

#[derive(Clone, Copy, Default)]
struct Run {
    postings: Duration,
    verify: Duration,
    total: Duration,
    postings_bytes: u64,
    index_bytes: u64,
    verified_bytes: u64,
}

fn trial(
    corpus: &Corpus,
    q: usize,
    src: &Sources,
    plan: &Plan,
    extra_docs: &[u32],
) -> crate::Result<(Run, Vec<u32>, Vec<u32>)> {
    let start = Instant::now();
    let read = read(src, plan, extra_docs)?;
    let (bands, matches) = verify(
        &corpus.samples[q].grep,
        &read.docs,
        &corpus.sizes,
        &corpus.readable,
        &|doc| corpus.snapshot.path(doc),
    )?;
    let total = start.elapsed();
    Ok((
        Run {
            postings: read.time,
            verify: bands[0].verify,
            total,
            postings_bytes: read.postings_bytes,
            index_bytes: read.index_bytes,
            verified_bytes: bands[0].verified_bytes,
        },
        read.docs,
        matches,
    ))
}

fn evict_files(corpus: &Corpus, docs: &[u32]) -> crate::Result<()> {
    for &doc in docs {
        if corpus.readable[doc as usize] {
            let file = File::open(corpus.snapshot.path(doc))?;
            rustix::fs::fadvise(&file, 0, None, rustix::fs::Advice::DontNeed)?;
        }
    }
    Ok(())
}

fn readable_oracle(corpus: &Corpus, q: usize) -> Vec<u32> {
    corpus.samples[q]
        .oracle
        .iter()
        .copied()
        .filter(|&d| corpus.readable[d as usize])
        .collect()
}

fn ms(values: Vec<u64>) -> String {
    let (median, p90, _) = stats(values);
    format!("{:.3}/{:.3}", median as f64 / 1e6, p90 as f64 / 1e6)
}

fn pair(values: Vec<u64>) -> String {
    let (median, p90, _) = stats(values);
    format!("{median}/{p90}")
}

/// What one arm run measures: which classes it times (the rest are
/// volume only), and whether a missed oracle match is a measurement (an
/// unsound screen's upper bound) or an error.
struct Measure<'a> {
    timed: &'a dyn Fn(&str) -> bool,
    allow_miss: bool,
}

const ALL_TIMED: Measure = Measure {
    timed: &|_| true,
    allow_miss: false,
};

struct Row {
    candidates: super::Counts,
    runs: Option<(Run, Run)>,
    missed: usize,
    prefix_terms: u64,
    capped: u64,
}

/// One screen arm: discovery outside the timing, the positive control,
/// then, for timed classes, eviction and a cold and a warm run per query.
fn time_arm(
    arm: &str,
    corpus: &Corpus,
    plans: &[Plan],
    src: &Sources,
    extra_docs: &[u32],
    measure: &Measure,
    private: &mut String,
) -> crate::Result<()> {
    let mut rows = Vec::new();
    for (q, plan) in plans.iter().enumerate() {
        crate::trigram::check_disk()?;
        let discovery = read(src, plan, extra_docs)?;
        let oracle = readable_oracle(corpus, q);
        let found: Vec<u32> = oracle
            .iter()
            .copied()
            .filter(|d| discovery.docs.binary_search(d).is_ok())
            .collect();
        let missed = oracle.len() - found.len();
        if missed > 0 && !measure.allow_miss {
            return Err(format!("positive control failed: {arm} query {q}").into());
        }
        let class = corpus.samples[q].class;
        let runs = if (measure.timed)(class) {
            evict_files(corpus, &discovery.docs)?;
            for segment in src.segments() {
                segment.source().evict()?;
            }
            if let Some(resident) = src.resident {
                resident.evict()?;
            }
            let (cold, cold_docs, cold_matches) = trial(corpus, q, src, plan, extra_docs)?;
            let (warm, warm_docs, warm_matches) = trial(corpus, q, src, plan, extra_docs)?;
            if cold_docs != discovery.docs || warm_docs != discovery.docs {
                return Err(format!("candidates moved between runs: {arm} query {q}").into());
            }
            if cold_matches != found || warm_matches != found {
                return Err(format!("matches differ from the oracle: {arm} query {q}").into());
            }
            Some((cold, warm))
        } else {
            None
        };
        let c = counts(&discovery.docs, &corpus.sizes, &corpus.readable)[0];
        let (cold_ns, warm_ns) = runs.map_or((0, 0), |(c, w)| (ns(c.total), ns(w.total)));
        let _ = writeln!(
            private,
            "{arm}\t{q}\t{class}\t{}\t{}\t{}\t{missed}\t{cold_ns}\t{warm_ns}\t{}\t{}\t{}",
            c.docs,
            c.bytes,
            oracle.len(),
            ns(discovery.time),
            discovery.prefix_terms,
            discovery.capped_prefixes,
        );
        println!(
            "progress arm={arm} query={} candidate_B={} cold_ms={:.1} warm_ms={:.1}",
            q + 1,
            c.bytes,
            cold_ns as f64 / 1e6,
            warm_ns as f64 / 1e6
        );
        rows.push(Row {
            candidates: c,
            runs,
            missed,
            prefix_terms: discovery.prefix_terms,
            capped: discovery.capped_prefixes,
        });
    }
    println!(
        "| arm | class | cache | queries | timed | candidate docs median/p90 | candidate B median/p90 | readable B median/p90 | missed oracle docs | posting ms median/p90 | verify ms median/p90 | total ms median/p90 | posting B median/p90 | index B median/p90 | verified B median/p90 | total ns sum | lookup terms sum | capped lookups |"
    );
    for class in CLASSES {
        let picked: Vec<&Row> = corpus
            .samples
            .iter()
            .zip(&rows)
            .filter(|(s, _)| s.class == class)
            .map(|(_, r)| r)
            .collect();
        for cache in ["cold", "warm"] {
            let runs: Vec<Run> = picked
                .iter()
                .filter_map(|r| r.runs)
                .map(|(c, w)| if cache == "cold" { c } else { w })
                .collect();
            let times = |f: &dyn Fn(&Run) -> u64| {
                if runs.is_empty() {
                    "-".to_string()
                } else {
                    ms(runs.iter().map(f).collect())
                }
            };
            let bytes = |f: &dyn Fn(&Run) -> u64| {
                if runs.is_empty() {
                    "-".to_string()
                } else {
                    pair(runs.iter().map(f).collect())
                }
            };
            println!(
                "| {arm} | {class} | {cache} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                picked.len(),
                runs.len(),
                pair(picked.iter().map(|r| r.candidates.docs).collect()),
                pair(picked.iter().map(|r| r.candidates.bytes).collect()),
                pair(picked.iter().map(|r| r.candidates.readable).collect()),
                picked.iter().map(|r| r.missed).sum::<usize>(),
                times(&|r| ns(r.postings)),
                times(&|r| ns(r.verify)),
                times(&|r| ns(r.total)),
                bytes(&|r| r.postings_bytes),
                bytes(&|r| r.index_bytes),
                bytes(&|r| r.verified_bytes),
                runs.iter().map(|r| ns(r.total)).sum::<u64>(),
                picked.iter().map(|r| r.prefix_terms).sum::<u64>(),
                picked.iter().map(|r| r.capped).sum::<u64>(),
            );
        }
    }
    println!(
        "correctness arm={arm} queries={} missed_oracle_docs={} candidates_stable=yes matches_equal_found=yes",
        plans.len(),
        rows.iter().map(|r| r.missed).sum::<usize>()
    );
    Ok(())
}

fn rg_arm(
    arm: &str,
    corpus: &Corpus,
    flags: GrepFlags,
    corpus_dir: &Path,
    threads: Option<usize>,
    private: &mut String,
) -> crate::Result<()> {
    let all: Vec<u32> = (0..corpus.live.bound())
        .filter(|&d| corpus.readable[d as usize])
        .collect();
    let mut rows = Vec::new();
    for (q, sample) in corpus.samples.iter().enumerate() {
        crate::trigram::check_disk()?;
        evict_files(corpus, &all)?;
        let cold = rg::scan(
            sample.grep.pattern(),
            flags.case_insensitive,
            corpus_dir,
            threads,
        )?;
        let warm = rg::scan(
            sample.grep.pattern(),
            flags.case_insensitive,
            corpus_dir,
            threads,
        )?;
        let oracle = readable_oracle(corpus, q);
        if cold.docs != oracle || warm.docs != oracle {
            return Err(format!(
                "rg file set differs from the oracle: query {q} ({}, rg {}/{} oracle {})",
                sample.class,
                cold.docs.len(),
                warm.docs.len(),
                oracle.len()
            )
            .into());
        }
        let _ = writeln!(
            private,
            "{arm}\t{q}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            sample.class,
            oracle.len(),
            ns(cold.wall),
            ns(warm.wall),
            cold.rchar,
            cold.device,
            warm.device
        );
        println!(
            "progress arm={arm} query={} cold_ms={:.1} warm_ms={:.1}",
            q + 1,
            cold.wall.as_secs_f64() * 1e3,
            warm.wall.as_secs_f64() * 1e3
        );
        rows.push((cold, warm));
    }
    println!(
        "| arm | class | cache | queries | wall ms median/p90 | cpu s median/p90 (x1000) | rchar B median/p90 | device read B median/p90 | wall ns sum |"
    );
    for class in CLASSES {
        let picked: Vec<_> = corpus
            .samples
            .iter()
            .zip(&rows)
            .filter(|(s, _)| s.class == class)
            .map(|(_, r)| r)
            .collect();
        for (cache, pick) in [
            ("cold", (|r: &(rg::Scan, rg::Scan)| &r.0) as fn(&_) -> &_),
            ("warm", |r| &r.1),
        ] {
            println!(
                "| {arm} | {class} | {cache} | {} | {} | {} | {} | {} | {} |",
                picked.len(),
                ms(picked.iter().map(|r| ns(pick(r).wall)).collect()),
                pair(
                    picked
                        .iter()
                        .map(|r| (pick(r).cpu * 1000.0) as u64)
                        .collect()
                ),
                pair(picked.iter().map(|r| pick(r).rchar).collect()),
                pair(picked.iter().map(|r| pick(r).device).collect()),
                picked.iter().map(|r| ns(pick(r).wall)).sum::<u64>(),
            );
        }
    }
    println!(
        "correctness arm={arm} queries={} file_set_equals_oracle=all",
        corpus.samples.len()
    );
    Ok(())
}

/// Documents whose term lists are incomplete, so a term screen must admit
/// them and leave them to trigrams: T1 dropped a run's tokens (blob), or a
/// token was longer than the 64-byte cap and lost its tail (long). Read
/// from `cache` when present.
fn fallback_docs(corpus: &Corpus, cache: &Path) -> crate::Result<DocSet> {
    let docs: Vec<u32> = if cache.exists() {
        fs::read_to_string(cache)?
            .lines()
            .map(str::parse)
            .collect::<Result<_, _>>()?
    } else {
        let start = Instant::now();
        let (mut docs, mut blob, mut long) = (Vec::new(), (0u64, 0u64), (0u64, 0u64));
        let mut scratch = ferret_text::Scratch::default();
        for doc in 0..corpus.live.bound() {
            if !corpus.readable[doc as usize] {
                continue;
            }
            let bytes = fs::read(corpus.snapshot.path(doc))?;
            let has_blob = ferret_text::has_base64_run(&bytes);
            let mut has_long = false;
            ferret_text::tokenize_terms(&bytes, &mut scratch, |token| {
                has_long |= token.bytes.len() > ferret_text::MAX_TOKEN_BYTES;
            });
            let size = bytes.len() as u64;
            if has_blob {
                blob = (blob.0 + 1, blob.1 + size);
            }
            if has_long {
                long = (long.0 + 1, long.1 + size);
            }
            if has_blob || has_long {
                docs.push(doc);
            }
        }
        let union: u64 = docs.iter().map(|&d| corpus.sizes[d as usize]).sum();
        println!(
            "fallback_docs blob_docs={} blob_bytes={} long_docs={} long_bytes={} union_docs={} union_bytes={union} wall_s={:.3}",
            blob.0,
            blob.1,
            long.0,
            long.1,
            docs.len(),
            start.elapsed().as_secs_f64()
        );
        let text: Vec<_> = docs.iter().map(u32::to_string).collect();
        fs::write(cache, text.join("\n") + "\n")?;
        docs
    };
    Ok(DocSet::new(corpus.live.bound(), docs))
}

/// The product planner's trigram selection for each seed. The population's
/// case rides in the pattern, since `grep:` itself is case-sensitive.
fn shipped_plans(corpus: &Corpus, flags: GrepFlags) -> crate::Result<Vec<Plan>> {
    let pinned = Pinned::new(Some(&corpus.view), &corpus.live);
    let names = NameIndex::new(&corpus.catalog);
    let inline = if flags.case_insensitive { "(?i)" } else { "" };
    let mut plans = Vec::new();
    for sample in &corpus.samples {
        let query = Query::from_args(
            &[format!("grep:{inline}{}", sample.grep.pattern())],
            SystemTime::now(),
        )?;
        let explained = query.explain_content(&corpus.catalog, &names, &pinned)?;
        let [plan] = explained.greps.as_slice() else {
            return Err("screens requires one regex plan".into());
        };
        if plan.derived != *sample.grep.trigrams()
            && plan.derived != *sample.grep.planning_trigrams()
        {
            return Err("product grep: derived a different tree than the seed".into());
        }
        plans.push(from_trigrams(&plan.selected));
    }
    Ok(plans)
}

const ARMS: [&str; 11] = [
    "rg",
    "product",
    "residue",
    "resident",
    "rg-j1",
    "trigrams",
    "terms",
    "terms-raw",
    "word45",
    "hashed",
    "units",
];

/// Hashed variants are timed where a screen can change verification;
/// broad, alternation and code classes are volume only, to keep the six
/// variants from crowding out arms 1-4.
const HASHED_TIMED: [&str; 2] = ["rare literal", "identifier pattern"];

pub(crate) fn run(args: &[String]) -> crate::Result<()> {
    let [catalog, index, corpus_dir, scratch, case, arms] = args else {
        return Err("screens <catalog> <index> <corpus> <scratch> <ci|cs> <arm,...>".into());
    };
    let (index, corpus_dir, scratch) =
        (Path::new(index), Path::new(corpus_dir), Path::new(scratch));
    let arms: Vec<&str> = arms.split(',').collect();
    if let Some(bad) = arms
        .iter()
        .find(|a| !ARMS.contains(a) && !Hashed::ALL.iter().any(|h| h.label() == **a))
    {
        return Err(format!("unknown screens arm {bad}").into());
    }
    let flags = GrepFlags {
        case_insensitive: match case.as_str() {
            "ci" => true,
            "cs" => false,
            _ => return Err("case must be ci or cs".into()),
        },
    };
    fs::create_dir_all(scratch)?;
    let corpus = prepare_with(
        Path::new(catalog),
        index,
        corpus_dir,
        flags,
        Some(&scratch.join(format!("oracle-{case}.tsv"))),
    )?;
    let live_ids: Vec<_> = corpus.live.range(0, corpus.live.bound()).collect();
    let pinned = Pinned::new(Some(&corpus.view), &corpus.live);
    let mut extra_docs: Vec<u32> = pinned.uncovered().range(0, corpus.live.bound()).collect();
    extra_docs.extend(live_ids.iter().filter(|&&d| !corpus.snapshot.contains(d)));
    extra_docs.sort_unstable();
    extra_docs.dedup();
    for class in CLASSES {
        let picked: Vec<u64> = corpus
            .samples
            .iter()
            .filter(|s| s.class == class)
            .map(|s| s.oracle.len() as u64)
            .collect();
        let bytes: Vec<u64> = corpus
            .samples
            .iter()
            .filter(|s| s.class == class)
            .map(|s| counts(&s.oracle, &corpus.sizes, &corpus.readable)[0].bytes)
            .collect();
        println!(
            "population case={case} class={class} queries={} oracle_docs_median_p90={} oracle_B_median_p90={} zero_match_queries={}",
            picked.len(),
            pair(picked.clone()),
            pair(bytes),
            picked.iter().filter(|&&n| n == 0).count()
        );
    }
    println!("uncovered_or_unreadable_docs={}", extra_docs.len());

    let [entry] = corpus.view.manifest().segments.as_slice() else {
        return Err("screens requires a fully merged index".into());
    };
    let tri = Meter::open(&index.join(entry.trigram_file_name()))?;
    let term = Meter::open(&index.join(entry.file_name()))?;
    println!(
        "index_bytes trigram={} terms={}",
        fs::metadata(index.join(entry.trigram_file_name()))?.len(),
        fs::metadata(index.join(entry.file_name()))?.len(),
    );

    let shipped = shipped_plans(&corpus, flags)?;
    let mut private = String::new();
    let empty = DocSet::new(corpus.live.bound(), []);
    let wants = |arm: &str| arms.contains(&arm);
    // The brief's unit rules are sound only case-sensitive; substrings of
    // whole terms are sound either way.
    let lookups = if flags.case_insensitive {
        Lookups::Substrings
    } else {
        Lookups::Both
    };

    if wants("units") || wants("terms") || wants("terms-raw") {
        unit_report(&corpus, flags)?;
    }
    if wants("rg") {
        rg_arm("rg", &corpus, flags, corpus_dir, None, &mut private)?;
    }
    if wants("product") {
        product::arm(&corpus, flags, index, &mut private)?;
    }
    if wants("rg-j1") {
        rg_arm("rg-j1", &corpus, flags, corpus_dir, Some(1), &mut private)?;
    }
    if wants("trigrams") {
        let src = Sources {
            tri: &tri,
            term: None,
            extra: None,
            fallback: &empty,
            live: &corpus.live,
            resident: None,
        };
        time_arm(
            "trigrams",
            &corpus,
            &shipped,
            &src,
            &extra_docs,
            &ALL_TIMED,
            &mut private,
        )?;
    }
    if wants("terms-raw") {
        let fallback = fallback_docs(&corpus, &scratch.join("fallback-docs.txt"))?;
        let src = Sources {
            tri: &tri,
            term: Some(&term),
            extra: None,
            fallback: &fallback,
            live: &corpus.live,
            resident: None,
        };
        // Without the fallback: what T1 and the cap lose, measured, never
        // timed.
        let raw: Vec<_> = corpus
            .samples
            .iter()
            .zip(&shipped)
            .map(|(s, p)| Plan::and([p.clone(), term_plan(&s.facts, lookups)]))
            .collect();
        if wants("terms-raw") {
            miss_report(&corpus, &raw, &src, &extra_docs)?;
            // The unsound screen's upper bound, timed on rare literals.
            time_arm(
                "terms-raw",
                &corpus,
                &raw,
                &src,
                &extra_docs,
                &Measure {
                    timed: &|class| class == "rare literal",
                    allow_miss: true,
                },
                &mut private,
            )?;
        }
    }
    if wants("residue") {
        let fallback = fallback_docs(&corpus, &scratch.join("fallback-docs.txt"))?;
        let src = Sources {
            tri: &tri,
            term: Some(&term),
            extra: None,
            fallback: &fallback,
            live: &corpus.live,
            resident: None,
        };
        residue_arm(&corpus, &shipped, lookups, &src, &extra_docs)?;
    }
    if wants("resident") {
        let fallback = fallback_docs(&corpus, &scratch.join("fallback-docs.txt"))?;
        let docs: Vec<u32> = fallback.range(0, corpus.live.bound()).collect();
        let resident = Resident::build(&term, &corpus, &docs, scratch)?;
        let (dictionary, residue, terms, residue_docs) = resident.sizes();
        println!(
            "resident dictionary_bytes={dictionary} terms={terms} residue_bytes={residue} residue_docs={residue_docs}"
        );
        let src = Sources {
            tri: &tri,
            term: Some(&term),
            extra: None,
            fallback: &empty,
            live: &corpus.live,
            resident: Some(&resident),
        };
        let plans: Vec<_> = corpus
            .samples
            .iter()
            .zip(&shipped)
            .map(|(s, p)| {
                Plan::and([
                    p.clone(),
                    Plan::or([term_plan(&s.facts, lookups), Plan::Leaf(Leaf::Fallback)]),
                ])
            })
            .collect();
        time_arm(
            "resident",
            &corpus,
            &plans,
            &src,
            &extra_docs,
            &ALL_TIMED,
            &mut private,
        )?;
    }
    if wants("terms") {
        let fallback = fallback_docs(&corpus, &scratch.join("fallback-docs.txt"))?;
        let src = Sources {
            tri: &tri,
            term: Some(&term),
            extra: None,
            fallback: &fallback,
            live: &corpus.live,
            resident: None,
        };
        let plans: Vec<_> = corpus
            .samples
            .iter()
            .zip(&shipped)
            .map(|(s, p)| {
                Plan::and([
                    p.clone(),
                    Plan::or([term_plan(&s.facts, lookups), Plan::Leaf(Leaf::Fallback)]),
                ])
            })
            .collect();
        let narrowed: Vec<_> = plans.iter().zip(&shipped).map(|(a, b)| a != b).collect();
        for class in CLASSES {
            println!(
                "terms_plan class={class} narrowed_queries={}",
                corpus
                    .samples
                    .iter()
                    .zip(&narrowed)
                    .filter(|(s, n)| s.class == class && **n)
                    .count()
            );
        }
        time_arm(
            "terms",
            &corpus,
            &plans,
            &src,
            &extra_docs,
            &ALL_TIMED,
            &mut private,
        )?;
    }
    if wants("word45") {
        crate::trigram::check_disk()?;
        let (cpu, wall) = (crate::segment_build::cpu_seconds()?, Instant::now());
        let (owned, lists) = write_index(&corpus, Variant::Word45, scratch)?;
        drop(lists);
        println!(
            "word45_build cpu_s={:.3} wall_s={:.3} bytes={}",
            crate::segment_build::cpu_seconds()? - cpu,
            wall.elapsed().as_secs_f64(),
            fs::metadata(&owned.0)?.len()
        );
        let segment = Meter::open(&owned.0)?;
        let plans = extra_plans(&corpus, &segment, None, |s| {
            Need::literals(&s.facts, Variant::Word45, &[])
        })?;
        let src = Sources {
            tri: &tri,
            term: None,
            extra: Some(&segment),
            fallback: &empty,
            live: &corpus.live,
            resident: None,
        };
        time_arm(
            "word45",
            &corpus,
            &plans,
            &src,
            &extra_docs,
            &ALL_TIMED,
            &mut private,
        )?;
        drop(segment);
        owned.remove()?;
    }
    for variant in Hashed::ALL {
        let label = variant.label();
        if !(wants("hashed") || wants(&label)) {
            continue;
        }
        crate::trigram::check_disk()?;
        let built = hashed::build(
            variant,
            &corpus.snapshot,
            &live_ids,
            corpus.live.bound(),
            scratch,
        )?;
        println!("{}", built.report);
        let segment = Meter::open(&built.owned.0)?;
        let plans = extra_plans(&corpus, &segment, Some(&shipped), |s| {
            hashed_need(&s.facts, variant)
        })?;
        let src = Sources {
            tri: &tri,
            term: None,
            extra: Some(&segment),
            fallback: &empty,
            live: &corpus.live,
            resident: None,
        };
        time_arm(
            &label,
            &corpus,
            &plans,
            &src,
            &extra_docs,
            &Measure {
                timed: &|class| HASHED_TIMED.contains(&class),
                allow_miss: false,
            },
            &mut private,
        )?;
        drop(segment);
        built.owned.remove()?;
    }
    finish(scratch, case, &arms, private)
}

/// The per-query rows: classes and numbers, no pattern or path.
fn finish(scratch: &Path, case: &str, arms: &[&str], private: String) -> crate::Result<()> {
    let path = scratch.join(format!(
        "rows-{case}-{}-{}.tsv",
        arms.join("+"),
        std::process::id()
    ));
    fs::write(&path, private)?;
    println!("private_rows written");
    Ok(())
}

/// Word and hashed plans: the four-rarest selection by the segment's own
/// df, made before timing as the latency harness does, then ANDed with the
/// shipped trigram plan when given.
fn extra_plans(
    corpus: &Corpus,
    segment: &Segment<Meter>,
    shipped: Option<&[Plan]>,
    need: impl Fn(&super::Sample) -> Need,
) -> crate::Result<Vec<Plan>> {
    let mut plans = Vec::new();
    for (q, sample) in corpus.samples.iter().enumerate() {
        let need = need(sample);
        let mut keys = BTreeSet::new();
        need.keys(&mut keys);
        let df: BTreeMap<_, _> = keys
            .into_iter()
            .map(|key| {
                let df = segment.entry(&key)?.map_or(0, |e| e.df as usize);
                Ok((key, df))
            })
            .collect::<crate::Result<_>>()?;
        let selected = from_need(&need.selected(&|key| df[key], corpus.live.len() as usize));
        plans.push(match shipped {
            Some(shipped) => Plan::and([shipped[q].clone(), selected]),
            None => selected,
        });
    }
    Ok(plans)
}

/// Lookup coverage per class: how many queries each kind of term lookup
/// can narrow at all. The unit rules are reported for a case-sensitive
/// population only.
fn unit_report(corpus: &Corpus, flags: GrepFlags) -> crate::Result<()> {
    for class in CLASSES {
        let mut row = [0usize; 6];
        for sample in corpus.samples.iter().filter(|s| s.class == class) {
            let units = term_plan(&sample.facts, Lookups::Units);
            let substrings = term_plan(&sample.facts, Lookups::Substrings);
            row[0] += 1;
            row[1] += usize::from(!matches!(sample.facts, RequiredLiterals::All));
            row[2] += usize::from(units != Plan::All);
            row[3] += usize::from(units.has(&|l| matches!(l, Leaf::Term(_))));
            row[4] += usize::from(units.has(&|l| matches!(l, Leaf::Prefix(_))));
            row[5] += usize::from(substrings != Plan::All);
        }
        println!(
            "lookups case_insensitive={} class={class} queries={} with_literal_facts={} unit_narrowed={} with_exact_unit={} with_prefix_unit={} substring_narrowed={}",
            flags.case_insensitive, row[0], row[1], row[2], row[3], row[4], row[5]
        );
    }
    Ok(())
}

/// The term screen without the fallback: oracle matches it would lose,
/// and how many of those are in fallback documents.
fn miss_report(
    corpus: &Corpus,
    plans: &[Plan],
    src: &Sources,
    extra_docs: &[u32],
) -> crate::Result<()> {
    let mut by_class: BTreeMap<&str, (usize, usize, usize, Vec<u64>)> = BTreeMap::new();
    for (q, plan) in plans.iter().enumerate() {
        let read = read(src, plan, extra_docs)?;
        let missed: Vec<_> = readable_oracle(corpus, q)
            .into_iter()
            .filter(|d| read.docs.binary_search(d).is_err())
            .collect();
        let entry = by_class.entry(corpus.samples[q].class).or_default();
        entry.0 += usize::from(!missed.is_empty());
        entry.1 += missed.len();
        entry.2 += missed.iter().filter(|&&d| src.fallback.contains(d)).count();
        entry
            .3
            .push(counts(&read.docs, &corpus.sizes, &corpus.readable)[0].bytes);
    }
    for (class, (queries, missed, fallback, bytes)) in by_class {
        println!(
            "terms_raw class={class} queries_with_miss={queries} missed_docs={missed} missed_docs_in_fallback={fallback} candidate_B_median_p90={}",
            pair(bytes)
        );
    }
    Ok(())
}

/// The term screen made sound by a per-query residue fallback instead of
/// the arm's blanket one: volumes and the positive control, plus the cost
/// of the substring lookups against a resident dictionary.
fn residue_arm(
    corpus: &Corpus,
    shipped: &[Plan],
    lookups: Lookups,
    src: &Sources,
    extra_docs: &[u32],
) -> crate::Result<()> {
    let raw: Vec<_> = corpus
        .samples
        .iter()
        .zip(shipped)
        .map(|(s, p)| Plan::and([p.clone(), term_plan(&s.facts, lookups)]))
        .collect();
    let per_query: Vec<Vec<Vec<u8>>> = raw
        .iter()
        .map(|plan| {
            let mut leaves = BTreeSet::new();
            plan.leaves(&mut leaves);
            leaves
                .into_iter()
                .filter_map(|leaf| match leaf {
                    Leaf::Substring(needle) => Some(needle),
                    _ => None,
                })
                .collect()
        })
        .collect();
    let needles: Vec<Vec<u8>> = per_query
        .iter()
        .flatten()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let start = Instant::now();
    let fallback: Vec<u32> = src.fallback.range(0, corpus.live.bound()).collect();
    let admitted = residue::admit(corpus, &fallback, &needles)?;
    println!(
        "residue fallback_docs={} residue_docs={} residue_bytes={} residue_trigram_pairs={} needles={} wall_s={:.3}",
        fallback.len(),
        admitted.residue_docs,
        admitted.residue_bytes,
        admitted.pairs,
        needles.len(),
        start.elapsed().as_secs_f64()
    );

    // The resident dictionary: every term once, newline-separated, scanned
    // with memmem per needle.
    let mut dictionary = Vec::new();
    let mut terms = src.term.ok_or("residue needs terms")?.terms();
    let mut term_count = 0u64;
    while let Some((term, _)) = terms.next_term()? {
        dictionary.extend_from_slice(term);
        dictionary.push(b'\n');
        term_count += 1;
    }
    println!(
        "resident_dictionary terms={term_count} bytes={}",
        dictionary.len()
    );

    let union = |lists: &[Vec<u32>], wanted: &[Vec<u8>]| -> Vec<u32> {
        let mut docs: Vec<u32> = wanted
            .iter()
            // Every query's needles are in the deduplicated list.
            .filter_map(|n| needles.binary_search(n).ok())
            .flat_map(|i| lists[i].iter().copied())
            .collect();
        docs.sort_unstable();
        docs.dedup();
        docs
    };
    type ClassRows = (
        Vec<u64>,
        Vec<u64>,
        Vec<u64>,
        Vec<u64>,
        Vec<u64>,
        usize,
        usize,
    );
    let mut by_class: BTreeMap<&str, ClassRows> = BTreeMap::new();
    for (q, sample) in corpus.samples.iter().enumerate() {
        let shipped_docs = read(src, &shipped[q], extra_docs)?.docs;
        let raw_docs = read(src, &raw[q], extra_docs)?.docs;
        let with = |admit: &[u32]| -> Vec<u32> {
            let mut docs = raw_docs.clone();
            docs.extend(
                admit
                    .iter()
                    .filter(|d| shipped_docs.binary_search(d).is_ok()),
            );
            docs.sort_unstable();
            docs.dedup();
            docs
        };
        let by_trigrams = with(&union(&admitted.by_trigrams, &per_query[q]));
        let exact = with(&union(&admitted.exact, &per_query[q]));
        let oracle = readable_oracle(corpus, q);
        let missing = |docs: &[u32]| {
            oracle
                .iter()
                .filter(|d| docs.binary_search(d).is_err())
                .count()
        };
        let scan = Instant::now();
        let mut hits = 0usize;
        for needle in &per_query[q] {
            hits += memchr::memmem::find_iter(&dictionary, needle).count();
        }
        let scan_ns = scan.elapsed().as_nanos() as u64;
        std::hint::black_box(hits);
        let bytes = |docs: &[u32]| counts(docs, &corpus.sizes, &corpus.readable)[0].bytes;
        let entry = by_class.entry(sample.class).or_default();
        entry.0.push(bytes(&raw_docs));
        entry.1.push(bytes(&by_trigrams));
        entry.2.push(bytes(&exact));
        entry.3.push(bytes(&shipped_docs));
        entry.4.push(scan_ns);
        entry.5 += missing(&by_trigrams);
        entry.6 += missing(&exact);
        if q % 25 == 24 {
            eprintln!("progress arm=residue query={}", q + 1);
        }
    }
    for (class, (raw, by_trigrams, exact, shipped, scan, miss_tri, miss_exact)) in by_class {
        println!(
            "residue class={class} shipped_B_median_p90={} raw_B_median_p90={} residue_trigram_B_median_p90={} residue_exact_B_median_p90={} missed_docs_trigram={miss_tri} missed_docs_exact={miss_exact} resident_scan_ms_median_p90={}",
            pair(shipped),
            pair(raw),
            pair(by_trigrams),
            pair(exact),
            ms(scan)
        );
    }
    Ok(())
}
