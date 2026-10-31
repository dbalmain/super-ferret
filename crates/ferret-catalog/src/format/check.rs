//! Each section's structural check, run once when a reader first loads it.

use super::*;

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
    let terminated = |heap: &[u8]| heap.last().is_none_or(|&b| b == 0);
    let strings_len = l.len(Section::Strings);
    for column in COLUMNS {
        let coding = column.coding();
        if column.section() == section && coding.is_blocked() {
            let desc = l.columns[column as usize].desc;
            let nullable = coding == Coding::NullableBlocked;
            if !l
                .blocked(column, get(section))
                .check(desc.width, desc.base, nullable)
            {
                return Err(DecodeError::Corrupt(section.label()));
            }
        }
    }
    match section {
        Section::NameHeap => {
            if !terminated(get(Section::NameHeap)) {
                return Err(DecodeError::Corrupt("name heap"));
            }
        }
        Section::Strings => {
            if !terminated(get(Section::Strings)) {
                return Err(DecodeError::Corrupt("strings"));
            }
        }
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
        Section::Roots => {
            let roots = get(Section::Roots);
            let dir_names = l.view(Column::DirName, get(Section::DirNames));
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
        }
        Section::Specials => {
            let mut last = None;
            for pair in get(Section::Specials).chunks_exact(PAIR_ROW) {
                let (ino, kind) = (u32_at(pair, 0), u32_at(pair, 4));
                if !(l.dirs..l.inodes).contains(&(ino as usize))
                    || last.is_some_and(|last| ino <= last)
                    || !(3..=6).contains(&kind)
                {
                    return Err(DecodeError::Corrupt("specials"));
                }
                last = Some(ino);
            }
        }
        Section::Links => {
            let specials = get(Section::Specials);
            let mut at = 0;
            let mut last = None;
            for pair in get(Section::Links).chunks_exact(PAIR_ROW) {
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
        }
        Section::WorkTrees => {
            let mut last = None;
            for row in get(Section::WorkTrees).chunks_exact(WORK_TREE_ROW) {
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
        }
        Section::Docs => {
            // Each id as the reader decodes it: sorted, and assigned.
            let ids = l.view(Column::DocId, get(Section::Docs));
            let mut last = None;
            for (row, id) in ids.values(l.docs).enumerate() {
                let id = id.wrapping_add(row as u64);
                if id >= u64::from(l.next_doc) || last.is_some_and(|last| id <= last) {
                    return Err(DecodeError::Corrupt("docs"));
                }
                last = Some(id);
            }
        }
        Section::DocRefs => {
            let counts = document_references(l, get(Section::Docs), get(Section::Doc))?;
            for (row, count) in counts.into_iter().enumerate() {
                if count == 0 || count != u32_at(get(section), row * 4) {
                    return Err(DecodeError::Corrupt("document references"));
                }
            }
        }
        Section::RetainedAt => {
            let retained = l.blocked(Column::RetainedAt, get(section));
            if (0..l.dirs).any(|row| {
                retained
                    .nullable(row)
                    .is_some_and(|n| n > l.generation.sequence)
            }) {
                return Err(DecodeError::Corrupt("retained at"));
            }
        }
        Section::Policy => {}
        Section::Revealed => {
            let mut last = None;
            for row in get(section).chunks_exact(4) {
                let id = u32_at(row, 0);
                if id as usize >= l.names || last.is_some_and(|last| id <= last) {
                    return Err(DecodeError::Corrupt("revealed"));
                }
                last = Some(id);
            }
        }
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
    for row in 0..l.inodes {
        let Some(id) = bindings.nullable(row) else {
            continue;
        };
        let at = ids
            .sequence_row(id, l.docs)
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
fn check_names(l: &Layout, rows: &[u8], heap: &[u8]) -> Result<usize, DecodeError> {
    if count_nuls(heap) != l.names {
        return Err(DecodeError::Corrupt("name order"));
    }
    let parents = l.blocked(Column::NameParent, rows);
    let children = l.blocked(Column::NameChild, rows);
    let offsets = l.blocked(Column::NameOffset, rows);
    // The name before this row: its parent and where its bytes start; and
    // the one before that, for sibling order.
    let mut open: Option<(u64, usize)> = None;
    let mut previous: Option<(u64, &[u8])> = None;
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
    let (mut last_parent, mut next_offset, mut dir_children) = (0, 0, 0);
    let mut block = [[0; RUN]; 3];
    for first in (0..l.names).step_by(RUN) {
        let n = RUN.min(l.names - first);
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
    close(heap.len(), open)?;
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
