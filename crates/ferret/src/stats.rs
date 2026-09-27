//! `ferret stats`: the catalog's counts and section sizes, and a census of
//! what it holds (ROADMAP S1): entries by type, depth and name length,
//! content states, file sizes, extensions, hard links and duplicate
//! content. Reads every section, so it costs what the file costs to read.

use std::collections::HashMap;
use std::fmt::Write as _;

use ferret_catalog::{Catalog, ContentState, InoId, Kind, NameId};

use crate::cli::{Context, Exit, error};
use crate::index::bytes;

/// `ferret stats`.
pub fn run(context: &Context) -> Exit {
    let catalog = match Catalog::open(&context.index) {
        Ok(Some(catalog)) => catalog,
        Ok(None) => {
            error(&format!(
                "no index in {}: run `ferret index DIR` first",
                context.index.display()
            ));
            return Exit::Error;
        }
        Err(e) => {
            error(&format!("{}: {e}", context.index.display()));
            return Exit::Error;
        }
    };
    if let Err(e) = catalog.load_all() {
        error(&format!("{}: {e}", context.index.display()));
        return Exit::Error;
    }
    let mut text = String::new();
    let _ = writeln!(text, "index {}", context.index.display());
    report(&catalog, &mut Census::of(&catalog), &mut text);
    crate::cli::print("stats", text.as_bytes())
}

/// Buckets `0`, `1`, `2–3`, `4–7`, …: bucket `i > 0` holds `2^(i-1)` up to
/// `2^i - 1`.
#[derive(Default)]
struct Histogram {
    counts: Vec<u64>,
    /// Summed values per bucket, for the size histogram's bytes.
    sums: Vec<u64>,
    values: Vec<u32>,
}

impl Histogram {
    fn add(&mut self, value: u64) {
        let bucket = (u64::BITS - value.leading_zeros()) as usize;
        if self.counts.len() <= bucket {
            self.counts.resize(bucket + 1, 0);
            self.sums.resize(bucket + 1, 0);
        }
        self.counts[bucket] += 1;
        self.sums[bucket] += value;
    }

    /// Also keeps `value` for percentiles; only for small values.
    fn add_kept(&mut self, value: u32) {
        self.add(value.into());
        self.values.push(value);
    }

    fn percentiles(&mut self) -> String {
        self.values.sort_unstable();
        let at = |q: f64| {
            let n = self.values.len();
            match n {
                0 => 0,
                _ => self.values[((n - 1) as f64 * q).round() as usize],
            }
        };
        format!(
            "median {}, p90 {}, p99 {}, max {}",
            at(0.5),
            at(0.9),
            at(0.99),
            at(1.0)
        )
    }

    fn range(bucket: usize) -> (u64, u64) {
        match bucket {
            0 => (0, 0),
            _ => (1 << (bucket - 1), (1 << bucket) - 1),
        }
    }
}

#[derive(Default)]
struct Census {
    files: u64,
    symlinks: u64,
    traversed: u64,
    /// Names by the kind they name: directory, file, symlink.
    names_by_kind: [u64; 3],
    depth: Histogram,
    name_len: Histogram,
    file_size: Histogram,
    /// Per content state: file inodes and their bytes.
    states: [(u64, u64); 4],
    /// Extension (lower-cased) to file names and bytes.
    extensions: HashMap<Vec<u8>, (u64, u64)>,
    /// File inodes with more than one name, and the names beyond the first.
    linked: (u64, u64),
    /// Documents held by more than one inode, the inodes beyond the first,
    /// and those inodes' bytes.
    duplicates: (u64, u64, u64),
}

impl Census {
    fn of(catalog: &Catalog) -> Census {
        let mut census = Census::default();
        let dirs = catalog.dir_count();
        // A directory's parent has a lower id, so one pass in id order sees
        // every parent before its children.
        let mut depth = vec![0u32; dirs as usize];
        for dir in 0..dirs {
            if let Some(name) = catalog.dir_name(InoId(dir)) {
                depth[dir as usize] = depth[catalog.name(name).parent.0 as usize] + 1;
            }
            census.traversed += u64::from(catalog.is_traversed(InoId(dir)));
        }

        let mut names_of = vec![0u32; catalog.inode_count() as usize];
        for id in (0..catalog.name_count()).map(NameId) {
            let name = catalog.name(id);
            let kind = catalog.kind(name.child);
            names_of[name.child.0 as usize] += 1;
            census.names_by_kind[kind as usize] += 1;
            census.depth.add_kept(depth[name.parent.0 as usize] + 1);
            census.name_len.add_kept(name.bytes.len() as u32);
            if kind == Kind::File {
                let size = catalog.inode(name.child).stat.size;
                let entry = census.extensions.entry(extension(name.bytes)).or_default();
                entry.0 += 1;
                entry.1 += size;
            }
        }

        let mut holders = vec![0u32; catalog.next_doc().0 as usize];
        for id in (dirs..catalog.inode_count()).map(InoId) {
            if catalog.kind(id) == Kind::Symlink {
                census.symlinks += 1;
                continue;
            }
            census.files += 1;
            let inode = catalog.inode(id);
            let size = inode.stat.size;
            census.file_size.add(size);
            let state = &mut census.states[inode.state as usize];
            state.0 += 1;
            state.1 += size;
            let names = names_of[id.0 as usize];
            if names > 1 {
                census.linked.0 += 1;
                census.linked.1 += u64::from(names - 1);
            }
            if let Some(doc) = inode.doc {
                let held = &mut holders[doc.0 as usize];
                *held += 1;
                if *held == 2 {
                    census.duplicates.0 += 1;
                }
                if *held >= 2 {
                    census.duplicates.1 += 1;
                    census.duplicates.2 += size;
                }
            }
        }
        census
    }
}

/// The lower-cased text after a name's last `.`; empty for none, and for a
/// name that is only an extension (`.bashrc`).
fn extension(name: &[u8]) -> Vec<u8> {
    match name.iter().rposition(|&b| b == b'.') {
        Some(at) if at > 0 => name[at + 1..].to_ascii_lowercase(),
        _ => Vec::new(),
    }
}

fn report(catalog: &Catalog, census: &mut Census, out: &mut String) {
    let names = u64::from(catalog.name_count());
    let total: u64 = catalog.section_sizes().map(|(_, len)| len).sum();
    let per_name = |n: u64| match names {
        0 => 0.0,
        _ => n as f64 / names as f64,
    };
    let [dir_names, file_names, link_names] = census.names_by_kind;
    let _ = writeln!(
        out,
        "\nentries   {names} names: {dir_names} directories, {file_names} files, \
         {link_names} symlinks"
    );
    let _ = writeln!(
        out,
        "inodes    {} ({} directories, {} traversed without cataloguing; {} files; {} symlinks)",
        catalog.inode_count(),
        catalog.dir_count(),
        census.traversed,
        census.files,
        census.symlinks
    );
    let live = catalog.doc_count();
    let next = catalog.next_doc().0;
    let _ = writeln!(
        out,
        "documents {live} live; {} dead (ids assigned and no longer held, dropped per D36 B); \
         next id {next}",
        next - live
    );
    let _ = writeln!(out, "sniffer   version {}", catalog.sniffer_version());

    let _ = writeln!(
        out,
        "\nsections  {} in all, {:.1} B per name (the 200 B header and table aside)",
        bytes(total),
        per_name(total)
    );
    for (section, len) in catalog.section_sizes() {
        let _ = writeln!(
            out,
            "  {:<10} {:>12} {:>9.1} B/name",
            format!("{section:?}"),
            bytes(len),
            per_name(len)
        );
    }

    let _ = writeln!(out, "\ncontent (file inodes)");
    for (label, state) in [
        ("hashed", ContentState::Hashed),
        ("binary", ContentState::Binary),
        ("unindexed", ContentState::Unindexed),
        ("fault", ContentState::Fault),
    ] {
        let (n, b) = census.states[state as usize];
        let _ = writeln!(out, "  {label:<10} {n:>10} {:>12}", bytes(b));
    }
    let _ = match census.linked {
        (0, _) => writeln!(out, "  hard links: no file has a second name in the index"),
        (files, extra) => writeln!(
            out,
            "  hard links: {files} files have {extra} names beyond their first"
        ),
    };
    let (docs, extra, extra_bytes) = census.duplicates;
    let _ = writeln!(
        out,
        "  duplicates: {docs} documents are held by {extra} more inodes, {}",
        bytes(extra_bytes)
    );

    let _ = writeln!(
        out,
        "\ndepth below the root: {}",
        census.depth.percentiles()
    );
    histogram(out, &census.depth, false);
    let _ = writeln!(
        out,
        "\nname length in bytes: {}",
        census.name_len.percentiles()
    );
    histogram(out, &census.name_len, false);
    let _ = writeln!(out, "\nfile size");
    histogram(out, &census.file_size, true);

    let mut extensions: Vec<_> = census.extensions.iter().collect();
    extensions.sort_unstable_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));
    let _ = writeln!(
        out,
        "\nextensions, by file names ({} distinct)",
        extensions.len()
    );
    for (ext, (n, b)) in extensions.iter().take(15) {
        let ext = match ext.is_empty() {
            true => "(none)".to_owned(),
            false => format!(".{}", ext.escape_ascii()),
        };
        let _ = writeln!(out, "  {ext:<12} {n:>10} {:>12}", bytes(*b));
    }
}

fn histogram(out: &mut String, histogram: &Histogram, sizes: bool) {
    for (bucket, &n) in histogram.counts.iter().enumerate() {
        if n == 0 {
            continue;
        }
        let (low, high) = Histogram::range(bucket);
        let label = match (sizes, low == high) {
            (true, true) => bytes(low),
            (true, false) => format!("{}–{}", bytes(low), bytes(high)),
            (false, true) => low.to_string(),
            (false, false) => format!("{low}–{high}"),
        };
        let _ = write!(out, "  {label:<22} {n:>10}");
        if sizes {
            let _ = write!(out, " {:>12}", bytes(histogram.sums[bucket]));
        }
        out.push('\n');
    }
}
