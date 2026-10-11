//! S2 M3's measurements (docs/S2.md § M3): the content index built by
//! [`IndexWriter::follow`] and compacted by its merge policy, and a
//! synthetic churn trace.
//!
//! `index-build <catalog-dir> <index-dir>` follows every live document of
//! the catalog into a fresh index in `<index-dir>`, unpaced, as an explicit
//! `ferret index` would; merges to steady state with
//! [`IndexWriter::merge_if_needed`]; then forces one full merge to a single
//! segment. Each phase reports the index's shape: bytes per content byte,
//! the dictionary's share, h (dictionary entries per distinct term, counted
//! exactly by merging every segment's term stream) and the singleton share.
//!
//! Documents are read by path, bracketed against the full catalog version
//! key before and after the read (`document_read`); the bench has no crawl
//! edge. A changed or unreadable version is [`Fault::Unreadable`]. The real
//! product also checks each parent directory through no-follow opens.
//!
//! `index-churn <out-dir> <rounds>` runs generated text only: each round
//! adds documents, kills a share of the live ones, follows and merges. It
//! reports merge write amplification and the segment count.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::time::Instant;

use ferret_catalog::{Catalog, NameId};
use ferret_index::segment::{DICTIONARY_CODEC, FINGERPRINT_CODEC, Section};
use ferret_index::{Budget, CatalogView, DocSet, Fault, IndexWriter, Stopped, View};

use crate::census::hashed_documents;

/// M1c's count of distinct terms in the same catalog, for comparison with
/// the exact count this run takes.
const M1C_DISTINCT_TERMS: u64 = 18_456_581;

/// The shape of one published index view.
#[derive(Debug, Default)]
struct Shape {
    segments: usize,
    bytes: u64,
    trigram_bytes: u64,
    dictionary: u64,
    postings: u64,
    entries: u64,
    singletons: u64,
    distinct: u64,
    pairs: u64,
    records: u64,
    fingerprints: u64,
    fingerprint_index: u64,
    sums: u64,
}

fn shape(view: &View) -> crate::Result<Shape> {
    let mut shape = Shape {
        segments: view.segments().len(),
        bytes: view.manifest().segments.iter().map(|s| s.bytes).sum(),
        trigram_bytes: view
            .manifest()
            .segments
            .iter()
            .map(|s| s.trigram_bytes)
            .sum(),
        ..Shape::default()
    };
    let mut cursors = Vec::new();
    for segment in view.segments() {
        shape.dictionary +=
            segment.section_bytes(Section::Blocks) + segment.section_bytes(Section::Index);
        shape.postings += segment.section_bytes(Section::Postings);
        shape.pairs += segment.info().pairs;
        shape.records += segment.fingerprint_records();
        shape.fingerprints += segment.section_bytes(Section::Fingerprints);
        shape.fingerprint_index += segment.section_bytes(Section::FingerprintIndex);
        shape.sums += segment.section_bytes(Section::Sums);
        cursors.push(segment.terms());
    }
    // Every segment's terms, merged, to count distinct terms exactly.
    let mut heap = BinaryHeap::new();
    for (i, cursor) in cursors.iter_mut().enumerate() {
        if let Some((term, entry)) = cursor.next_term()? {
            heap.push(Reverse((term.to_vec(), i, entry.df)));
        }
    }
    let mut last: Option<Vec<u8>> = None;
    while let Some(Reverse((term, i, df))) = heap.pop() {
        shape.entries += 1;
        shape.singletons += u64::from(df == 1);
        if last.as_deref() != Some(term.as_slice()) {
            shape.distinct += 1;
            last = Some(term);
        }
        if let Some((term, entry)) = cursors[i].next_term()? {
            heap.push(Reverse((term.to_vec(), i, entry.df)));
        }
    }
    Ok(shape)
}

fn ratio(numerator: u64, denominator: u64) -> String {
    if denominator == 0 {
        return "-".into();
    }
    format!("{:.4}", numerator as f64 / denominator as f64)
}

fn shape_lines(out: &mut String, prefix: &str, s: &Shape, content_bytes: u64) {
    let mut line = |key: &str, value: String| {
        let _ = writeln!(out, "{prefix}{key}: {value}");
    };
    line("segments", s.segments.to_string());
    line("total_bytes", s.bytes.to_string());
    line("trigram_bytes", s.trigram_bytes.to_string());
    line("paired_bytes", (s.bytes + s.trigram_bytes).to_string());
    line(
        "trigram_bytes_per_content_byte",
        ratio(s.trigram_bytes, content_bytes),
    );
    line("bytes_per_content_byte", ratio(s.bytes, content_bytes));
    line("dictionary_bytes", s.dictionary.to_string());
    line("dictionary_share", ratio(s.dictionary, s.bytes));
    line("postings_bytes", s.postings.to_string());
    line("postings_share", ratio(s.postings, s.bytes));
    line("dictionary_entries", s.entries.to_string());
    line("distinct_terms", s.distinct.to_string());
    line("h_entries_per_distinct_term", ratio(s.entries, s.distinct));
    line(
        "h_against_m1c_distinct_terms",
        ratio(s.entries, M1C_DISTINCT_TERMS),
    );
    line("singleton_entries", s.singletons.to_string());
    line("singleton_share", ratio(s.singletons, s.entries));
    line("pairs", s.pairs.to_string());
    line("fingerprint_records", s.records.to_string());
    line("fingerprint_bytes", s.fingerprints.to_string());
    line("fingerprint_index_bytes", s.fingerprint_index.to_string());
    line("checksum_bytes", s.sums.to_string());
}

/// Reads `/proc/self/io`'s `rchar` (bytes passed to `read`) and
/// `read_bytes` (bytes fetched from the device).
fn io_counters() -> crate::Result<(u64, u64)> {
    let io = fs::read_to_string("/proc/self/io")?;
    let field = |name: &str| -> crate::Result<u64> {
        Ok(io
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .ok_or_else(|| format!("no {name} in /proc/self/io"))?
            .trim()
            .parse()?)
    };
    Ok((field("rchar:")?, field("read_bytes:")?))
}

/// DocId → the name to read it through, and its catalogued size.
type Named = HashMap<u32, (NameId, u64)>;

/// The live documents and how to read them.
fn documents(catalog: &Catalog) -> crate::Result<(DocSet, Named)> {
    let docs = hashed_documents(catalog)?;
    let live = DocSet::new(catalog.next_doc().0, catalog.docs().map(|(doc, _)| doc.0));
    let named = docs.iter().map(|d| (d.doc.0, (d.name, d.size))).collect();
    Ok((live, named))
}

pub(crate) fn run(dir: &Path, out: &Path, options: &[String]) -> crate::Result<()> {
    let options = Options::parse(options)?;
    crate::trigram::check_disk()?;
    if out.exists() {
        return Err(format!("{} exists; the build wants a fresh index", out.display()).into());
    }
    let catalog = crate::open_catalog(dir)?;
    catalog.load(&ferret_catalog::Section::INODE)?;
    let snapshot = options
        .corpus
        .as_ref()
        .map(|dir| crate::content_snapshot::Snapshot::open(dir, &catalog))
        .transpose()?;
    let (live, named) = documents(&catalog)?;
    let view = CatalogView {
        incarnation: catalog.generation().incarnation,
        live: &live,
    };
    let mut report = String::new();
    let line = |out: &mut String, key: &str, value: String| {
        let _ = writeln!(out, "{key}: {value}");
    };
    line(&mut report, "live_documents", live.len().to_string());
    line(&mut report, "named_documents", named.len().to_string());

    // 1. The first build, unpaced.
    let (cpu_start, start) = (crate::segment_build::cpu_seconds()?, Instant::now());
    let io_start = io_counters()?;
    let mut writer = IndexWriter::open_with_codec(out, &view, options.codec)?;
    let (mut content_bytes, mut docs, mut unreadable, mut passes) = (0u64, 0u64, 0u64, 0u64);
    let mut written = 0u64;
    loop {
        crate::trigram::check_disk()?;
        let followed = writer.follow(&view, &Budget::unbounded(), &mut |doc, bytes| {
            let Some(&(name, _)) = named.get(&doc) else {
                return Err(Fault::Unreadable);
            };
            crate::document_read::read(&catalog, name, doc, snapshot.as_ref(), None, bytes)
                .map(|_| ())
                .ok_or(Fault::Unreadable)
        })?;
        passes += 1;
        content_bytes += followed.bytes;
        docs += followed.docs;
        unreadable += followed.unreadable;
        written += followed.written;
        if followed.stopped == Stopped::Covered {
            break;
        }
    }
    let (cpu, wall) = (
        crate::segment_build::cpu_seconds()? - cpu_start,
        start.elapsed().as_secs_f64(),
    );
    let io_end = io_counters()?;
    let (_, peak) = crate::memory()?;
    line(&mut report, "build_passes", passes.to_string());
    line(&mut report, "build_documents", docs.to_string());
    line(&mut report, "build_unreadable", unreadable.to_string());
    line(&mut report, "content_bytes", content_bytes.to_string());
    line(&mut report, "build_wall_seconds", format!("{wall:.2}"));
    line(&mut report, "build_cpu_seconds", format!("{cpu:.2}"));
    line(&mut report, "build_peak_rss", peak);
    line(
        &mut report,
        "build_rchar_bytes",
        (io_end.0 - io_start.0).to_string(),
    );
    line(
        &mut report,
        "build_device_read_bytes",
        (io_end.1 - io_start.1).to_string(),
    );
    line(&mut report, "build_written_bytes", written.to_string());
    print!("{report}");
    report.clear();
    let first = shape(&writer.view())?;
    shape_lines(&mut report, "built_", &first, content_bytes);
    print!("{report}");
    report.clear();

    if options.snapshots {
        save_state(out, "first")?;
    }
    if options
        .control_bytes
        .is_some_and(|control| first.bytes as f64 > control as f64 * 0.85)
    {
        println!("first_build_stop_gate: saving below 15%; stopped");
        return Ok(());
    }

    let (document_reads, document_bytes) = (std::cell::Cell::new(0u64), std::cell::Cell::new(0u64));
    let mut reader = |doc: u32, offset: Option<u32>, bytes: &mut Vec<u8>| {
        let &(name, _) = named.get(&doc)?;
        let window =
            crate::document_read::read(&catalog, name, doc, snapshot.as_ref(), offset, bytes)?;
        document_reads.set(document_reads.get() + 1);
        document_bytes.set(document_bytes.get() + bytes.len() as u64);
        Some(window)
    };
    // 2. Merge to steady state.
    let io_start = io_counters()?;
    let cpu_start = crate::segment_build::cpu_seconds()?;
    let start = Instant::now();
    let (mut merges, mut merge_read, mut merge_written) = (0u64, 0u64, 0u64);
    let (mut demoted, mut purged, mut graduated) = (0u64, 0u64, 0u64);
    while let Some(merged) =
        writer.merge_if_needed_verified(&view, &Budget::unbounded(), &mut reader)?
    {
        crate::trigram::check_disk()?;
        merges += 1;
        demoted += merged.records_added;
        purged += merged.records_removed;
        graduated += merged.records_graduated;
        merge_read += merged.read;
        merge_written += merged.written;
    }
    let io_end = io_counters()?;
    line(
        &mut report,
        "steady_rchar_bytes",
        (io_end.0 - io_start.0).to_string(),
    );
    line(
        &mut report,
        "steady_device_read_bytes",
        (io_end.1 - io_start.1).to_string(),
    );
    line(&mut report, "steady_records_demoted", demoted.to_string());
    line(&mut report, "steady_records_purged", purged.to_string());
    line(
        &mut report,
        "steady_records_graduated",
        graduated.to_string(),
    );
    line(&mut report, "steady_merges", merges.to_string());
    line(
        &mut report,
        "steady_merge_read_bytes",
        merge_read.to_string(),
    );
    line(
        &mut report,
        "steady_merge_written_bytes",
        merge_written.to_string(),
    );
    line(
        &mut report,
        "steady_merge_seconds",
        format!("{:.2}", start.elapsed().as_secs_f64()),
    );
    line(
        &mut report,
        "steady_merge_cpu_seconds",
        format!("{:.2}", crate::segment_build::cpu_seconds()? - cpu_start),
    );
    line(
        &mut report,
        "steady_document_reads",
        document_reads.get().to_string(),
    );
    line(
        &mut report,
        "steady_document_read_bytes",
        document_bytes.get().to_string(),
    );
    document_reads.set(0);
    document_bytes.set(0);
    let steady = shape(&writer.view())?;
    shape_lines(&mut report, "steady_", &steady, content_bytes);
    print!("{report}");
    report.clear();

    if options.snapshots {
        save_state(out, "steady")?;
    }
    crate::trigram::check_disk()?;
    // 3. One forced full merge.
    let io_start = io_counters()?;
    let cpu_start = crate::segment_build::cpu_seconds()?;
    let start = Instant::now();
    if let Some(merged) = writer.merge_all_verified(&view, &Budget::unbounded(), &mut reader)? {
        line(
            &mut report,
            "full_records_demoted",
            merged.records_added.to_string(),
        );
        line(
            &mut report,
            "full_records_purged",
            merged.records_removed.to_string(),
        );
        line(
            &mut report,
            "full_records_graduated",
            merged.records_graduated.to_string(),
        );
        line(&mut report, "full_merge_inputs", merged.inputs.to_string());
        line(
            &mut report,
            "full_merge_read_bytes",
            merged.read.to_string(),
        );
    }
    let io_end = io_counters()?;
    line(
        &mut report,
        "full_rchar_bytes",
        (io_end.0 - io_start.0).to_string(),
    );
    line(
        &mut report,
        "full_device_read_bytes",
        (io_end.1 - io_start.1).to_string(),
    );
    line(
        &mut report,
        "full_merge_seconds",
        format!("{:.2}", start.elapsed().as_secs_f64()),
    );
    line(
        &mut report,
        "full_merge_cpu_seconds",
        format!("{:.2}", crate::segment_build::cpu_seconds()? - cpu_start),
    );
    line(
        &mut report,
        "full_document_reads",
        document_reads.get().to_string(),
    );
    line(
        &mut report,
        "full_document_read_bytes",
        document_bytes.get().to_string(),
    );
    let full = shape(&writer.view())?;
    shape_lines(&mut report, "full_", &full, content_bytes);
    let (_, peak) = crate::memory()?;
    line(&mut report, "final_peak_rss", peak);
    print!("{report}");
    Ok(())
}

/// Repeats S2's real-engine 512 × 64 KiB, 40-refresh trace. Each refresh
/// replaces eight documents and follows their new DocIds into paired storage.
pub(crate) fn refresh_trace(out: &Path) -> crate::Result<()> {
    const INITIAL: u32 = 512;
    const REPLACED: usize = 8;
    const REFRESHES: usize = 40;
    const DOC_BYTES: usize = 64 << 10;
    if out.exists() {
        return Err("index-refresh wants a fresh index directory".into());
    }
    let words: Vec<Vec<u8>> = (0..256)
        .map(|n| format!("word{n:03}").into_bytes())
        .collect();
    let mut alive: Vec<u32> = (0..INITIAL).collect();
    let incarnation = [91; 16];
    let initial = DocSet::new(INITIAL, alive.iter().copied());
    let mut writer = IndexWriter::open(
        out,
        &CatalogView {
            incarnation,
            live: &initial,
        },
    )?;
    let read = |doc: u32, bytes: &mut Vec<u8>| -> std::io::Result<()> {
        bytes.clear();
        let mut seed = u64::from(doc).wrapping_add(0x9e37_79b9_7f4a_7c15);
        while bytes.len() < DOC_BYTES {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            bytes.extend_from_slice(&words[(seed as usize) & (words.len() - 1)]);
            bytes.push(b' ');
        }
        bytes.truncate(DOC_BYTES);
        Ok(())
    };
    writer.follow(
        &CatalogView {
            incarnation,
            live: &initial,
        },
        &Budget::unbounded(),
        &mut |doc, bytes| read(doc, bytes).map_err(|_| Fault::Unreadable),
    )?;

    let (cpu_start, wall_start) = (crate::segment_build::cpu_seconds()?, Instant::now());
    let (mut next_doc, mut followed_docs, mut followed_bytes) = (INITIAL, 0u64, 0u64);
    for _ in 0..REFRESHES {
        alive.drain(..REPLACED);
        alive.extend(next_doc..next_doc + REPLACED as u32);
        next_doc += REPLACED as u32;
        let live = DocSet::new(next_doc, alive.iter().copied());
        let followed = writer.follow(
            &CatalogView {
                incarnation,
                live: &live,
            },
            &Budget::unbounded(),
            &mut |doc, bytes| read(doc, bytes).map_err(|_| Fault::Unreadable),
        )?;
        followed_docs += followed.docs;
        followed_bytes += followed.bytes;
    }
    let wall = wall_start.elapsed().as_secs_f64();
    let cpu = crate::segment_build::cpu_seconds()? - cpu_start;
    let (_, peak) = crate::memory()?;
    println!("refreshes: {REFRESHES}");
    println!("replaced_documents: {followed_docs}");
    println!("changed_bytes: {followed_bytes}");
    println!("follow_cpu_seconds: {cpu:.6}");
    println!("follow_wall_seconds: {wall:.6}");
    println!("cpu_ms_per_refresh: {:.3}", cpu * 1000.0 / REFRESHES as f64);
    println!(
        "wall_ms_per_refresh: {:.3}",
        wall * 1000.0 / REFRESHES as f64
    );
    println!("peak_rss: {peak}");
    Ok(())
}

/// H remains opt-in; snapshots preserve the real encoded lifecycle states.
struct Options {
    codec: u32,
    snapshots: bool,
    control_bytes: Option<u64>,
    corpus: Option<std::path::PathBuf>,
}
impl Options {
    fn parse(args: &[String]) -> crate::Result<Self> {
        let mut result = Self {
            codec: DICTIONARY_CODEC,
            snapshots: false,
            control_bytes: None,
            corpus: None,
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--codec" => {
                    result.codec = match args.next().map(String::as_str) {
                        Some("h" | "h0") => FINGERPRINT_CODEC,
                        Some("c1") => ferret_index::segment::RUN_CODEC,
                        Some("c2") => ferret_index::segment::GRADUATION_CODEC,
                        Some("c3") => ferret_index::segment::OFFSET_CODEC,
                        Some("1") => DICTIONARY_CODEC,
                        _ => return Err("codec must be 1, h, c1, c2 or c3".into()),
                    }
                }
                "--corpus" => {
                    result.corpus = Some(args.next().ok_or("missing corpus snapshot")?.into())
                }
                "--snapshots" => result.snapshots = true,
                "--control-bytes" => {
                    result.control_bytes =
                        Some(args.next().ok_or("missing control bytes")?.parse()?)
                }
                _ => return Err("unknown index-build option".into()),
            }
        }
        Ok(result)
    }
}

fn save_state(index: &Path, state: &str) -> crate::Result<()> {
    let out = index.with_extension(state);
    fs::create_dir(&out)?;
    for entry in fs::read_dir(index)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            fs::hard_link(entry.path(), out.join(entry.file_name()))?;
        }
    }
    Ok(())
}

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

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Documents born per churn round.
const BIRTHS: u32 = 2_000;
/// Per round, each live document dies with this probability: 1 in 20.
const DEATH: u64 = 20;
/// Distinct words in the synthetic vocabulary.
const VOCABULARY: usize = 200_000;

/// One synthetic document: 20 to 400 words drawn log-uniformly over the
/// vocabulary's ranks (a Zipf-like head and a long tail), plus one word
/// unique to the document, as hashes and identifiers are in real text.
fn generated(doc: u32, vocabulary: &[Vec<u8>], out: &mut Vec<u8>) {
    let mut rng = Rng(u64::from(doc).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let words = 20 + rng.below(381);
    for _ in 0..words {
        let rank = (VOCABULARY as f64).powf(rng.unit()) as usize - 1;
        out.extend_from_slice(&vocabulary[rank.min(VOCABULARY - 1)]);
        out.push(b' ');
    }
    out.extend_from_slice(format!("u{doc:x}q").as_bytes());
}

fn generated_window(
    doc: u32,
    offset: Option<u32>,
    vocabulary: &[Vec<u8>],
    bytes: &mut Vec<u8>,
) -> ferret_index::segment::Window {
    bytes.clear();
    generated(doc, vocabulary, bytes);
    let mut start = 0;
    let mut complete = true;
    if let Some(offset) = offset {
        start = (offset as usize).saturating_sub(256).min(bytes.len());
        let end = (start + 1280).min(bytes.len());
        complete = start == 0 && end == bytes.len();
        bytes.truncate(end);
        bytes.drain(..start);
    }
    ferret_index::segment::Window {
        start: start as u64,
        complete,
    }
}

pub(crate) fn churn(out: &Path, rounds: &str, options: &[String]) -> crate::Result<()> {
    let options = Options::parse(options)?;
    if options.snapshots || options.control_bytes.is_some() || options.corpus.is_some() {
        return Err("index-churn only supports --codec".into());
    }
    let rounds: u32 = rounds.parse()?;
    if out.exists() {
        return Err(format!("{} exists; the trace wants a fresh index", out.display()).into());
    }
    let mut rng = Rng(0x00c0_ffee);
    let vocabulary: Vec<Vec<u8>> = (0..VOCABULARY)
        .map(|_| {
            let len = 3 + rng.below(10) as usize;
            (0..len).map(|_| b'a' + rng.below(26) as u8).collect()
        })
        .collect();
    let mut alive: Vec<bool> = Vec::new();
    let incarnation = [7; 16];
    let empty = DocSet::new(0, []);
    let mut writer = IndexWriter::open_with_codec(
        out,
        &CatalogView {
            incarnation,
            live: &empty,
        },
        options.codec,
    )?;
    let (mut followed_written, mut merge_written, mut merges) = (0u64, 0u64, 0u64);
    let (mut term_follow_written, mut term_merge_written) = (0u64, 0u64);
    let (mut content, mut max_segments) = (0u64, 0usize);
    let start = Instant::now();
    let cpu_start = crate::segment_build::cpu_seconds()?;
    let (mut document_reads, mut document_bytes, mut graduated, mut merge_cpu) =
        (0u64, 0u64, 0u64, 0f64);
    for round in 0..rounds {
        crate::trigram::check_disk()?;
        let (mut added, mut removed) = (0, 0);
        let before: u64 = writer
            .view()
            .segments()
            .iter()
            .map(|s| s.fingerprint_records())
            .sum();
        for doc in alive.iter_mut().filter(|d| **d) {
            *doc = rng.below(DEATH) != 0;
        }
        alive.extend(std::iter::repeat_n(true, BIRTHS as usize));
        let live = DocSet::new(
            alive.len() as u32,
            alive
                .iter()
                .enumerate()
                .filter(|&(_, &a)| a)
                .map(|(d, _)| d as u32),
        );
        let view = CatalogView {
            incarnation,
            live: &live,
        };
        loop {
            let followed = writer.follow(&view, &Budget::unbounded(), &mut |doc, bytes| {
                generated(doc, &vocabulary, bytes);
                Ok(())
            })?;
            followed_written += followed.written;
            term_follow_written += followed.segment.as_ref().map_or(0, |s| s.bytes);
            content += followed.bytes;
            if followed.stopped == Stopped::Covered {
                break;
            }
        }
        let followed_records = writer
            .view()
            .segments()
            .iter()
            .map(|s| s.fingerprint_records())
            .sum::<u64>()
            - before;
        max_segments = max_segments.max(writer.view().segments().len());
        let merge_start = crate::segment_build::cpu_seconds()?;
        let mut reader = |doc: u32, offset: Option<u32>, bytes: &mut Vec<u8>| {
            let window = generated_window(doc, offset, &vocabulary, bytes);
            document_reads += 1;
            document_bytes += bytes.len() as u64;
            Some(window)
        };
        while let Some(merged) =
            writer.merge_if_needed_verified(&view, &Budget::unbounded(), &mut reader)?
        {
            merges += 1;
            added += merged.records_added;
            removed += merged.records_removed;
            graduated += merged.records_graduated;
            merge_written += merged.written;
            term_merge_written += merged.segment.as_ref().map_or(0, |s| s.bytes);
        }
        merge_cpu += crate::segment_build::cpu_seconds()? - merge_start;
        let population: u64 = writer
            .view()
            .segments()
            .iter()
            .map(|s| s.fingerprint_records())
            .sum();
        println!(
            "churn_round: {} followed_records: {followed_records} demoted_records: {added} purged_records: {removed} graduated_total: {graduated} net_records: {population}",
            round + 1
        );
    }
    let live = alive.iter().filter(|&&a| a).count();
    let view = writer.view();
    let mut report = String::new();
    let mut line = |key: &str, value: String| {
        let _ = writeln!(report, "{key}: {value}");
    };
    line(
        "cpu_seconds",
        format!("{:.3}", crate::segment_build::cpu_seconds()? - cpu_start),
    );
    line("merge_cpu_seconds", format!("{merge_cpu:.3}"));
    line("merge_document_reads", document_reads.to_string());
    line("merge_document_read_bytes", document_bytes.to_string());
    line("graduated_records", graduated.to_string());
    line("rounds", rounds.to_string());
    line("births_per_round", BIRTHS.to_string());
    line("death_chance_per_round", format!("1/{DEATH}"));
    line("documents_born", alive.len().to_string());
    line("documents_live", live.to_string());
    line("content_bytes_followed", content.to_string());
    line("follow_written_bytes", followed_written.to_string());
    line("merges", merges.to_string());
    line("merge_written_bytes", merge_written.to_string());
    line(
        "merge_write_amplification",
        ratio(
            term_follow_written + term_merge_written,
            term_follow_written,
        ),
    );
    line(
        "paired_merge_write_amplification",
        ratio(followed_written + merge_written, followed_written),
    );
    line("term_follow_written_bytes", term_follow_written.to_string());
    line("term_merge_written_bytes", term_merge_written.to_string());
    line("max_segments_before_merge", max_segments.to_string());
    line("final_segments", view.segments().len().to_string());
    line(
        "final_index_bytes",
        view.manifest()
            .segments
            .iter()
            .map(|s| s.bytes)
            .sum::<u64>()
            .to_string(),
    );
    line("final_paired_bytes", view.bytes().to_string());
    line(
        "final_records",
        view.segments()
            .iter()
            .map(|s| s.fingerprint_records())
            .sum::<u64>()
            .to_string(),
    );
    line("seconds", format!("{:.2}", start.elapsed().as_secs_f64()));
    print!("{report}");
    // Keep the ordinary S2 trace and its write amplification intact. Measure
    // a final forced merge separately, including purge of the last deaths.
    let live = DocSet::new(
        alive.len() as u32,
        alive
            .iter()
            .enumerate()
            .filter(|&(_, &a)| a)
            .map(|(d, _)| d as u32),
    );
    let catalog = CatalogView {
        incarnation,
        live: &live,
    };
    let cpu = crate::segment_build::cpu_seconds()?;
    let io = io_counters()?;
    let (mut reads, mut bytes_read) = (0u64, 0u64);
    let mut reader = |doc, offset, bytes: &mut Vec<u8>| {
        let window = generated_window(doc, offset, &vocabulary, bytes);
        reads += 1;
        bytes_read += bytes.len() as u64;
        Some(window)
    };
    let merged = writer.merge_all_verified(&catalog, &Budget::unbounded(), &mut reader)?;
    let after_io = io_counters()?;
    println!(
        "churn_full_cpu_seconds: {:.3}",
        crate::segment_build::cpu_seconds()? - cpu
    );
    println!("churn_full_rchar_bytes: {}", after_io.0 - io.0);
    println!("churn_full_device_read_bytes: {}", after_io.1 - io.1);
    println!("churn_full_document_reads: {reads}");
    println!("churn_full_document_read_bytes: {bytes_read}");
    if let Some(merged) = merged {
        println!("churn_full_demoted_records: {}", merged.records_added);
        println!("churn_full_purged_records: {}", merged.records_removed);
        println!("churn_full_graduated_records: {}", merged.records_graduated);
    }
    println!(
        "churn_full_index_bytes: {}",
        writer
            .view()
            .manifest()
            .segments
            .iter()
            .map(|s| s.bytes)
            .sum::<u64>()
    );
    println!(
        "churn_full_records: {}",
        writer
            .view()
            .segments()
            .iter()
            .map(|s| s.fingerprint_records())
            .sum::<u64>()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two passes, one segment each: "alpha beta" then "beta gamma beta".
    /// Entries are counted per segment, distinct terms once.
    #[test]
    fn shape_counts_entries_per_segment_and_terms_once() {
        let dir = std::env::temp_dir().join(format!("ferret-bench-shape-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let texts: [&[u8]; 2] = [b"alpha beta", b"beta gamma beta"];
        let empty = DocSet::new(0, []);
        let open = CatalogView {
            incarnation: [1; 16],
            live: &empty,
        };
        let mut writer = IndexWriter::open(&dir, &open).unwrap();
        for bound in [1, 2] {
            let live = DocSet::new(bound, 0..bound);
            let view = CatalogView {
                incarnation: [1; 16],
                live: &live,
            };
            writer
                .follow(&view, &Budget::unbounded(), &mut |doc, bytes| {
                    bytes.extend_from_slice(texts[doc as usize]);
                    Ok(())
                })
                .unwrap();
        }
        let s = shape(&writer.view()).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            (s.segments, s.entries, s.distinct, s.singletons),
            (2, 4, 3, 4)
        );
        assert_eq!(s.pairs, 4);
        assert!(s.dictionary > 0 && s.dictionary + s.postings < s.bytes);
    }
}
