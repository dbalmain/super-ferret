//! `ferret-bench corpus-sample <catalog-dir> <out-dir> <n>`: a reproducible
//! sample of about `n` documents of the census population, for the tokenize
//! bench and later golden corpora.
//!
//! The choice is by hash, not by randomness, so the same catalog always gives
//! the same sample. The hash is the document's **BLAKE3-128** content hash,
//! which the catalog already holds: its first 8 bytes, big-endian, reduced
//! modulo the population size, and kept when under `n`. BLAKE3's output is
//! uniform, so each document is kept with probability `n / population`.
//!
//! Documents over 1 MiB are skipped after the choice, so the population stays
//! the census's and the skip shows in the summary. A chosen file that is gone,
//! unreadable or now over 1 MiB is skipped and counted too: the tree may have
//! moved on since the crawl. The manifest gives no paths, because a corpus may
//! leave the machine it was sampled on.

use std::fmt::Write as _;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use ferret_catalog::Catalog;

use crate::census::{HashedDoc, display, hashed_documents};

/// The largest document copied, so the tokenize corpus stays loadable in
/// memory. A document of exactly this size is kept.
const MAX_BYTES: u64 = 1 << 20;

/// The documents whose hash falls under `n` modulo the population, in DocId
/// order, and how many of those were over [`MAX_BYTES`].
fn sample(docs: &[HashedDoc], n: u64) -> (Vec<&HashedDoc>, u64) {
    let population = docs.len() as u64;
    let chosen = docs.iter().filter(|doc| {
        let mut prefix = [0; 8];
        prefix.copy_from_slice(&doc.hash[..8]);
        u64::from_be_bytes(prefix) % population < n
    });
    let (kept, large): (Vec<_>, Vec<_>) = chosen.partition(|doc| doc.size <= MAX_BYTES);
    (kept, large.len() as u64)
}

/// Copies each chosen document to `<out>/docs/<docid>` and writes
/// `<out>/manifest.tsv`, one `doc, extension, size` row per copied file.
/// Returns the summary.
fn write_sample(
    catalog: &Catalog,
    docs: &[HashedDoc],
    out: &Path,
    n: u64,
) -> crate::Result<String> {
    let (chosen, large) = sample(docs, n);
    let directory = out.join("docs");
    fs::create_dir_all(&directory)?;
    let mut manifest = String::from("doc\textension\tsize\n");
    let (mut copied, mut bytes, mut unreadable, mut grown) = (0, 0, 0, 0);
    let mut path = Vec::new();
    for doc in chosen {
        path.clear();
        catalog.path(doc.name, &mut path);
        let target = directory.join(doc.doc.0.to_string());
        let size = match fs::copy(Path::new(std::ffi::OsStr::from_bytes(&path)), &target) {
            Ok(size) => size,
            Err(_) => {
                unreadable += 1;
                continue;
            }
        };
        if size > MAX_BYTES {
            fs::remove_file(&target)?;
            grown += 1;
            continue;
        }
        copied += 1;
        bytes += size;
        let _ = writeln!(
            manifest,
            "{}\t{}\t{size}",
            doc.doc.0,
            display(&doc.extension)
        );
    }
    fs::write(out.join("manifest.tsv"), manifest)?;
    let mut summary = String::new();
    let _ = writeln!(
        summary,
        "hash: blake3-128, first 8 bytes big-endian, mod population"
    );
    let _ = writeln!(summary, "population: {}", docs.len());
    let _ = writeln!(summary, "requested: {n}");
    let _ = writeln!(summary, "copied: {copied}");
    let _ = writeln!(summary, "bytes: {bytes}");
    let _ = writeln!(summary, "skipped_over_1mib: {}", large + grown);
    let _ = writeln!(summary, "skipped_unreadable: {unreadable}");
    Ok(summary)
}

/// `ferret-bench corpus-sample <catalog-dir> <out-dir> <n>`.
pub(crate) fn run(dir: &Path, out: &Path, n: &str) -> crate::Result<()> {
    let n = n
        .parse()
        .map_err(|_| format!("sample size must be a count: {n}"))?;
    let catalog = crate::open_catalog(dir)?;
    let docs = hashed_documents(&catalog)?;
    print!("{}", write_sample(&catalog, &docs, out, n)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ferret_catalog::Content;

    use super::*;
    use crate::support::Fixture;

    /// A directory's files, by name.
    fn contents(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let name = entry.file_name().into_string().unwrap();
                (name, fs::read(entry.path()).unwrap())
            })
            .collect()
    }

    #[test]
    fn sampling_is_deterministic_and_skips_documents_over_1_mib() {
        let fixture = Fixture::new("corpus");
        let over = vec![b'x'; (1 << 20) + 1];
        let exact = vec![b'y'; 1 << 20];
        let small: Vec<_> = (0..40).map(|i| format!("document {i}\n")).collect();
        let mut files: Vec<(String, &[u8])> = small
            .iter()
            .enumerate()
            .map(|(i, text)| (format!("{i}.txt"), text.as_bytes()))
            .collect();
        files.push(("over.bin".into(), &over));
        files.push(("exact.md".into(), &exact));
        files.push(("binary.dat".into(), b"\x7fELF"));
        let files: Vec<_> = files
            .iter()
            .map(|(name, bytes)| (name.as_str(), *bytes))
            .collect();
        let catalog = fixture.commit(&files, &[("binary.dat", Content::Binary)]);
        let docs = hashed_documents(&catalog).unwrap();
        assert_eq!(docs.len(), 42, "the binary file is not in the population");

        // `n` at the population keeps every document: all but the one over
        // the cap are copied.
        let everything = fixture.out("everything");
        let summary = write_sample(&catalog, &docs, &everything, 42).unwrap();
        assert!(summary.contains("copied: 41\n"), "{summary}");
        assert!(summary.contains("skipped_over_1mib: 1\n"), "{summary}");
        let copied = contents(&everything.join("docs"));
        assert_eq!(copied.len(), 41);
        assert!(copied.values().all(|bytes| bytes.len() <= 1 << 20));
        assert!(copied.values().any(|bytes| bytes.len() == 1 << 20));
        let manifest = fs::read_to_string(everything.join("manifest.tsv")).unwrap();
        assert_eq!(manifest.lines().count(), 42, "a header and 41 rows");
        assert!(manifest.contains("\tmd\t1048576\n"));
        assert!(!manifest.contains("tree"), "no paths in the manifest");

        // A part of the population: two runs choose and copy the same files.
        let first = fixture.out("first");
        let second = fixture.out("second");
        let summary = write_sample(&catalog, &docs, &first, 10).unwrap();
        assert_eq!(write_sample(&catalog, &docs, &second, 10).unwrap(), summary);
        let chosen = contents(&first.join("docs"));
        assert_eq!(chosen, contents(&second.join("docs")));
        assert!((1..42).contains(&chosen.len()), "{summary}");
        assert_eq!(
            fs::read(first.join("manifest.tsv")).unwrap(),
            fs::read(second.join("manifest.tsv")).unwrap()
        );
    }
}
