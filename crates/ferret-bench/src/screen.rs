//! S3 M1b, D68's screening stage (docs/S3.md § Bench plan): real binary
//! fuse filters per document, for terms and for trigrams separately,
//! against the postings they would replace, on matched workloads.
//!
//! `filter-screen <catalog-dir> <term-segment> <tri-dir> <out-dir>`:
//!
//! - **Key sets** are exact: each document's keys are read back out of the
//!   index under test by inverting its postings, so a filter holds exactly the
//!   keys the postings hold. Trigram keys are the three key bytes as a number;
//!   term keys are FNV-1a 64 of the term's bytes.
//! - **Filter files** are written to `<out-dir>` in DocId order, with a
//!   resident `u64` offset per document. A probe reads a filter of at most
//!   [`WHOLE_READ`] bytes in one positional read, and a larger one's header and
//!   then its three fingerprint bytes per probed key, one read each: what a
//!   page-faulting map of the file would touch.
//! - **Postings** run through the product's cursor path: `postings::Term` reads
//!   each key's lists, and a [`Cursor`] tree of `And`, `Or` and `Bits` counts
//!   the candidates.
//! - **Workloads**: unnarrowed (every live document); narrowed by a name atom
//!   standing in for 1%, 10% and 50% of documents, chosen as live DocIds
//!   divisible by 100, 10 and 2; and narrowed by a rare `text:` term (df ≤ 100)
//!   read from the term postings, which a filter layout probes per document of
//!   that term.
//! - **Evicted** is `posix_fadvise(DONTNEED)` on the files under test, all
//!   written by the bench, before the run; **warm** is a run after an untimed
//!   run of the same query. The catalog is not part of either side's cost.
//!
//! Nothing here prints a term, a key, a path or a byte of content.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ferret_index::segment::{Hit, Segment};
use ferret_index::{Certainty, Cursor, DocSet, postings};
use ferret_verify::{Grep, GrepFlags, Trigrams};

use crate::census::hashed_documents;
use crate::content_query::{Rng, quantile};
use crate::fuse::{Fuse, HEADER, Header};

/// A filter at most this large is read whole; a larger one by probe.
const WHOLE_READ: u64 = 4096;

// ── keys ──

/// A trigram key as a filter key.
pub(crate) fn trigram_number(key: &[u8]) -> u64 {
    key.iter().fold(0, |n, &b| n << 8 | u64::from(b))
}

/// A term as a filter key: FNV-1a 64.
pub(crate) fn term_hash(term: &[u8]) -> u64 {
    term.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// What a document must hold, over filter keys: [`Trigrams`] for a regex,
/// one key for a term.
#[derive(Clone, Debug)]
pub(crate) enum Need {
    All,
    None,
    Key(u64),
    And(Vec<Need>),
    Or(Vec<Need>),
}

impl Need {
    pub(crate) fn from_trigrams(query: &Trigrams) -> Self {
        match query {
            Trigrams::All => Self::All,
            Trigrams::None => Self::None,
            Trigrams::Key(key) => Self::Key(trigram_number(key)),
            Trigrams::And(parts) => Self::And(parts.iter().map(Self::from_trigrams).collect()),
            Trigrams::Or(parts) => Self::Or(parts.iter().map(Self::from_trigrams).collect()),
        }
    }

    /// Evaluates with `test` answering each key, short-circuiting: an
    /// `And` stops at its first No, an `Or` at its first Maybe.
    fn eval(&self, test: &mut impl FnMut(u64) -> bool) -> bool {
        match self {
            Self::All => true,
            Self::None => false,
            Self::Key(key) => test(*key),
            Self::And(parts) => parts.iter().all(|part| part.eval(test)),
            Self::Or(parts) => parts.iter().any(|part| part.eval(test)),
        }
    }
}

// ── exact per-document key sets ──

/// Every document's distinct keys, read out of the index's postings:
/// document `d`'s are `keys[offsets[d]..offsets[d + 1]]`, ascending.
pub(crate) struct DocKeys {
    pub(crate) offsets: Vec<u64>,
    pub(crate) keys: Vec<u64>,
}

impl DocKeys {
    pub(crate) fn of(&self, doc: u32) -> &[u64] {
        let doc = doc as usize;
        &self.keys[self.offsets[doc] as usize..self.offsets[doc + 1] as usize]
    }
}

/// Calls `visit(term, doc)` for every posting in `segments`.
fn postings_of(segments: &[Segment<File>], mut visit: impl FnMut(&[u8], u32)) -> crate::Result<()> {
    let mut docs = Vec::new();
    for segment in segments {
        let mut terms = segment.terms();
        while let Some((term, entry)) = terms.next_term()? {
            match segment.read(&entry)? {
                Hit::Single(doc) => visit(term, doc),
                Hit::Postings(list) => {
                    docs.clear();
                    list.decode(&mut docs);
                    for &doc in &docs {
                        visit(term, doc);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Postings per document: each document's distinct-key count.
pub(crate) fn key_counts(segments: &[Segment<File>], bound: u32) -> crate::Result<Vec<u64>> {
    let mut counts = vec![0u64; bound as usize];
    postings_of(segments, |_, doc| counts[doc as usize] += 1)?;
    Ok(counts)
}

/// Inverts `segments` back into per-document key sets: one pass to count,
/// one to fill, then each document's keys sorted and deduplicated (a term
/// hash can collide; trigram numbers cannot).
pub(crate) fn invert(
    segments: &[Segment<File>],
    bound: u32,
    key: fn(&[u8]) -> u64,
) -> crate::Result<DocKeys> {
    let counts = key_counts(segments, bound)?;
    let mut offsets = Vec::with_capacity(counts.len() + 1);
    let mut total = 0u64;
    offsets.push(0);
    for count in &counts {
        total += count;
        offsets.push(total);
    }
    let mut keys = vec![0u64; total as usize];
    let mut fill: Vec<u64> = offsets[..counts.len()].to_vec();
    postings_of(segments, |term, doc| {
        let at = &mut fill[doc as usize];
        keys[*at as usize] = key(term);
        *at += 1;
    })?;
    // Sort, deduplicate and compact in place.
    let mut write = 0usize;
    let mut compacted = Vec::with_capacity(offsets.len());
    compacted.push(0);
    for doc in 0..counts.len() {
        let (start, end) = (offsets[doc] as usize, offsets[doc + 1] as usize);
        keys[start..end].sort_unstable();
        let mut last = None;
        for i in start..end {
            if last != Some(keys[i]) {
                last = Some(keys[i]);
                keys[write] = keys[i];
                write += 1;
            }
        }
        compacted.push(write as u64);
    }
    keys.truncate(write);
    Ok(DocKeys {
        offsets: compacted,
        keys,
    })
}

// ── filter files ──

/// A filter layout's bytes, by part.
#[derive(Default)]
pub(crate) struct FilterBytes {
    pub(crate) documents: u64,
    pub(crate) keys: u64,
    pub(crate) fingerprints: u64,
    pub(crate) headers: u64,
    pub(crate) offsets: u64,
    pub(crate) build_seconds: f64,
}

impl FilterBytes {
    pub(crate) fn total(&self) -> u64 {
        self.fingerprints + self.headers + self.offsets
    }
}

/// Builds one filter per document of `live` and writes them to `path`.
/// Returns the resident offsets and the layout's bytes.
pub(crate) fn write_filters(
    keys: &DocKeys,
    live: &DocSet,
    path: &Path,
) -> crate::Result<(Vec<u64>, FilterBytes)> {
    let start = Instant::now();
    let mut out = BufWriter::new(File::create(path)?);
    let bound = keys.offsets.len() - 1;
    let mut offsets = Vec::with_capacity(bound + 1);
    let mut bytes = FilterBytes::default();
    let mut buffer = Vec::new();
    let mut at = 0u64;
    for doc in 0..bound as u32 {
        offsets.push(at);
        if !live.contains(doc) {
            continue;
        }
        let set = keys.of(doc);
        let filter = Fuse::build(set, u64::from(doc)).ok_or("a filter did not peel")?;
        buffer.clear();
        filter.write(&mut buffer);
        out.write_all(&buffer)?;
        at += buffer.len() as u64;
        bytes.documents += 1;
        bytes.keys += set.len() as u64;
        if !buffer.is_empty() {
            bytes.headers += HEADER as u64;
            bytes.fingerprints += (buffer.len() - HEADER) as u64;
        }
    }
    offsets.push(at);
    out.flush()?;
    bytes.offsets = (offsets.len() * size_of::<u64>()) as u64;
    bytes.build_seconds = start.elapsed().as_secs_f64();
    Ok((offsets, bytes))
}

/// Probes a filter file by positional reads.
pub(crate) struct FilterReader {
    file: File,
    offsets: Vec<u64>,
    buffer: Vec<u8>,
}

impl FilterReader {
    pub(crate) fn new(file: File, offsets: Vec<u64>) -> Self {
        Self {
            file,
            offsets,
            buffer: Vec::new(),
        }
    }

    /// Whether `doc`'s filter admits `need`.
    pub(crate) fn admits(&mut self, doc: u32, need: &Need) -> crate::Result<bool> {
        let (start, end) = (self.offsets[doc as usize], self.offsets[doc as usize + 1]);
        if start == end || matches!(need, Need::All) {
            // No filter: the document holds no key.
            return Ok(need.eval(&mut |_| false));
        }
        if end - start <= WHOLE_READ {
            self.buffer.resize((end - start) as usize, 0);
            self.file.read_exact_at(&mut self.buffer, start)?;
            let header = Header::parse(self.buffer[..HEADER].try_into()?);
            let fingerprints = &self.buffer[HEADER..];
            return Ok(need.eval(&mut |key| {
                let (f, [a, b, c]) = header.locate(key);
                f ^ fingerprints[a] ^ fingerprints[b] ^ fingerprints[c] == 0
            }));
        }
        let mut head = [0u8; HEADER];
        self.file.read_exact_at(&mut head, start)?;
        let header = Header::parse(&head);
        let base = start + HEADER as u64;
        let mut error = None;
        let file = &self.file;
        let admitted = need.eval(&mut |key| {
            let (f, slots) = header.locate(key);
            let mut x = f;
            for slot in slots {
                let mut byte = [0u8];
                if let Err(e) = file.read_exact_at(&mut byte, base + slot as u64) {
                    error.get_or_insert(e);
                }
                x ^= byte[0];
            }
            x == 0
        });
        match error {
            Some(e) => Err(e.into()),
            None => Ok(admitted),
        }
    }
}

// ── postings through the cursor path ──

/// Reads the lists `keys` names.
pub(crate) fn read_terms(
    segments: &[Segment<File>],
    keys: impl IntoIterator<Item = Vec<u8>>,
) -> crate::Result<HashMap<Vec<u8>, postings::Term>> {
    keys.into_iter()
        .map(|key| {
            let term = postings::Term::read(segments, &key)?;
            Ok((key, term))
        })
        .collect()
}

/// A trigram query's cursor tree over lists `terms` already holds.
pub(crate) fn trigram_cursor<'a>(
    query: &Trigrams,
    terms: &'a HashMap<Vec<u8>, postings::Term>,
    live: &'a DocSet,
) -> Cursor<'a> {
    match query {
        Trigrams::All => Cursor::bits(live, Certainty::Maybe),
        Trigrams::None => Cursor::Empty,
        Trigrams::Key(key) => term_cursor(&terms[key.as_slice()]),
        Trigrams::And(parts) => Cursor::and(
            parts
                .iter()
                .map(|part| trigram_cursor(part, terms, live))
                .collect(),
        ),
        Trigrams::Or(parts) => Cursor::or(
            parts
                .iter()
                .map(|part| trigram_cursor(part, terms, live))
                .collect(),
            live.bound(),
        ),
    }
}

pub(crate) fn term_cursor(term: &postings::Term) -> Cursor<'_> {
    if term.docs() == 0 {
        Cursor::Empty
    } else {
        Cursor::Postings(Box::new(term.cursor()))
    }
}

/// Every document `cursor` yields.
pub(crate) fn drain(mut cursor: Cursor<'_>) -> Vec<u32> {
    let mut docs = Vec::new();
    let mut target = 0;
    while let Some((doc, _)) = cursor.next_geq(target) {
        docs.push(doc);
        let Some(next) = doc.checked_add(1) else {
            break;
        };
        target = next;
    }
    docs
}

/// Drops `paths` from the page cache. Only files the bench wrote.
pub(crate) fn evict(paths: &[PathBuf]) -> crate::Result<()> {
    for path in paths {
        let file = File::open(path)?;
        rustix::fs::fadvise(&file, 0, None, rustix::fs::Advice::DontNeed)?;
    }
    Ok(())
}

/// Opens every segment file in `dir` with extension `ext`, in DocId order.
pub(crate) fn open_segments(
    dir: &Path,
    ext: &str,
) -> crate::Result<(Vec<PathBuf>, Vec<Segment<File>>)> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<_, _>>()?;
    paths.retain(|path| path.extension().is_some_and(|e| e == ext));
    let mut opened: Vec<(PathBuf, Segment<File>)> = paths
        .into_iter()
        .map(|path| Ok((path.clone(), Segment::open(File::open(&path)?)?)))
        .collect::<crate::Result<_>>()?;
    opened.sort_by_key(|(_, segment)| segment.info().first);
    if opened.is_empty() {
        return Err(format!("no .{ext} segments").into());
    }
    Ok(opened.into_iter().unzip())
}

// ── workloads ──

/// A seeded reservoir of `capacity` items.
struct Reservoir {
    capacity: usize,
    seen: u64,
    items: Vec<Vec<u8>>,
}

impl Reservoir {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            seen: 0,
            items: Vec::new(),
        }
    }

    fn offer(&mut self, rng: &mut Rng, item: &[u8]) {
        self.seen += 1;
        if self.items.len() < self.capacity {
            self.items.push(item.to_vec());
        } else {
            let at = rng.below(self.seen) as usize;
            if at < self.capacity {
                self.items[at] = item.to_vec();
            }
        }
    }
}

/// A lowercase ASCII word, so it needs no regex escaping.
fn plain(term: &[u8]) -> bool {
    term.first().is_some_and(u8::is_ascii_lowercase)
        && term
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Terms drawn from the term dictionary, seeded, by df class.
pub(crate) struct TermClasses {
    pub(crate) rare: Vec<Vec<u8>>,
    pub(crate) mid: Vec<Vec<u8>>,
    pub(crate) common: Vec<Vec<u8>>,
    /// Narrowing terms, df ≤ 100, drawn separately.
    pub(crate) narrow: Vec<Vec<u8>>,
    /// Regex seeds: df ≤ 10, plain, 5–30 bytes.
    literal: Vec<Vec<u8>>,
    /// Plain, at least 10 bytes: an identifier's two ends.
    identifier: Vec<Vec<u8>>,
    /// Mid df, plain, at least 4 bytes: alternatives and code-class words.
    word: Vec<Vec<u8>>,
}

/// Samples `per_class` of each class from a single-segment term index.
pub(crate) fn term_classes(terms: &Segment<File>, per_class: usize) -> crate::Result<TermClasses> {
    let mut rng = Rng(0x5eed_0b01);
    let mut rare = Reservoir::new(per_class);
    let mut mid = Reservoir::new(per_class);
    let mut narrow = Reservoir::new(per_class);
    let mut literal = Reservoir::new(per_class);
    let mut identifier = Reservoir::new(per_class);
    let mut word = Reservoir::new(per_class * 4);
    let mut top: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut cursor = terms.terms();
    while let Some((term, entry)) = cursor.next_term()? {
        let df = entry.df;
        if !plain(term) {
            continue;
        }
        if df <= 100 {
            rare.offer(&mut rng, term);
            narrow.offer(&mut rng, term);
        }
        if (1_000..=100_000).contains(&df) {
            mid.offer(&mut rng, term);
            if term.len() >= 4 {
                word.offer(&mut rng, term);
            }
        }
        if df <= 10 && (5..=30).contains(&term.len()) {
            literal.offer(&mut rng, term);
        }
        if term.len() >= 10 && term.len() <= 40 {
            identifier.offer(&mut rng, term);
        }
        if top.len() < per_class || df > top[top.len() - 1].0 {
            top.push((df, term.to_vec()));
            top.sort_unstable_by_key(|&(df, _)| std::cmp::Reverse(df));
            top.truncate(per_class);
        }
    }
    // A narrowing term must differ from the rare term it narrows.
    narrow.items.retain(|term| !rare.items.contains(term));
    Ok(TermClasses {
        rare: rare.items,
        mid: mid.items,
        common: top.into_iter().map(|(_, term)| term).collect(),
        narrow: narrow.items,
        literal: literal.items,
        identifier: identifier.items,
        word: word.items,
    })
}

/// The most frequent trigram keys that are three lowercase letters.
fn common_keys(trigrams: &[Segment<File>], count: usize) -> crate::Result<Vec<[u8; 3]>> {
    let mut df: HashMap<[u8; 3], u64> = HashMap::new();
    for segment in trigrams {
        let mut cursor = segment.terms();
        while let Some((key, entry)) = cursor.next_term()? {
            if let Ok(key) = <[u8; 3]>::try_from(key)
                && key.iter().all(u8::is_ascii_lowercase)
            {
                *df.entry(key).or_default() += u64::from(entry.df);
            }
        }
    }
    let mut keys: Vec<_> = df.into_iter().collect();
    keys.sort_unstable_by_key(|&(key, df)| (std::cmp::Reverse(df), key));
    Ok(keys.into_iter().take(count).map(|(key, _)| key).collect())
}

/// M5's regex classes (S3.md § Bench plan), seeded from the index's own
/// dictionaries, compiled by M3's derivation. `refused` is left out: it
/// derives `All`, so neither layout narrows it and there is nothing to
/// screen.
pub(crate) fn regex_classes(
    classes: &TermClasses,
    trigrams: &[Segment<File>],
    per_class: usize,
) -> crate::Result<Vec<(&'static str, Vec<Grep>)>> {
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    let mut rng = Rng(0x5eed_0b02);
    let words = &classes.word;
    let pick = |rng: &mut Rng| text(&words[rng.below(words.len() as u64) as usize]);
    let literal: Vec<String> = classes.literal.iter().map(|t| text(t)).collect();
    let identifier: Vec<String> = classes
        .identifier
        .iter()
        .map(|t| {
            let (head, tail) = (&t[..4], &t[t.len() - 4..]);
            format!(r"{}\w+{}", text(head), text(tail))
        })
        .collect();
    let (mut alternation, mut code) = (Vec::new(), Vec::new());
    if !words.is_empty() {
        for _ in 0..per_class {
            alternation.push(format!(
                "({}|{}|{})",
                pick(&mut rng),
                pick(&mut rng),
                pick(&mut rng)
            ));
            code.push(format!("[A-Z_]+{}", pick(&mut rng)));
        }
    }
    let broad: Vec<String> = common_keys(trigrams, per_class)?
        .iter()
        .map(|key| format!(r"{}\d+", text(key)))
        .collect();
    let flags = GrepFlags::default();
    let mut out = Vec::new();
    for (class, patterns) in [
        ("rare literal", literal),
        ("identifier pattern", identifier),
        ("alternation", alternation),
        ("broad", broad),
        ("code class", code),
    ] {
        let compiled = patterns
            .iter()
            .map(|pattern| flags.compile(pattern))
            .collect::<Result<Vec<_>, _>>()?;
        if !compiled.is_empty() {
            out.push((class, compiled));
        }
    }
    Ok(out)
}

/// Where a workload's documents come from before the atom under test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Narrow {
    /// Every live document.
    None,
    /// Live DocIds divisible by this: a name atom's stand-in.
    Every(u32),
    /// The documents of a rare `text:` term.
    RareTerm,
}

impl Narrow {
    pub(crate) const ALL: [Narrow; 5] = [
        Narrow::None,
        Narrow::Every(100),
        Narrow::Every(10),
        Narrow::Every(2),
        Narrow::RareTerm,
    ];

    pub(crate) fn label(self) -> String {
        match self {
            Narrow::None => "unnarrowed".into(),
            Narrow::Every(m) => format!("name 1/{m}"),
            Narrow::RareTerm => "rare term".into(),
        }
    }
}

/// One layout's run of one query.
#[derive(Clone, Copy, Default)]
pub(crate) struct Run {
    pub(crate) time: Duration,
    /// Documents the layout examined: probed for filters, narrowed
    /// documents for postings.
    pub(crate) probed: u64,
    pub(crate) candidates: u64,
}

/// One query: what the atom under test needs, with the narrowing term.
pub(crate) struct Case<'a> {
    pub(crate) keys: Vec<Vec<u8>>,
    pub(crate) trigrams: Option<&'a Trigrams>,
    pub(crate) need: Need,
    pub(crate) narrow: Narrow,
    pub(crate) narrow_term: &'a [u8],
}

/// The shared inputs of every run.
pub(crate) struct Index<'a> {
    pub(crate) live: &'a DocSet,
    pub(crate) names: [&'a DocSet; 3],
    /// The atom's postings: trigram or term segments.
    pub(crate) segments: &'a [Segment<File>],
    /// The term postings, for the rare-term narrowing.
    pub(crate) terms: &'a [Segment<File>],
}

impl Index<'_> {
    fn name_set(&self, modulus: u32) -> &DocSet {
        match modulus {
            100 => self.names[0],
            10 => self.names[1],
            _ => self.names[2],
        }
    }
}

/// The postings layout: read every list, then leapfrog.
pub(crate) fn run_postings(index: &Index<'_>, case: &Case<'_>) -> crate::Result<(Run, Vec<u32>)> {
    let start = Instant::now();
    let lists = read_terms(index.segments, case.keys.iter().cloned())?;
    let narrow = (case.narrow == Narrow::RareTerm)
        .then(|| postings::Term::read(index.terms, case.narrow_term))
        .transpose()?;
    let atom = match case.trigrams {
        Some(query) => trigram_cursor(query, &lists, index.live),
        None => term_cursor(&lists[&case.keys[0]]),
    };
    let (driver, probed) = match case.narrow {
        Narrow::None => (Cursor::bits(index.live, Certainty::Yes), index.live.len()),
        Narrow::Every(m) => {
            let set = index.name_set(m);
            (Cursor::bits(set, Certainty::Yes), set.len())
        }
        Narrow::RareTerm => {
            let term = narrow.as_ref().ok_or("no narrowing term")?;
            (
                Cursor::and(vec![
                    term_cursor(term),
                    Cursor::bits(index.live, Certainty::Yes),
                ]),
                term.docs() as u32,
            )
        }
    };
    let docs = drain(Cursor::and(vec![atom, driver]));
    let run = Run {
        time: start.elapsed(),
        probed: u64::from(probed),
        candidates: docs.len() as u64,
    };
    Ok((run, docs))
}

/// The filter layout: probe each narrowed document's filter.
pub(crate) fn run_filters(
    index: &Index<'_>,
    filters: &mut FilterReader,
    case: &Case<'_>,
) -> crate::Result<(Run, Vec<u32>)> {
    let start = Instant::now();
    let mut docs = Vec::new();
    let mut probed = 0u64;
    let mut probe = |doc: u32| -> crate::Result<()> {
        probed += 1;
        if filters.admits(doc, &case.need)? {
            docs.push(doc);
        }
        Ok(())
    };
    match case.narrow {
        Narrow::None => {
            for doc in index.live.range(0, index.live.bound()) {
                probe(doc)?;
            }
        }
        Narrow::Every(m) => {
            let set = index.name_set(m);
            for doc in set.range(0, set.bound()) {
                probe(doc)?;
            }
        }
        Narrow::RareTerm => {
            let term = postings::Term::read(index.terms, case.narrow_term)?;
            for doc in drain(Cursor::and(vec![
                term_cursor(&term),
                Cursor::bits(index.live, Certainty::Yes),
            ])) {
                probe(doc)?;
            }
        }
    }
    let run = Run {
        time: start.elapsed(),
        probed,
        candidates: docs.len() as u64,
    };
    Ok((run, docs))
}

/// Whether sorted `small` is a subset of sorted `large`.
pub(crate) fn subset(small: &[u32], large: &[u32]) -> bool {
    let mut at = 0;
    small.iter().all(|doc| {
        while at < large.len() && large[at] < *doc {
            at += 1;
        }
        at < large.len() && large[at] == *doc
    })
}

// ── the command ──

/// How many queries, and which, `filter-screen` runs.
pub(crate) struct Config {
    pub(crate) per_class: usize,
    /// Queries per class that also run evicted.
    pub(crate) evicted_per_class: usize,
}

pub(crate) const CONFIG: Config = Config {
    per_class: 20,
    evicted_per_class: 5,
};

/// One table row's samples.
#[derive(Default)]
pub(crate) struct Row {
    pub(crate) postings_warm: Vec<f64>,
    pub(crate) filters_warm: Vec<f64>,
    pub(crate) postings_evicted: Vec<f64>,
    pub(crate) filters_evicted: Vec<f64>,
    pub(crate) probed: Vec<f64>,
    pub(crate) postings_candidates: Vec<f64>,
    pub(crate) filter_candidates: Vec<f64>,
    /// Documents a filter admitted that postings did not, and documents it
    /// could have: false-positive numerator and denominator.
    pub(crate) false_positives: u64,
    pub(crate) negatives: u64,
}

/// What a run measured, for tests and the done-note.
pub(crate) struct Screen {
    pub(crate) trigram_bytes: (FilterBytes, u64),
    pub(crate) term_bytes: (FilterBytes, u64),
    pub(crate) rows: Vec<(String, String, String, Row)>,
}

fn ms(time: Duration) -> f64 {
    time.as_secs_f64() * 1e3
}

pub(crate) fn run(
    dir: &Path,
    term_segment: &Path,
    tri_dir: &Path,
    out: &Path,
    config: &Config,
) -> crate::Result<Screen> {
    if out.exists() {
        return Err("filter-screen wants a fresh output directory".into());
    }
    fs::create_dir(out)?;
    let catalog = crate::open_catalog(dir)?;
    let bound = catalog.next_doc().0;
    let live = DocSet::new(bound, hashed_documents(&catalog)?.iter().map(|d| d.doc.0));
    drop(catalog);
    let names: Vec<DocSet> = [100u32, 10, 2]
        .iter()
        .map(|&m| DocSet::new(bound, live.range(0, bound).filter(|doc| doc % m == 0)))
        .collect();
    // The term index is copied so that eviction only touches bench files.
    let term_copy = out.join("terms.seg");
    fs::copy(term_segment, &term_copy)?;
    let terms = vec![Segment::open(File::open(&term_copy)?)?];
    let (tri_paths, trigrams) = open_segments(tri_dir, "tri")?;
    println!(
        "filter_screen live_documents={} bound={bound} name_sets={}/{}/{} trigram_segments={}",
        live.len(),
        names[0].len(),
        names[1].len(),
        names[2].len(),
        trigrams.len()
    );

    let mut screen = Screen {
        trigram_bytes: (FilterBytes::default(), 0),
        term_bytes: (FilterBytes::default(), 0),
        rows: Vec::new(),
    };
    let classes = term_classes(&terms[0], config.per_class)?;
    let regexes = regex_classes(&classes, &trigrams, config.per_class)?;

    for structure in ["trigram", "term"] {
        let (segments, postings_paths) = if structure == "trigram" {
            (&trigrams, tri_paths.clone())
        } else {
            (&terms, vec![term_copy.clone()])
        };
        let postings_bytes: u64 = postings_paths
            .iter()
            .map(|p| fs::metadata(p).map(|m| m.len()))
            .sum::<Result<_, _>>()?;
        let key = if structure == "trigram" {
            trigram_number
        } else {
            term_hash
        };
        let start = Instant::now();
        let keys = invert(segments, bound, key)?;
        let invert_seconds = start.elapsed().as_secs_f64();
        crate::trigram::check_disk()?;
        let filter_path = out.join(format!("{structure}.filters"));
        let (offsets, bytes) = write_filters(&keys, &live, &filter_path)?;
        drop(keys);
        println!(
            "{structure}_filters documents={} keys={} fingerprints={} headers={} offsets={} total={} bits_per_key={:.4} postings_file_bytes={postings_bytes} filters_over_postings={:.4} invert_seconds={invert_seconds:.2} build_seconds={:.2}",
            bytes.documents,
            bytes.keys,
            bytes.fingerprints,
            bytes.headers,
            bytes.offsets,
            bytes.total(),
            bytes.total() as f64 * 8.0 / bytes.keys as f64,
            bytes.total() as f64 / postings_bytes as f64,
            bytes.build_seconds
        );
        let mut filters = FilterReader::new(File::open(&filter_path)?, offsets);
        let index = Index {
            live: &live,
            names: [&names[0], &names[1], &names[2]],
            segments,
            terms: &terms,
        };
        // The postings side's files: the atom's, plus the narrowing term's.
        let mut postings_files = postings_paths.clone();
        if structure == "trigram" {
            postings_files.push(term_copy.clone());
        }
        let filter_files = vec![filter_path.clone(), term_copy.clone()];

        let queries: Vec<(&str, Vec<Case<'_>>)> = if structure == "trigram" {
            regexes
                .iter()
                .map(|(class, greps)| {
                    let cases = greps
                        .iter()
                        .enumerate()
                        .map(|(i, grep)| Case {
                            keys: grep.trigrams().keys().iter().map(|k| k.to_vec()).collect(),
                            trigrams: Some(grep.trigrams()),
                            need: Need::from_trigrams(grep.trigrams()),
                            narrow: Narrow::None,
                            narrow_term: classes
                                .narrow
                                .get(i % classes.narrow.len().max(1))
                                .map_or(&[][..], Vec::as_slice),
                        })
                        .collect();
                    (*class, cases)
                })
                .collect()
        } else {
            [
                ("rare", &classes.rare),
                ("mid", &classes.mid),
                ("common", &classes.common),
            ]
            .into_iter()
            .filter(|(_, list)| !list.is_empty())
            .map(|(class, list)| {
                let cases = list
                    .iter()
                    .enumerate()
                    .map(|(i, term)| Case {
                        keys: vec![term.clone()],
                        trigrams: None,
                        need: Need::Key(term_hash(term)),
                        narrow: Narrow::None,
                        narrow_term: classes
                            .narrow
                            .get(i % classes.narrow.len().max(1))
                            .map_or(&[][..], Vec::as_slice),
                    })
                    .collect();
                (class, cases)
            })
            .collect()
        };

        println!();
        println!(
            "| structure | class | workload | queries | probed median | postings candidates median | filter candidates median | postings warm median ms | postings warm p90 ms | filters warm median ms | filters warm p90 ms | filters/postings warm | postings evicted median ms | filters evicted median ms | filters/postings evicted | evicted queries | false-positive rate |"
        );
        println!(
            "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"
        );
        for (class, cases) in queries {
            for narrow in Narrow::ALL {
                if narrow == Narrow::RareTerm && classes.narrow.is_empty() {
                    continue;
                }
                let mut row = Row::default();
                for (i, case) in cases.iter().enumerate() {
                    let case = Case {
                        keys: case.keys.clone(),
                        trigrams: case.trigrams,
                        need: case.need.clone(),
                        narrow,
                        narrow_term: case.narrow_term,
                    };
                    if i < config.evicted_per_class {
                        evict(&postings_files)?;
                        row.postings_evicted
                            .push(ms(run_postings(&index, &case)?.0.time));
                        evict(&filter_files)?;
                        row.filters_evicted
                            .push(ms(run_filters(&index, &mut filters, &case)?.0.time));
                    }
                    // Warm: the run after an untimed run of the same query.
                    run_postings(&index, &case)?;
                    let (posting, exact) = run_postings(&index, &case)?;
                    run_filters(&index, &mut filters, &case)?;
                    let (filter, admitted) = run_filters(&index, &mut filters, &case)?;
                    if !subset(&exact, &admitted) {
                        return Err(format!(
                            "a {structure} filter missed a document its postings hold ({class})"
                        )
                        .into());
                    }
                    row.postings_warm.push(ms(posting.time));
                    row.filters_warm.push(ms(filter.time));
                    row.probed.push(filter.probed as f64);
                    row.postings_candidates.push(posting.candidates as f64);
                    row.filter_candidates.push(filter.candidates as f64);
                    row.false_positives += filter.candidates - posting.candidates;
                    row.negatives += filter.probed - posting.candidates;
                }
                let q = |v: &mut Vec<f64>, at| quantile(v, at);
                let (pw, fw) = (
                    q(&mut row.postings_warm, 0.5),
                    q(&mut row.filters_warm, 0.5),
                );
                let (pe, fe) = (
                    q(&mut row.postings_evicted, 0.5),
                    q(&mut row.filters_evicted, 0.5),
                );
                println!(
                    "| {structure} | {class} | {} | {} | {:.0} | {:.0} | {:.0} | {pw:.3} | {:.3} | {fw:.3} | {:.3} | {:.2} | {pe:.3} | {fe:.3} | {:.2} | {} | {:.5} |",
                    narrow.label(),
                    cases.len(),
                    q(&mut row.probed, 0.5),
                    q(&mut row.postings_candidates, 0.5),
                    q(&mut row.filter_candidates, 0.5),
                    q(&mut row.postings_warm, 0.9),
                    q(&mut row.filters_warm, 0.9),
                    fw / pw,
                    fe / pe,
                    row.postings_evicted.len(),
                    row.false_positives as f64 / row.negatives.max(1) as f64,
                );
                screen
                    .rows
                    .push((structure.to_owned(), class.to_owned(), narrow.label(), row));
            }
        }
        println!();
        let layout = (bytes, postings_bytes);
        if structure == "trigram" {
            screen.trigram_bytes = layout;
        } else {
            screen.term_bytes = layout;
        }
        drop(filters);
        fs::remove_file(&filter_path)?;
    }
    fs::remove_file(&term_copy)?;
    Ok(screen)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::os::unix::ffi::OsStrExt;

    use super::*;
    use crate::support::Fixture;
    use ferret_index::BUFFER;
    use ferret_index::segment::{Inverter, TrigramInverter};
    use ferret_text::{Scratch, cap, tokenize};

    /// A small tree: rare identifiers, words shared by every document, and
    /// one long document of distinct pseudo-random words.
    pub(crate) fn files() -> Vec<(String, Vec<u8>)> {
        let mut files = Vec::new();
        let mut state = 0x1234_5678u64;
        // Digits and punctuation only: many distinct keys, and no term a
        // regex class could be seeded from.
        const SYMBOLS: &[u8] = b"0123456789!#$%&*+-/<=>?@^~";
        let mut long = Vec::new();
        for _ in 0..6000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            long.push(SYMBOLS[(state % SYMBOLS.len() as u64) as usize]);
        }
        files.push(("long.txt".to_owned(), long));
        for i in 0..30u32 {
            let text = format!(
                "the common words\nparsehttprequest{i}x handler theme 2024\nrareword{i}abc\n"
            );
            files.push((format!("f{i}.rs"), text.into_bytes()));
        }
        files
    }

    /// Commits `files` and writes one term segment and one `.tri` segment
    /// over every document, as the real index files are laid out.
    pub(crate) fn build(fixture: &Fixture, files: &[(String, Vec<u8>)]) -> (PathBuf, PathBuf) {
        let named: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        let catalog = fixture.commit(&named, &[]);
        let docs = hashed_documents(&catalog).unwrap();
        let mut term = Inverter::new(BUFFER);
        let mut tri = TrigramInverter::new();
        let mut scratch = Scratch::default();
        let mut path = Vec::new();
        for doc in &docs {
            path.clear();
            catalog.path(doc.name, &mut path);
            let bytes = fs::read(Path::new(std::ffi::OsStr::from_bytes(&path))).unwrap();
            tokenize(&bytes, &mut scratch, |token| {
                term.add(doc.doc.0, cap(token.bytes)).unwrap()
            });
            tri.add(doc.doc.0, &bytes).unwrap();
        }
        let (first, last) = (docs[0].doc.0, docs[docs.len() - 1].doc.0);
        let index = fixture.out("index");
        fs::create_dir(&index).unwrap();
        let term_path = index.join("terms.seg");
        term.drain_into(first, last)
            .unwrap()
            .finish(&mut File::create(&term_path).unwrap())
            .unwrap();
        let tri_dir = fixture.out("tri");
        fs::create_dir(&tri_dir).unwrap();
        tri.drain_into(first, last)
            .unwrap()
            .finish(&mut File::create(tri_dir.join("seg.tri")).unwrap())
            .unwrap();
        (term_path, tri_dir)
    }

    /// The inversion reproduces each document's own key set, computed
    /// independently by the inverter from the document's bytes.
    #[test]
    fn inversion_gives_each_document_its_exact_keys() {
        let fixture = Fixture::new("screen-invert");
        let files = files();
        let (_, tri_dir) = build(&fixture, &files);
        let (_, segments) = open_segments(&tri_dir, "tri").unwrap();
        let catalog = crate::open_catalog(&fixture.catalog_dir()).unwrap();
        let docs = hashed_documents(&catalog).unwrap();
        let keys = invert(&segments, catalog.next_doc().0, trigram_number).unwrap();
        let mut path = Vec::new();
        for doc in &docs {
            path.clear();
            catalog.path(doc.name, &mut path);
            let bytes = fs::read(Path::new(std::ffi::OsStr::from_bytes(&path))).unwrap();
            let mut own = Vec::new();
            TrigramInverter::new()
                .add_with(0, &bytes, |key| own.push(trigram_number(&key)))
                .unwrap();
            own.sort_unstable();
            assert_eq!(keys.of(doc.doc.0), own.as_slice());
        }
    }

    /// End to end over the fixture: both structures' bytes and every
    /// workload. A filter that missed a posting fails the run itself.
    #[test]
    fn filter_screen_runs_end_to_end() {
        let fixture = Fixture::new("screen");
        let files = files();
        let (term_path, tri_dir) = build(&fixture, &files);
        let out = fixture.out("screen-out");
        let config = Config {
            per_class: 5,
            evicted_per_class: 1,
        };
        let screen = run(&fixture.catalog_dir(), &term_path, &tri_dir, &out, &config).unwrap();
        let (_, segments) = open_segments(&tri_dir, "tri").unwrap();
        let pairs: u64 = segments.iter().map(|s| s.info().pairs).sum();
        assert_eq!(screen.trigram_bytes.0.keys, pairs);
        assert_eq!(screen.trigram_bytes.0.documents, files.len() as u64);
        assert!(screen.term_bytes.0.keys > 0 && screen.term_bytes.1 > 0);
        // Every rare literal is a word some document holds, so postings
        // find it, unnarrowed, and so must the filters.
        let literal = screen
            .rows
            .iter()
            .find(|(s, c, w, _)| s == "trigram" && c == "rare literal" && w == "unnarrowed")
            .expect("a rare literal row");
        assert!(literal.3.postings_candidates.iter().all(|&n| n >= 1.0));
        let narrowed = screen
            .rows
            .iter()
            .find(|(s, c, w, _)| s == "trigram" && c == "rare literal" && w == "name 1/2")
            .unwrap();
        assert!(narrowed.3.probed.iter().all(|&n| n < files.len() as f64));
        assert!(!out.join("trigram.filters").exists());
    }
}
