//! S3 M1a offline layouts and census, without regex derivation or manifests.
//! M3 boundaries use the production term inverter's legacy flush rule in
//! a separate planning process. D70 feeds both structures from each read,
//! charges capacities, and merges pairs by combined file bytes.

use std::cell::Cell;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::Instant;

use ferret_catalog::Catalog;
use ferret_index::segment::{Inverter, ReadAt, Section, Segment, TrigramInverter, Writer};
use ferret_index::{BUFFER, DocSet, SegmentEntry, merge};
use ferret_text::{Scratch, TERM_VERSION, TRIGRAM_VERSION, cap, tokenize};

use crate::census::{HashedDoc, hashed_documents};
use crate::segment_build::cpu_seconds;

pub(crate) fn check_disk() -> crate::Result<()> {
    let output = Command::new("df")
        .args(["-B1", "--output=avail", "."])
        .output()?;
    let available: u64 = std::str::from_utf8(&output.stdout)?
        .lines()
        .last()
        .ok_or("missing disk availability")?
        .trim()
        .parse()?;
    if !output.status.success() || available < 5_000_000_000 {
        return Err("free disk below 5 GB; stop and report".into());
    }
    Ok(())
}

fn read(catalog: &Catalog, doc: &HashedDoc, path: &mut Vec<u8>) -> Option<Vec<u8>> {
    path.clear();
    catalog.path(doc.name, path);
    fs::read(Path::new(std::ffi::OsStr::from_bytes(path)))
        .ok()
        .filter(|bytes| bytes.len() as u64 == doc.size)
}

fn add_terms(
    inverter: &mut Inverter,
    scratch: &mut Scratch,
    doc: u32,
    bytes: &[u8],
) -> crate::Result<()> {
    let mut error = None;
    tokenize(bytes, scratch, |token| {
        if let Err(failed) = inverter.add(doc, cap(token.bytes)) {
            error = Some(failed);
        }
    });
    if let Some(error) = error {
        return Err(error.into());
    }
    Ok(())
}

/// Reproduce follow's buffer checks before each document, including failed
/// reads in ranges. No output keys or document bytes leave this process.
pub(crate) fn boundaries(dir: &Path, out: &Path) -> crate::Result<()> {
    let catalog = crate::open_catalog(dir)?;
    let docs = hashed_documents(&catalog)?;
    let mut inverter = Inverter::new(BUFFER);
    let mut scratch = Scratch::default();
    let mut path = Vec::new();
    let mut ranges = Vec::new();
    let mut range = None;
    let mut content = 0u64;
    let mut readable = 0u64;
    for doc in &docs {
        if inverter.is_full() {
            if let Some(pair) = range.take() {
                ranges.push(pair);
            }
            inverter = Inverter::new(BUFFER);
            check_disk()?;
        }
        if let Some(bytes) = read(&catalog, doc, &mut path) {
            add_terms(&mut inverter, &mut scratch, doc.doc.0, &bytes)?;
            content += bytes.len() as u64;
            readable += 1;
        }
        range = Some((range.map_or(doc.doc.0, |(first, _)| first), doc.doc.0));
    }
    if let Some(pair) = range {
        ranges.push(pair);
    }
    let text: String = ranges
        .iter()
        .map(|(first, last)| format!("{first}\t{last}\n"))
        .collect();
    fs::write(out, text)?;
    println!(
        "boundary_segments={} boundary_documents={readable} boundary_content_bytes={content}",
        ranges.len()
    );
    Ok(())
}

pub(crate) fn census(dir: &Path) -> crate::Result<()> {
    check_disk()?;
    let catalog = crate::open_catalog(dir)?;
    let docs = hashed_documents(&catalog)?;
    let mut inverter = TrigramInverter::new();
    let mut overall = vec![0u64; (1 << 24) / 64];
    let mut histogram = [(0u64, 0u64, 0u64); 25];
    let thresholds = [16 << 10, 32 << 10, 64 << 10, 128 << 10, 256 << 10];
    let mut above = [(0u64, 0u64, 0u64); 5];
    let (mut path, mut unreadable) = (Vec::new(), 0);
    let (mut large_docs, mut large_bytes, mut large_pairs) = (0u64, 0u64, 0u64);
    let (mut total_docs, mut content, mut pairs) = (0u64, 0u64, 0u64);
    let start = Instant::now();
    let cpu = cpu_seconds()?;
    for (i, doc) in docs.iter().enumerate() {
        if i % 1000 == 0 {
            check_disk()?;
        }
        let Some(bytes) = read(&catalog, doc, &mut path) else {
            unreadable += 1;
            continue;
        };
        let count = inverter.add_with(doc.doc.0, &bytes, |key| {
            let number = u32::from_be_bytes([0, key[0], key[1], key[2]]) as usize;
            overall[number / 64] |= 1 << (number % 64);
        })? as u64;
        inverter.clear();
        let bucket = (u64::BITS - count.leading_zeros()) as usize;
        histogram[bucket].0 += 1;
        histogram[bucket].1 += bytes.len() as u64;
        histogram[bucket].2 += count;
        for (threshold, row) in thresholds.iter().zip(&mut above) {
            if count > *threshold {
                row.0 += 1;
                row.1 += bytes.len() as u64;
                row.2 += count;
            }
        }
        if bytes.len() >= 1 << 20 {
            large_docs += 1;
            large_bytes += bytes.len() as u64;
            large_pairs += count;
        }
        total_docs += 1;
        content += bytes.len() as u64;
        pairs += count;
    }
    let distinct: u64 = overall
        .iter()
        .map(|word| u64::from(word.count_ones()))
        .sum();
    println!(
        "census documents={total_docs} unreadable={unreadable} content_bytes={content} pairs={pairs} distinct_keys={distinct} large_documents={large_docs} large_bytes={large_bytes} large_pairs={large_pairs} small_pairs_per_byte={:.6} large_mean_keys={:.2} cpu_seconds={:.2} wall_seconds={:.2}",
        (pairs - large_pairs) as f64 / (content - large_bytes) as f64,
        large_pairs as f64 / large_docs as f64,
        cpu_seconds()? - cpu,
        start.elapsed().as_secs_f64()
    );
    for (i, &(docs, bytes, pairs)) in histogram.iter().enumerate().filter(|(_, row)| row.0 != 0) {
        let (low, high) = if i == 0 {
            (0, 0)
        } else {
            (1u64 << (i - 1), (1u64 << i) - 1)
        };
        println!("histogram low={low} high={high} documents={docs} bytes={bytes} pairs={pairs}");
    }
    for (threshold, (docs, bytes, pairs)) in thresholds.into_iter().zip(above) {
        println!("above threshold={threshold} documents={docs} bytes={bytes} pairs={pairs}");
    }
    Ok(())
}

struct Artifact {
    path: PathBuf,
    entry: SegmentEntry,
}

struct Pair {
    term: Option<Artifact>,
    trigram: Artifact,
}

fn artifact(path: PathBuf, number: u64, docs: u32, version: u32) -> crate::Result<Artifact> {
    let file = File::open(&path)?;
    let bytes = file.metadata()?.len();
    let segment = Segment::open(file)?;
    let info = segment.info();
    let entry = SegmentEntry {
        number,
        first: info.first,
        last: info.last,
        docs,
        terms: info.terms,
        pairs: info.pairs,
        bytes,
        digest: segment.digest(),
        trigram_bytes: 0,
        trigram_keys: 0,
        trigram_pairs: 0,
        trigram_digest: [0; 16],
    };
    if !entry.matches(&info, &segment.digest(), bytes, version) {
        return Err("offline segment extractor mismatch".into());
    }
    Ok(Artifact { path, entry })
}

#[derive(Default)]
struct Shape {
    bytes: u64,
    dictionary: u64,
    postings: u64,
    resident: usize,
    entries: u64,
    distinct: u64,
    pairs: u64,
}

fn shape(artifacts: &[&Artifact]) -> crate::Result<Shape> {
    let mut result = Shape::default();
    let mut segments = Vec::new();
    for artifact in artifacts {
        let segment = Segment::open(File::open(&artifact.path)?)?;
        result.bytes += artifact.entry.bytes;
        result.dictionary +=
            segment.section_bytes(Section::Blocks) + segment.section_bytes(Section::Index);
        result.postings += segment.section_bytes(Section::Postings);
        result.resident += segment.resident_bytes();
        result.pairs += segment.info().pairs;
        segments.push(segment);
    }
    let mut cursors: Vec<_> = segments.iter().map(Segment::terms).collect();
    let mut heap = BinaryHeap::new();
    for (i, cursor) in cursors.iter_mut().enumerate() {
        if let Some((term, _)) = cursor.next_term()? {
            heap.push(Reverse((term.to_vec(), i)));
        }
    }
    let mut last = None;
    while let Some(Reverse((term, i))) = heap.pop() {
        result.entries += 1;
        if last.as_ref() != Some(&term) {
            result.distinct += 1;
            last = Some(term);
        }
        if let Some((term, _)) = cursors[i].next_term()? {
            heap.push(Reverse((term.to_vec(), i)));
        }
    }
    Ok(result)
}

fn report(state: &str, kind: &str, count: usize, s: &Shape, content: u64) {
    println!(
        "{state}_{kind} segments={count} bytes={} bytes_per_content_byte={:.6} dictionary={} postings={} other={} entries={} distinct={} pairs={} postings_bits_per_pair={:.6} file_bits_per_pair={:.6} h={:.6} resident_bytes={}",
        s.bytes,
        s.bytes as f64 / content as f64,
        s.dictionary,
        s.postings,
        s.bytes - s.dictionary - s.postings,
        s.entries,
        s.distinct,
        s.pairs,
        s.postings as f64 * 8.0 / s.pairs as f64,
        s.bytes as f64 * 8.0 / s.pairs as f64,
        s.entries as f64 / s.distinct as f64,
        s.resident
    );
}

fn report_state(state: &str, pairs: &[Pair], content: u64) -> crate::Result<Shape> {
    let trigrams: Vec<_> = pairs.iter().map(|pair| &pair.trigram).collect();
    let tri = shape(&trigrams)?;
    report(state, "trigram", pairs.len(), &tri, content);
    let terms: Vec<_> = pairs.iter().filter_map(|pair| pair.term.as_ref()).collect();
    if !terms.is_empty() {
        report(state, "term", terms.len(), &shape(&terms)?, content);
    }
    Ok(tri)
}

/// Separate process runs keep boundary planning out of the trigram build's
/// RSS and CPU. D70's A/B runs each have their own process as well.
#[derive(Clone, Copy)]
pub(crate) enum Policy {
    M3,
    Own { trigram_bound: usize },
    Shared,
}

impl Policy {
    fn capacity_full(self, term: &Inverter, tri: &TrigramInverter, host: usize) -> bool {
        match self {
            Self::M3 => false,
            Self::Own { trigram_bound } => {
                term.flush_bytes() >= BUFFER || tri.flush_bytes() >= trigram_bound
            }
            Self::Shared => term.flush_bytes() + tri.flush_bytes() + host >= BUFFER,
        }
    }
}

fn reset_peak() -> crate::Result<()> {
    // Linux clear_refs value 5 resets VmHWM to current RSS, so later merge
    // peaks do not merely repeat the preceding build's high water.
    fs::write("/proc/self/clear_refs", "5")?;
    Ok(())
}

fn peak_kib() -> crate::Result<u64> {
    Ok(crate::memory()?
        .1
        .split_whitespace()
        .next()
        .ok_or("missing RSS")?
        .parse()?)
}

pub(crate) fn build(dir: &Path, out: &Path, policy: Policy) -> crate::Result<()> {
    check_disk()?;
    if out.exists() {
        return Err("offline build wants a fresh output directory".into());
    }
    fs::create_dir(out)?;
    let mut boundaries = Vec::new();
    if matches!(policy, Policy::M3) {
        let plan = out.join("boundaries.tsv");
        let status = Command::new(std::env::current_exe()?)
            .arg("trigram-boundaries")
            .arg(dir)
            .arg(&plan)
            .status()?;
        if !status.success() {
            return Err("M3 boundary planning failed".into());
        }
        for line in fs::read_to_string(plan)?.lines() {
            let (first, last) = line.split_once('\t').ok_or("bad boundary")?;
            boundaries.push((first.parse::<u32>()?, last.parse::<u32>()?));
        }
    }
    let catalog = crate::open_catalog(dir)?;
    let docs = hashed_documents(&catalog)?;
    let live = DocSet::new(catalog.next_doc().0, docs.iter().map(|d| d.doc.0));
    let paired = !matches!(policy, Policy::M3);
    match policy {
        Policy::M3 => println!("policy=M3 term_estimate_bound={BUFFER} tri_bound=none"),
        Policy::Own { trigram_bound } => {
            println!("policy=A term_capacity_bound={BUFFER} trigram_capacity_bound={trigram_bound}")
        }
        Policy::Shared => println!("policy=B shared_capacity_bound={BUFFER}"),
    }
    reset_peak()?;
    let start = Instant::now();
    let cpu = cpu_seconds()?;
    let mut term = Inverter::new(BUFFER);
    let mut tri = TrigramInverter::new();
    let mut scratch = Scratch::default();
    let (mut path, mut bytes) = (Vec::new(), Vec::new());
    let (mut pairs, mut range) = (Vec::new(), None);
    let (mut content, mut readable, mut unreadable, mut segment_docs) = (0u64, 0u64, 0u64, 0u32);
    let (mut capacity_peak, mut flush_charge_peak) = (0usize, 0usize);
    let (mut term_flushes, mut trigram_flushes) = (0usize, 0usize);
    for doc in &docs {
        let term_charge = if paired { term.flush_bytes() } else { 0 };
        let tri_charge = tri.flush_bytes();
        let shared_charge = term_charge + tri_charge + bytes.capacity() + scratch.allocated_bytes();
        flush_charge_peak = flush_charge_peak.max(shared_charge);
        let full = match policy {
            Policy::M3 => range.is_some_and(|(_, last)| boundaries[pairs.len()].1 == last),
            Policy::Own { .. } | Policy::Shared => {
                policy.capacity_full(&term, &tri, bytes.capacity() + scratch.allocated_bytes())
            }
        };
        if full && let Some((first, last)) = range.take() {
            if let Policy::Own { trigram_bound } = policy {
                term_flushes += usize::from(term_charge >= BUFFER);
                trigram_flushes += usize::from(tri_charge >= trigram_bound);
            }
            pairs.push(
                Flush {
                    out,
                    paired,
                    read_capacity: bytes.capacity() + scratch.allocated_bytes(),
                    peak: &mut capacity_peak,
                }
                .write(
                    pairs.len() as u64,
                    (first, last),
                    segment_docs,
                    &mut term,
                    &mut tri,
                )?,
            );
            segment_docs = 0;
            // Follow creates fresh buffers for every segment. Dropping
            // retained capacity prevents empty segments after a flush.
            drop(term);
            term = Inverter::new(BUFFER);
            drop(tri);
            tri = TrigramInverter::new();
            if paired {
                // Production follow starts a new tokenizer scratch and
                // document buffer for each segment, not just inverters.
                scratch = Scratch::default();
                bytes = Vec::new();
            }
        }
        if let Some(read) = read(&catalog, doc, &mut path) {
            capacity_peak = capacity_peak.max(
                term.allocated_bytes()
                    + tri.allocated_bytes()
                    + scratch.allocated_bytes()
                    + bytes.capacity()
                    + read.capacity(),
            );
            bytes = read;
            if paired {
                add_terms(&mut term, &mut scratch, doc.doc.0, &bytes)?;
            }
            tri.add(doc.doc.0, &bytes)?;
            readable += 1;
            content += bytes.len() as u64;
            segment_docs += 1;
            capacity_peak = capacity_peak.max(
                term.allocated_bytes()
                    + tri.allocated_bytes()
                    + bytes.capacity()
                    + scratch.allocated_bytes(),
            );
            flush_charge_peak = flush_charge_peak.max(
                (if paired { term.flush_bytes() } else { 0 })
                    + tri.flush_bytes()
                    + bytes.capacity()
                    + scratch.allocated_bytes(),
            );
        } else {
            unreadable += 1;
        }
        range = Some((range.map_or(doc.doc.0, |(first, _)| first), doc.doc.0));
    }
    if let Some((first, last)) = range {
        pairs.push(
            Flush {
                out,
                paired,
                read_capacity: bytes.capacity() + scratch.allocated_bytes(),
                peak: &mut capacity_peak,
            }
            .write(
                pairs.len() as u64,
                (first, last),
                segment_docs,
                &mut term,
                &mut tri,
            )?,
        );
    }
    drop((term, tri, scratch, bytes));
    if matches!(policy, Policy::Own { .. }) {
        println!("own_flushes term={term_flushes} trigram={trigram_flushes}");
    }
    println!(
        "build documents={readable} unreadable={unreadable} content_bytes={content} cpu_seconds={:.2} wall_seconds={:.2} peak_rss={} capacity_peak={capacity_peak} flush_charge_peak={flush_charge_peak}",
        cpu_seconds()? - cpu,
        start.elapsed().as_secs_f64(),
        crate::memory()?.1.replace(' ', "")
    );
    report_state("built", &pairs, content)?;
    let mut combined_peak = peak_kib()?;
    let mut number = pairs.len() as u64;
    reset_peak()?;
    let start = Instant::now();
    let cpu = cpu_seconds()?;
    let mut work = Work::default();
    loop {
        let entries: Vec<_> = pairs
            .iter()
            .map(|pair| {
                let mut entry = pair.trigram.entry;
                entry.bytes += pair.term.as_ref().map_or(0, |term| term.entry.bytes);
                entry
            })
            .collect();
        let Some(run) = merge::choose(&entries, &|entry| entry.docs) else {
            break;
        };
        merge_run(out, &mut pairs, run, &live, &mut number, &mut work)?;
    }
    report_work("steady", &work, start, cpu)?;
    let steady = report_state("steady", &pairs, content)?;
    combined_peak = combined_peak.max(peak_kib()?);
    if steady.bytes as f64 / content as f64 > 0.20 {
        println!("gate=STOP trigram_steady_exceeds_20_percent D69_D70_return_to_Dave");
        return Ok(());
    }
    reset_peak()?;
    let start = Instant::now();
    let cpu = cpu_seconds()?;
    let mut work = Work::default();
    if pairs.len() > 1 {
        let count = pairs.len();
        merge_run(out, &mut pairs, 0..count, &live, &mut number, &mut work)?;
    }
    report_work("full", &work, start, cpu)?;
    report_state("full", &pairs, content)?;
    combined_peak = combined_peak.max(peak_kib()?);
    println!("combined_peak_rss_kib={combined_peak}");
    Ok(())
}

// This function is the paired flush boundary: both inverters remain alive
// while either writer encodes, and its observer charges that overlap.
struct Flush<'a> {
    out: &'a Path,
    paired: bool,
    read_capacity: usize,
    peak: &'a mut usize,
}

impl Flush<'_> {
    fn write(
        self,
        number: u64,
        range: (u32, u32),
        docs: u32,
        term: &mut Inverter,
        tri: &mut TrigramInverter,
    ) -> crate::Result<Pair> {
        let Self {
            out,
            paired,
            read_capacity,
            peak,
        } = self;
        let (first, last) = range;
        check_disk()?;
        let stem = format!("seg-{first}-{last}-{number}");
        let term_file = if paired {
            let other = tri.allocated_bytes() + read_capacity;
            let writer =
                term.drain_into_measured(first, last, |bytes| *peak = (*peak).max(other + bytes))?;
            *peak = (*peak).max(other + term.allocated_bytes() + writer.finishing_bytes());
            let path = out.join(format!("{stem}.seg"));
            writer.finish(&mut File::create(&path)?)?;
            Some(artifact(path, number, docs, TERM_VERSION)?)
        } else {
            None
        };
        let other = term.allocated_bytes() + read_capacity;
        let writer =
            tri.drain_into_with(first, last, |bytes| *peak = (*peak).max(other + bytes))?;
        *peak = (*peak).max(other + tri.allocated_bytes() + writer.finishing_bytes());
        let path = out.join(format!("{stem}.tri"));
        writer.finish(&mut File::create(&path)?)?;
        Ok(Pair {
            term: term_file,
            trigram: artifact(path, number, docs, TRIGRAM_VERSION)?,
        })
    }
}

#[derive(Default)]
struct Work {
    merges: u64,
    inputs: u64,
    input_bytes: u64,
    output_bytes: u64,
    read_bytes: u64,
    transfers: u64,
}

struct Metered {
    file: File,
    read: Rc<Cell<u64>>,
}
impl ReadAt for Metered {
    fn size(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.read.set(self.read.get() + buf.len() as u64);
        ReadAt::read_exact_at(&self.file, buf, offset)
    }
}

fn merge_files(
    out: &Path,
    inputs: &[&Artifact],
    live: &DocSet,
    number: u64,
    version: u32,
    work: &mut Work,
) -> crate::Result<Artifact> {
    let read = Rc::new(Cell::new(0));
    let segments: Vec<_> = inputs
        .iter()
        .map(|input| {
            work.input_bytes += input.entry.bytes;
            Segment::open(Metered {
                file: File::open(&input.path)?,
                read: read.clone(),
            })
        })
        .collect::<Result<_, ferret_index::ReadError>>()?;
    let first = inputs.first().ok_or("empty merge")?.entry.first;
    let last = inputs.last().ok_or("empty merge")?.entry.last;
    let docs = inputs.iter().map(|input| input.entry.docs).sum();
    let ext = if version == TRIGRAM_VERSION {
        "tri"
    } else {
        "seg"
    };
    let path = out.join(format!("seg-{first}-{last}-{number}.{ext}"));
    let scratch = out.join("postings.scratch");
    let writer = Writer::keyed_spilling(
        first,
        last,
        version,
        File::create(&path)?,
        File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&scratch)?,
    )?;
    let transfers = Cell::new(0u64);
    let sizes = merge::stream(
        &segments,
        live,
        writer,
        &|bytes| {
            transfers.set(transfers.get() + bytes as u64);
            Ok(())
        },
        &|| false,
    )?;
    work.read_bytes += read.get();
    work.output_bytes += sizes.total();
    work.transfers += transfers.get();
    fs::remove_file(scratch)?;
    artifact(path, number, docs, version)
}

fn merge_run(
    out: &Path,
    pairs: &mut Vec<Pair>,
    run: std::ops::Range<usize>,
    live: &DocSet,
    number: &mut u64,
    work: &mut Work,
) -> crate::Result<()> {
    check_disk()?;
    let inputs = &pairs[run.clone()];
    let trigram = merge_files(
        out,
        &inputs.iter().map(|pair| &pair.trigram).collect::<Vec<_>>(),
        live,
        *number,
        TRIGRAM_VERSION,
        work,
    )?;
    let terms: Vec<_> = inputs
        .iter()
        .filter_map(|pair| pair.term.as_ref())
        .collect();
    let term = if terms.is_empty() {
        None
    } else {
        Some(merge_files(out, &terms, live, *number, TERM_VERSION, work)?)
    };
    work.merges += 1;
    work.inputs += inputs.len() as u64;
    for pair in inputs {
        fs::remove_file(&pair.trigram.path)?;
        if let Some(term) = &pair.term {
            fs::remove_file(&term.path)?;
        }
    }
    pairs.splice(run, [Pair { term, trigram }]);
    *number += 1;
    Ok(())
}

fn report_work(state: &str, work: &Work, start: Instant, cpu: f64) -> crate::Result<()> {
    println!(
        "{state}_work merges={} input_segments={} input_bytes={} output_bytes={} segment_read_bytes={} paced_transfer_bytes={} cpu_seconds={:.2} wall_seconds={:.2} peak_rss={}",
        work.merges,
        work.inputs,
        work.input_bytes,
        work.output_bytes,
        work.read_bytes,
        work.transfers,
        cpu_seconds()? - cpu,
        start.elapsed().as_secs_f64(),
        crate::memory()?.1.replace(' ', "")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::Fixture;

    #[test]
    fn shared_flush_counts_both_real_inverters_and_radix_capacity() {
        let mut term = Inverter::new(BUFFER);
        for i in 0..230_000 {
            term.add(0, format!("{i:016x}").as_bytes()).unwrap();
        }
        let mut seed = 1234567u64;
        let bytes: Vec<_> = (0..2_000_000)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        let mut tri = TrigramInverter::new();
        tri.add(0, &bytes).unwrap();
        assert!(term.flush_bytes() < BUFFER);
        assert!(tri.flush_bytes() < BUFFER);
        assert!(
            !Policy::Own {
                trigram_bound: BUFFER
            }
            .capacity_full(&term, &tri, 0)
        );
        // Checking whichever inverter fills first misses simultaneous
        // allocations. The production B condition must flush this pair.
        assert!(Policy::Shared.capacity_full(&term, &tri, bytes.capacity()));
        assert!(tri.flush_bytes() > tri.allocated_bytes());
    }

    #[test]
    fn paired_outputs_and_combined_policy_merge_preserve_both_indexes() {
        let fixture = Fixture::new("trigram-pair");
        let out = fixture.out("outputs");
        fs::create_dir(&out).unwrap();
        let mut pairs = Vec::new();
        let mut peak = 0;
        for doc in 0..11 {
            let mut term = Inverter::new(BUFFER);
            term.add(doc, b"aaa").unwrap();
            term.add(doc, format!("item{doc}").as_bytes()).unwrap();
            let mut tri = TrigramInverter::new();
            tri.add(doc, b"AAaaa").unwrap();
            pairs.push(
                Flush {
                    out: &out,
                    paired: true,
                    read_capacity: 0,
                    peak: &mut peak,
                }
                .write(u64::from(doc), (doc, doc), 1, &mut term, &mut tri)
                .unwrap(),
            );
        }
        let before = shape(&pairs.iter().map(|pair| &pair.trigram).collect::<Vec<_>>()).unwrap();
        assert_eq!((before.pairs, before.entries, before.distinct), (11, 11, 1));
        let live = DocSet::new(11, 0..11);
        let entries: Vec<_> = pairs
            .iter()
            .map(|pair| {
                let mut entry = pair.trigram.entry;
                entry.bytes += pair.term.as_ref().unwrap().entry.bytes;
                entry
            })
            .collect();
        let run = merge::choose(&entries, &|entry| entry.docs).unwrap();
        let mut number = 11;
        let mut work = Work::default();
        merge_run(&out, &mut pairs, run, &live, &mut number, &mut work).unwrap();
        assert_eq!(pairs.len(), 2);
        merge_run(&out, &mut pairs, 0..2, &live, &mut number, &mut work).unwrap();
        let after = shape(&[&pairs[0].trigram]).unwrap();
        assert_eq!((after.pairs, after.entries, after.distinct), (11, 1, 1));
        let term = shape(&[pairs[0].term.as_ref().unwrap()]).unwrap();
        assert_eq!((term.pairs, term.entries, term.distinct), (22, 12, 12));
        assert!(work.read_bytes > 0 && work.output_bytes > 0 && peak > 0);
    }
}
