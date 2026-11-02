//! Steps 1 and 2 of the numbering: roots take the first directory ids, then
//! directories breadth first, each one's children sorted into name rows.

use super::*;

/// Numbers the sorted `roots` first, in path order, storing their paths.
pub(super) fn number_roots(plan: &mut Plan, roots: Roots<'_>) -> Result<(), BuildError> {
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
pub(super) fn take_entry_counts(
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

/// Each root's `(path, directory)`.
type Roots<'a> = Vec<(&'a [u8], usize)>;

/// The roots, and each entry's parent directory, NONE for a root.
pub(super) fn parents<'a>(
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
pub(super) fn group_by_parent(parent_of: &[u32], dirs: usize) -> (Vec<u32>, Vec<u32>) {
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
pub(super) fn visible_dirs(
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
pub(super) fn number_names(
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
