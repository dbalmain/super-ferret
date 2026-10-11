//! Offline whole-corpus sparse-gram experiment. Only this benchmark constructs
//! these indexes; the product's index format and query planner are unchanged.
//! Seams: `ferret-verify` supplies necessary literal facts through its Cox
//! fold; `grams` selects substring-local keys; `query` weakens conjunctions by
//! df. All reported fields are aggregate counts, bytes and times.

use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::time::Instant;

use ferret_catalog::Catalog;
use ferret_index::segment::{Segment, TrigramInverter, Writer};
use ferret_index::{CatalogView, DocSet, Pinned, View};
use ferret_query::GrepAtom;
use ferret_verify::trigram::RequiredLiterals;
use ferret_verify::{Grep, GrepFlags, Matcher};

use crate::content_snapshot::Snapshot;
use crate::segment_build::cpu_seconds;

mod grams;
pub(crate) mod latency;
mod query;
#[cfg(test)]
mod tests;

use grams::{Variant, extract, extract_partition, fold, weights};
use query::{Lists, Need};

const PER_CLASS: usize = 50;
const BANDS: [&str; 4] = ["all", "size<1MiB", "size1-16MiB", "size>=16MiB"];

struct Sample {
    class: &'static str,
    grep: Grep,
    facts: RequiredLiterals,
    oracle: Vec<u32>,
    shipped: Vec<u32>,
}

#[derive(Clone, Copy, Default)]
struct Counts {
    docs: u64,
    bytes: u64,
    readable: u64,
}

fn counts(docs: &[u32], sizes: &[u64], readable: &[bool]) -> [Counts; 4] {
    let mut counts = [Counts::default(); 4];
    for &doc in docs {
        let size = sizes[doc as usize];
        let band = if size < 1 << 20 {
            1
        } else if size < 16 << 20 {
            2
        } else {
            3
        };
        for at in [0, band] {
            counts[at].docs += 1;
            counts[at].bytes += size;
            counts[at].readable += if readable[doc as usize] { size } else { 0 };
        }
    }
    counts
}

fn stats(mut values: Vec<u64>) -> (u64, u64, u64) {
    values.sort_unstable();
    (
        values[(values.len() - 1) / 2],
        values[(values.len() * 9).div_ceil(10) - 1],
        values.iter().sum(),
    )
}

fn report(
    mode: &str,
    samples: &[Sample],
    candidates: &[Vec<u32>],
    sizes: &[u64],
    readable: &[bool],
) -> crate::Result<()> {
    for (sample, docs) in samples.iter().zip(candidates) {
        if !sample
            .oracle
            .iter()
            .all(|doc| docs.binary_search(doc).is_ok())
        {
            return Err(format!("oracle match missing: {mode}, {}", sample.class).into());
        }
    }
    for class in [
        "rare literal",
        "identifier pattern",
        "alternation",
        "broad",
        "code class",
    ] {
        let rows: Vec<_> = samples
            .iter()
            .zip(candidates)
            .filter(|(sample, _)| sample.class == class)
            .map(|(sample, docs)| {
                (
                    counts(docs, sizes, readable),
                    counts(&sample.oracle, sizes, readable),
                )
            })
            .collect();
        if rows.is_empty() {
            continue;
        }
        for (band, label) in BANDS.iter().enumerate() {
            let docs = stats(rows.iter().map(|(c, _)| c[band].docs).collect());
            let bytes = stats(rows.iter().map(|(c, _)| c[band].bytes).collect());
            let reads = stats(rows.iter().map(|(c, _)| c[band].readable).collect());
            let oracle_docs = stats(rows.iter().map(|(_, c)| c[band].docs).collect());
            let oracle = stats(rows.iter().map(|(_, c)| c[band].bytes).collect());
            println!(
                "| {mode} | {class} | {label} | {} | {}/{} | {}/{} | {}/{} | {}/{} | {}/{} | {} | {} | {} | {:.6} |",
                rows.len(),
                docs.0,
                docs.1,
                bytes.0,
                bytes.1,
                reads.0,
                reads.1,
                oracle_docs.0,
                oracle_docs.1,
                oracle.0,
                oracle.1,
                bytes.2,
                reads.2,
                oracle.2,
                if reads.2 == 0 {
                    0.0
                } else {
                    1.0 - oracle.2 as f64 / reads.2 as f64
                }
            );
        }
    }
    Ok(())
}

fn build(
    snapshot: &Snapshot,
    live: &[u32],
    variant: Variant,
    weights: &[(u64, u16)],
    partition: Option<u8>,
) -> crate::Result<Lists> {
    let mut lists = Lists::new();
    for (ordinal, &doc) in live.iter().enumerate() {
        if ordinal.is_multiple_of(1024) {
            crate::trigram::check_disk()?;
        }
        if !snapshot.contains(doc) {
            continue;
        }
        add_document(
            &mut lists,
            doc,
            &fs::read(snapshot.path(doc))?,
            variant,
            weights,
            partition,
        );
        if partition.is_none() && ordinal.is_multiple_of(16384) {
            println!(
                "progress mode={} documents={} distinct_keys={}",
                variant.label(),
                ordinal + 1,
                lists.len()
            );
        }
    }
    Ok(lists)
}

fn add_document(
    lists: &mut Lists,
    doc: u32,
    bytes: &[u8],
    variant: Variant,
    weights: &[(u64, u16)],
    partition: Option<u8>,
) {
    let bytes = fold(bytes);
    // Borrowed slices deduplicate without allocating every occurrence.
    let mut distinct = HashSet::new();
    let mut emit = |gram| {
        distinct.insert(gram);
    };
    if let Some(partition) = partition {
        extract_partition(&bytes, variant, weights, partition, &mut emit);
    } else {
        extract(&bytes, variant, weights, &mut emit);
    }
    for gram in distinct {
        if let Some(docs) = lists.get_mut(gram) {
            docs.push(doc);
        } else {
            lists.insert(gram.into(), vec![doc]);
        }
    }
}

/// Encodes a fully merged index through the actual segment writer, including
/// relative DocIds, PFor blocks, inlined singletons, dictionary and checksums.
/// A sink retains the exact size accounting without writing large scratch
/// files.
fn encode(lists: &Lists, bound: u32) -> crate::Result<ferret_index::segment::Sizes> {
    let mut sorted: Vec<_> = lists.iter().collect();
    sorted.sort_unstable_by_key(|(key, _)| *key);
    let mut writer = Writer::keyed(0, bound.saturating_sub(1), 1)?;
    for (key, docs) in sorted {
        writer.push(key, docs)?;
    }
    Ok(writer.finish(&mut io::sink())?)
}

/// Ranges of the first folded byte are disjoint and contiguous in key order.
/// Keeping one writer across ranges gives byte-identical whole-index encoding;
/// only the in-memory map is partitioned. Query lists survive each range.
fn bounded_build(
    snapshot: &Snapshot,
    live: &[u32],
    bound: u32,
    needs: &[Need],
    variant: Variant,
    weights: &[(u64, u16)],
) -> crate::Result<(Lists, ferret_index::segment::Sizes)> {
    let mut required = BTreeSet::new();
    for need in needs {
        need.keys(&mut required);
    }
    let mut query_lists = Lists::new();
    let mut writer = Writer::keyed(0, bound.saturating_sub(1), 1)?;
    for part in 0..32 {
        let lists = build(snapshot, live, variant, weights, Some(part))?;
        let count = lists.len();
        append_partition(&mut writer, lists, &required, &mut query_lists)?;
        println!(
            "partition mode={} completed={} distinct_keys={count}",
            variant.label(),
            part + 1
        );
    }
    Ok((query_lists, writer.finish(&mut io::sink())?))
}

fn append_partition(
    writer: &mut Writer,
    lists: Lists,
    required: &BTreeSet<Vec<u8>>,
    query_lists: &mut Lists,
) -> crate::Result<()> {
    let mut sorted: Vec<_> = lists.into_iter().collect();
    sorted.sort_unstable();
    for (key, docs) in sorted {
        writer.push(&key, &docs)?;
        if required.contains(key.as_ref()) {
            query_lists.insert(key, docs);
        }
    }
    Ok(())
}

fn product_trigram(
    snapshot: &Snapshot,
    live: &[u32],
    bound: u32,
) -> crate::Result<ferret_index::segment::Sizes> {
    let mut inverter = TrigramInverter::new();
    for (ordinal, &doc) in live.iter().enumerate() {
        if ordinal.is_multiple_of(1024) {
            crate::trigram::check_disk()?;
        }
        if snapshot.contains(doc) {
            inverter.add(doc, &fs::read(snapshot.path(doc))?)?;
        }
    }
    Ok(inverter
        .drain_into(0, bound.saturating_sub(1))?
        .finish(&mut io::sink())?)
}

pub(crate) fn run(
    catalog_dir: &Path,
    index_dir: &Path,
    snapshot_dir: &Path,
    only: Option<&str>,
) -> crate::Result<()> {
    if only.is_some_and(|labels| {
        labels
            .split(',')
            .any(|label| !Variant::ALL.iter().any(|variant| variant.label() == label))
    }) {
        return Err("unknown sparse variant".into());
    }
    let variants: Vec<_> = Variant::ALL
        .into_iter()
        .filter(|v| only.is_none_or(|labels| labels.split(',').any(|label| v.label() == label)))
        .collect();
    if variants.is_empty() {
        return Err("unknown sparse variant".into());
    }
    let Corpus {
        snapshot,
        live,
        samples,
        sizes,
        readable,
        ..
    } = prepare(catalog_dir, index_dir, snapshot_dir)?;
    let live_ids: Vec<_> = live.range(0, live.bound()).collect();
    println!(
        "| mode | class | band | queries | candidate docs median/p90 | candidate B median/p90 | readable B median/p90 | oracle docs median/p90 | oracle B median/p90 | candidate B sum | readable B sum | oracle B sum | false readable share | "
    );
    let shipped: Vec<_> = samples.iter().map(|s| s.shipped.clone()).collect();
    report("shipped-four", &samples, &shipped, &sizes, &readable)?;
    let (cpu, wall) = (cpu_seconds()?, Instant::now());
    let encoded = product_trigram(&snapshot, &live_ids, live.bound())?;
    println!(
        "index mode=product-trigram distinct_keys={} pairs={} postings_bytes={} singleton_bytes={} dictionary_bytes={} total_bytes={} build_cpu_s={:.3} build_wall_s={:.3}",
        encoded.terms,
        encoded.pairs,
        encoded.postings,
        encoded.singleton_bytes,
        encoded.dictionary(),
        encoded.total(),
        cpu_seconds()? - cpu,
        wall.elapsed().as_secs_f64()
    );
    let (cpu, wall) = (cpu_seconds()?, Instant::now());
    let mut frequencies = vec![0u64; 1 << 16];
    for (ordinal, &doc) in live_ids.iter().enumerate() {
        if ordinal.is_multiple_of(1024) {
            crate::trigram::check_disk()?;
        }
        if !snapshot.contains(doc) {
            continue;
        }
        for pair in fold(&fs::read(snapshot.path(doc))?).windows(2) {
            frequencies[usize::from(u16::from_be_bytes([pair[0], pair[1]]))] += 1;
        }
    }
    println!(
        "frequency_training cpu_s={:.3} wall_s={:.3} bigram_occurrences={} distinct_bigrams={}",
        cpu_seconds()? - cpu,
        wall.elapsed().as_secs_f64(),
        frequencies.iter().sum::<u64>(),
        frequencies.iter().filter(|&&n| n > 0).count()
    );
    for variant in &variants {
        let variant = *variant;
        let weights = weights(
            &frequencies,
            matches!(
                variant,
                Variant::Sparse {
                    frequency: true,
                    ..
                }
            ),
        );
        let needs: Vec<_> = samples
            .iter()
            .map(|sample| Need::literals(&sample.facts, variant, &weights))
            .collect();
        let (cpu, wall) = (cpu_seconds()?, Instant::now());
        let (lists, encoded) = if !matches!(
            variant,
            Variant::Trigram
                | Variant::Word4
                | Variant::Sparse {
                    frequency: false,
                    cap: 8
                }
        ) {
            bounded_build(
                &snapshot,
                &live_ids,
                live.bound(),
                &needs,
                variant,
                &weights,
            )?
        } else {
            let lists = build(&snapshot, &live_ids, variant, &weights, None)?;
            let encoded = encode(&lists, live.bound())?;
            (lists, encoded)
        };
        println!(
            "index mode={} distinct_keys={} pairs={} postings_bytes={} singleton_bytes={} dictionary_bytes={} total_bytes={} build_cpu_s={:.3} build_wall_s={:.3}",
            variant.label(),
            encoded.terms,
            encoded.pairs,
            encoded.postings,
            encoded.singleton_bytes,
            encoded.dictionary(),
            encoded.total(),
            cpu_seconds()? - cpu,
            wall.elapsed().as_secs_f64()
        );
        let candidates: Vec<_> = needs
            .iter()
            .map(|need| {
                let mut docs = need.candidates(&lists, &live_ids);
                docs.extend(live_ids.iter().filter(|&&doc| !snapshot.contains(doc)));
                docs.sort_unstable();
                docs.dedup();
                docs
            })
            .collect();
        report(&variant.label(), &samples, &candidates, &sizes, &readable)?;
        println!(
            "correctness mode={} queries={} missed_matches=0",
            variant.label(),
            samples.len()
        );
    }
    println!(
        "correctness shipped_and_variants={} queries={} missed_matches=0",
        variants.len() + 1,
        samples.len()
    );
    Ok(())
}

/// Seed generation and the frozen oracle are shared by volume and latency;
/// neither experiment may quietly choose a different query population.
struct Corpus {
    catalog: Catalog,
    view: View,
    snapshot: Snapshot,
    live: DocSet,
    samples: Vec<Sample>,
    sizes: Vec<u64>,
    readable: Vec<bool>,
}

fn prepare(catalog_dir: &Path, index_dir: &Path, snapshot_dir: &Path) -> crate::Result<Corpus> {
    crate::trigram::check_disk()?;
    let catalog = crate::open_catalog(catalog_dir)?;
    catalog.load_all()?;
    let catalog = catalog.into_resident()?;
    let snapshot = Snapshot::open(snapshot_dir, &catalog)?;
    let live = DocSet::new(catalog.next_doc().0, catalog.docs().map(|(doc, _)| doc.0));
    let view = View::open(
        index_dir,
        &CatalogView {
            incarnation: catalog.generation().incarnation,
            live: &live,
        },
    )?
    .ok_or("no paired control index")?;
    let [segment] = view.segments() else {
        return Err("sparse requires full merge".into());
    };
    let pinned = Pinned::new(Some(&view), &live);
    let trigrams = view
        .manifest()
        .segments
        .iter()
        .map(|entry| {
            Segment::open(File::open(index_dir.join(entry.trigram_file_name()))?)
                .map_err(Into::into)
        })
        .collect::<crate::Result<Vec<_>>>()?;
    let classes = crate::screen::term_classes(segment, PER_CLASS)?;
    let mut samples = Vec::new();
    for (class, mut greps) in crate::screen::regex_classes(&classes, &trigrams, PER_CLASS)? {
        if class == "identifier pattern" {
            greps = crate::content_query::regex_identifiers(&snapshot, &live)?
                .iter()
                .map(|pattern| GrepFlags::default().compile(pattern).map_err(Into::into))
                .collect::<crate::Result<Vec<_>>>()?;
        }
        if greps.len() != PER_CLASS {
            return Err("insufficient sparse query seeds".into());
        }
        for grep in greps {
            let facts = GrepFlags::default().required_literals(grep.pattern())?;
            let atom = GrepAtom::read_until(grep.clone(), &pinned, &|| false)?;
            let shipped = crate::screen::drain(pinned.top(atom.cursor_until(&pinned, &|| false)?));
            samples.push(Sample {
                class,
                grep,
                facts,
                oracle: Vec::new(),
                shipped,
            });
        }
    }
    let mut sizes = vec![0; live.bound() as usize];
    for document in crate::census::hashed_documents(&catalog)? {
        sizes[document.doc.0 as usize] = document.size;
    }
    let mut readable = vec![false; live.bound() as usize];
    let live_ids: Vec<_> = live.range(0, live.bound()).collect();
    let (cpu, wall) = (cpu_seconds()?, Instant::now());
    for (ordinal, &doc) in live_ids.iter().enumerate() {
        if ordinal.is_multiple_of(1024) {
            crate::trigram::check_disk()?;
        }
        if !snapshot.contains(doc) {
            continue;
        }
        let bytes = fs::read(snapshot.path(doc))?;
        if bytes.len() as u64 != sizes[doc as usize] {
            return Err("frozen size differs from catalog".into());
        }
        readable[doc as usize] = true;
        for sample in &mut samples {
            if sample.grep.regex().is_match(&bytes) {
                sample.oracle.push(doc);
            }
        }
        if ordinal.is_multiple_of(16384) {
            println!("oracle_progress documents={}", ordinal + 1);
        }
    }
    println!(
        "population live={} readable={} unreadable={} content_bytes={} queries={} oracle_cpu_s={:.3} oracle_wall_s={:.3}",
        live.len(),
        readable.iter().filter(|&&r| r).count(),
        readable
            .iter()
            .filter(|&&r| r)
            .count()
            .abs_diff(live.len() as usize),
        sizes
            .iter()
            .enumerate()
            .filter(|(i, _)| readable[*i])
            .map(|(_, &s)| s)
            .sum::<u64>(),
        samples.len(),
        cpu_seconds()? - cpu,
        wall.elapsed().as_secs_f64()
    );
    Ok(Corpus {
        catalog,
        view,
        snapshot,
        live,
        samples,
        sizes,
        readable,
    })
}
