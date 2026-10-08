//! Matched-snapshot M5 diagnostic: shipped selection, every capped constraint,
//! and actual matches. Candidate bytes use catalog sizes; readable bytes use
//! successful snapshot reads. Only aggregate counts and sizes leave this
//! module.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::Path;

use ferret_index::{Atom, CatalogView, DocSet, Pinned, Source, View};
use ferret_query::GrepAtom;
use ferret_verify::{Grep, GrepFlags, Matcher};

use crate::content_snapshot::Snapshot;
use crate::screen::{drain, key_counts, read_terms, trigram_cursor};

const PER_CLASS: usize = 50;
const BANDS: [&str; 8] = [
    "all",
    "size<1MiB",
    "size1-16MiB",
    "size>=16MiB",
    "keys<=16k",
    "keys16k-64k",
    "keys>64k",
    "keys_unknown",
];

struct Sample {
    class: &'static str,
    grep: Grep,
    shipped: Vec<u32>,
    all: Vec<u32>,
    oracle: Vec<u32>,
}

#[derive(Clone, Copy, Default)]
struct Counts {
    covered: u64,
    uncovered: u64,
    candidate_bytes: u64,
    readable_bytes: u64,
}

fn candidates(
    grep: &Grep,
    pinned: &Pinned<'_>,
    view: &View,
    trigrams: &[ferret_index::segment::Segment<File>],
) -> crate::Result<(Vec<u32>, Vec<u32>)> {
    let atom = GrepAtom::read_until(grep.clone(), pinned, &|| false)?;
    let shipped = drain(pinned.top(atom.cursor_until(pinned, &|| false)?));
    let frequencies = grep
        .planning_trigrams()
        .keys()
        .into_iter()
        .map(|key| {
            let estimate = Source::Trigrams(view)
                .estimate(&Atom::Trigram(key))?
                .ok_or("missing trigram source")?;
            Ok((key, estimate.docs))
        })
        .collect::<crate::Result<BTreeMap<_, _>>>()?;
    let tree = grep.trigrams_by_frequency(&frequencies);
    let lists = read_terms(trigrams, tree.keys().into_iter().map(|key| key.to_vec()))?;
    let all = drain(pinned.top(pinned.atom(trigram_cursor(&tree, &lists, pinned.live()))));
    if !all.iter().all(|doc| shipped.binary_search(doc).is_ok()) {
        return Err("all-constraint candidates exceed shipped candidates".into());
    }
    Ok((shipped, all))
}

fn count(
    docs: &[u32],
    pinned: &Pinned<'_>,
    sizes: &[u64],
    readable: &[Option<u64>],
    keys: &[u64],
) -> [Counts; 8] {
    let mut bands = [Counts::default(); 8];
    for &doc in docs {
        let i = doc as usize;
        let uncovered = pinned.uncovered().contains(doc);
        let size_band = if sizes[i] < 1 << 20 {
            1
        } else if sizes[i] < 16 << 20 {
            2
        } else {
            3
        };
        // Uncovered documents have no observed trigram-key count.
        let key_band = if uncovered {
            7
        } else if keys[i] <= 16 << 10 {
            4
        } else if keys[i] <= 64 << 10 {
            5
        } else {
            6
        };
        for band in [0, size_band, key_band] {
            bands[band].covered += u64::from(!uncovered);
            bands[band].uncovered += u64::from(uncovered);
            bands[band].candidate_bytes += sizes[i];
            bands[band].readable_bytes += readable[i].unwrap_or(0);
        }
    }
    bands
}

pub(crate) fn run(catalog_dir: &Path, index_dir: &Path, snapshot_dir: &Path) -> crate::Result<()> {
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
    .ok_or("no paired index")?;
    let pinned = Pinned::new(Some(&view), &live);
    let [segment] = view.segments() else {
        return Err("regex-volume requires full merge".into());
    };
    let trigrams = view
        .manifest()
        .segments
        .iter()
        .map(|entry| {
            ferret_index::segment::Segment::open(File::open(
                index_dir.join(entry.trigram_file_name()),
            )?)
            .map_err(Into::into)
        })
        .collect::<crate::Result<Vec<_>>>()?;
    let keys = key_counts(&trigrams, live.bound())?;
    let classes = crate::screen::term_classes(segment, PER_CLASS)?;
    let mut samples = Vec::new();
    for (class, mut greps) in crate::screen::regex_classes(&classes, &trigrams, PER_CLASS)? {
        if class == "identifier pattern" {
            greps = crate::content_query::regex_identifiers(&snapshot, &live)?
                .iter()
                .map(|pattern| GrepFlags::default().compile(pattern).map_err(Into::into))
                .collect::<crate::Result<Vec<_>>>()?;
        } else if class != "rare literal" {
            continue;
        }
        if greps.len() != PER_CLASS {
            return Err("insufficient diagnostic seeds".into());
        }
        for grep in greps {
            let (shipped, all) = candidates(&grep, &pinned, &view, &trigrams)?;
            samples.push(Sample {
                class,
                grep,
                shipped,
                all,
                oracle: Vec::new(),
            });
        }
    }
    let mut sizes = vec![0; live.bound() as usize];
    for doc in crate::census::hashed_documents(&catalog)? {
        sizes[doc.doc.0 as usize] = doc.size;
    }
    let mut readable = vec![None; live.bound() as usize];
    // Read each frozen document once, then brute-force every compiled regex.
    for doc in live.range(0, live.bound()) {
        if doc.is_multiple_of(256) {
            crate::trigram::check_disk()?;
        }
        if !snapshot.contains(doc) {
            continue;
        }
        let bytes = fs::read(snapshot.path(doc))?;
        readable[doc as usize] = Some(bytes.len() as u64);
        for sample in &mut samples {
            if sample.grep.regex().is_match(&bytes) {
                if sample.all.binary_search(&doc).is_err() {
                    return Err("actual match missing from full constraints".into());
                }
                sample.oracle.push(doc);
            }
        }
    }
    println!(
        "volume_live={} readable={} readable_bytes={} uncovered={} queries_per_class={PER_CLASS}",
        live.len(),
        readable.iter().filter(|size| size.is_some()).count(),
        readable.iter().flatten().sum::<u64>(),
        pinned.uncovered().len()
    );
    println!(
        "| class | mode | band | covered median/p90 | uncovered median/p90 | candidate B median/p90 | readable B median/p90 | covered sum | uncovered sum | candidate B sum | readable B sum |"
    );
    for class in ["rare literal", "identifier pattern"] {
        for mode in ["four", "all", "oracle"] {
            let rows: Vec<_> = samples
                .iter()
                .filter(|sample| sample.class == class)
                .map(|sample| {
                    let docs = match mode {
                        "four" => &sample.shipped,
                        "all" => &sample.all,
                        _ => &sample.oracle,
                    };
                    count(docs, &pinned, &sizes, &readable, &keys)
                })
                .collect();
            for (band, name) in BANDS.iter().enumerate() {
                let fields: [fn(&Counts) -> u64; 4] = [
                    |c| c.covered,
                    |c| c.uncovered,
                    |c| c.candidate_bytes,
                    |c| c.readable_bytes,
                ];
                let values: Vec<_> = fields
                    .iter()
                    .map(|field| {
                        let mut values: Vec<_> = rows.iter().map(|row| field(&row[band])).collect();
                        values.sort_unstable();
                        (
                            values[(values.len() - 1) / 2],
                            values[(values.len() * 9).div_ceil(10) - 1],
                            values.iter().sum::<u64>(),
                        )
                    })
                    .collect();
                println!(
                    "| {class} | {mode} | {name} | {}/{} | {}/{} | {}/{} | {}/{} | {} | {} | {} | {} |",
                    values[0].0,
                    values[0].1,
                    values[1].0,
                    values[1].1,
                    values[2].0,
                    values[2].1,
                    values[3].0,
                    values[3].1,
                    values[0].2,
                    values[1].2,
                    values[2].2,
                    values[3].2
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferret_index::{Budget, Fault, IndexWriter};

    #[test]
    fn real_selection_and_full_constraints_bound_the_match_oracle() {
        let dir = std::env::temp_dir().join(format!("ferret-volume-{}", std::process::id()));
        let live = DocSet::new(4, 0..4);
        let catalog = CatalogView {
            incarnation: [27; 16],
            live: &live,
        };
        let mut writer = IndexWriter::open(&dir, &catalog).unwrap();
        let bytes: [&[u8]; 3] = [b"abcdefgh", b"abc bcd cde def", b"abc bcd cde def efg fgh"];
        writer
            .follow(&catalog, &Budget::unbounded(), &mut |doc, out| {
                if doc == 3 {
                    return Err(Fault::Unreadable);
                }
                out.extend_from_slice(bytes[doc as usize]);
                Ok(())
            })
            .unwrap();
        let view = writer.view();
        let pinned = Pinned::new(Some(&view), &live);
        let trigrams = view
            .manifest()
            .segments
            .iter()
            .map(|entry| {
                ferret_index::segment::Segment::open(
                    File::open(dir.join(entry.trigram_file_name())).unwrap(),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let grep = GrepFlags::default().compile("abcdefgh").unwrap();
        let (four, all) = candidates(&grep, &pinned, &view, &trigrams).unwrap();
        assert!(four.contains(&0));
        assert_eq!(all, [0, 2, 3]);
        let oracle: Vec<_> = bytes
            .iter()
            .enumerate()
            .filter(|(_, b)| grep.regex().is_match(b))
            .map(|(i, _)| i as u32)
            .collect();
        assert_eq!(oracle, [0]);
        let counts = count(
            &four,
            &pinned,
            &[8, 15, 23, 19],
            &[Some(8), Some(15), Some(23), None],
            &[6, 4, 6, 0],
        );
        assert_eq!(counts[0].uncovered, 1);
        assert_eq!(counts[0].candidate_bytes - counts[0].readable_bytes, 19);
        assert_eq!(counts[7].uncovered, 1);
        assert_eq!(counts[7].readable_bytes, 0);
        fs::remove_dir_all(dir).unwrap();
    }
}
