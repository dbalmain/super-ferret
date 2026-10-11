//! `ferret-bench rank-fp <catalog-dir> <out-dir>`: a fully compacted static
//! term index, measured two ways.
//!
//! Both arms keep every term with global document frequency at least 2 in one
//! front-coded dictionary segment, the shape left after a merge reassigns
//! ranks. A term seen in one document is rare, and that is the only
//! difference:
//!
//! - **32-bit** stores codec 2's sparse `(fingerprint, doc)` records. A hit is
//!   Maybe, so the lookup reads each candidate file and runs [`TextMatcher`].
//! - **20-bit** takes the low 20 bits of that same BLAKE3 fingerprint as a
//!   bucket. Bucket `b` is the set of documents that hold any rare term in the
//!   bucket. A dense `u32` array, one past the last address, gives each
//!   bucket's byte range in a postings blob encoded like a segment list
//!   (`vbyte` count, then `pfor128skip`). Every lookup of a rare or absent term
//!   reads that bucket and verifies every document in it.
//!
//! Dictionary hits are exact (`exact_under_cap`) and are not verified: the
//! string is in the dictionary. The follow delta both designs would keep in
//! memory is not part of this index; these files are the static side only.
//!
//! Sampled queries are ASCII tokens shorter than the cap, drawn uniformly
//! from the readable documents. **Evicted** drops the bytes that probe or
//! verify is about to read (`posix_fadvise` `DONTNEED`) and then times one
//! run. **Warm** is the run immediately after. Fifty queries per class.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::hint::black_box;
use std::io::{self, Write as _};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::{Duration, Instant};

use ferret_index::segment::{FINGERPRINT_CODEC, Hit, Segment, Writer, fingerprint};
use ferret_text::{Scratch, cap, exact_under_cap, tokenize};
use ferret_verify::{Text, TextMatcher};
use intpack::pfor128skip;
use intpack::vbyte;

use crate::census::hashed_documents;
use crate::content_query::Rng;
use crate::terms::read_documents;

/// Queries in each class.
const QUERIES: usize = 50;
/// Low bits of [`fingerprint`] that select a 20-bit bucket.
const BITS: u32 = 20;
const BUCKETS: usize = 1 << BITS;
const MASK: u32 = (1 << BITS) - 1;
/// Bucket-size histogram. A count past this is overflow, tracked apart.
const HIST: usize = 4096;

/// One term's documents while the census streams in DocId order.
struct Acc {
    last: u32,
    /// `None` while the term has been seen in only `last`.
    docs: Option<Vec<u32>>,
}

/// A sampled term. `doc` is the hapax's document; dictionary samples do not
/// read it back.
struct Picked {
    term: Vec<u8>,
    doc: u32,
    df: u32,
}

struct Reservoir {
    seen: u64,
    items: Vec<Picked>,
}

impl Reservoir {
    fn new() -> Self {
        Self {
            seen: 0,
            items: Vec::with_capacity(QUERIES),
        }
    }

    fn offer(&mut self, rng: &mut Rng, term: &[u8], doc: u32, df: u32) {
        self.seen += 1;
        let picked = || Picked {
            term: term.to_vec(),
            doc,
            df,
        };
        if self.items.len() < QUERIES {
            self.items.push(picked());
            return;
        }
        let at = rng.below(self.seen) as usize;
        if at < QUERIES {
            self.items[at] = picked();
        }
    }
}

/// A query whose `term` is the dictionary key and the whole token of `text`.
struct Query {
    term: Vec<u8>,
    text: Text,
    /// The hapax's document, when the term is a known rare hit.
    expect: Option<u32>,
    df: u32,
}

/// One finished query.
struct Obs {
    probe_evicted: Duration,
    probe_warm: Duration,
    verify_evicted: Duration,
    verify_warm: Duration,
    candidates: usize,
    bytes: u64,
    hit: bool,
    read_errors: u64,
}

struct BucketStats {
    docs: u64,
    max: u64,
    /// Documents per bucket. The last slot counts buckets of size >= [`HIST`].
    hist: Vec<u64>,
    overflow_max: u64,
}

impl BucketStats {
    fn new() -> Self {
        Self {
            docs: 0,
            max: 0,
            hist: vec![0; HIST + 1],
            overflow_max: 0,
        }
    }

    fn add(&mut self, n: usize) {
        self.docs += n as u64;
        self.max = self.max.max(n as u64);
        if n < HIST {
            self.hist[n] += 1;
        } else {
            self.hist[HIST] += 1;
            self.overflow_max = self.overflow_max.max(n as u64);
        }
    }

    /// Smallest size that covers at least fraction `p` of every bucket,
    /// empty ones included. A lookup always consults exactly one bucket.
    fn percentile(&self, p: f64) -> u64 {
        let want = (p * BUCKETS as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (n, &count) in self.hist.iter().enumerate() {
            seen += count;
            if seen >= want {
                return if n == HIST {
                    self.overflow_max
                } else {
                    n as u64
                };
            }
        }
        self.max
    }
}

/// `rank-fp <catalog-dir> <out-dir>`.
pub(crate) fn run(catalog_dir: &Path, out_dir: &Path) -> crate::Result<()> {
    let started = Instant::now();
    fs::create_dir_all(out_dir)?;
    let catalog = crate::open_catalog(catalog_dir)?;
    let docs = hashed_documents(&catalog)?;
    let mut paths: HashMap<u32, Vec<u8>> = HashMap::with_capacity(docs.len());
    for doc in &docs {
        let mut path = Vec::new();
        catalog.path(doc.name, &mut path);
        paths.insert(doc.doc.0, path);
    }

    eprintln!(
        "rank-fp: tokenizing {} catalogued documents (rss {} KiB)",
        docs.len(),
        rss_kib()
    );
    let mut terms: HashMap<Vec<u8>, Acc> = HashMap::with_capacity(8_000_000);
    let mut content_bytes = 0u64;
    let mut min_doc = u32::MAX;
    let mut max_doc = 0u32;
    let mut readable = 0u64;
    let mut scratch = Scratch::default();
    let scan_started = Instant::now();
    let skipped = read_documents(&catalog, &docs, |doc, bytes| {
        readable += 1;
        content_bytes += bytes.len() as u64;
        let id = doc.doc.0;
        min_doc = min_doc.min(id);
        max_doc = max_doc.max(id);
        if readable.is_multiple_of(10_000) {
            eprintln!(
                "rank-fp: {readable} documents, {} terms, {:.0}s, rss {} KiB",
                terms.len(),
                scan_started.elapsed().as_secs_f64(),
                rss_kib()
            );
        }
        tokenize(bytes, &mut scratch, |token| {
            observe(&mut terms, cap(token.bytes), id);
        });
    });
    if readable == 0 {
        return Err("no readable documents".into());
    }
    eprintln!(
        "rank-fp: census done, {} terms, {:.1}s, rss {} KiB",
        terms.len(),
        scan_started.elapsed().as_secs_f64(),
        rss_kib()
    );

    let (rare_picks, dict_picks) = sample(&terms);
    check_samples(&paths, &rare_picks)?;
    check_samples_parse(&dict_picks)?;
    let absent_terms = absent_like(&rare_picks, &terms);
    let distinct = terms.len() as u64;
    let split = split(terms);
    let hapax = split.hapax;
    let listed = split.listed;
    let mut dict = split.dict;
    let mut records = split.records;
    drop(scratch);
    eprintln!(
        "rank-fp: split into {} dictionary terms and {} hapaxes, rss {} KiB",
        dict.len(),
        hapax,
        rss_kib()
    );

    dict.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let dict_path = out_dir.join("dict.seg");
    let dict_sizes = write_dictionary(&dict_path, min_doc, max_doc, &dict)?;
    drop(dict);
    eprintln!(
        "rank-fp: dictionary {} bytes, rss {} KiB",
        dict_sizes.total(),
        rss_kib()
    );

    records.sort_unstable();
    records.dedup();
    let (multi_runs, extra_docs) = multi_fingerprint_runs(&records);
    let rare32_path = out_dir.join("rare32.seg");
    let rare32_sizes = write_fingerprints(&rare32_path, min_doc, max_doc, &records)?;
    let (addrs, blob, buckets) = encode_buckets(&mut records, min_doc)?;
    let rare20_path = out_dir.join("rare20.postings");
    fs::write(&rare20_path, &blob)?;
    eprintln!(
        "rank-fp: rare terms written, 32-bit file {} bytes, 20-bit blob {} bytes, rss {} KiB",
        rare32_sizes.total(),
        blob.len(),
        rss_kib()
    );

    let dict_seg = Segment::open(File::open(&dict_path)?)?;
    let rare32 = Segment::open(File::open(&rare32_path)?)?;
    let rare20 = File::open(&rare20_path)?;
    let dict_queries = queries(&dict_picks, false)?;
    let rare_queries = queries(&rare_picks, true)?;
    let absent_queries = queries(
        &absent_terms
            .into_iter()
            .map(|term| Picked {
                term,
                doc: 0,
                df: 0,
            })
            .collect::<Vec<_>>(),
        false,
    )?;

    let mut report = String::new();
    write_sizes(
        &mut report,
        &Sizes {
            readable,
            skipped,
            content_bytes,
            distinct,
            hapax,
            listed,
            records: rare32.fingerprint_records(),
            multi_runs,
            extra_docs,
            dict: dict_sizes,
            rare32: rare32_sizes,
            dict_resident: dict_seg.resident_bytes(),
            rare32_resident: rare32.resident_bytes(),
            blob: blob.len(),
            buckets: &buckets,
        },
    );
    let sizes_end = report.len();
    print!("{report}");
    fs::write(out_dir.join("report.txt"), &report)?;
    let _ = std::io::stdout().flush();
    eprintln!("rank-fp: sizes above; timing lookups");

    let dictionary = measure_dictionary(&dict_path, &dict_seg, &dict_queries)?;
    let rare32_obs = measure_rare(
        &dict_path,
        &rare32_path,
        |term| probe32(&dict_seg, &rare32, term),
        &paths,
        &rare_queries,
    )?;
    let absent32_obs = measure_rare(
        &dict_path,
        &rare32_path,
        |term| probe32(&dict_seg, &rare32, term),
        &paths,
        &absent_queries,
    )?;
    let rare20_obs = measure_rare(
        &dict_path,
        &rare20_path,
        |term| probe20(&dict_seg, &rare20, &addrs, min_doc, term),
        &paths,
        &rare_queries,
    )?;
    let absent20_obs = measure_rare(
        &dict_path,
        &rare20_path,
        |term| probe20(&dict_seg, &rare20, &addrs, min_doc, term),
        &paths,
        &absent_queries,
    )?;

    push_class(&mut report, "dictionary", "shared", &dictionary);
    push_class(&mut report, "rare hit", "32-bit", &rare32_obs);
    push_class(&mut report, "rare hit", "20-bit", &rare20_obs);
    push_class(&mut report, "absent", "32-bit", &absent32_obs);
    push_class(&mut report, "absent", "20-bit", &absent20_obs);
    let _ = writeln!(
        report,
        "wall_seconds: {:.1}",
        started.elapsed().as_secs_f64()
    );
    let _ = writeln!(report, "rss_kib: {}", rss_kib());
    print!("{}", &report[sizes_end..]);
    fs::write(out_dir.join("report.txt"), &report)?;
    let _ = std::io::stdout().flush();
    Ok(())
}

fn observe(terms: &mut HashMap<Vec<u8>, Acc>, key: &[u8], doc: u32) {
    if let Some(acc) = terms.get_mut(key) {
        if acc.last == doc {
            return;
        }
        match acc.docs.as_mut() {
            Some(docs) => docs.push(doc),
            None => acc.docs = Some(vec![acc.last, doc]),
        }
        acc.last = doc;
        return;
    }
    terms.insert(
        key.to_vec(),
        Acc {
            last: doc,
            docs: None,
        },
    );
}

fn ascii_token(term: &[u8]) -> bool {
    !term.is_empty() && term.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

fn sample(terms: &HashMap<Vec<u8>, Acc>) -> (Vec<Picked>, Vec<Picked>) {
    let mut rng = Rng(0x0C0F_FEE0_0D15_C0FF);
    let mut rare = Reservoir::new();
    let mut dict = Reservoir::new();
    for (term, acc) in terms {
        if !ascii_token(term) || !exact_under_cap(term) {
            continue;
        }
        match &acc.docs {
            None => rare.offer(&mut rng, term, acc.last, 1),
            Some(docs) => dict.offer(&mut rng, term, acc.last, docs.len() as u32),
        }
    }
    (rare.items, dict.items)
}

/// One-byte mutations of rare samples that are not themselves terms.
fn absent_like(rare: &[Picked], terms: &HashMap<Vec<u8>, Acc>) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(rare.len());
    for (s, picked) in rare.iter().enumerate() {
        for k in 0..36 {
            let mut term = picked.term.clone();
            let i = term.len() - 1;
            let n = ((s + k) % 36) as u8;
            term[i] = if n < 26 { b'a' + n } else { b'0' + (n - 26) };
            if term != picked.term && !terms.contains_key(term.as_slice()) {
                out.push(term);
                break;
            }
        }
    }
    out
}

struct Split {
    dict: Vec<(Vec<u8>, Vec<u32>)>,
    records: Vec<(u32, u32)>,
    hapax: u64,
    listed: u64,
}

fn split(terms: HashMap<Vec<u8>, Acc>) -> Split {
    let mut dict = Vec::new();
    let mut records = Vec::new();
    let mut hapax = 0u64;
    let mut listed = 0u64;
    for (term, acc) in terms {
        match acc.docs {
            Some(docs) => {
                listed += docs.len() as u64;
                dict.push((term, docs));
            }
            None => {
                hapax += 1;
                records.push((fingerprint(&term), acc.last));
            }
        }
    }
    Split {
        dict,
        records,
        hapax,
        listed,
    }
}

fn write_dictionary(
    path: &Path,
    first: u32,
    last: u32,
    dict: &[(Vec<u8>, Vec<u32>)],
) -> crate::Result<ferret_index::segment::Sizes> {
    let mut writer = Writer::new(first, last)?;
    let started = Instant::now();
    for (i, (term, docs)) in dict.iter().enumerate() {
        writer.push(term, docs)?;
        if i > 0 && i.is_multiple_of(1_000_000) {
            eprintln!(
                "rank-fp: dictionary {i} terms, {:.0}s, rss {} KiB",
                started.elapsed().as_secs_f64(),
                rss_kib()
            );
        }
    }
    let mut file = File::create(path)?;
    Ok(writer.finish(&mut file)?)
}

fn write_fingerprints(
    path: &Path,
    first: u32,
    last: u32,
    records: &[(u32, u32)],
) -> crate::Result<ferret_index::segment::Sizes> {
    let mut writer = Writer::with_codec(first, last, FINGERPRINT_CODEC)?;
    for &(fp, doc) in records {
        writer.push_record(fp, doc)?;
    }
    let mut file = File::create(path)?;
    Ok(writer.finish(&mut file)?)
}

/// How many distinct fingerprints point at more than one document.
fn multi_fingerprint_runs(records: &[(u32, u32)]) -> (u64, u64) {
    let mut runs = 0u64;
    let mut extra = 0u64;
    let mut i = 0;
    while i < records.len() {
        let fp = records[i].0;
        let start = i;
        i += 1;
        while i < records.len() && records[i].0 == fp {
            i += 1;
        }
        let n = i - start;
        if n > 1 {
            runs += 1;
            extra += (n - 1) as u64;
        }
    }
    (runs, extra)
}

/// Sorts `records` by `(fingerprint & MASK, doc)` and encodes one list per
/// bucket. The address array has [`BUCKETS`] + 1 entries: bucket `b` is the
/// half-open byte range `addrs[b]..addrs[b + 1]`.
fn encode_buckets(
    records: &mut [(u32, u32)],
    first: u32,
) -> crate::Result<(Vec<u32>, Vec<u8>, BucketStats)> {
    records.sort_unstable_by_key(|record| (record.0 & MASK, record.1));
    let mut addrs = vec![0u32; BUCKETS + 1];
    let mut blob = Vec::new();
    let mut stats = BucketStats::new();
    let mut cursor = 0;
    for (bucket, addr) in addrs.iter_mut().enumerate().take(BUCKETS) {
        *addr = u32::try_from(blob.len()).map_err(|_| "20-bit postings exceed 4 GiB")?;
        if cursor >= records.len() || (records[cursor].0 & MASK) != bucket as u32 {
            stats.add(0);
            continue;
        }
        let mut docs = Vec::new();
        while cursor < records.len() && (records[cursor].0 & MASK) == bucket as u32 {
            let doc = records[cursor].1;
            if docs.last() != Some(&doc) {
                docs.push(doc);
            }
            cursor += 1;
        }
        stats.add(docs.len());
        let relative: Vec<u32> = docs.into_iter().map(|doc| doc - first).collect();
        vbyte::put(relative.len() as u32, &mut blob);
        pfor128skip::encode_sorted(&relative, &mut blob);
    }
    addrs[BUCKETS] = u32::try_from(blob.len()).map_err(|_| "20-bit postings exceed 4 GiB")?;
    if cursor != records.len() {
        return Err("20-bit bucket scan left records behind".into());
    }
    Ok((addrs, blob, stats))
}

fn decode_list(buf: &[u8], first: u32) -> Vec<u32> {
    let mut pos = 0;
    let n = vbyte::take(buf, &mut pos) as usize;
    let mut docs = Vec::with_capacity(n);
    pfor128skip::decode_sorted(n, &buf[pos..], &mut docs);
    for doc in &mut docs {
        *doc = doc.wrapping_add(first);
    }
    docs
}

/// The sampled rare term is a whole token, and the real matcher finds it in
/// its own file. A failure here means the timed verification would be
/// measuring the wrong predicate.
fn check_samples(paths: &HashMap<u32, Vec<u8>>, picks: &[Picked]) -> crate::Result<()> {
    let mut matcher = TextMatcher::new();
    for picked in picks {
        let text = query_text(&picked.term)?;
        let Some(path) = paths.get(&picked.doc) else {
            return Err("sampled document has no path".into());
        };
        let file = fs::read(Path::new(std::ffi::OsStr::from_bytes(path)))?;
        if !matcher.is_match(&text, &file) {
            return Err("rare sample did not verify in its own file".into());
        }
    }
    Ok(())
}

fn check_samples_parse(picks: &[Picked]) -> crate::Result<()> {
    for picked in picks {
        query_text(&picked.term)?;
    }
    Ok(())
}

fn query_text(term: &[u8]) -> crate::Result<Text> {
    let Some(text) = Text::new(term, false) else {
        return Err("sampled term is not a query".into());
    };
    if text.whole() != Some(term) {
        return Err("sampled term is not the query's whole token".into());
    }
    Ok(text)
}

fn queries(picks: &[Picked], rare_hit: bool) -> crate::Result<Vec<Query>> {
    let mut out = Vec::with_capacity(picks.len());
    for picked in picks {
        out.push(Query {
            text: query_text(&picked.term)?,
            term: picked.term.clone(),
            expect: rare_hit.then_some(picked.doc),
            df: picked.df,
        });
    }
    if out.is_empty() {
        return Err("no sampled queries".into());
    }
    Ok(out)
}

fn probe32(dict: &Segment<File>, rare: &Segment<File>, term: &[u8]) -> crate::Result<Vec<u32>> {
    // A rare or absent term misses the df >= 2 dictionary. The miss still
    // reads one dictionary block: that is part of the lookup.
    let found = dict.lookup(term)?;
    black_box(found.is_some());
    Ok(rare.fingerprint_docs(term)?)
}

fn probe20(
    dict: &Segment<File>,
    postings: &File,
    addrs: &[u32],
    first: u32,
    term: &[u8],
) -> crate::Result<Vec<u32>> {
    let found = dict.lookup(term)?;
    black_box(found.is_some());
    let bucket = (fingerprint(term) & MASK) as usize;
    let start = addrs[bucket];
    let end = addrs[bucket + 1];
    if start == end {
        return Ok(Vec::new());
    }
    let mut buf = vec![0u8; (end - start) as usize];
    postings.read_exact_at(&mut buf, u64::from(start))?;
    Ok(decode_list(&buf, first))
}

fn measure_dictionary(
    dict_path: &Path,
    dict: &Segment<File>,
    queries: &[Query],
) -> crate::Result<Vec<Obs>> {
    let mut out = Vec::with_capacity(queries.len());
    for query in queries {
        evict(dict_path)?;
        let (probe_evicted, n) = time_lookup(dict, &query.term)?;
        let (probe_warm, again) = time_lookup(dict, &query.term)?;
        if n == 0 || n != again || n != query.df {
            return Err("dictionary lookup did not return the term's documents".into());
        }
        out.push(Obs {
            probe_evicted,
            probe_warm,
            verify_evicted: Duration::ZERO,
            verify_warm: Duration::ZERO,
            candidates: n as usize,
            bytes: 0,
            hit: true,
            read_errors: 0,
        });
    }
    Ok(out)
}

fn time_lookup(dict: &Segment<File>, term: &[u8]) -> crate::Result<(Duration, u32)> {
    let started = Instant::now();
    let hit = dict.lookup(term)?;
    let n = match &hit {
        Some(Hit::Single(doc)) => {
            black_box(doc);
            1
        }
        Some(Hit::Postings(list)) => list.len(),
        None => 0,
    };
    black_box(n);
    Ok((started.elapsed(), n))
}

fn measure_rare(
    dict_path: &Path,
    rare_path: &Path,
    mut probe: impl FnMut(&[u8]) -> crate::Result<Vec<u32>>,
    paths: &HashMap<u32, Vec<u8>>,
    queries: &[Query],
) -> crate::Result<Vec<Obs>> {
    let mut matcher = TextMatcher::new();
    let mut out = Vec::with_capacity(queries.len());
    for query in queries {
        evict(dict_path)?;
        evict(rare_path)?;
        let started = Instant::now();
        let docs = probe(&query.term)?;
        let probe_evicted = started.elapsed();
        let started = Instant::now();
        let again = probe(&query.term)?;
        let probe_warm = started.elapsed();
        black_box(again.len());
        for &doc in &docs {
            evict_doc(paths, doc);
        }
        let started = Instant::now();
        let (bytes, hit, read_errors) = verify(paths, &mut matcher, &query.text, &docs);
        let verify_evicted = started.elapsed();
        let started = Instant::now();
        let (_, warm_hit, _) = verify(paths, &mut matcher, &query.text, &docs);
        let verify_warm = started.elapsed();
        black_box(warm_hit);
        if let Some(expect) = query.expect
            && !docs.contains(&expect)
        {
            return Err("rare-term candidate list missed its document".into());
        }
        out.push(Obs {
            probe_evicted,
            probe_warm,
            verify_evicted,
            verify_warm,
            candidates: docs.len(),
            bytes,
            hit,
            read_errors,
        });
    }
    Ok(out)
}

fn verify(
    paths: &HashMap<u32, Vec<u8>>,
    matcher: &mut TextMatcher,
    text: &Text,
    docs: &[u32],
) -> (u64, bool, u64) {
    let mut bytes = 0u64;
    let mut hit = false;
    let mut errors = 0u64;
    for &doc in docs {
        let Some(path) = paths.get(&doc) else {
            errors += 1;
            continue;
        };
        match fs::read(Path::new(std::ffi::OsStr::from_bytes(path))) {
            Ok(file) => {
                bytes += file.len() as u64;
                hit |= matcher.is_match(text, &file);
            }
            Err(_) => errors += 1,
        }
    }
    (bytes, hit, errors)
}

fn evict(path: &Path) -> io::Result<()> {
    let file = File::open(path)?;
    rustix::fs::fadvise(&file, 0, None, rustix::fs::Advice::DontNeed)?;
    Ok(())
}

fn evict_doc(paths: &HashMap<u32, Vec<u8>>, doc: u32) {
    let Some(path) = paths.get(&doc) else {
        return;
    };
    let Ok(file) = File::open(Path::new(std::ffi::OsStr::from_bytes(path))) else {
        return;
    };
    let _ = rustix::fs::fadvise(&file, 0, None, rustix::fs::Advice::DontNeed);
}

struct Sizes<'a> {
    readable: u64,
    skipped: u64,
    content_bytes: u64,
    distinct: u64,
    hapax: u64,
    listed: u64,
    records: u64,
    multi_runs: u64,
    extra_docs: u64,
    dict: ferret_index::segment::Sizes,
    rare32: ferret_index::segment::Sizes,
    dict_resident: usize,
    rare32_resident: usize,
    blob: usize,
    buckets: &'a BucketStats,
}

fn write_sizes(out: &mut String, sizes: &Sizes<'_>) {
    let address = (BUCKETS + 1) * 4;
    {
        let mut line = |key: &str, value: u64| {
            let _ = writeln!(out, "{key}: {value}");
        };
        line("readable_documents", sizes.readable);
        line("skipped_documents", sizes.skipped);
        line("content_bytes", sizes.content_bytes);
        line("distinct_terms", sizes.distinct);
        line("hapax_terms", sizes.hapax);
        line("dictionary_terms", sizes.dict.terms);
        line("dictionary_postings", sizes.listed);
        line("dictionary_file_bytes", sizes.dict.total());
        line("dictionary_blocks_bytes", sizes.dict.blocks);
        line("dictionary_postings_bytes", sizes.dict.postings);
        line("dictionary_index_bytes", sizes.dict.index);
        line("dictionary_term_bytes", sizes.dict.term_bytes);
        line("dictionary_resident_bytes", sizes.dict_resident as u64);
        line("rare32_records", sizes.records);
        line("rare32_file_bytes", sizes.rare32.total());
        line("rare32_fingerprint_bytes", sizes.rare32.fingerprints);
        line(
            "rare32_fingerprint_index_bytes",
            sizes.rare32.fingerprint_index,
        );
        line("rare32_resident_bytes", sizes.rare32_resident as u64);
        line("rare32_multi_doc_fingerprints", sizes.multi_runs);
        line("rare32_extra_collision_docs", sizes.extra_docs);
        line("rare20_buckets", BUCKETS as u64);
        line("rare20_address_bytes", address as u64);
        line("rare20_postings_bytes", sizes.blob as u64);
        line("rare20_docs", sizes.buckets.docs);
        line("rare20_max_docs", sizes.buckets.max);
        line("rare20_docs_p50", sizes.buckets.percentile(0.50));
        line("rare20_docs_p90", sizes.buckets.percentile(0.90));
        line("rare20_docs_p99", sizes.buckets.percentile(0.99));
        line(
            "total_32bit_bytes",
            sizes.dict.total() + sizes.rare32.total(),
        );
        line(
            "total_20bit_bytes",
            sizes.dict.total() + address as u64 + sizes.blob as u64,
        );
    }
    let _ = writeln!(
        out,
        "rare20_mean_docs: {:.3}",
        sizes.buckets.docs as f64 / BUCKETS as f64
    );
}

fn push_class(out: &mut String, class: &str, design: &str, rows: &[Obs]) {
    let _ = writeln!(
        out,
        "query: class={class} design={design} n={} hits={} read_errors={} \
         candidates_p50={} candidates_p90={} bytes_p50={} bytes_p90={} \
         probe_evicted_p50_ms={} probe_warm_p50_ms={} probe_warm_p90_ms={} \
         verify_evicted_p50_ms={} verify_evicted_p90_ms={} \
         verify_warm_p50_ms={} verify_warm_p90_ms={} \
         total_warm_p50_ms={} total_warm_p90_ms={}",
        rows.len(),
        rows.iter().filter(|row| row.hit).count(),
        rows.iter().map(|row| row.read_errors).sum::<u64>(),
        pct_usize(rows, |row| row.candidates, 0.50),
        pct_usize(rows, |row| row.candidates, 0.90),
        pct_u64(rows, |row| row.bytes, 0.50),
        pct_u64(rows, |row| row.bytes, 0.90),
        ms(pct_dur(rows, |row| row.probe_evicted, 0.50)),
        ms(pct_dur(rows, |row| row.probe_warm, 0.50)),
        ms(pct_dur(rows, |row| row.probe_warm, 0.90)),
        ms(pct_dur(rows, |row| row.verify_evicted, 0.50)),
        ms(pct_dur(rows, |row| row.verify_evicted, 0.90)),
        ms(pct_dur(rows, |row| row.verify_warm, 0.50)),
        ms(pct_dur(rows, |row| row.verify_warm, 0.90)),
        ms(pct_dur(rows, |row| row.probe_warm + row.verify_warm, 0.50)),
        ms(pct_dur(rows, |row| row.probe_warm + row.verify_warm, 0.90)),
    );
}

fn pct_dur(rows: &[Obs], f: impl Fn(&Obs) -> Duration, p: f64) -> Duration {
    let mut values: Vec<Duration> = rows.iter().map(f).collect();
    values.sort();
    at(&values, p)
}

fn pct_usize(rows: &[Obs], f: impl Fn(&Obs) -> usize, p: f64) -> usize {
    let mut values: Vec<usize> = rows.iter().map(f).collect();
    values.sort_unstable();
    at(&values, p)
}

fn pct_u64(rows: &[Obs], f: impl Fn(&Obs) -> u64, p: f64) -> u64 {
    let mut values: Vec<u64> = rows.iter().map(f).collect();
    values.sort_unstable();
    at(&values, p)
}

fn at<T: Copy>(sorted: &[T], p: f64) -> T {
    let n = sorted.len();
    let i = ((p * n as f64).ceil() as usize).clamp(1, n) - 1;
    sorted[i]
}

fn ms(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64() * 1e3)
}

fn rss_kib() -> u64 {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    status
        .lines()
        .find_map(|line| {
            let rest = line.strip_prefix("VmRSS:")?;
            rest.split_whitespace().next()?.parse().ok()
        })
        .unwrap_or(0)
}

/// The 20-bit address array round-trips a bucket, and an empty bucket has
/// equal adjacent addresses.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_round_trip_docs_and_leave_gaps_empty() {
        let first = 10u32;
        // Bucket 1 (low 20 bits) holds three documents, one of them twice.
        // Bucket 5 holds one. Fingerprints outside those low bits stay put.
        let mut records = vec![
            (0x0000_0001, 12),
            (0x0010_0001, 10),
            (0x0010_0001, 12),
            (0x0000_0005, 11),
            (0x0000_0001, 15),
        ];
        let (addrs, blob, stats) = encode_buckets(&mut records, first).unwrap();
        assert_eq!(addrs.len(), BUCKETS + 1);
        assert_eq!(decode_at(&blob, &addrs, 1, first), vec![10, 12, 15]);
        assert_eq!(decode_at(&blob, &addrs, 5, first), vec![11]);
        assert!(decode_at(&blob, &addrs, 0, first).is_empty());
        assert_eq!(addrs[0], addrs[1]);
        assert_eq!(stats.max, 3);
        assert_eq!(stats.docs, 4);
    }

    fn decode_at(blob: &[u8], addrs: &[u32], bucket: usize, first: u32) -> Vec<u32> {
        let start = addrs[bucket] as usize;
        let end = addrs[bucket + 1] as usize;
        if start == end {
            Vec::new()
        } else {
            decode_list(&blob[start..end], first)
        }
    }
}
