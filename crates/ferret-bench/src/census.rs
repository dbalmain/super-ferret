//! `ferret-bench census <catalog-dir>`: what the content index would hold.
//!
//! The population — live documents whose content is `Hashed`, one row per
//! DocId — is defined here and shared with the corpus sampler, so both
//! commands count the same set. A document's representative is the first
//! name in name order that holds it; that name gives its extension and the
//! path the sampler copies. The size is the representative inode's
//! `st_size` as the catalog recorded it: the census reads only the catalog
//! and never opens a file.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use ferret_catalog::{Catalog, DocId, Hash, NameId, OpenError, Section, Target};

/// One live `Hashed` document.
pub(crate) struct HashedDoc {
    pub(crate) doc: DocId,
    /// The BLAKE3-128 content hash the crawler recorded.
    pub(crate) hash: Hash,
    /// The representative inode's `st_size`.
    pub(crate) size: u64,
    /// The representative name's extension, lower-cased; empty for none.
    pub(crate) extension: Vec<u8>,
    /// The representative name.
    pub(crate) name: NameId,
}

/// The population, in DocId order. Only a `Hashed` inode has a document, so
/// directories, symlinks and binary, over-cap or faulted files drop out at
/// `Catalog::doc`; a document with no live hash is not live.
pub(crate) fn hashed_documents(catalog: &Catalog) -> Result<Vec<HashedDoc>, OpenError> {
    catalog.load(&[
        Section::Names,
        Section::Roots,
        Section::Size,
        Section::Doc,
        Section::Docs,
    ])?;
    let mut docs = BTreeMap::new();
    for (name, edge) in catalog.name_reader().runs_from(NameId(0)) {
        let Target::Inode(inode) = edge.target() else {
            continue;
        };
        let Some(doc) = catalog.doc(inode) else {
            continue;
        };
        let Some(hash) = catalog.doc_hash(doc) else {
            continue;
        };
        docs.entry(doc).or_insert_with(|| HashedDoc {
            doc,
            hash,
            size: catalog.size(inode),
            extension: extension(edge.bytes),
            name,
        });
    }
    Ok(docs.into_values().collect())
}

/// The lower-cased text after a name's last `.`; empty for none, and for a
/// name that is only an extension (`.bashrc`), as `ferret stats` counts it.
fn extension(name: &[u8]) -> Vec<u8> {
    match name.iter().rposition(|&b| b == b'.') {
        Some(at) if at > 0 => name[at + 1..].to_ascii_lowercase(),
        _ => Vec::new(),
    }
}

/// An extension as one line or table cell: lossy UTF-8, control characters
/// escaped, `|` escaped for Markdown, and `(none)` for no extension.
pub(crate) fn display(extension: &[u8]) -> String {
    if extension.is_empty() {
        return "(none)".into();
    }
    String::from_utf8_lossy(extension)
        .escape_debug()
        .to_string()
        .replace('|', "\\|")
}

/// The most extensions listed; the rest are summed into one `(other)` row.
const TOP_EXTENSIONS: usize = 50;

/// The census report: totals, a log2 size histogram, and the extensions by
/// bytes.
pub(crate) fn report(docs: &[HashedDoc]) -> String {
    // Bucket `i` holds sizes `2^(i-1)..=2^i - 1`; bucket 0 holds size 0.
    let mut histogram = [(0u64, 0u64); 65];
    let mut extensions: HashMap<&[u8], (u64, u64)> = HashMap::new();
    for doc in docs {
        let bucket = &mut histogram[(u64::BITS - doc.size.leading_zeros()) as usize];
        bucket.0 += 1;
        bucket.1 += doc.size;
        let entry = extensions.entry(&doc.extension).or_default();
        entry.0 += 1;
        entry.1 += doc.size;
    }

    let mut out = String::new();
    let total: u64 = docs.iter().map(|doc| doc.size).sum();
    let _ = writeln!(out, "documents: {}", docs.len());
    let _ = writeln!(out, "bytes: {total}");

    out.push_str("\n| size | documents | bytes |\n|---|---:|---:|\n");
    for (bucket, &(documents, bytes)) in histogram.iter().enumerate() {
        if documents == 0 {
            continue;
        }
        let (low, high) = match bucket {
            0 => (0, 0),
            _ => (1u64 << (bucket - 1), u64::MAX >> (64 - bucket)),
        };
        let _ = writeln!(out, "| {low}–{high} | {documents} | {bytes} |");
    }

    // By bytes, then by extension, so the order does not depend on the map.
    let mut extensions: Vec<_> = extensions.into_iter().collect();
    extensions.sort_unstable_by(|a, b| b.1.1.cmp(&a.1.1).then_with(|| a.0.cmp(b.0)));
    let rest = extensions.split_off(TOP_EXTENSIONS.min(extensions.len()));
    out.push_str("\n| extension | documents | bytes |\n|---|---:|---:|\n");
    for (extension, (documents, bytes)) in &extensions {
        let _ = writeln!(out, "| {} | {documents} | {bytes} |", display(extension));
    }
    if !rest.is_empty() {
        let (documents, bytes) = rest.iter().fold((0, 0), |(d, b), (_, (documents, bytes))| {
            (d + documents, b + bytes)
        });
        let _ = writeln!(out, "| (other) | {documents} | {bytes} |");
    }
    out
}

/// `ferret-bench census <catalog-dir>`.
pub(crate) fn run(dir: &Path) -> crate::Result<()> {
    let catalog = crate::open_catalog(dir)?;
    print!("{}", report(&hashed_documents(&catalog)?));
    Ok(())
}

#[cfg(test)]
mod tests {
    use ferret_catalog::Content;

    use super::*;
    use crate::support::Fixture;

    #[test]
    fn counts_hashed_documents_once_each() {
        let fixture = Fixture::new("census");
        let catalog = fixture.commit(
            &[
                ("a.txt", b"hello\n"),
                ("b.rs", b"fn f() -> u32 { 7 }\n"),
                // Two names, one content: one document, its bytes once.
                ("dup1.txt", b"duplicate content here\n"),
                ("dup2.TXT", b"duplicate content here\n"),
                // Not `Hashed`: outside the population.
                ("bin.dat", b"\x7fELF"),
                ("big.dat", b"over the cap"),
                (".bashrc", b"x"),
            ],
            &[
                ("bin.dat", Content::Binary),
                ("big.dat", Content::Unindexed),
            ],
        );
        let docs = hashed_documents(&catalog).unwrap();
        let extensions: Vec<_> = docs.iter().map(|doc| doc.extension.as_slice()).collect();
        assert_eq!(docs.len(), 4);
        assert!(docs.windows(2).all(|pair| pair[0].doc < pair[1].doc));

        let mut sorted = extensions.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, [&b""[..], b"rs", b"txt", b"txt"]);

        assert_eq!(
            report(&docs),
            "documents: 4\n\
             bytes: 50\n\
             \n\
             | size | documents | bytes |\n\
             |---|---:|---:|\n\
             | 1–1 | 1 | 1 |\n\
             | 4–7 | 1 | 6 |\n\
             | 16–31 | 2 | 43 |\n\
             \n\
             | extension | documents | bytes |\n\
             |---|---:|---:|\n\
             | txt | 2 | 29 |\n\
             | rs | 1 | 20 |\n\
             | (none) | 1 | 1 |\n"
        );
    }

    #[test]
    fn extensions_past_the_top_fall_into_other() {
        let docs: Vec<_> = (0..TOP_EXTENSIONS as u32 + 2)
            .map(|i| HashedDoc {
                doc: DocId(i),
                hash: [0; 16],
                size: 1000 - u64::from(i),
                extension: format!("e{i}").into_bytes(),
                name: NameId(i),
            })
            .collect();
        let report = report(&docs);
        assert!(report.contains("| e49 | 1 | 951 |\n| (other) | 2 | 1899 |\n"));
        assert!(!report.contains("| e50 "));
    }

    #[test]
    fn display_is_one_safe_cell() {
        assert_eq!(display(b""), "(none)");
        assert_eq!(display(b"a|b\n\xff"), "a\\|b\\n\u{fffd}");
    }
}
