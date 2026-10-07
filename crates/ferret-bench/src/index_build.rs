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
//! Documents are read by path with a size check, as `segment-build` and
//! `terms` read them, not through `ferret_crawl::Documents`' checked opens:
//! `ferret-bench` has no `ferret-crawl` edge, and the extra syscalls
//! (one `fstat` per document, one `openat` per directory change) are small
//! beside the read and tokenize. A size mismatch or failed read is
//! [`Fault::Unreadable`], as a checked open's refusal would be.
//!
//! `index-churn <out-dir> <rounds>` runs generated text only: each round
//! adds documents, kills a share of the live ones, follows and merges. It
//! reports merge write amplification and the segment count.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::Instant;

use ferret_catalog::{Catalog, NameId};
use ferret_index::segment::Section;
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
    dictionary: u64,
    postings: u64,
    entries: u64,
    singletons: u64,
    distinct: u64,
    pairs: u64,
}

fn shape(view: &View) -> crate::Result<Shape> {
    let mut shape = Shape {
        segments: view.segments().len(),
        bytes: view.bytes(),
        ..Shape::default()
    };
    let mut cursors = Vec::new();
    for segment in view.segments() {
        shape.dictionary +=
            segment.section_bytes(Section::Blocks) + segment.section_bytes(Section::Index);
        shape.postings += segment.section_bytes(Section::Postings);
        shape.pairs += segment.info().pairs;
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

pub(crate) fn run(dir: &Path, out: &Path) -> crate::Result<()> {
    if out.exists() {
        return Err(format!("{} exists; the build wants a fresh index", out.display()).into());
    }
    let catalog = crate::open_catalog(dir)?;
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
    let mut writer = IndexWriter::open(out, &view)?;
    let (mut path, mut content_bytes, mut docs, mut unreadable, mut passes) =
        (Vec::new(), 0u64, 0u64, 0u64, 0u64);
    let mut written = 0u64;
    loop {
        let followed = writer.follow(&view, &Budget::unbounded(), &mut |doc, bytes| {
            let Some(&(name, size)) = named.get(&doc) else {
                return Err(Fault::Unreadable);
            };
            path.clear();
            catalog.path(name, &mut path);
            match fs::read(Path::new(std::ffi::OsStr::from_bytes(&path))) {
                Ok(read) if read.len() as u64 == size => {
                    *bytes = read;
                    Ok(())
                }
                _ => Err(Fault::Unreadable),
            }
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

    // 2. Merge to steady state.
    let start = Instant::now();
    let (mut merges, mut merge_read, mut merge_written) = (0u64, 0u64, 0u64);
    while let Some(merged) = writer.merge_if_needed(&view, &Budget::unbounded())? {
        merges += 1;
        merge_read += merged.read;
        merge_written += merged.written;
    }
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
    let steady = shape(&writer.view())?;
    shape_lines(&mut report, "steady_", &steady, content_bytes);
    print!("{report}");
    report.clear();

    // 3. One forced full merge.
    let start = Instant::now();
    if let Some(merged) = writer.merge_all(&view, &Budget::unbounded())? {
        line(&mut report, "full_merge_inputs", merged.inputs.to_string());
        line(
            &mut report,
            "full_merge_read_bytes",
            merged.read.to_string(),
        );
    }
    line(
        &mut report,
        "full_merge_seconds",
        format!("{:.2}", start.elapsed().as_secs_f64()),
    );
    let full = shape(&writer.view())?;
    shape_lines(&mut report, "full_", &full, content_bytes);
    let (_, peak) = crate::memory()?;
    line(&mut report, "final_peak_rss", peak);
    print!("{report}");
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

pub(crate) fn churn(out: &Path, rounds: &str) -> crate::Result<()> {
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
    let mut writer = IndexWriter::open(
        out,
        &CatalogView {
            incarnation,
            live: &empty,
        },
    )?;
    let (mut followed_written, mut merge_written, mut merges) = (0u64, 0u64, 0u64);
    let (mut content, mut max_segments) = (0u64, 0usize);
    let start = Instant::now();
    for _ in 0..rounds {
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
            content += followed.bytes;
            if followed.stopped == Stopped::Covered {
                break;
            }
        }
        max_segments = max_segments.max(writer.view().segments().len());
        while let Some(merged) = writer.merge_if_needed(&view, &Budget::unbounded())? {
            merges += 1;
            merge_written += merged.written;
        }
    }
    let live = alive.iter().filter(|&&a| a).count();
    let view = writer.view();
    let mut report = String::new();
    let mut line = |key: &str, value: String| {
        let _ = writeln!(report, "{key}: {value}");
    };
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
        ratio(followed_written + merge_written, followed_written),
    );
    line("max_segments_before_merge", max_segments.to_string());
    line("final_segments", view.segments().len().to_string());
    line("final_index_bytes", view.bytes().to_string());
    line("seconds", format!("{:.2}", start.elapsed().as_secs_f64()));
    print!("{report}");
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
