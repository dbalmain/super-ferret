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
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::FileExt;

use crate::batch::{Batch, Content, DirToken, Stat};
use crate::format::{
    self, At, Bits, COLUMNS, Coding, Column, ColumnWriter, Descriptor, HASH_ROW, Head, NONE,
    PAIR_ROW, Range, SECTIONS, Section, WORK_TREE_ROW,
};
use crate::packed::BlockSizer;
use crate::{ContentState, Hash};

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
}

impl Index {
    fn new(batches: &[Batch]) -> Self {
        let (mut dir_base, mut file_base) = (Vec::new(), Vec::new());
        let (mut dirs, mut files) = (0, 0);
        for batch in batches {
            dir_base.push(dirs);
            file_base.push(files);
            dirs += batch.dirs.len();
            files += batch.files.len();
        }
        Self {
            dir_base,
            file_base,
            dirs,
            files,
        }
    }

    /// (batch position, index in its dirs) of directory `g`.
    fn dir(&self, g: usize) -> (usize, usize) {
        let b = self.dir_base.partition_point(|&start| start <= g) - 1;
        (b, g - self.dir_base[b])
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
    /// Whether fresh observations of each file inode disagreed.
    fault: Vec<bool>,
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
    /// (dir InoId, offset in strings, common id, kind), sorted by dir.
    work_trees: Vec<(u32, u32, (u64, u64), u8)>,
    /// (hash as big-endian halves, DocId), sorted by id.
    docs: Vec<((u64, u64), u32)>,
}

impl Plan {
    fn name<'a>(&self, batches: &'a [Batch], entry: u32) -> &'a [u8] {
        let entry = entry as usize;
        if entry < self.index.dirs {
            let (b, d) = self.index.dir(entry);
            batches[b].dirs[d].name.of(&batches[b].names)
        } else {
            let (b, f) = self.index.file(entry - self.index.dirs);
            batches[b].files[f].name.of(&batches[b].names)
        }
    }

    /// The inode an entry names.
    fn child(&self, entry: u32) -> u32 {
        let e = entry as usize;
        match e < self.index.dirs {
            true => self.dir_id[e],
            false => self.inode_of_file[e - self.index.dirs],
        }
    }

    fn children(&self, dir: u32) -> &[u32] {
        let d = dir as usize;
        &self.edges[self.edge_start[d] as usize..self.edge_start[d + 1] as usize]
    }

    /// Each inode row's stat, `DocId` (or NONE) and content state, in inode
    /// order: directories, then files and symlinks.
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
            let state = match self.fault[k] {
                true => ContentState::Fault,
                false => batches[b].contents[f].state(),
            };
            (&batches[b].file_stats[f], self.doc[k], state)
        });
        dirs.chain(files)
    }

    /// The head of the file, and each dictionary column's sorted values.
    /// One pass over the inode rows finds every stat column's range, blocks
    /// and distinct values; the id columns are sized from the counts.
    fn head(&self, batches: &[Batch]) -> (Head, [Vec<u64>; COLUMNS.len()]) {
        let dirs = self.index.dirs;
        let (inodes, names) = (dirs + self.winner.len(), self.edges.len());
        let mut ranges = [Range::default(); COLUMNS.len()];
        let mut sets: [HashSet<u64>; COLUMNS.len()] = Default::default();
        let mut blocks: [BlockSizer; COLUMNS.len()] = Default::default();
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
                    Coding::Frame | Coding::Nullable | Coding::Sequence => ranges[c].add(value),
                }
            }
            if doc != NONE {
                ranges[Column::Doc as usize].add(u64::from(doc));
            }
        }
        for &count in &self.entry_count {
            if count != NONE {
                ranges[Column::Entries as usize].add(u64::from(count));
            }
        }
        // Ids are sized by what they index: the counts and the heap.
        let below = |n: usize| Range((n > 0).then(|| (0, n as u64 - 1)));
        let [parent, child] = blocks
            .get_disjoint_mut([Column::NameParent as usize, Column::NameChild as usize])
            .unwrap_or_else(|_| unreachable!("two columns"));
        for (id, &dir) in self.order.iter().enumerate() {
            for &entry in self.children(dir) {
                parent.push(id as u64);
                child.push(u64::from(self.child(entry)));
            }
        }
        ranges[Column::DirName as usize] = below(names);
        for (row, &(_, id)) in self.docs.iter().enumerate() {
            ranges[Column::DocId as usize].add(u64::from(id) - row as u64);
        }

        let mut dicts: [Vec<u64>; COLUMNS.len()] = Default::default();
        let columns = COLUMNS.map(|column| {
            let c = column as usize;
            match column.coding() {
                Coding::Frame | Coding::Sequence => Descriptor::frame(ranges[c]),
                Coding::Nullable => Descriptor::nullable(ranges[c]),
                Coding::Blocked if column == Column::NameOffset => {
                    Descriptor::blocked(self.offset_blocks)
                }
                Coding::Blocked => Descriptor::blocked(std::mem::take(&mut blocks[c]).finish()),
                Coding::Dictionary => {
                    let mut dict: Vec<u64> = std::mem::take(&mut sets[c]).into_iter().collect();
                    dict.sort_unstable();
                    let len = dict.len() as u32;
                    dicts[c] = dict;
                    Descriptor::dictionary(len)
                }
            }
        });
        let mut lens = [0; SECTIONS.len()];
        for (section, len) in [
            (Section::NameHeap, self.heap_len),
            (Section::Traversed, dirs.div_ceil(8)),
            (Section::Roots, self.roots.len() * PAIR_ROW),
            (Section::Strings, self.strings.len()),
            (Section::States, inodes.div_ceil(4)),
            (Section::Links, self.links.len() * PAIR_ROW),
            (Section::WorkTrees, self.work_trees.len() * WORK_TREE_ROW),
            (Section::Docs, self.docs.len() * HASH_ROW),
        ] {
            lens[section as usize] = len as u64;
        }
        let head = Head {
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
}

/// The inode columns read straight from a [`Stat`]: all but the `DocId`.
const STAT_COLUMNS: [Column; 10] = [
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
fn stat_field(column: Column, stat: &Stat) -> u64 {
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
    if dirs + files >= NONE as usize {
        return Err(BuildError::TooLarge);
    }
    // Each batch's first directory number and its directories, by batch id.
    let position: HashMap<u32, (usize, usize)> = batches
        .iter()
        .zip(&index.dir_base)
        .map(|(b, &base)| (b.id, (base, b.dirs.len())))
        .collect();
    let resolve = |token: DirToken| -> Result<usize, BuildError> {
        match position.get(&token.batch) {
            Some(&(base, len)) if (token.index as usize) < len => Ok(base + token.index as usize),
            _ => Err(BuildError::UnknownToken(token)),
        }
    };

    // Each entry's parent, then the entries grouped by parent: count, prefix
    // sum, scatter. Only each directory's own children are ever sorted.
    let mut roots = Vec::new();
    let mut parent_of = vec![NONE; dirs + files];
    for (i, batch) in batches.iter().enumerate() {
        for (d, dir) in batch.dirs.iter().enumerate() {
            let g = index.dir_base[i] + d;
            match dir.parent {
                None => roots.push((dir.name.of(&batch.names), g)),
                Some(parent) => parent_of[g] = resolve(parent)? as u32,
            }
        }
        for (f, file) in batch.files.iter().enumerate() {
            parent_of[dirs + index.file_base[i] + f] = resolve(file.parent)? as u32;
        }
    }
    let mut edge_start = vec![0u32; dirs + 1];
    for &parent in &parent_of {
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
    drop((parent_of, cursor));

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
        doc: Vec::new(),
        heap_len: 0,
        offset_blocks: (0, 0),
        roots: Vec::with_capacity(roots.len()),
        strings: Vec::new(),
        links: Vec::new(),
        work_trees: Vec::new(),
        docs: Vec::new(),
    };
    for &(path, global) in &roots {
        if path.is_empty() || path.contains(&0) {
            return Err(BuildError::BadPath(path.to_vec()));
        }
        let id = plan.order.len() as u32;
        plan.dir_id[global] = id;
        plan.roots.push((id, push_string(&mut plan.strings, path)?));
        plan.order.push(global as u32);
    }
    drop(roots);

    // Breadth first: sort each directory's children, check them, number the
    // child directories, and note each file's first name position.
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
            let entry = entry as usize;
            if entry < dirs {
                plan.dir_id[entry] = plan.order.len() as u32;
                plan.order.push(entry as u32);
                plan.name_of_dir[entry] = name_id as u32;
            } else {
                file_pos[entry - dirs] = name_id as u32;
            }
            name_id += 1;
        }
        next += 1;
    }
    if plan.order.len() != dirs {
        return Err(BuildError::Unreachable);
    }
    plan.offset_blocks = offsets.finish();

    for batch in batches.iter_mut() {
        for (dir, count) in std::mem::take(&mut batch.entry_counts) {
            plan.entry_count[resolve(dir)?] = count;
        }
    }

    number_files(&mut plan, batches, file_pos)?;
    assign_docs(&mut plan, batches, known.docs)?;

    // Link targets in inode order, then work-tree paths sorted by directory,
    // so the strings do not depend on which batch reported what.
    for k in 0..plan.winner.len() {
        let (b, f) = plan.index.file(plan.winner[k] as usize);
        if let Some(target) = batches[b].target(f) {
            if target.contains(&0) {
                return Err(BuildError::BadPath(target.to_vec()));
            }
            let offset = push_string(&mut plan.strings, target)?;
            plan.links.push(((dirs + k) as u32, offset));
        }
    }
    let mut work_trees = Vec::new();
    for batch in batches {
        for wt in &batch.work_trees {
            let dir = plan.dir_id[resolve(wt.dir)?];
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
    Ok(plan)
}

/// One inode row per `(dev, ino)` among files, numbered by first name (D31 C).
///
/// Sorts `(fingerprint, name position, file)` triples, 16 B per file, where a
/// map keyed by `(dev, ino)` measured several times that. Fingerprints that
/// collide are told apart by re-sorting their group on the real identity.
fn number_files(plan: &mut Plan, batches: &[Batch], file_pos: Vec<u32>) -> Result<(), BuildError> {
    let index = &plan.index;
    let identity = |f: u32| -> (u64, u64) {
        let (b, i) = index.file(f as usize);
        let s: &Stat = &batches[b].file_stats[i];
        (s.dev, s.ino)
    };
    let mut keys: Vec<(u64, u32, u32)> = file_pos
        .iter()
        .enumerate()
        .map(|(f, &pos)| {
            let (dev, ino) = identity(f as u32);
            (fingerprint(dev, ino), pos, f as u32)
        })
        .collect();
    drop(file_pos);
    keys.sort_unstable();

    // Each identity's observations, in name order, become one group: the
    // observation it keeps, and whether it is a fault. Nearly every run is
    // one file, which is its own winner and cannot disagree with itself, so
    // only longer runs read the stats, which lie scattered in fingerprint
    // order (a cache miss apiece: 112 ms of a 270 ms plan at 358k files
    // before this shortcut).
    let mut group_of = vec![NONE; index.files];
    let mut groups: Vec<(u32, bool)> = Vec::new();
    let mut members = Vec::new();
    let mut start = 0;
    while start < keys.len() {
        let fp = keys[start].0;
        let end = start + keys[start..].iter().take_while(|k| k.0 == fp).count();
        let run = &mut keys[start..end];
        start = end;
        if let [single] = run {
            let group = u32::try_from(groups.len()).map_err(|_| BuildError::TooLarge)?;
            groups.push((single.2, false));
            group_of[single.2 as usize] = group;
            continue;
        }
        let first = identity(run[0].2);
        if run.iter().any(|k| identity(k.2) != first) {
            run.sort_unstable_by_key(|k| (identity(k.2), k.1));
        }
        let mut at = 0;
        while at < run.len() {
            let id = identity(run[at].2);
            let len = run[at..].iter().take_while(|k| identity(k.2) == id).count();
            members.clear();
            members.extend(run[at..at + len].iter().map(|k| k.2));
            let group = u32::try_from(groups.len()).map_err(|_| BuildError::TooLarge)?;
            groups.push(choose(index, batches, &members));
            for &f in &members {
                group_of[f as usize] = group;
            }
            at += len;
        }
    }
    drop(keys);

    // Number the groups by their first name: the names in order again.
    let dirs = index.dirs;
    let mut inode_of_group = vec![NONE; groups.len()];
    plan.winner.reserve_exact(groups.len());
    plan.fault.reserve_exact(groups.len());
    let (edges, edge_start) = (&plan.edges, &plan.edge_start);
    for &dir in &plan.order {
        let d = dir as usize;
        for &entry in &edges[edge_start[d] as usize..edge_start[d + 1] as usize] {
            let Some(file) = (entry as usize).checked_sub(dirs) else {
                continue;
            };
            let group = group_of[file] as usize;
            if inode_of_group[group] == NONE {
                let id = dirs + plan.winner.len();
                if id >= NONE as usize {
                    return Err(BuildError::TooLarge);
                }
                inode_of_group[group] = id as u32;
                plan.winner.push(groups[group].0);
                plan.fault.push(groups[group].1);
            }
        }
    }
    drop(groups);
    for group in &mut group_of {
        *group = inode_of_group[*group as usize];
    }
    plan.inode_of_file = group_of;
    Ok(())
}

/// Which of one inode's observations (files, in name order) is kept, and
/// whether it is a fault: the first fresh one if any, else the first carried
/// one; a fault when another observation of the same kind disagrees with it.
fn choose(index: &Index, batches: &[Batch], members: &[u32]) -> (u32, bool) {
    let carried = |f: u32| batches[index.file(f as usize).0].carried;
    let winner = members
        .iter()
        .copied()
        .find(|&f| !carried(f))
        .unwrap_or(members[0]);
    let fault = members
        .iter()
        .any(|&f| carried(f) == carried(winner) && !same_observation(index, batches, winner, f));
    (winner, fault)
}

fn same_observation(index: &Index, batches: &[Batch], a: u32, b: u32) -> bool {
    let (ba, fa) = index.file(a as usize);
    let (bb, fb) = index.file(b as usize);
    let (xa, xb) = (&batches[ba], &batches[bb]);
    xa.file_stats[fa] == xb.file_stats[fb]
        && xa.contents[fa] == xb.contents[fb]
        && xa.target(fa) == xb.target(fb)
}

/// Gives each hashed inode its document: the old `DocId` for known content,
/// else a new one, assigned in inode order of first appearance (D36 B).
fn assign_docs(
    plan: &mut Plan,
    batches: &[Batch],
    known: &[(Hash, u32)],
) -> Result<(), BuildError> {
    // Hashes as big-endian `u64` pairs: the same order as the bytes, which
    // `known` is sorted in, and compared in two instructions rather than a
    // 16-byte `memcmp`.
    let mut hashed: Vec<((u64, u64), u32)> = Vec::new();
    for (k, &file) in plan.winner.iter().enumerate() {
        let (b, f) = plan.index.file(file as usize);
        if let (false, Content::Hashed(hash)) = (plan.fault[k], batches[b].contents[f]) {
            hashed.push((split_hash(&hash), k as u32));
        }
    }
    hashed.sort_unstable();
    plan.doc = vec![NONE; plan.winner.len()];
    // New content: (first inode, where its run starts in `hashed`).
    let mut fresh: Vec<(u32, u32)> = Vec::new();
    let mut start = 0;
    while start < hashed.len() {
        let hash = hashed[start].0;
        let end = start + hashed[start..].iter().take_while(|h| h.0 == hash).count();
        match known.binary_search_by(|probe| split_hash(&probe.0).cmp(&hash)) {
            Ok(at) => {
                for &(_, k) in &hashed[start..end] {
                    plan.doc[k as usize] = known[at].1;
                }
            }
            Err(_) => fresh.push((hashed[start].1, start as u32)),
        }
        start = end;
    }
    fresh.sort_unstable();
    for (_, start) in fresh {
        let doc = plan.next_doc;
        plan.next_doc = plan.next_doc.saturating_add(1);
        let hash = hashed[start as usize].0;
        for &(_, k) in hashed[start as usize..].iter().take_while(|h| h.0 == hash) {
            plan.doc[k as usize] = doc;
        }
    }
    if plan.next_doc == NONE {
        return Err(BuildError::TooLarge);
    }
    // The docs rows, made in place: one per distinct hash, sorted by id.
    hashed.dedup_by_key(|h| h.0);
    for row in &mut hashed {
        row.1 = plan.doc[row.1 as usize];
    }
    hashed.sort_unstable_by_key(|&(_, id)| id);
    plan.docs = hashed;
    Ok(())
}

fn split_hash(hash: &Hash) -> (u64, u64) {
    let (mut hi, mut lo) = ([0; 8], [0; 8]);
    hi.copy_from_slice(&hash[..8]);
    lo.copy_from_slice(&hash[8..]);
    (u64::from_be_bytes(hi), u64::from_be_bytes(lo))
}

/// Writes the planned generation to `out`: the head, then each section at the
/// place the head gives it. Each table's columns fill in one pass over its
/// rows, each column through its own bounded buffer (D40). Frees each batch's
/// names once the name sections are written, and the batches once the inode
/// sections are.
pub(crate) fn write(mut plan: Plan, mut batches: Vec<Batch>, out: &File) -> io::Result<()> {
    let (head, dicts) = plan.head(&batches);
    let (bytes, len) = head.encode();
    out.write_all_at(&bytes, 0)?;
    // The reader's placement of every section and column, from the head just
    // made: the writer puts each exactly where a reader will look.
    let layout = format::decode_table(&bytes, len).map_err(io::Error::other)?;
    let column = |c: Column| ColumnWriter::start(out, &layout, c, &dicts[c as usize]);
    let section = |s: Section| At::new(out, layout.range(s).0 as u64);
    let (mut parents, mut children) = (column(Column::NameParent)?, column(Column::NameChild)?);
    let (mut offsets, mut heap) = (column(Column::NameOffset)?, section(Section::NameHeap));
    let mut offset = 0;
    for (id, &dir) in plan.order.iter().enumerate() {
        for &entry in plan.children(dir) {
            let child = plan.child(entry);
            let name = plan.name(&batches, entry);
            parents.value(id as u64)?;
            children.value(u64::from(child))?;
            offsets.value(offset)?;
            heap.write_all(name)?;
            heap.write_all(&[0])?;
            offset += name.len() as u64 + 1;
        }
    }
    parents.finish()?;
    children.finish()?;
    offsets.finish()?;
    heap.finish()?;
    plan.edges = Vec::new();
    plan.edge_start = Vec::new();
    plan.inode_of_file = Vec::new();
    plan.dir_id = Vec::new();
    batches.iter_mut().for_each(Batch::drop_structure);

    let known = |id: u32| (id != NONE).then_some(u64::from(id));
    let (mut dir_names, mut entries) = (column(Column::DirName)?, column(Column::Entries)?);
    let (mut traversed, mut bits) = (section(Section::Traversed), Bits::new(1));
    for &dir in &plan.order {
        let (b, d) = plan.index.dir(dir as usize);
        dir_names.nullable(known(plan.name_of_dir[dir as usize]))?;
        entries.nullable(known(plan.entry_count[dir as usize]))?;
        bits.push(&mut traversed, u8::from(batches[b].dirs[d].traversed))?;
    }
    dir_names.finish()?;
    entries.finish()?;
    bits.finish(&mut traversed)?;
    traversed.finish()?;
    plan.entry_count = Vec::new();
    let mut roots = section(Section::Roots);
    for &(dir, offset) in &plan.roots {
        format::put_pair(&mut roots, dir, offset)?;
    }
    roots.finish()?;
    let mut strings = section(Section::Strings);
    strings.write_all(&plan.strings)?;
    strings.finish()?;

    // Every inode column in one pass over the rows, each row's stat found
    // once. A column of width 0 holds nothing per row, so it is only started
    // and finished.
    let mut stat_columns = Vec::new();
    for field in STAT_COLUMNS {
        stat_columns.push((field, column(field)?));
    }
    let (mut doc, mut states, mut state_bits) =
        (column(Column::Doc)?, section(Section::States), Bits::new(2));
    let doc_live = head.columns[Column::Doc as usize].width > 0;
    let live =
        |field: Column| field.coding() == Coding::Blocked || head.columns[field as usize].width > 0;
    let (mut live_columns, mut idle): (Vec<_>, Vec<_>) = stat_columns
        .into_iter()
        .partition(|&(field, _)| live(field));
    for (stat, doc_id, state) in plan.inode_rows(&batches) {
        for (field, column) in &mut live_columns {
            let value = stat_field(*field, stat);
            match field.coding() {
                Coding::Dictionary => {
                    let dict = &dicts[*field as usize];
                    column.index(dict.partition_point(|&known| known < value))?;
                }
                _ => column.value(value)?,
            }
        }
        if doc_live {
            doc.nullable(known(doc_id))?;
        }
        state_bits.push(&mut states, state as u8)?;
    }
    for (_, column) in live_columns.drain(..).chain(idle.drain(..)) {
        column.finish()?;
    }
    doc.finish()?;
    state_bits.finish(&mut states)?;
    states.finish()?;
    drop(batches);

    let mut links = section(Section::Links);
    for &(inode, offset) in &plan.links {
        format::put_pair(&mut links, inode, offset)?;
    }
    links.finish()?;
    let mut work_trees = section(Section::WorkTrees);
    for &(dir, offset, common_id, kind) in &plan.work_trees {
        format::put_work_tree(&mut work_trees, dir, offset, common_id, kind)?;
    }
    work_trees.finish()?;
    let mut ids = column(Column::DocId)?;
    let mut hashes = At::new(out, (layout.range(Section::Docs).0 + layout.hashes) as u64);
    for &((hi, lo), id) in &plan.docs {
        ids.value(u64::from(id))?;
        hashes.write_all(&hi.to_be_bytes())?;
        hashes.write_all(&lo.to_be_bytes())?;
    }
    ids.finish()?;
    hashes.finish()?;
    Ok(())
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

/// Mixes `(dev, ino)` into 64 bits for sorting. Equal identities always agree;
/// unequal ones that collide are told apart by the caller.
fn fingerprint(dev: u64, ino: u64) -> u64 {
    let mut x = ino ^ dev.rotate_left(29).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 31;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^ (x >> 29)
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
