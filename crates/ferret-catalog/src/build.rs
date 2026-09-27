//! Merges batches into one generation's tables (D29, D30, D31, D36).
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

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;

use crate::batch::{Batch, Content, DirToken, FileEntry};
use crate::format::{NONE, NameRow, Tables, WorkTreeRow};
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
    /// Each live document's hash, so known content keeps its `DocId`.
    pub(crate) docs: &'a HashMap<Hash, u32>,
    /// The persisted next-id counter; never goes back (D36 B).
    pub(crate) next_doc: u32,
}

#[derive(Clone, Copy)]
enum Child {
    Dir(usize),
    /// (batch position, index in its files).
    File(usize, usize),
}

#[derive(Clone)]
struct Edge {
    batch: usize,
    name: std::ops::Range<usize>,
    child: Child,
}

/// One file inode being built: which observation wins, and whether
/// observations disagreed.
struct FileInode {
    batch: usize,
    file: usize,
    fault: bool,
}

pub(crate) fn build(
    batches: &[Batch],
    sniffer: u32,
    known: Known<'_>,
) -> Result<Tables, BuildError> {
    let mut position = HashMap::with_capacity(batches.len());
    let mut base = Vec::with_capacity(batches.len());
    let mut total_dirs = 0usize;
    for (i, batch) in batches.iter().enumerate() {
        position.insert(batch.id, i);
        base.push(total_dirs);
        total_dirs += batch.dirs.len();
    }
    if total_dirs >= NONE as usize {
        return Err(BuildError::TooLarge);
    }
    let resolve = |token: DirToken| -> Result<usize, BuildError> {
        let &i = position
            .get(&token.batch)
            .ok_or(BuildError::UnknownToken(token))?;
        if (token.index as usize) < batches[i].dirs.len() {
            Ok(base[i] + token.index as usize)
        } else {
            Err(BuildError::UnknownToken(token))
        }
    };

    // Every edge, grouped by its parent's global index, and the roots. The
    // first pass resolves each edge's parent and counts edges per directory;
    // the second scatters each edge into its directory's counted range, in
    // the same order, so only each directory's children are ever sorted.
    let mut roots = Vec::new();
    let mut edges_per_dir = vec![0usize; total_dirs + 1];
    let mut parent_of = Vec::new();
    for (i, batch) in batches.iter().enumerate() {
        for (d, dir) in batch.dirs.iter().enumerate() {
            match dir.parent {
                None => roots.push((&batch.bytes[dir.name.clone()], base[i] + d)),
                Some(parent) => parent_of.push(resolve(parent)?),
            }
        }
        for file in &batch.files {
            parent_of.push(resolve(file.parent)?);
        }
    }
    for &parent in &parent_of {
        edges_per_dir[parent + 1] += 1;
    }
    for d in 0..total_dirs {
        edges_per_dir[d + 1] += edges_per_dir[d];
    }
    let placeholder = Edge {
        batch: 0,
        name: 0..0,
        child: Child::Dir(0),
    };
    let mut edges = vec![placeholder; parent_of.len()];
    let mut cursor = edges_per_dir.clone();
    let mut parents = parent_of.iter();
    let mut place = |edge: Edge| {
        if let Some(&parent) = parents.next() {
            edges[cursor[parent]] = edge;
            cursor[parent] += 1;
        }
    };
    for (i, batch) in batches.iter().enumerate() {
        for (d, dir) in batch.dirs.iter().enumerate() {
            if dir.parent.is_some() {
                place(Edge {
                    batch: i,
                    name: dir.name.clone(),
                    child: Child::Dir(base[i] + d),
                });
            }
        }
        for (f, file) in batch.files.iter().enumerate() {
            place(Edge {
                batch: i,
                name: file.name.clone(),
                child: Child::File(i, f),
            });
        }
    }
    drop(parent_of);

    roots.sort_by(|a, b| a.0.cmp(b.0));
    if let Some(pair) = roots.windows(2).find(|w| w[0].0 == w[1].0) {
        return Err(BuildError::DuplicateRoot(pair[0].0.to_vec()));
    }

    let mut t = Tables {
        sniffer,
        ..Tables::default()
    };
    let mut dir_id = vec![NONE; total_dirs];
    let mut order = Vec::with_capacity(total_dirs);
    for &(path, global) in &roots {
        if path.is_empty() || path.contains(&0) {
            return Err(BuildError::BadPath(path.to_vec()));
        }
        dir_id[global] = order.len() as u32;
        t.roots
            .push((order.len() as u32, push_string(&mut t.strings, path)?));
        order.push(global);
    }

    // Breadth first: emit each directory's children sorted by name.
    let mut file_children = Vec::new();
    let mut name_of_dir = vec![NONE; total_dirs];
    let mut next = 0;
    while next < order.len() {
        let global = order[next];
        let parent = next as u32;
        let children = &mut edges[edges_per_dir[global]..edges_per_dir[global + 1]];
        let name_of = |e: &Edge| &batches[e.batch].bytes[e.name.clone()];
        children.sort_unstable_by(|a, b| name_of(a).cmp(name_of(b)));
        for (k, edge) in children.iter().enumerate() {
            let name = name_of(edge);
            if name.is_empty()
                || name == b"."
                || name == b".."
                || name.iter().any(|&b| b == 0 || b == b'/')
            {
                return Err(BuildError::BadName(name.to_vec()));
            }
            if k > 0 && name_of(&children[k - 1]) == name {
                return Err(BuildError::DuplicateName(name.to_vec()));
            }
            let name_id = t.names.len() as u32;
            if name_id == NONE || t.name_heap.len() >= NONE as usize {
                return Err(BuildError::TooLarge);
            }
            let offset = t.name_heap.len() as u32;
            t.name_heap.extend_from_slice(name);
            t.name_heap.push(0);
            let child = match edge.child {
                Child::Dir(g) => {
                    dir_id[g] = order.len() as u32;
                    order.push(g);
                    name_of_dir[g] = name_id;
                    dir_id[g]
                }
                Child::File(b, f) => {
                    file_children.push((name_id, b, f));
                    NONE
                }
            };
            t.names.push(NameRow {
                parent,
                child,
                offset,
            });
        }
        next += 1;
    }
    if order.len() != total_dirs {
        return Err(BuildError::Unreachable);
    }

    // Directory rows, in id order.
    let dir_entry = |global: usize| {
        let b = base.partition_point(|&start| start <= global) - 1;
        &batches[b].dirs[global - base[b]]
    };
    t.dir_names = order.iter().map(|&global| name_of_dir[global]).collect();
    for &global in &order {
        let dir = dir_entry(global);
        t.inodes.push((dir.stat, NONE));
        t.states.push(ContentState::Unindexed);
        t.traversed.push(dir.traversed);
    }

    // File rows: one per (dev, ino), numbered by first name.
    let mut by_identity: HashMap<(u64, u64), u32> = HashMap::with_capacity(file_children.len());
    let mut inodes: Vec<FileInode> = Vec::new();
    let file = |b: usize, f: usize| -> &FileEntry { &batches[b].files[f] };
    for &(name_id, b, f) in &file_children {
        let entry = file(b, f);
        let id = match by_identity.entry((entry.stat.dev, entry.stat.ino)) {
            Entry::Vacant(slot) => {
                let id = total_dirs + inodes.len();
                if id >= NONE as usize {
                    return Err(BuildError::TooLarge);
                }
                inodes.push(FileInode {
                    batch: b,
                    file: f,
                    fault: false,
                });
                *slot.insert(id as u32)
            }
            Entry::Occupied(slot) => {
                let id = *slot.get();
                let held = &mut inodes[id as usize - total_dirs];
                let (held_carried, carried) = (batches[held.batch].carried, batches[b].carried);
                if held_carried && !carried {
                    *held = FileInode {
                        batch: b,
                        file: f,
                        fault: false,
                    };
                } else if held_carried == carried
                    && !same_observation(batches, (held.batch, held.file), (b, f))
                {
                    held.fault = true;
                }
                id
            }
        };
        t.names[name_id as usize].child = id;
    }

    let mut new_docs: HashMap<Hash, u32> = HashMap::new();
    let mut next_doc = known.next_doc;
    let mut docs = Vec::new();
    for (k, inode) in inodes.iter().enumerate() {
        let entry = file(inode.batch, inode.file);
        let id = (total_dirs + k) as u32;
        if let Some(target) = &entry.target {
            let target = &batches[inode.batch].bytes[target.clone()];
            if target.contains(&0) {
                return Err(BuildError::BadPath(target.to_vec()));
            }
            t.links.push((id, push_string(&mut t.strings, target)?));
        }
        let (state, doc) = match (inode.fault, entry.content) {
            (true, _) => (ContentState::Fault, NONE),
            (false, Content::Hashed(hash)) => {
                let doc = match known.docs.get(&hash) {
                    Some(&doc) => doc,
                    None => *new_docs.entry(hash).or_insert_with(|| {
                        next_doc = next_doc.saturating_add(1);
                        next_doc - 1
                    }),
                };
                docs.push((doc, hash));
                (ContentState::Hashed, doc)
            }
            (false, content) => (content.state(), NONE),
        };
        t.inodes.push((entry.stat, doc));
        t.states.push(state);
    }
    if next_doc == NONE {
        return Err(BuildError::TooLarge);
    }
    docs.sort_unstable_by_key(|&(id, _)| id);
    docs.dedup_by_key(|&mut (id, _)| id);
    t.docs = docs;
    t.next_doc = next_doc;

    // Sorted before their paths enter the heap, so the file does not depend
    // on which batch reported which work tree.
    let mut work_trees = Vec::new();
    for batch in batches {
        for wt in &batch.work_trees {
            work_trees.push((
                dir_id[resolve(wt.dir)?],
                wt,
                &batch.bytes[wt.common_dir.clone()],
            ));
        }
    }
    work_trees.sort_unstable_by_key(|&(dir, _, _)| dir);
    if work_trees.windows(2).any(|w| w[0].0 == w[1].0) {
        return Err(BuildError::DuplicateWorkTree);
    }
    for (dir, wt, path) in work_trees {
        if path.contains(&0) {
            return Err(BuildError::BadPath(path.to_vec()));
        }
        let offset = push_string(&mut t.strings, path)?;
        t.work_trees.push(WorkTreeRow {
            dir,
            offset,
            common_id: wt.common_id,
            kind: wt.kind as u8,
        });
    }
    Ok(t)
}

fn same_observation(batches: &[Batch], a: (usize, usize), b: (usize, usize)) -> bool {
    let (fa, fb) = (&batches[a.0].files[a.1], &batches[b.0].files[b.1]);
    let target = |batch: usize, f: &FileEntry| f.target.clone().map(|r| &batches[batch].bytes[r]);
    fa.stat == fb.stat && fa.content == fb.content && target(a.0, fa) == target(b.0, fb)
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
