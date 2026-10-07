//! `ferret-bench segment-build <catalog-dir> <out-dir> [--breakdown]`: builds
//! real segments from every live `Hashed` document and reports what they
//! cost, for M2's dictionary decision (docs/S2.md § M2, § Term dictionary).
//!
//! Documents stream in DocId order through `tokenize` and `cap`, the walk
//! `terms` uses ([`read_documents`]), into an [`Inverter`]. A segment is
//! written each time [`SEGMENT_TEXT`] bytes of text have gone in, or the
//! inverter reaches [`MEMORY_BOUND`]. Each written file is then reopened and
//! read end to end, outside the timed build, so the figures describe
//! segments the reader accepts.
//!
//! `--breakdown` (M2b, docs/S2.md § M2 "M2b breakdown") additionally splits
//! the dictionary's bytes by who they belong to, using the real per-entry
//! sizes `Writer::push` returns rather than estimates. "Dictionary bytes" in
//! the breakdown means the entry's bytes in the blocks section only: a
//! block's first entry's term and offsets live in the resident index
//! instead, so the breakdown slightly undercounts against
//! `dictionary_bytes` (blocks + index). At M2's measured scale the index is
//! about 4% of the dictionary, so this is the brief's "narrowest hook",
//! traded for not reaching into the index's per-block accounting.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use ferret_catalog::Catalog;
use ferret_index::segment::{Hit, Inverter, Segment, Sizes};
use ferret_text::{Scratch, cap, tokenize};

use crate::census::hashed_documents;
use crate::terms::read_documents;
use crate::tokenize::is_hash_like;

/// Input text per segment: S2.md's buffer flush point, an assumption M3
/// tunes.
const SEGMENT_TEXT: u64 = 64 << 20;

/// The inverter's memory bound; a 64 MiB segment estimates well under it.
const MEMORY_BOUND: usize = 1 << 30;

/// A document's extension, bucketed the way the brief lists: `jsonl`, none,
/// `js`, `json`, `map`, `log`, and other.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ext {
    Jsonl,
    None,
    Js,
    Json,
    Map,
    Log,
    Other,
}

/// Every bucket, in report order.
const EXTENSIONS: [Ext; 7] = [
    Ext::Jsonl,
    Ext::None,
    Ext::Js,
    Ext::Json,
    Ext::Map,
    Ext::Log,
    Ext::Other,
];

impl Ext {
    fn label(self) -> &'static str {
        match self {
            Ext::Jsonl => "jsonl",
            Ext::None => "none",
            Ext::Js => "js",
            Ext::Json => "json",
            Ext::Map => "map",
            Ext::Log => "log",
            Ext::Other => "other",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

fn classify(extension: &[u8]) -> Ext {
    match extension {
        b"jsonl" => Ext::Jsonl,
        b"" => Ext::None,
        b"js" => Ext::Js,
        b"json" => Ext::Json,
        b"map" => Ext::Map,
        b"log" => Ext::Log,
        _ => Ext::Other,
    }
}

/// A count of terms and the dictionary bytes their entries took.
#[derive(Default, Clone, Copy)]
struct Counted {
    terms: u64,
    bytes: u64,
}

impl Counted {
    fn add(&mut self, bytes: u64) {
        self.terms += 1;
        self.bytes += bytes;
    }
}

/// A distinct term's dictionary bytes summed over the whole build (every
/// entry it got, across every segment), and whether any of those entries
/// held a document outside `jsonl`.
#[derive(Default)]
struct TermBytes {
    bytes: u64,
    non_jsonl: bool,
}

/// `--breakdown`'s accumulated counts (docs/S2.md § M2 "M2b breakdown",
/// brief items 1-3).
#[derive(Default)]
struct Breakdown {
    /// Item 1: singleton entries, hash-like or not.
    singleton_hash_like: Counted,
    singleton_not_hash_like: Counted,
    /// The overlap needed for item 4(d)'s arithmetic: singleton entries that
    /// are both hash-like and in a `jsonl` document.
    singleton_hash_like_and_jsonl: Counted,
    /// Item 2: singleton entries by their document's extension.
    singleton_by_extension: [Counted; EXTENSIONS.len()],
    /// Item 3: every entry's bytes, by term text, to learn after the whole
    /// build whether a term occurred only in `jsonl` documents.
    terms: HashMap<Box<[u8]>, TermBytes>,
    /// Every document seen so far (DocId, extension), in increasing DocId
    /// order: `read_documents` walks that way, and entries for a document
    /// are only drained after it is added.
    doc_ext: Vec<(u32, Ext)>,
    /// Content bytes of `jsonl` documents, for item 4(b) and (d).
    jsonl_content_bytes: u64,
}

impl Breakdown {
    fn record_doc(&mut self, doc: u32, ext: Ext, bytes: u64) {
        self.doc_ext.push((doc, ext));
        if ext == Ext::Jsonl {
            self.jsonl_content_bytes += bytes;
        }
    }

    /// A document's extension bucket. Entries for a document are only
    /// drained after it was added, so `doc` is always present; `Other` is
    /// an unreachable-in-practice fallback rather than a panic.
    fn ext_of(&self, doc: u32) -> Ext {
        self.doc_ext
            .binary_search_by_key(&doc, |&(d, _)| d)
            .map_or(Ext::Other, |i| self.doc_ext[i].1)
    }

    /// Called as each entry is written, with its term, documents and the
    /// real bytes `Writer::push` gave it.
    fn record_entry(&mut self, term: &[u8], docs: &[u32], bytes: u64) {
        let all_jsonl = docs.iter().all(|&d| self.ext_of(d) == Ext::Jsonl);
        let state = self.terms.entry(term.into()).or_default();
        state.bytes += bytes;
        state.non_jsonl |= !all_jsonl;

        if let [doc] = docs {
            let ext = self.ext_of(*doc);
            let hash_like = is_hash_like(term);
            if hash_like {
                self.singleton_hash_like.add(bytes);
            } else {
                self.singleton_not_hash_like.add(bytes);
            }
            if hash_like && ext == Ext::Jsonl {
                self.singleton_hash_like_and_jsonl.add(bytes);
            }
            self.singleton_by_extension[ext.index()].add(bytes);
        }
    }

    /// After the whole build: dictionary bytes of terms that occurred only
    /// in `jsonl` documents, versus every other term (brief item 3).
    fn jsonl_only(&self) -> (Counted, Counted) {
        let (mut only, mut rest) = (Counted::default(), Counted::default());
        for state in self.terms.values() {
            if state.non_jsonl {
                rest.add(state.bytes);
            } else {
                only.add(state.bytes);
            }
        }
        (only, rest)
    }
}

/// Segments under construction and what the written ones hold.
struct Build {
    out: PathBuf,
    segment_text: u64,
    inverter: Inverter,
    scratch: Scratch,
    /// DocId range of the segment being built.
    range: Option<(u32, u32)>,
    text: u64,
    documents: u64,
    content_bytes: u64,
    files: Vec<PathBuf>,
    sizes: Sizes,
    breakdown: Option<Breakdown>,
    /// Wall-clock in `drain_into` and `finish`: sorting, encoding, writing.
    write_seconds: f64,
    error: Option<Box<dyn std::error::Error>>,
}

impl Build {
    fn new(out: &Path, segment_text: u64, breakdown: bool) -> Self {
        Self {
            out: out.to_path_buf(),
            segment_text,
            inverter: Inverter::new(MEMORY_BOUND),
            scratch: Scratch::default(),
            range: None,
            text: 0,
            documents: 0,
            content_bytes: 0,
            files: Vec::new(),
            sizes: Sizes::default(),
            breakdown: breakdown.then(Breakdown::default),
            write_seconds: 0.0,
            error: None,
        }
    }

    fn add(&mut self, doc: u32, ext: Ext, bytes: &[u8]) {
        if self.error.is_some() {
            return;
        }
        if let Some(breakdown) = self.breakdown.as_mut() {
            breakdown.record_doc(doc, ext, bytes.len() as u64);
        }
        let inverter = &mut self.inverter;
        let mut failed = None;
        tokenize(bytes, &mut self.scratch, |token| {
            if let Err(error) = inverter.add(doc, cap(token.bytes)) {
                failed = Some(error);
            }
        });
        if let Some(error) = failed {
            self.error = Some(error.into());
            return;
        }
        let first = self.range.map_or(doc, |(first, _)| first);
        self.range = Some((first, doc));
        self.text += bytes.len() as u64;
        self.documents += 1;
        self.content_bytes += bytes.len() as u64;
        if (self.text >= self.segment_text || self.inverter.is_full())
            && let Err(error) = self.flush()
        {
            self.error = Some(error);
        }
    }

    fn flush(&mut self) -> crate::Result<()> {
        let Some((first, last)) = self.range.take() else {
            return Ok(());
        };
        let start = Instant::now();
        let writer = if let Some(breakdown) = self.breakdown.as_mut() {
            self.inverter
                .drain_into_with(first, last, |term, docs, bytes| {
                    breakdown.record_entry(term, docs, bytes);
                })?
        } else {
            self.inverter.drain_into(first, last)?
        };
        let path = self
            .out
            .join(format!("seg-{first}-{last}-{}.seg", self.files.len()));
        let mut file = BufWriter::new(File::create(&path)?);
        let sizes = writer.finish(&mut file)?;
        file.flush()?;
        self.write_seconds += start.elapsed().as_secs_f64();
        self.sizes.add(&sizes);
        self.files.push(path);
        self.text = 0;
        Ok(())
    }
}

/// Builds segments into `out` from `catalog`'s documents. Returns the build
/// and how many documents were skipped.
fn build_segments(
    catalog: &Catalog,
    out: &Path,
    segment_text: u64,
    breakdown: bool,
) -> crate::Result<(Build, u64)> {
    let docs = hashed_documents(catalog)?;
    fs::create_dir_all(out)?;
    let mut build = Build::new(out, segment_text, breakdown);
    let skipped = read_documents(catalog, &docs, |doc, bytes| {
        build.add(doc.doc.0, classify(&doc.extension), bytes)
    });
    if let Some(error) = build.error.take() {
        return Err(error);
    }
    build.flush()?;
    Ok((build, skipped))
}

/// Reopens every segment and reads every term and list. Returns the terms
/// and postings read.
fn verify(files: &[PathBuf]) -> crate::Result<(u64, u64)> {
    let (mut terms, mut pairs) = (0, 0);
    let mut docs = Vec::new();
    for path in files {
        let segment = Segment::open(File::open(path)?)?;
        let mut iter = segment.terms();
        while let Some((_, entry)) = iter.next_term()? {
            terms += 1;
            pairs += match segment.read(&entry)? {
                Hit::Single(_) => 1,
                Hit::Postings(postings) => {
                    docs.clear();
                    postings.decode(&mut docs);
                    docs.len() as u64
                }
            };
        }
    }
    Ok((terms, pairs))
}

/// User plus system CPU seconds of this process so far, from
/// `/proc/self/stat` at Linux's fixed 100 ticks per second.
pub(crate) fn cpu_seconds() -> io::Result<f64> {
    let stat = fs::read_to_string("/proc/self/stat")?;
    let fields: Vec<&str> = stat
        .rsplit_once(") ")
        .map_or(Vec::new(), |(_, rest)| rest.split(' ').collect());
    let tick = |i: usize| fields.get(i).and_then(|f| f.parse::<u64>().ok());
    match (tick(11), tick(12)) {
        (Some(user), Some(system)) => Ok((user + system) as f64 / 100.0),
        _ => Err(io::Error::other("unparsable /proc/self/stat")),
    }
}

fn ratio(numerator: u64, denominator: u64) -> String {
    if denominator == 0 {
        return "-".into();
    }
    format!("{:.4}", numerator as f64 / denominator as f64)
}

/// The `key: value` report. Every figure is from this run.
fn report(build: &Build, skipped: u64, cpu: f64, wall: f64, rss_kib: &str) -> String {
    let s = &build.sizes;
    let total = s.total();
    let mb = build.content_bytes as f64 / 1e6;
    let mut out = String::new();
    let mut line = |key: &str, value: String| {
        let _ = writeln!(out, "{key}: {value}");
    };
    line("documents", build.documents.to_string());
    line("skipped_unreadable_or_changed", skipped.to_string());
    line("content_bytes", build.content_bytes.to_string());
    line("segments", build.files.len().to_string());
    line("total_bytes", total.to_string());
    line("bytes_per_content_byte", ratio(total, build.content_bytes));
    line("head_bytes", s.head.to_string());
    line("dictionary_block_bytes", s.blocks.to_string());
    line("block_index_bytes", s.index.to_string());
    line("postings_bytes", s.postings.to_string());
    line("chunk_sum_bytes", s.sums.to_string());
    line("dictionary_bytes", s.dictionary().to_string());
    line("dictionary_share", ratio(s.dictionary(), total));
    line("postings_share", ratio(s.postings, total));
    line("terms_summed_over_segments", s.terms.to_string());
    line("dictionary_bytes_per_term", ratio(s.dictionary(), s.terms));
    line("term_string_bytes", s.term_bytes.to_string());
    line(
        "term_string_share_of_dictionary",
        ratio(s.term_bytes, s.dictionary()),
    );
    line("singletons", s.singletons.to_string());
    line("singleton_share", ratio(s.singletons, s.terms));
    line("singleton_dictionary_bytes", s.singleton_bytes.to_string());
    line(
        "singleton_share_of_dictionary",
        ratio(s.singleton_bytes, s.dictionary()),
    );
    line("pairs", s.pairs.to_string());
    line(
        "postings_bits_per_listed_pair",
        ratio(s.postings * 8, s.pairs - s.singletons),
    );
    line("cpu_seconds", format!("{cpu:.2}"));
    line("cpu_seconds_per_mb", format!("{:.4}", cpu / mb.max(1e-9)));
    line("wall_seconds", format!("{wall:.2}"));
    line("write_seconds", format!("{:.2}", build.write_seconds));
    line("peak_rss_kib", rss_kib.to_string());
    if let Some(breakdown) = &build.breakdown {
        out.push_str(&breakdown_report(breakdown, s, build.content_bytes));
    }
    out
}

/// The `--breakdown` section: items 1-4 from the brief. Every "measured"
/// figure is a real sum of `Writer::push`'s entry sizes; every "derived"
/// figure is arithmetic over those sums, shown via its `*_formula` line.
fn breakdown_report(breakdown: &Breakdown, s: &Sizes, content_bytes: u64) -> String {
    let mut out = String::new();

    // Item 1: singleton entries, hash-like or not (measured).
    let counted = |prefix: &str, out: &mut String, c: Counted| {
        let _ = writeln!(out, "measured_{prefix}_terms: {}", c.terms);
        let _ = writeln!(out, "measured_{prefix}_dictionary_block_bytes: {}", c.bytes);
    };
    counted(
        "singleton_hash_like",
        &mut out,
        breakdown.singleton_hash_like,
    );
    counted(
        "singleton_not_hash_like",
        &mut out,
        breakdown.singleton_not_hash_like,
    );

    // Item 2: singleton entries by document extension (measured).
    for &ext in &EXTENSIONS {
        counted(
            &format!("singleton_ext_{}", ext.label()),
            &mut out,
            breakdown.singleton_by_extension[ext.index()],
        );
    }

    // Item 3: all dictionary bytes, by whether the term is jsonl-only
    // (measured).
    let (jsonl_only, not_jsonl_only) = breakdown.jsonl_only();
    counted("jsonl_only", &mut out, jsonl_only);
    counted("not_jsonl_only", &mut out, not_jsonl_only);
    let _ = writeln!(
        out,
        "measured_jsonl_content_bytes: {}",
        breakdown.jsonl_content_bytes
    );

    // Item 4: the derived index size per content byte under each policy,
    // applied alone. Removed dictionary bytes are block-only (see the
    // module header); content and postings are untouched except by (b) and
    // (d), which drop jsonl's documents (and so their postings) wholesale.
    let total = s.total();
    let hash_like_bytes = breakdown.singleton_hash_like.bytes;
    let jsonl_only_bytes = jsonl_only.bytes;
    let overlap = breakdown.singleton_hash_like_and_jsonl.bytes;

    let derive = |out: &mut String, label: &str, bytes_removed: u64, content_removed: u64| {
        let total_after = total - bytes_removed;
        let content_after = (content_bytes - content_removed).max(1);
        let _ = writeln!(
            out,
            "derived_{label}_formula: ({total} - {bytes_removed}) / ({content_bytes} - {content_removed})"
        );
        let _ = writeln!(out, "derived_{label}_total_bytes: {total_after}");
        let _ = writeln!(out, "derived_{label}_content_bytes: {content_after}");
        let _ = writeln!(
            out,
            "derived_{label}_bytes_per_content_byte: {}",
            ratio(total_after, content_after)
        );
    };

    derive(&mut out, "drop_hash_like", hash_like_bytes, 0);
    derive(
        &mut out,
        "drop_jsonl",
        jsonl_only_bytes,
        breakdown.jsonl_content_bytes,
    );
    derive(&mut out, "drop_singletons", s.singleton_bytes, 0);
    derive(
        &mut out,
        "drop_hash_like_and_jsonl",
        hash_like_bytes + jsonl_only_bytes - overlap,
        breakdown.jsonl_content_bytes,
    );
    out
}

/// `ferret-bench segment-build <catalog-dir> <out-dir> [--breakdown]`.
pub(crate) fn run(dir: &Path, out: &Path, breakdown: bool) -> crate::Result<()> {
    let catalog = crate::open_catalog(dir)?;
    let (cpu_start, start) = (cpu_seconds()?, Instant::now());
    let (build, skipped) = build_segments(&catalog, out, SEGMENT_TEXT, breakdown)?;
    let (cpu, wall) = (cpu_seconds()? - cpu_start, start.elapsed().as_secs_f64());
    let (_, peak) = crate::memory()?;
    print!("{}", report(&build, skipped, cpu, wall, &peak));
    let verify_start = Instant::now();
    let (terms, pairs) = verify(&build.files)?;
    if (terms, pairs) != (build.sizes.terms, build.sizes.pairs) {
        return Err(format!("reopened segments hold {terms} terms and {pairs} pairs").into());
    }
    println!(
        "verified_seconds: {:.2}",
        verify_start.elapsed().as_secs_f64()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::Fixture;

    #[test]
    fn segments_hold_every_document_and_reopen() {
        let fixture = Fixture::new("segment-build");
        let catalog = fixture.commit(
            &[
                ("a.rs", b"fn main parseHttpRequest\n"),
                ("b.txt", b"fn helper\n"),
                ("c", b"main main main\n"),
            ],
            &[],
        );
        let out = fixture.out("segments");
        // One byte of text per segment: every document flushes its own.
        let (build, skipped) = build_segments(&catalog, &out, 1, false).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(build.files.len(), 3);
        assert_eq!(build.documents, 3);
        // a: fn main parsehttprequest parse http request; b: fn helper;
        // c: main.
        assert_eq!(build.sizes.terms, 6 + 2 + 1);
        assert_eq!(build.sizes.singletons, 9);
        assert_eq!(verify(&build.files).unwrap(), (9, 9));

        let out = fixture.out("one");
        let (build, _) = build_segments(&catalog, &out, u64::MAX, false).unwrap();
        assert_eq!(build.files.len(), 1);
        assert_eq!(build.sizes.terms, 7, "fn and main are shared");
        assert_eq!(build.sizes.singletons, 5);
        assert_eq!(build.sizes.pairs, 9);
        assert_eq!(verify(&build.files).unwrap(), (7, 9));

        let report = report(&build, 0, 1.0, 2.0, "3 kB");
        assert!(report.contains("segments: 1\n"));
        assert!(report.contains("terms_summed_over_segments: 7\n"));
        assert!(report.contains("singletons: 5\n"));
    }

    /// Hand-computed breakdown over a tiny synthetic catalog, with
    /// `segment_text: 1` so every document is its own segment. A term
    /// outside a writer's first block costs, in `blocks`:
    /// `vbyte(shared) + vbyte(suffix_len) + suffix + vbyte(df=1) +
    /// vbyte(doc - first = 0)`; a writer's first term skips the first two
    /// fields (its text sits in the resident index instead), costing just
    /// `vbyte(1) + vbyte(0) = 2`.
    ///
    /// "`deadbeef1234`" is hash-like (mixed digits and lowercase hex, len
    /// 12) and the tokenizer's digit-boundary rule (D9) also emits it as two
    /// parts, `"deadbeef"` and `"1234"`, neither of which is hash-like
    /// (each is missing one of "digit" or "lowercase"). All three land in
    /// `a.jsonl`'s one writer, sorted `"1234" < "deadbeef" <
    /// "deadbeef1234"`:
    /// - `"1234"` (first in block): 2 bytes.
    /// - `"deadbeef"` (shared 0, suffix 8): 1+1+8+1+1 = 12 bytes.
    /// - `"deadbeef1234"` (shared 8, suffix 4): 1+1+4+1+1 = 8 bytes.
    #[test]
    fn breakdown_matches_hand_computed_figures() {
        let fixture = Fixture::new("segment-build-breakdown");
        let catalog = fixture.commit(
            &[
                // Hash-like "deadbeef1234" plus its non-hash-like parts
                // "deadbeef" and "1234", all singleton, all jsonl.
                ("a.jsonl", b"deadbeef1234\n"),
                // Singleton, not hash-like; occurs in two jsonl documents,
                // so item 3 calls it jsonl-only even though it is never a
                // list entry (each occurrence is its own segment, since
                // `segment_text: 1`). The case differs so the two are
                // distinct documents (the catalog dedupes by content hash)
                // while tokenizing to the same term.
                ("f.jsonl", b"onlyjsonl\n"),
                ("g.jsonl", b"Onlyjsonl\n"),
                // Singleton, not hash-like, ext js.
                ("b.js", b"main\n"),
                // Not jsonl-only: occurs in json and log, neither jsonl.
                // Case differs for the same reason as above.
                ("c.json", b"shared\n"),
                ("d.log", b"Shared\n"),
                // Singleton, not hash-like, ext log.
                ("e.log", b"localonly\n"),
            ],
            &[],
        );
        let out = fixture.out("breakdown");
        let (build, skipped) = build_segments(&catalog, &out, 1, true).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(build.files.len(), 7, "one segment per document");
        assert_eq!(
            build.sizes.terms, 9,
            "a.jsonl holds 3 terms, the rest 1 each"
        );
        assert_eq!(build.sizes.singletons, 9, "every entry has df = 1");
        assert_eq!(build.sizes.singleton_bytes, 34, "22 (doc a) + 6 * 2");
        assert_eq!(build.sizes.blocks, 34, "nothing else writes to blocks here");

        let breakdown = build.breakdown.as_ref().unwrap();

        // Item 1: hash-like vs not, by hand: only "deadbeef1234".
        assert_eq!(breakdown.singleton_hash_like.terms, 1);
        assert_eq!(breakdown.singleton_hash_like.bytes, 8);
        assert_eq!(breakdown.singleton_not_hash_like.terms, 8);
        assert_eq!(breakdown.singleton_not_hash_like.bytes, 34 - 8);

        // Item 2: by extension, by hand: jsonl holds all three of a's
        // terms plus both "onlyjsonl" occurrences (2 + 12 + 8 + 2 + 2);
        // log holds "shared" (its d.log occurrence) and "localonly".
        let by_ext = |ext: Ext| breakdown.singleton_by_extension[ext.index()];
        assert_eq!(
            (by_ext(Ext::Jsonl).terms, by_ext(Ext::Jsonl).bytes),
            (5, 26)
        );
        assert_eq!((by_ext(Ext::Js).terms, by_ext(Ext::Js).bytes), (1, 2));
        assert_eq!((by_ext(Ext::Json).terms, by_ext(Ext::Json).bytes), (1, 2));
        assert_eq!((by_ext(Ext::Log).terms, by_ext(Ext::Log).bytes), (2, 4));
        assert_eq!((by_ext(Ext::None).terms, by_ext(Ext::None).bytes), (0, 0));
        assert_eq!((by_ext(Ext::Map).terms, by_ext(Ext::Map).bytes), (0, 0));
        assert_eq!((by_ext(Ext::Other).terms, by_ext(Ext::Other).bytes), (0, 0));

        // The hash-like/jsonl overlap item 4(d) needs: only "deadbeef1234".
        assert_eq!(breakdown.singleton_hash_like_and_jsonl.terms, 1);
        assert_eq!(breakdown.singleton_hash_like_and_jsonl.bytes, 8);

        // Item 3: jsonl-only is a's three terms (2 + 12 + 8) plus
        // "onlyjsonl" (2 + 2, one per jsonl document); "main", "shared" and
        // "localonly" all occur outside jsonl.
        let (jsonl_only, not_jsonl_only) = breakdown.jsonl_only();
        assert_eq!((jsonl_only.terms, jsonl_only.bytes), (4, 26));
        assert_eq!((not_jsonl_only.terms, not_jsonl_only.bytes), (3, 8));
        assert_eq!(jsonl_only.bytes + not_jsonl_only.bytes, build.sizes.blocks);

        // jsonl's content bytes: "deadbeef1234\n" (13) + two "onlyjsonl\n"
        // (10 each).
        assert_eq!(breakdown.jsonl_content_bytes, 13 + 10 + 10);
        assert_eq!(
            build.content_bytes,
            13 + 10 + 10 + 5 /* main\n */ + 7 /* shared\n */ + 7 /* Shared\n */ + 10 /* localonly\n */
        );

        // Item 4's arithmetic, checked against the real totals. The
        // hash-like bytes here are a subset of the jsonl-only bytes, so
        // (d)'s union equals (b)'s removal exactly: 8 + 26 - 8 = 26.
        let report = report(&build, skipped, 1.0, 2.0, "1 kB");
        let total = build.sizes.total();
        assert!(report.contains(&format!(
            "derived_drop_hash_like_total_bytes: {}\n",
            total - 8
        )));
        assert!(report.contains(&format!(
            "derived_drop_jsonl_total_bytes: {}\n",
            total - jsonl_only.bytes
        )));
        assert!(report.contains(&format!(
            "derived_drop_jsonl_content_bytes: {}\n",
            build.content_bytes - breakdown.jsonl_content_bytes
        )));
        assert!(report.contains(&format!(
            "derived_drop_singletons_total_bytes: {}\n",
            total - 34
        )));
        assert!(report.contains(&format!(
            "derived_drop_hash_like_and_jsonl_total_bytes: {}\n",
            total - 26
        )));
    }
}
