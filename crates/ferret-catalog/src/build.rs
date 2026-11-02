//! Merges batches into one generation and streams it out (D29, D30, D31, D36,
//! D40).
//!
//! Numbering is deterministic whatever order the workers ran in:
//!
//! 1. Roots take the first directory ids, sorted by path.
//! 2. Directories are numbered breadth first. Taking directories in id order,
//!    each one's children are sorted by name, emitted as name rows, and its
//!    child directories take the next ids. So name rows come out sorted by
//!    (parent, name), and a directory's parent always has a lower id, which is
//!    what makes a walk upwards terminate (see `format`).
//! 3. Files and symlinks follow the directories, numbered by their first name
//!    in name order, one row per `(dev, ino)` (D31 C). A directory is one row
//!    per name.
//! 4. New `DocId`s are assigned in inode order, which is breadth-first path
//!    order: the locality D4 wanted from a first crawl.
//!
//! A `(dev, ino)` seen fresh supersedes a carried observation of it (D34).
//! Two fresh observations that disagree make the inode a content fault: one
//! observation's stat is kept and it gets no document (D26).
//!
//! # Memory (D40)
//!
//! Built for 10M entries. [`plan`] decides every id with index arrays of a few
//! `u32`s per entry and never copies an entry: edges are entry numbers grouped
//! by parent with a counting pass, files are deduplicated by sorting
//! `(fingerprint, name position, file)` triples rather than through a map, and
//! documents by sorting `(hash, inode)` pairs; each batch's entry counts are
//! freed as soon as the plan has them. [`write`] then writes every section
//! to its place in the file through a bounded buffer per column, filling all
//! of a table's columns in one pass over its rows, so the encoded generation
//! is never held in memory; it frees each batch's names once the name
//! sections are out.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::batch::{Batch, DirToken, Stat};
use crate::format::{
    self, COLUMNS, Coding, Column, Descriptor, HASH_ROW, Head, NONE, PAIR_ROW, Range, SECTIONS,
    Section, WORK_TREE_ROW,
};
use crate::packed::BlockSizer;
use crate::{ContentState, Hash};

mod files;
mod write;

use files::{assign_docs, number_files};
pub(crate) use write::{StatColumns, write};

/// Why a set of batches could not be built into a generation. Nothing is
/// published.
#[derive(Debug, PartialEq, Eq)]
pub enum BuildError {
    /// A parent or work-tree token that no added batch minted.
    UnknownToken(DirToken),
    /// Two roots with one path, fresh or kept.
    DuplicateRoot(Vec<u8>),
    /// Two entries with one name in one directory.
    DuplicateName(Vec<u8>),
    /// A name that is empty, `.`, `..`, or contains `/` or NUL.
    BadName(Vec<u8>),
    /// A root path that is empty or contains NUL, or a link target or
    /// work-tree path that contains NUL.
    BadPath(Vec<u8>),
    /// Directories whose parent tokens never lead to a root.
    Unreachable,
    /// Two work-tree records for one directory.
    DuplicateWorkTree,
    /// Retained coverage cannot refer to a future or reserved sequence.
    FutureRetention { retained_at: u64, sequence: u64 },
    /// More rows, documents or heap bytes than 32-bit ids can address.
    TooLarge,
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let show = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        match self {
            Self::UnknownToken(t) => write!(f, "directory token {t:?} was never minted"),
            Self::DuplicateRoot(p) => write!(f, "root {} given twice", show(p)),
            Self::DuplicateName(n) => write!(f, "name {} given twice in one directory", show(n)),
            Self::BadName(n) => write!(f, "invalid name {:?}", show(n)),
            Self::BadPath(p) => write!(f, "invalid path {:?}", show(p)),
            Self::Unreachable => write!(f, "directories not connected to any root"),
            Self::DuplicateWorkTree => write!(f, "two work-tree records for one directory"),
            Self::FutureRetention {
                retained_at,
                sequence,
            } => write!(
                f,
                "retained sequence {retained_at} exceeds checkpoint sequence {sequence}"
            ),
            Self::TooLarge => write!(f, "catalog too large for 32-bit ids"),
        }
    }
}

impl std::error::Error for BuildError {}

/// What the previous generation lends a build.
pub(crate) struct Known<'a> {
    /// Each live document's `(hash, DocId)`, sorted by hash, so known content
    /// keeps its `DocId`.
    pub(crate) docs: &'a [(Hash, u32)],
    /// The persisted next-id counter; never goes back (D36 B).
    pub(crate) next_doc: u32,
}

/// Global entry numbers: directories `0..dirs` in batch order, then files
/// `dirs..dirs + files`.
struct Index {
    dir_base: Vec<usize>,
    file_base: Vec<usize>,
    dirs: usize,
    files: usize,
    ignored_base: Vec<usize>,
    ignored: usize,
}

impl Index {
    fn new(batches: &[Batch]) -> Self {
        let (mut dir_base, mut file_base) = (Vec::new(), Vec::new());
        let (mut dirs, mut files) = (0, 0);
        let (mut ignored_base, mut ignored) = (Vec::new(), 0);
        for batch in batches {
            dir_base.push(dirs);
            file_base.push(files);
            ignored_base.push(ignored);
            ignored += batch.ignored.len();
            dirs += batch.dirs.len();
            files += batch.files.len();
        }
        Self {
            dir_base,
            file_base,
            dirs,
            files,
            ignored_base,
            ignored,
        }
    }

    /// (batch position, index in its dirs) of directory `g`.
    fn dir(&self, g: usize) -> (usize, usize) {
        let b = self.dir_base.partition_point(|&start| start <= g) - 1;
        (b, g - self.dir_base[b])
    }

    fn ignored(&self, i: usize) -> (usize, usize) {
        let b = self.ignored_base.partition_point(|&start| start <= i) - 1;
        (b, i - self.ignored_base[b])
    }

    /// (batch position, index in its files) of file `f`, counted from 0.
    fn file(&self, f: usize) -> (usize, usize) {
        let b = self.file_base.partition_point(|&start| start <= f) - 1;
        (b, f - self.file_base[b])
    }
}

/// Every id of the new generation, decided; [`write`] needs only this and the
/// batches.
pub(crate) struct Plan {
    sniffer: u32,
    next_doc: u32,
    index: Index,
    /// Entry numbers grouped by parent directory, each group sorted by name:
    /// directory `g`'s children are `edges[edge_start[g]..edge_start[g + 1]]`.
    edges: Vec<u32>,
    edge_start: Vec<u32>,
    /// Directory by id: breadth-first order.
    order: Vec<u32>,
    /// Id by directory.
    dir_id: Vec<u32>,
    /// Each directory's own NameId; NONE for a root.
    name_of_dir: Vec<u32>,
    /// Raw entry count by directory (global number); NONE if unknown.
    entry_count: Vec<u32>,
    /// InoId by file.
    inode_of_file: Vec<u32>,
    /// The file whose observation each file inode row takes, in inode order.
    winner: Vec<u32>,
    /// Whether fresh observations of each file inode disagreed; freed once
    /// `state` holds it.
    fault: Vec<bool>,
    /// Each file inode's content state, in inode order: taken as the
    /// documents are assigned, so the batches' contents can go before the
    /// hashes are sorted.
    state: Vec<ContentState>,
    /// Each file inode's DocId, or NONE.
    doc: Vec<u32>,
    heap_len: usize,
    /// The name offsets' blocks: bytes of packed values, widest block.
    offset_blocks: (u64, u32),
    /// (root InoId, offset in strings), sorted by path.
    roots: Vec<(u32, u32)>,
    strings: Vec<u8>,
    /// (symlink InoId, offset in strings), in inode order.
    links: Vec<(u32, u32)>,
    specials: Vec<(u32, u32)>,
    /// (dir InoId, offset in strings, common id, kind), sorted by dir.
    work_trees: Vec<(u32, u32, (u64, u64), u8)>,
    /// The NameIds a repository ignore file's `!` line reveals (D127),
    /// rising.
    revealed: Vec<u32>,
    /// (hash as big-endian halves, DocId), sorted by id.
    docs: Vec<((u64, u64), u32)>,
}

impl Plan {
    fn name<'a>(&self, batches: &'a [Batch], entry: u32) -> &'a [u8] {
        let entry = entry as usize;
        if entry < self.index.dirs {
            let (b, d) = self.index.dir(entry);
            batches[b].dirs[d].name.of(&batches[b].names)
        } else if entry < self.index.dirs + self.index.files {
            let (b, f) = self.index.file(entry - self.index.dirs);
            batches[b].files[f].name.of(&batches[b].names)
        } else {
            let (b, i) = self
                .index
                .ignored(entry - self.index.dirs - self.index.files);
            batches[b].ignored[i].name.of(&batches[b].names)
        }
    }

    /// Whether a repository ignore file's `!` line reveals the entry (D127).
    /// An ignored marker is never revealed.
    fn revealed(&self, batches: &[Batch], entry: u32) -> bool {
        let entry = entry as usize;
        if entry < self.index.dirs {
            let (b, d) = self.index.dir(entry);
            batches[b].dirs[d].revealed
        } else if entry < self.index.dirs + self.index.files {
            let (b, f) = self.index.file(entry - self.index.dirs);
            batches[b].revealed_files.binary_search(&(f as u32)).is_ok()
        } else {
            false
        }
    }

    /// The inode an entry names.
    fn child(&self, batches: &[Batch], entry: u32) -> u32 {
        let e = entry as usize;
        if e < self.index.dirs {
            if self.dir_id[e] == NONE {
                crate::Kind::Dir.ignored_child()
            } else {
                self.dir_id[e]
            }
        } else if e < self.index.dirs + self.index.files {
            self.inode_of_file[e - self.index.dirs]
        } else {
            let (b, i) = self.index.ignored(e - self.index.dirs - self.index.files);
            batches[b].ignored[i].kind.ignored_child()
        }
    }

    fn children(&self, dir: u32) -> &[u32] {
        let d = dir as usize;
        &self.edges[self.edge_start[d] as usize..self.edge_start[d + 1] as usize]
    }

    /// Each inode row's stat, `DocId` (or NONE) and content state, in inode
    /// order: directories, then non-directory entries.
    fn inode_rows<'a>(
        &'a self,
        batches: &'a [Batch],
    ) -> impl Iterator<Item = (&'a Stat, u32, ContentState)> {
        let dirs = self.order.iter().map(|&dir| {
            let (b, d) = self.index.dir(dir as usize);
            (&batches[b].dir_stats[d], NONE, ContentState::Unindexed)
        });
        let files = self.winner.iter().enumerate().map(|(k, &file)| {
            let (b, f) = self.index.file(file as usize);
            (&batches[b].file_stats[f], self.doc[k], self.state[k])
        });
        dirs.chain(files)
    }

    /// The head of the file, and each dictionary column's sorted values.
    /// One pass over the inode rows finds every stat column's range, blocks
    /// and distinct values; the id columns are sized from the counts.
    fn head(&self, batches: &[Batch]) -> (Head, [Vec<u64>; COLUMNS.len()]) {
        let dirs = self.order.len();
        let (inodes, names) = (
            dirs + self.winner.len(),
            self.order
                .iter()
                .map(|&d| self.children(d).len())
                .sum::<usize>(),
        );
        let mut sizing = Sizing::default();
        self.size_inodes(batches, &mut sizing);
        self.size_dirs(batches, &mut sizing);
        self.size_names(batches, &mut sizing, names);
        for (row, &(_, id)) in self.docs.iter().enumerate() {
            sizing.ranges[Column::DocId as usize].add(u64::from(id) - row as u64);
        }
        let (columns, dicts) = sizing.descriptors(self.offset_blocks);
        let lens = self.section_lens(dirs, inodes);
        let head = Head {
            generation: crate::Generation {
                incarnation: [1; 16],
                checkpoint: 0,
                sequence: 0,
            },
            sniffer: self.sniffer,
            next_doc: self.next_doc,
            dirs: dirs as u32,
            inodes: inodes as u32,
            names: names as u32,
            docs: self.docs.len() as u32,
            columns,
            lens,
        };
        (head, dicts)
    }

    /// Sizes the stat and doc columns in one pass over the inode rows.
    fn size_inodes(&self, batches: &[Batch], sizing: &mut Sizing) {
        let Sizing {
            ranges,
            sets,
            blocks,
        } = sizing;
        // Neighbouring rows nearly always share a dictionary value; skipping
        // the repeat saves a hash per row.
        let mut last = [None; COLUMNS.len()];
        for (stat, doc, _) in self.inode_rows(batches) {
            for column in STAT_COLUMNS {
                let (c, value) = (column as usize, stat_field(column, stat));
                match column.coding() {
                    Coding::Dictionary if last[c] != Some(value) => {
                        sets[c].insert(value);
                        last[c] = Some(value);
                    }
                    Coding::Dictionary => {}
                    Coding::Blocked => blocks[c].push(value),
                    Coding::Nullable | Coding::NullableBlocked | Coding::Sequence => {
                        ranges[c].add(value)
                    }
                }
            }
            push_known(&mut blocks[Column::Doc as usize], doc);
        }
    }

    /// Sizes the directory columns, in the order they are written: by InoId.
    fn size_dirs(&self, batches: &[Batch], sizing: &mut Sizing) {
        let blocks = &mut sizing.blocks;
        for &dir in &self.order {
            let dir = dir as usize;
            push_known(&mut blocks[Column::Entries as usize], self.entry_count[dir]);
            let (b, d) = self.index.dir(dir);
            match batches[b].dirs[d].retained_at {
                Some(n) => blocks[Column::RetainedAt as usize].push(n),
                None => blocks[Column::RetainedAt as usize].push_null(),
            }
        }
    }

    /// Sizes the name columns. Ids are sized by what they index: the counts
    /// and the heap.
    fn size_names(&self, batches: &[Batch], sizing: &mut Sizing, names: usize) {
        sizing.ranges[Column::DirName as usize] = Range((names > 0).then(|| (0, names as u64 - 1)));
        let [parent, child] = sizing
            .blocks
            .get_disjoint_mut([Column::NameParent as usize, Column::NameChild as usize])
            .unwrap_or_else(|_| unreachable!("two columns"));
        for (id, &dir) in self.order.iter().enumerate() {
            for &entry in self.children(dir) {
                parent.push(id as u64);
                child.push(u64::from(self.child(batches, entry)));
            }
        }
    }

    /// Each section's length in bytes, for `dirs` directories and `inodes`
    /// inodes.
    fn section_lens(&self, dirs: usize, inodes: usize) -> [u64; SECTIONS.len()] {
        let mut lens = [0; SECTIONS.len()];
        for (section, len) in [
            (Section::NameHeap, self.heap_len),
            (Section::Traversed, dirs.div_ceil(8)),
            (Section::Roots, self.roots.len() * PAIR_ROW),
            (Section::Strings, self.strings.len()),
            (Section::States, inodes.div_ceil(4)),
            (Section::Links, self.links.len() * PAIR_ROW),
            (Section::Specials, self.specials.len() * PAIR_ROW),
            (Section::WorkTrees, self.work_trees.len() * WORK_TREE_ROW),
            (Section::Docs, self.docs.len() * HASH_ROW),
            (Section::DocRefs, self.docs.len() * 4),
            (Section::Policy, 16),
            (Section::Revealed, self.revealed.len() * 4),
        ] {
            lens[section as usize] = len as u64;
        }
        lens
    }
}

/// What [`Plan::head`] gathers to size each column, by its coding: a range,
/// the distinct values, or the blocks.
#[derive(Default)]
struct Sizing {
    ranges: [Range; COLUMNS.len()],
    sets: [HashSet<u64>; COLUMNS.len()],
    blocks: [BlockSizer; COLUMNS.len()],
}

impl Sizing {
    /// Each column's descriptor, and each dictionary column's sorted values.
    /// The name offsets' blocks were sized as the heap was laid out.
    fn descriptors(
        mut self,
        offset_blocks: (u64, u32),
    ) -> ([Descriptor; COLUMNS.len()], [Vec<u64>; COLUMNS.len()]) {
        let mut dicts: [Vec<u64>; COLUMNS.len()] = Default::default();
        let columns = COLUMNS.map(|column| {
            let c = column as usize;
            match column.coding() {
                Coding::Sequence => Descriptor::frame(self.ranges[c]),
                Coding::Nullable => Descriptor::nullable(self.ranges[c]),
                Coding::Blocked if column == Column::NameOffset => {
                    Descriptor::blocked(offset_blocks)
                }
                Coding::Blocked | Coding::NullableBlocked => {
                    Descriptor::blocked(std::mem::take(&mut self.blocks[c]).finish())
                }
                Coding::Dictionary => {
                    let mut dict: Vec<u64> =
                        std::mem::take(&mut self.sets[c]).into_iter().collect();
                    dict.sort_unstable();
                    let len = dict.len() as u32;
                    dicts[c] = dict;
                    Descriptor::dictionary(len)
                }
            }
        });
        (columns, dicts)
    }
}

/// The inode columns read straight from a [`Stat`]: all but the `DocId`.
pub(crate) const STAT_COLUMNS: [Column; 10] = [
    Column::Dev,
    Column::Ino,
    Column::Size,
    Column::Mtime,
    Column::MtimeNs,
    Column::Ctime,
    Column::CtimeNs,
    Column::Mode,
    Column::Owner,
    Column::Nlink,
];

/// One of [`STAT_COLUMNS`]' values, as the column stores it; the reader's
/// accessors invert it.
pub(crate) fn stat_field(column: Column, stat: &Stat) -> u64 {
    match column {
        Column::Dev => stat.dev,
        Column::Ino => stat.ino,
        Column::Size => stat.size,
        Column::Mtime => format::order(stat.mtime_sec),
        Column::MtimeNs => u64::from(stat.mtime_nsec),
        Column::Ctime => format::order(stat.ctime_sec),
        Column::CtimeNs => u64::from(stat.ctime_nsec),
        Column::Mode => u64::from(stat.mode),
        Column::Owner => u64::from(stat.uid) << 32 | u64::from(stat.gid),
        Column::Nlink => stat.nlink,
        _ => unreachable!("{column:?} is not a stat column"),
    }
}

/// Decides every id of the new generation, or finds why it cannot be built.
/// Writes nothing, so every [`BuildError`] comes before the first byte. Takes
/// each batch's entry counts, which nothing needs once they are in the plan.
pub(crate) fn plan(
    batches: &mut [Batch],
    sniffer: u32,
    known: Known<'_>,
) -> Result<Plan, BuildError> {
    if batches.iter().any(|b| b.overflow) {
        return Err(BuildError::TooLarge);
    }
    let index = Index::new(batches);
    let (dirs, files) = (index.dirs, index.files);
    if dirs + files + index.ignored >= (NONE - 16) as usize {
        return Err(BuildError::TooLarge);
    }
    let tokens = Tokens::new(batches, &index);
    let (mut roots, parent_of) = parents(batches, &index, &tokens)?;
    let (edge_start, edges) = group_by_parent(&parent_of, dirs);
    let visible = visible_dirs(batches, &index, &roots, &parent_of, &edge_start, &edges)?;
    drop(parent_of);

    roots.sort_by(|a, b| a.0.cmp(b.0));
    if let Some(pair) = roots.windows(2).find(|w| w[0].0 == w[1].0) {
        return Err(BuildError::DuplicateRoot(pair[0].0.to_vec()));
    }
    let mut plan = Plan {
        sniffer,
        next_doc: known.next_doc,
        index,
        edges,
        edge_start,
        order: Vec::with_capacity(dirs),
        dir_id: vec![NONE; dirs],
        name_of_dir: vec![NONE; dirs],
        entry_count: vec![NONE; dirs],
        inode_of_file: Vec::new(),
        winner: Vec::new(),
        fault: Vec::new(),
        state: Vec::new(),
        doc: Vec::new(),
        heap_len: 0,
        offset_blocks: (0, 0),
        roots: Vec::with_capacity(roots.len()),
        strings: Vec::new(),
        links: Vec::new(),
        specials: Vec::new(),
        work_trees: Vec::new(),
        docs: Vec::new(),
        revealed: Vec::new(),
    };
    number_roots(&mut plan, roots)?;

    let file_pos = number_names(&mut plan, batches, &visible)?;

    take_entry_counts(&mut plan, batches, &tokens)?;

    number_files(&mut plan, batches, file_pos)?;
    assign_docs(&mut plan, batches, known.docs)?;
    push_links(&mut plan, batches)?;
    push_work_trees(&mut plan, batches, &tokens)?;
    Ok(plan)
}

/// Numbers the sorted `roots` first, in path order, storing their paths.
fn number_roots(plan: &mut Plan, roots: Roots<'_>) -> Result<(), BuildError> {
    for &(path, global) in &roots {
        if path.is_empty() || path.contains(&0) {
            return Err(BuildError::BadPath(path.to_vec()));
        }
        let id = plan.order.len() as u32;
        plan.dir_id[global] = id;
        plan.roots.push((id, push_string(&mut plan.strings, path)?));
        plan.order.push(global as u32);
    }
    Ok(())
}

/// Moves each batch's directory entry counts into the plan.
fn take_entry_counts(
    plan: &mut Plan,
    batches: &mut [Batch],
    tokens: &Tokens,
) -> Result<(), BuildError> {
    for batch in batches.iter_mut() {
        for (dir, count) in std::mem::take(&mut batch.entry_counts) {
            plan.entry_count[tokens.resolve(dir)?] = count;
        }
    }
    Ok(())
}

/// Each batch's first directory number and its directory count, by batch
/// id: resolves a [`DirToken`] to its global directory number.
struct Tokens(HashMap<u32, (usize, usize)>);

impl Tokens {
    fn new(batches: &[Batch], index: &Index) -> Self {
        Self(
            batches
                .iter()
                .zip(&index.dir_base)
                .map(|(b, &base)| (b.id, (base, b.dirs.len())))
                .collect(),
        )
    }

    fn resolve(&self, token: DirToken) -> Result<usize, BuildError> {
        match self.0.get(&token.batch) {
            Some(&(base, len)) if (token.index as usize) < len => Ok(base + token.index as usize),
            _ => Err(BuildError::UnknownToken(token)),
        }
    }
}

/// Each root's `(path, directory)`.
type Roots<'a> = Vec<(&'a [u8], usize)>;

/// The roots, and each entry's parent directory, NONE for a root.
fn parents<'a>(
    batches: &'a [Batch],
    index: &Index,
    tokens: &Tokens,
) -> Result<(Roots<'a>, Vec<u32>), BuildError> {
    let mut roots = Vec::new();
    let mut parent_of = vec![NONE; index.dirs + index.files + index.ignored];
    let (dirs, files) = (index.dirs, index.files);
    for (i, batch) in batches.iter().enumerate() {
        for (d, dir) in batch.dirs.iter().enumerate() {
            let g = index.dir_base[i] + d;
            match dir.parent {
                None => roots.push((dir.name.of(&batch.names), g)),
                Some(parent) => parent_of[g] = tokens.resolve(parent)? as u32,
            }
        }
        for (f, file) in batch.files.iter().enumerate() {
            parent_of[dirs + index.file_base[i] + f] = tokens.resolve(file.parent)? as u32;
        }
        for (j, ignored) in batch.ignored.iter().enumerate() {
            parent_of[dirs + files + index.ignored_base[i] + j] =
                tokens.resolve(ignored.parent)? as u32;
        }
    }
    Ok((roots, parent_of))
}

/// The entries grouped by parent, as `(edge_start, edges)`: count, prefix
/// sum, scatter. Only each directory's own children are ever sorted.
fn group_by_parent(parent_of: &[u32], dirs: usize) -> (Vec<u32>, Vec<u32>) {
    let mut edge_start = vec![0u32; dirs + 1];
    for &parent in parent_of {
        if parent != NONE {
            edge_start[parent as usize + 1] += 1;
        }
    }
    for d in 0..dirs {
        edge_start[d + 1] += edge_start[d];
    }
    let mut edges = vec![0u32; edge_start[dirs] as usize];
    let mut cursor = edge_start.clone();
    for (entry, &parent) in parent_of.iter().enumerate() {
        if parent != NONE {
            edges[cursor[parent as usize] as usize] = entry as u32;
            cursor[parent as usize] += 1;
        }
    }
    (edge_start, edges)
}

/// Whether each directory stays visible: it was not traversed, or it holds
/// a file, or a visible directory. Fails unless every directory is
/// reachable from a root.
fn visible_dirs(
    batches: &[Batch],
    index: &Index,
    roots: &[(&[u8], usize)],
    parent_of: &[u32],
    edge_start: &[u32],
    edges: &[u32],
) -> Result<Vec<bool>, BuildError> {
    let (dirs, files) = (index.dirs, index.files);
    let mut reachable: Vec<u32> = roots.iter().map(|&(_, g)| g as u32).collect();
    let mut next = 0;
    while next < reachable.len() {
        let d = reachable[next] as usize;
        reachable.extend(
            edges[edge_start[d] as usize..edge_start[d + 1] as usize]
                .iter()
                .copied()
                .filter(|&e| (e as usize) < dirs),
        );
        next += 1;
    }
    if reachable.len() != dirs {
        return Err(BuildError::Unreachable);
    }
    let mut visible = vec![false; dirs];
    for &dir in reachable.iter().rev() {
        let d = dir as usize;
        let (b, i) = index.dir(d);
        visible[d] |= !batches[b].dirs[i].traversed
            || edges[edge_start[d] as usize..edge_start[d + 1] as usize]
                .iter()
                .any(|&e| (dirs..dirs + files).contains(&(e as usize)));
        if visible[d] && parent_of[d] != NONE {
            visible[parent_of[d] as usize] = true;
        }
    }
    Ok(visible)
}

/// Breadth first: sorts each directory's children, checks them, numbers the
/// child directories, and returns each file's first name position.
fn number_names(
    plan: &mut Plan,
    batches: &[Batch],
    visible: &[bool],
) -> Result<Vec<u32>, BuildError> {
    let (dirs, files) = (plan.index.dirs, plan.index.files);
    let mut file_pos = vec![NONE; files];
    let mut name_id = 0usize;
    let mut offsets = BlockSizer::default();
    let mut next = 0;
    while next < plan.order.len() {
        let global = plan.order[next] as usize;
        let range = plan.edge_start[global] as usize..plan.edge_start[global + 1] as usize;
        let mut edges = std::mem::take(&mut plan.edges);
        edges[range.clone()]
            .sort_unstable_by(|&a, &b| plan.name(batches, a).cmp(plan.name(batches, b)));
        plan.edges = edges;
        for k in range.clone() {
            let entry = plan.edges[k];
            let name = plan.name(batches, entry);
            if name.is_empty()
                || name == b"."
                || name == b".."
                || name.iter().any(|&b| b == 0 || b == b'/')
            {
                return Err(BuildError::BadName(name.to_vec()));
            }
            if k > range.start && plan.name(batches, plan.edges[k - 1]) == name {
                return Err(BuildError::DuplicateName(name.to_vec()));
            }
            // The prospective end, not the current length: one long name must
            // not carry the heap past what `write`'s `u32` offsets can hold.
            if name_id >= NONE as usize {
                return Err(BuildError::TooLarge);
            }
            offsets.push(plan.heap_len as u64);
            plan.heap_len = plan
                .heap_len
                .checked_add(name.len() + 1)
                .filter(|&end| end <= heap_limit())
                .ok_or(BuildError::TooLarge)?;
            // A directory without a visible descendant becomes an ignored
            // marker, which is never revealed.
            if plan.revealed(batches, entry) && (entry as usize >= dirs || visible[entry as usize])
            {
                plan.revealed.push(name_id as u32);
            }
            let entry = entry as usize;
            if entry < dirs {
                if visible[entry] {
                    plan.dir_id[entry] = plan.order.len() as u32;
                    plan.order.push(entry as u32);
                    plan.name_of_dir[entry] = name_id as u32;
                }
            } else if entry < dirs + files {
                file_pos[entry - dirs] = name_id as u32;
            }
            name_id += 1;
        }
        next += 1;
    }
    plan.offset_blocks = offsets.finish();
    Ok(file_pos)
}

/// Special files and link targets in inode order, so the strings do not
/// depend on which batch reported what.
fn push_links(plan: &mut Plan, batches: &[Batch]) -> Result<(), BuildError> {
    for k in 0..plan.winner.len() {
        let (b, f) = plan.index.file(plan.winner[k] as usize);
        let kind = crate::Kind::from_mode(batches[b].file_stats[f].mode);
        if matches!(
            kind,
            crate::Kind::Fifo | crate::Kind::Socket | crate::Kind::Block | crate::Kind::Character
        ) {
            plan.specials
                .push(((plan.order.len() + k) as u32, kind as u32));
        }
        if let Some(target) = batches[b].target(f) {
            if target.contains(&0) {
                return Err(BuildError::BadPath(target.to_vec()));
            }
            let offset = push_string(&mut plan.strings, target)?;
            plan.links.push(((plan.order.len() + k) as u32, offset));
        }
    }
    Ok(())
}

/// Work-tree paths sorted by directory, so the strings do not depend on
/// which batch reported what.
fn push_work_trees(plan: &mut Plan, batches: &[Batch], tokens: &Tokens) -> Result<(), BuildError> {
    let mut work_trees = Vec::new();
    for batch in batches {
        for wt in &batch.work_trees {
            let dir = plan.dir_id[tokens.resolve(wt.dir)?];
            work_trees.push((dir, wt.common_dir.of(&batch.strings), wt));
        }
    }
    work_trees.sort_unstable_by_key(|&(dir, _, _)| dir);
    if work_trees.windows(2).any(|w| w[0].0 == w[1].0) {
        return Err(BuildError::DuplicateWorkTree);
    }
    for (dir, path, wt) in work_trees {
        if path.contains(&0) {
            return Err(BuildError::BadPath(path.to_vec()));
        }
        let offset = push_string(&mut plan.strings, path)?;
        plan.work_trees
            .push((dir, offset, wt.common_id, wt.kind as u8));
    }
    Ok(())
}

/// A nullable id to a blocked column's sizer: none for [`NONE`].
fn push_known(sizer: &mut BlockSizer, id: u32) {
    match id {
        NONE => sizer.push_null(),
        id => sizer.push(u64::from(id)),
    }
}

/// The largest name heap: its offsets, and the running offset [`write`]
/// keeps, are `u32`.
fn heap_limit() -> usize {
    #[cfg(test)]
    if let Some(limit) = HEAP_LIMIT.get() {
        return limit;
    }
    NONE as usize
}

#[cfg(test)]
thread_local! {
    /// Test seam: a lower name-heap limit, since a 4 GiB heap is too big to
    /// build in a unit test. The check is the same code either way.
    pub(crate) static HEAP_LIMIT: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

fn push_string(strings: &mut Vec<u8>, bytes: &[u8]) -> Result<u32, BuildError> {
    let offset = u32::try_from(strings.len()).map_err(|_| BuildError::TooLarge)?;
    strings.extend_from_slice(bytes);
    strings.push(0);
    if strings.len() > NONE as usize {
        return Err(BuildError::TooLarge);
    }
    Ok(offset)
}
