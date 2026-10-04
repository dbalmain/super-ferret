//! `ferret stats`: the catalog's counts and section sizes, and a census of
//! what it holds (ROADMAP S1): entries by type, depth and name length,
//! content states, file sizes, extensions, hard links and duplicate
//! content. Reads every section, so it costs what the file costs to read.

use std::collections::HashMap;
use std::fmt::Write as _;

use ferret_catalog::{Catalog, ContentState, Kind, NameId, Target};

use crate::cli::{Context, Exit, error};
use crate::index::bytes;

/// `ferret stats`.
pub fn run(context: &Context) -> Exit {
    let published = match ferret_catalog::log::Published::open(&context.index) {
        Ok(Some(published)) => published,
        Ok(None) => {
            error(&format!(
                "no index in {}: run `ferret index DIR` first",
                context.index.display()
            ));
            return Exit::Error;
        }
        Err(e) => {
            error(&crate::index::open_failed(context, &e));
            return Exit::Error;
        }
    };
    let usage = match published.budget_usage() {
        Ok(usage) => usage,
        Err(e) => {
            error(&e.to_string());
            return Exit::Error;
        }
    };
    let catalog = published.into_catalog();
    if let Err(e) = catalog.load_all() {
        error(&e.to_string());
        return Exit::Error;
    }
    let mut text = String::new();
    let _ = writeln!(text, "index {}", context.index.display());
    let _ = writeln!(
        text,
        "log {} bytes / 67108864, {} records / 500000, {} transactions",
        usage.log_bytes, usage.records, usage.transactions
    );
    let _ = writeln!(
        text,
        "dirty inodes {}/{}, names {}/{} (1%); dead base inodes {}, names {} (5%)",
        usage.dirty_inodes,
        usage.base_inodes,
        usage.dirty_names,
        usage.base_names,
        usage.dead_inodes,
        usage.dead_names
    );
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
    ignored: u64,
    specials: u64,
    /// All names by kind, including stat-free ignored rows.
    names_by_kind: [u64; 7],
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
    /// File inodes naming a document the docs section does not hold: a
    /// corrupt reference, since the decoder does not check an inode's DocId.
    dangling: u64,
}

impl Census {
    fn of(catalog: &Catalog) -> Census {
        let mut census = Census::default();
        let mut depth = HashMap::new();
        let mut pending: Vec<_> = catalog.roots().map(|(id, _)| (id, 0u32)).collect();
        while let Some((dir, d)) = pending.pop() {
            depth.insert(dir, d);
            census.traversed += u64::from(catalog.is_traversed(dir));
            pending.extend(catalog.children(dir).filter_map(
                |id| match catalog.name(id).target() {
                    Target::Inode(child) if catalog.is_directory(child) => Some((child, d + 1)),
                    _ => None,
                },
            ));
        }

        let mut names_of = vec![0u32; catalog.next_inode().0 as usize];
        for (_, name) in catalog.name_reader().runs_from(NameId(0)) {
            let kind = match name.target() {
                Target::Inode(inode) => {
                    names_of[inode.0 as usize] += 1;
                    catalog.kind(inode)
                }
                Target::Ignored(kind) => {
                    census.ignored += 1;
                    kind
                }
            };
            census.names_by_kind[kind as usize] += 1;
            census.depth.add_kept(depth[&name.parent] + 1);
            census.name_len.add_kept(name.bytes.len() as u32);
            if kind == Kind::File && matches!(name.target(), Target::Inode(_)) {
                let size = catalog.size(name.child);
                let entry = census.extensions.entry(extension(name.bytes)).or_default();
                entry.0 += 1;
                entry.1 += size;
            }
        }

        // (document, bytes) per holding inode, in inode order. Counted by
        // sorting, not by an array indexed by DocId: ids are sparse after
        // churn, and nothing is sized by `next_doc` (D36 B).
        let mut held = Vec::new();
        for id in catalog.inode_ids().filter(|&id| !catalog.is_directory(id)) {
            match catalog.kind(id) {
                Kind::Symlink => {
                    census.symlinks += 1;
                    continue;
                }
                Kind::File => {}
                _ => {
                    census.specials += 1;
                    continue;
                }
            }
            census.files += 1;
            let size = catalog.size(id);
            census.file_size.add(size);
            let state = &mut census.states[catalog.state(id) as usize];
            state.0 += 1;
            state.1 += size;
            let names = names_of[id.0 as usize];
            if names > 1 {
                census.linked.0 += 1;
                census.linked.1 += u64::from(names - 1);
            }
            match catalog.doc(id) {
                Some(doc) if catalog.doc_hash(doc).is_some() => held.push((doc.0, size)),
                Some(_) => census.dangling += 1,
                None => {}
            }
        }
        // Stable, so each document's first holder stays first.
        held.sort_by_key(|&(doc, _)| doc);
        for group in held.chunk_by(|a, b| a.0 == b.0).filter(|g| g.len() > 1) {
            census.duplicates.0 += 1;
            census.duplicates.1 += group.len() as u64 - 1;
            census.duplicates.2 += group[1..].iter().map(|&(_, size)| size).sum::<u64>();
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
    let [
        dir_names,
        file_names,
        link_names,
        pipes,
        sockets,
        blocks,
        characters,
    ] = census.names_by_kind;
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
    if census.ignored > 0 {
        let _ = writeln!(out, "ignored   {} names without inode rows", census.ignored);
    }
    if pipes + sockets + blocks + characters > 0 {
        let _ = writeln!(
            out,
            "specials  {pipes} FIFO names, {sockets} socket names, {blocks} block-device names, {characters} character-device names; {} visible inodes",
            census.specials
        );
    }
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
        "\nsections  {} in all, {:.1} B per name (the {} B head of the file aside)",
        bytes(total),
        per_name(total),
        catalog.head_len()
    );
    for (section, len) in catalog.section_sizes() {
        let _ = write!(
            out,
            "  {:<10} {:>12} {:>9.1} B/name",
            format!("{section:?}"),
            bytes(len),
            per_name(len)
        );
        // A packed column's width, and its dictionary's size if it has one.
        let columns = catalog.column_widths().filter(|&(s, ..)| s == section);
        for (i, (_, width, dict)) in columns.enumerate() {
            let _ = write!(out, "{}{width}", if i == 0 { "  bits " } else { "+" });
            if dict > 0 {
                let _ = write!(out, " ({dict} values)");
            }
        }
        out.push('\n');
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
    if census.dangling > 0 {
        let _ = writeln!(
            out,
            "  corrupt: {} files name a document the index does not hold",
            census.dangling
        );
    }

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
