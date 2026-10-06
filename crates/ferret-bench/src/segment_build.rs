//! `ferret-bench segment-build <catalog-dir> <out-dir>`: builds real
//! segments from every live `Hashed` document and reports what they cost,
//! for M2's dictionary decision (docs/S2.md § M2, § Term dictionary).
//!
//! Documents stream in DocId order through `tokenize` and `cap`, the walk
//! `terms` uses ([`read_documents`]), into an [`Inverter`]. A segment is
//! written each time [`SEGMENT_TEXT`] bytes of text have gone in, or the
//! inverter reaches [`MEMORY_BOUND`]. Each written file is then reopened and
//! read end to end, outside the timed build, so the figures describe
//! segments the reader accepts.

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

/// Input text per segment: S2.md's buffer flush point, an assumption M3
/// tunes.
const SEGMENT_TEXT: u64 = 64 << 20;

/// The inverter's memory bound; a 64 MiB segment estimates well under it.
const MEMORY_BOUND: usize = 1 << 30;

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
    /// Wall-clock in `drain_into` and `finish`: sorting, encoding, writing.
    write_seconds: f64,
    error: Option<Box<dyn std::error::Error>>,
}

impl Build {
    fn new(out: &Path, segment_text: u64) -> Self {
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
            write_seconds: 0.0,
            error: None,
        }
    }

    fn add(&mut self, doc: u32, bytes: &[u8]) {
        if self.error.is_some() {
            return;
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
        let writer = self.inverter.drain_into(first, last)?;
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
fn build_segments(catalog: &Catalog, out: &Path, segment_text: u64) -> crate::Result<(Build, u64)> {
    let docs = hashed_documents(catalog)?;
    fs::create_dir_all(out)?;
    let mut build = Build::new(out, segment_text);
    let skipped = read_documents(catalog, &docs, |doc, bytes| build.add(doc.doc.0, bytes));
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
fn cpu_seconds() -> io::Result<f64> {
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
    out
}

/// `ferret-bench segment-build <catalog-dir> <out-dir>`.
pub(crate) fn run(dir: &Path, out: &Path) -> crate::Result<()> {
    let catalog = crate::open_catalog(dir)?;
    let (cpu_start, start) = (cpu_seconds()?, Instant::now());
    let (build, skipped) = build_segments(&catalog, out, SEGMENT_TEXT)?;
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
        let (build, skipped) = build_segments(&catalog, &out, 1).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(build.files.len(), 3);
        assert_eq!(build.documents, 3);
        // a: fn main parsehttprequest parse http request; b: fn helper;
        // c: main.
        assert_eq!(build.sizes.terms, 6 + 2 + 1);
        assert_eq!(build.sizes.singletons, 9);
        assert_eq!(verify(&build.files).unwrap(), (9, 9));

        let out = fixture.out("one");
        let (build, _) = build_segments(&catalog, &out, u64::MAX).unwrap();
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
}
