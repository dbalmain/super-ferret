//! Each section's structural check, run once when a reader first loads it.

use std::sync::OnceLock;

use super::*;
use crate::fan_out::{cores, fan_out};
use crate::packed::RUN;

/// What one section's check found that a later check reuses, so that no
/// check decodes a column a check before it already decoded. Kept by the
/// reader for as long as it loads sections.
#[derive(Debug, Default)]
pub(crate) struct Facts {
    /// Names whose child is a directory, counted by the name-rows check.
    dir_children: OnceLock<usize>,
}

/// Every section, each after those it [needs](Section::needs).
pub(super) const CHECK_ORDER: [Section; SECTIONS.len()] = [
    Section::NameHeap,
    Section::Names,
    Section::DirNames,
    Section::Entries,
    Section::Traversed,
    Section::Strings,
    Section::Roots,
    Section::Dev,
    Section::Ino,
    Section::Size,
    Section::Mtime,
    Section::MtimeNs,
    Section::Ctime,
    Section::CtimeNs,
    Section::Mode,
    Section::Owner,
    Section::Nlink,
    Section::Doc,
    Section::States,
    Section::Links,
    Section::Specials,
    Section::WorkTrees,
    Section::Docs,
    Section::DocRefs,
    Section::RetainedAt,
    Section::Policy,
    Section::Revealed,
    Section::Dormant,
];

/// Validates one section, given its bytes and those of every section it
/// [needs](Section::needs), through `get`. See the [format module doc](super)
/// for what is checked. The sections in the last arm have nothing beyond their
/// lengths, which [`decode_table`] checked.
pub(crate) fn check<'a>(
    section: Section,
    l: &Layout,
    facts: &Facts,
    get: impl Fn(Section) -> &'a [u8],
) -> Result<(), DecodeError> {
    if checksum(get(section)) != l.checksums[section as usize] {
        return Err(DecodeError::Corrupt(section.label()));
    }
    check_blocked(section, l, get(section))?;
    match section {
        Section::NameHeap => check_terminated(get(section), "name heap")?,
        Section::Strings => check_terminated(get(section), "strings")?,
        Section::Names => {
            let dir_children = check_names(l, get(Section::Names), get(Section::NameHeap))?;
            // A racing loader may have set it first; its count is the same.
            let _ = facts.dir_children.set(dir_children);
        }
        Section::DirNames => {
            let dir_children = *facts.dir_children.get().unwrap_or_else(|| {
                panic!("dir names checked before the names they need");
            });
            check_dir_names(l, get(Section::DirNames), get(Section::Names), dir_children)?;
        }
        Section::Dev => check_dictionary(l, Column::Dev, get(Section::Dev))?,
        Section::Mode => check_dictionary(l, Column::Mode, get(Section::Mode))?,
        Section::Owner => check_dictionary(l, Column::Owner, get(Section::Owner))?,
        Section::Roots => check_roots(l, get(Section::Roots), get(Section::DirNames))?,
        Section::Specials => check_specials(l, get(Section::Specials))?,
        Section::Links => check_links(l, get(Section::Links), get(Section::Specials))?,
        Section::WorkTrees => check_work_trees(l, get(Section::WorkTrees))?,
        Section::Docs => check_docs(l, get(Section::Docs))?,
        Section::DocRefs => check_doc_refs(l, get(section), get(Section::Docs), get(Section::Doc))?,
        Section::RetainedAt => check_retained_at(l, get(section))?,
        Section::Policy => {}
        Section::Revealed => check_revealed(l, get(section))?,
        Section::Dormant => crate::dormant::check(l, get(section), get(Section::Docs))?,
        Section::Entries
        | Section::Traversed
        | Section::Ino
        | Section::Size
        | Section::Mtime
        | Section::MtimeNs
        | Section::Ctime
        | Section::CtimeNs
        | Section::Nlink
        | Section::Doc
        | Section::States => {}
    }
    Ok(())
}

/// Every blocked column stored in `section` decodes within its width.
fn check_blocked(section: Section, l: &Layout, bytes: &[u8]) -> Result<(), DecodeError> {
    for column in COLUMNS {
        let coding = column.coding();
        if column.section() == section && coding.is_blocked() {
            let desc = l.columns[column as usize].desc;
            let nullable = coding == Coding::NullableBlocked;
            if !l
                .blocked(column, bytes)
                .check(desc.width, desc.base, nullable)
            {
                return Err(DecodeError::Corrupt(section.label()));
            }
        }
    }
    Ok(())
}

/// A string heap ends in the terminator of its last string.
fn check_terminated(heap: &[u8], label: &'static str) -> Result<(), DecodeError> {
    if heap.last().is_none_or(|&b| b == 0) {
        Ok(())
    } else {
        Err(DecodeError::Corrupt(label))
    }
}

/// One root per unnamed directory, ascending, each naming a string.
fn check_roots(l: &Layout, roots: &[u8], dir_names: &[u8]) -> Result<(), DecodeError> {
    let strings_len = l.len(Section::Strings);
    let dir_names = l.view(Column::DirName, dir_names);
    let unnamed = dir_names.nullables(l.dirs).filter(Option::is_none).count();
    let mut last = None;
    for pair in roots.chunks_exact(PAIR_ROW) {
        let (dir, offset) = (u32_at(pair, 0), u32_at(pair, 4) as usize);
        let ok = (dir as usize) < l.dirs
            && dir_names.nullable(dir as usize).is_none()
            && last.is_none_or(|last| dir > last)
            && offset < strings_len;
        if !ok {
            return Err(DecodeError::Corrupt("roots"));
        }
        last = Some(dir);
    }
    if roots.len() / PAIR_ROW != unnamed {
        return Err(DecodeError::Corrupt("roots"));
    }
    Ok(())
}

/// Non-directory inodes with a special kind, ascending.
fn check_specials(l: &Layout, specials: &[u8]) -> Result<(), DecodeError> {
    let mut last = None;
    for pair in specials.chunks_exact(PAIR_ROW) {
        let (ino, kind) = (u32_at(pair, 0), u32_at(pair, 4));
        if !(l.dirs..l.inodes).contains(&(ino as usize))
            || last.is_some_and(|last| ino <= last)
            || !(3..=6).contains(&kind)
        {
            return Err(DecodeError::Corrupt("specials"));
        }
        last = Some(ino);
    }
    Ok(())
}

/// Symlink targets for non-directory, non-special inodes, ascending.
fn check_links(l: &Layout, links: &[u8], specials: &[u8]) -> Result<(), DecodeError> {
    let strings_len = l.len(Section::Strings);
    let mut at = 0;
    let mut last = None;
    for pair in links.chunks_exact(PAIR_ROW) {
        let (ino, offset) = (u32_at(pair, 0), u32_at(pair, 4) as usize);
        while at < specials.len() && u32_at(specials, at) < ino {
            at += PAIR_ROW;
        }
        let ok = (at == specials.len() || u32_at(specials, at) != ino)
            && (l.dirs..l.inodes).contains(&(ino as usize))
            && last.is_none_or(|last| ino > last)
            && offset < strings_len;
        if !ok {
            return Err(DecodeError::Corrupt("links"));
        }
        last = Some(ino);
    }
    Ok(())
}

/// Work trees by directory, ascending, each with a known kind.
fn check_work_trees(l: &Layout, work_trees: &[u8]) -> Result<(), DecodeError> {
    let strings_len = l.len(Section::Strings);
    let mut last = None;
    for row in work_trees.chunks_exact(WORK_TREE_ROW) {
        let (dir, offset) = (u32_at(row, 0), u32_at(row, 4) as usize);
        let ok = (dir as usize) < l.dirs
            && last.is_none_or(|last| dir > last)
            && offset < strings_len
            && crate::WorkTreeKind::from_byte(row[24]).is_some();
        if !ok {
            return Err(DecodeError::Corrupt("work trees"));
        }
        last = Some(dir);
    }
    Ok(())
}

/// Document ids are sorted and assigned.
fn check_docs(l: &Layout, docs: &[u8]) -> Result<(), DecodeError> {
    // Each id as the reader decodes it: sorted, and assigned.
    let ids = l.view(Column::DocId, docs);
    let mut last = None;
    for (row, id) in ids.values(l.docs).enumerate() {
        let id = id.wrapping_add(row as u64);
        if id >= u64::from(l.next_doc) || last.is_some_and(|last| id <= last) {
            return Err(DecodeError::Corrupt("docs"));
        }
        last = Some(id);
    }
    Ok(())
}

/// Each stored reference count matches the inode bindings, and none is zero.
fn check_doc_refs(
    l: &Layout,
    refs: &[u8],
    docs: &[u8],
    bindings: &[u8],
) -> Result<(), DecodeError> {
    let counts = document_references(l, docs, bindings)?;
    for (row, count) in counts.into_iter().enumerate() {
        if count == 0 || count != u32_at(refs, row * 4) {
            return Err(DecodeError::Corrupt("document references"));
        }
    }
    Ok(())
}

/// No directory was retained after this generation.
fn check_retained_at(l: &Layout, retained_at: &[u8]) -> Result<(), DecodeError> {
    let retained = l.blocked(Column::RetainedAt, retained_at);
    if retained
        .nullable_values()
        .any(|n| n.is_some_and(|n| n > l.generation.sequence))
    {
        return Err(DecodeError::Corrupt("retained at"));
    }
    Ok(())
}

/// Revealed name ids are in range and ascending.
fn check_revealed(l: &Layout, revealed: &[u8]) -> Result<(), DecodeError> {
    let mut last = None;
    for row in revealed.chunks_exact(4) {
        let id = u32_at(row, 0);
        if id as usize >= l.names || last.is_some_and(|last| id <= last) {
            return Err(DecodeError::Corrupt("revealed"));
        }
        last = Some(id);
    }
    Ok(())
}

/// Counts indexed inode bindings in live document row order. Used by import
/// and by the lazy DocRefs validator; hard-link name edges are never counted.
pub(crate) fn document_references(
    l: &Layout,
    docs: &[u8],
    bindings: &[u8],
) -> Result<Vec<u32>, DecodeError> {
    let ids = l.view(Column::DocId, docs);
    let bindings = l.blocked(Column::Doc, bindings);
    let mut counts = vec![0u32; l.docs];
    // Bindings mostly ascend with the inode, so each search starts at the
    // last one's row.
    let mut at = 0;
    for id in bindings.nullable_values().flatten() {
        at = ids
            .sequence_row_near(id, l.docs, at)
            .ok_or(DecodeError::Corrupt("document references"))?;
        counts[at] = counts[at]
            .checked_add(1)
            .ok_or(DecodeError::Corrupt("document references"))?;
    }
    Ok(counts)
}

/// Every inode's dictionary index is inside the dictionary. Nothing is read
/// when the base and width cannot reach past its end, as when the writer's
/// dictionary length is a power of two.
fn check_dictionary(l: &Layout, column: Column, bytes: &[u8]) -> Result<(), DecodeError> {
    let desc = l.columns[column as usize].desc;
    let len = u64::from(desc.dict_len);
    let reach = desc.base.checked_add(packed::mask(desc.width));
    if reach.is_some_and(|reach| reach < len) {
        return Ok(());
    }
    if l.view(column, bytes).values(l.inodes).any(|i| i >= len) {
        return Err(DecodeError::Corrupt(column.section().label()));
    }
    Ok(())
}

/// Name rows: ids in range, offsets strictly increasing inside the heap from
/// 0, parents in order. Offsets and parents must be sorted because readers
/// binary-search them, and the first name must start the heap so every heap
/// byte belongs to a name.
///
/// Each name is exactly the bytes up to the next name's offset: non-empty,
/// its NUL last. The spans tile the heap from 0, so one NUL per name in the
/// whole heap means none inside a name; one vectorised count is far cheaper
/// than a search per name. Siblings are in strictly increasing byte order,
/// because `lookup` binary-searches them by name.
///
/// One pass decodes each column once: name `i - 1`'s span is checked when
/// row `i`'s offset, its end, has been. Returns the names whose child is a
/// directory, for [`check_dir_names`].
///
/// A large table is checked in row ranges on a few threads, since this pass
/// is most of the longest section chain an open waits for. The NUL count
/// is reported first and then each range's first error in row order, which
/// is the error a single pass would have met first.
fn check_names(l: &Layout, rows: &[u8], heap: &[u8]) -> Result<usize, DecodeError> {
    let threads = if l.names >= PARALLEL_NAMES {
        cores().min(NAME_THREADS)
    } else {
        1
    };
    let per = l.names.div_ceil(threads).next_multiple_of(RUN);
    let ranges: Vec<_> = (0..threads)
        .map(|k| (k * per).min(l.names)..((k + 1) * per).min(l.names))
        .enumerate()
        .collect();
    let checked = fan_out(ranges, threads, |(k, range)| {
        let nuls = &heap[k * heap.len() / threads..(k + 1) * heap.len() / threads];
        (count_nuls(nuls), check_name_rows(l, rows, heap, range))
    });
    if checked.iter().map(|(nuls, _)| nuls).sum::<usize>() != l.names {
        return Err(DecodeError::Corrupt("name order"));
    }
    checked.into_iter().map(|(_, dirs)| dirs).sum()
}

/// Names tables smaller than this are checked on the calling thread.
const PARALLEL_NAMES: usize = 1 << 15;

/// The most threads one names table is checked on.
const NAME_THREADS: usize = 4;

/// [`check_names`] over the rows in `range`, which starts at a multiple of
/// [`RUN`]. The two rows before it are the previous range's to check; they
/// seed this one, so a value only has to be safe to use here.
fn check_name_rows(
    l: &Layout,
    rows: &[u8],
    heap: &[u8],
    range: std::ops::Range<usize>,
) -> Result<usize, DecodeError> {
    let parents = l.blocked(Column::NameParent, rows);
    let children = l.blocked(Column::NameChild, rows);
    let offsets = l.blocked(Column::NameOffset, rows);
    // The name before this row: its parent and where its bytes start; and
    // the one before that, for sibling order.
    let mut open: Option<(u64, usize)> = None;
    let mut previous: Option<(u64, &[u8])> = None;
    let (mut last_parent, mut next_offset) = (0, 0);
    if let Some(before) = range.start.checked_sub(1) {
        let (parent, offset) = (parents.get(before), offsets.get(before));
        if offset >= heap.len() as u64 {
            return Err(DecodeError::Corrupt("names"));
        }
        open = Some((parent, offset as usize));
        (last_parent, next_offset) = (parent, offset + 1);
        if let Some(before) = before.checked_sub(1) {
            let start = offsets.get(before);
            if start >= offset {
                return Err(DecodeError::Corrupt("names"));
            }
            previous = Some((
                parents.get(before),
                &heap[start as usize..offset as usize - 1],
            ));
        }
    }
    let mut close = |end: usize, open: Option<(u64, usize)>| {
        let Some((parent, start)) = open else {
            return Ok(());
        };
        let (name, nul) = (&heap[start..end - 1], heap[end - 1]);
        let ordered = previous.is_none_or(|(p, prev)| p != parent || prev < name);
        if name.is_empty() || nul != 0 || !ordered {
            return Err(DecodeError::Corrupt("name order"));
        }
        previous = Some((parent, name));
        Ok(())
    };
    let mut dir_children = 0;
    let mut block = [[0; RUN]; 3];
    for first in range.clone().step_by(RUN) {
        let n = RUN.min(range.end - first);
        let [p, c, o] = &mut block;
        parents.decode(first, &mut p[..n]);
        children.decode(first, &mut c[..n]);
        offsets.decode(first, &mut o[..n]);
        for j in 0..n {
            let (parent, child, offset) = (p[j], c[j], o[j]);
            let ok = parent < l.dirs as u64
                && (child < l.inodes as u64
                    || (child <= u64::from(NONE)
                        && crate::Kind::from_ignored_child(child as u32).is_some()))
                && parent >= last_parent
                && offset >= next_offset
                && (next_offset > 0 || offset == 0)
                && offset < heap.len() as u64;
            if !ok {
                return Err(DecodeError::Corrupt("names"));
            }
            close(offset as usize, open)?;
            open = Some((parent, offset as usize));
            last_parent = parent;
            next_offset = offset + 1;
            dir_children += usize::from(child < l.dirs as u64);
        }
    }
    if range.end == l.names {
        close(heap.len(), open)?;
    }
    Ok(dir_children)
}

/// Every directory's name edge names it, from a lower-numbered parent, so a
/// walk upwards strictly descends and ends at a root. And the converse: a
/// name whose child is a directory must be that directory's recorded edge,
/// so each directory has exactly one name and a root has none. Without it a
/// name could make a root its own child, and a walk down (retention copying
/// a kept root) would never end.
///
/// The converse is a count. The first loop proves each named directory's
/// edge names it, so those edges are distinct names with directory children;
/// if no other name has a directory child, every such name is some
/// directory's edge. The names with a directory child, `dir_children`, were
/// counted as [`check_names`] decoded the child column, where a check per
/// name looked up its child's edge at random.
fn check_dir_names(
    l: &Layout,
    dir_names: &[u8],
    rows: &[u8],
    dir_children: usize,
) -> Result<(), DecodeError> {
    let dir_names = l.view(Column::DirName, dir_names);
    let parents = l.blocked(Column::NameParent, rows);
    let children = l.blocked(Column::NameChild, rows);
    let mut named = 0;
    for (dir, name) in dir_names.nullables(l.dirs).enumerate() {
        let Some(name) = name else {
            continue;
        };
        let ok = name < l.names as u64 && {
            let name = name as usize;
            children.get(name) == dir as u64 && parents.get(name) < dir as u64
        };
        if !ok {
            return Err(DecodeError::Corrupt("dir names"));
        }
        named += 1;
    }
    if dir_children != named {
        return Err(DecodeError::Corrupt("dir names"));
    }
    Ok(())
}
