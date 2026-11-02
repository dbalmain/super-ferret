//! Steps 3 and 4 of the numbering: one inode row per `(dev, ino)` among
//! files, then each hashed inode's document.

use super::*;
use crate::batch::Content;

/// One inode row per `(dev, ino)` among files, numbered by first name (D31 C).
///
/// Sorts `(fingerprint, name position, file)` triples, 16 B per file, where a
/// map keyed by `(dev, ino)` measured several times that. Fingerprints that
/// collide are told apart by re-sorting their group on the real identity.
pub(super) fn number_files(
    plan: &mut Plan,
    batches: &[Batch],
    file_pos: Vec<u32>,
) -> Result<(), BuildError> {
    let index = &plan.index;
    let identity = |f: u32| -> (u64, u64) {
        let (b, i) = index.file(f as usize);
        let s: &Stat = &batches[b].file_stats[i];
        (s.dev, s.ino)
    };
    let mut keys: Vec<(u64, u32, u32)> = file_pos
        .iter()
        .enumerate()
        .filter(|&(_, &pos)| pos != NONE)
        .map(|(f, &pos)| {
            let (dev, ino) = identity(f as u32);
            (fingerprint(dev, ino), pos, f as u32)
        })
        .collect();
    drop(file_pos);
    keys.sort_unstable();
    let grouped = group_files(index, batches, keys)?;
    number_groups(plan, grouped)
}

/// Files grouped by identity: each file's group, or `NONE`, and each
/// group's kept observation and whether it is a fault.
struct Grouped {
    group_of: Vec<u32>,
    groups: Vec<(u32, bool)>,
}

/// The observation each group of a file's identity keeps, and whether it is
/// a fault, with each file's group, from `keys` sorted by fingerprint.
fn group_files(
    index: &Index,
    batches: &[Batch],
    mut keys: Vec<(u64, u32, u32)>,
) -> Result<Grouped, BuildError> {
    let identity = |f: u32| -> (u64, u64) {
        let (b, i) = index.file(f as usize);
        let s: &Stat = &batches[b].file_stats[i];
        (s.dev, s.ino)
    };
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
    Ok(Grouped { group_of, groups })
}

/// Numbers the groups by their first name, the names in order again, and
/// records each inode's kept observation and each file's inode.
fn number_groups(plan: &mut Plan, grouped: Grouped) -> Result<(), BuildError> {
    let Grouped {
        mut group_of,
        groups,
    } = grouped;
    let index = &plan.index;
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
            if file >= index.files || group_of[file] == NONE {
                continue;
            }
            let group = group_of[file] as usize;
            if inode_of_group[group] == NONE {
                let id = plan.order.len() + plan.winner.len();
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
        if *group != NONE {
            *group = inode_of_group[*group as usize];
        }
    }
    plan.inode_of_file = group_of;
    Ok(())
}

/// Which of one inode's observations (files, in name order) is kept, and
/// whether it is a fault: the first fresh one if any, else the first carried
/// one; a fault when another observation of the same kind disagrees with it.
fn choose(index: &Index, batches: &[Batch], members: &[u32]) -> (u32, bool) {
    let carried = |f: u32| {
        let (batch, row) = index.file(f as usize);
        batches[batch].file_carried(row)
    };
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
/// Takes each inode's content state on the way, then frees the batches'
/// contents and the fault flags: every hash they held is in `hashed` by then,
/// and the inode rows need only the state.
pub(super) fn assign_docs(
    plan: &mut Plan,
    batches: &mut [Batch],
    known: &[(Hash, u32)],
) -> Result<(), BuildError> {
    // Hashes as big-endian `u64` pairs: the same order as the bytes, which
    // `known` is sorted in, and compared in two instructions rather than a
    // 16-byte `memcmp`.
    let mut hashed: Vec<((u64, u64), u32)> = Vec::new();
    plan.state.reserve_exact(plan.winner.len());
    for (k, &file) in plan.winner.iter().enumerate() {
        let (b, f) = plan.index.file(file as usize);
        let content = batches[b].contents[f];
        plan.state.push(match plan.fault[k] {
            true => ContentState::Fault,
            false => content.state(),
        });
        if let (false, Content::Hashed(hash)) = (plan.fault[k], content) {
            hashed.push((split_hash(&hash), k as u32));
        }
    }
    plan.fault = Vec::new();
    for batch in batches.iter_mut() {
        batch.contents = Vec::new();
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

/// Mixes `(dev, ino)` into 64 bits for sorting. Equal identities always agree;
/// unequal ones that collide are told apart by the caller.
fn fingerprint(dev: u64, ino: u64) -> u64 {
    let mut x = ino ^ dev.rotate_left(29).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 31;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^ (x >> 29)
}
