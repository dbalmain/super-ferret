//! Placing a walk's directories: breadth-first from the observed roots,
//! reusing continuing identities, then sweeping what the walk covered but
//! did not see, and recording directory and work-tree rows.

use std::collections::VecDeque;
use std::os::unix::ffi::OsStrExt;

use super::*;

impl Final<'_> {
    /// Places every directory breadth-first from the observed roots, reusing
    /// old IDs where the identity continues.
    pub(super) fn place_directories(
        &mut self,
        tree: &mut Tree<'_>,
        refreshed: &[PathBuf],
        protection: &crate::coverage::Protection,
    ) -> Result<(), Error> {
        let Tree { dirs, tokens } = tree;
        let mut roots: Vec<_> = dirs
            .iter()
            .enumerate()
            .filter(|(_, d)| d.observation.parent.is_none())
            .map(|(i, _)| i)
            .collect();
        roots.sort_unstable_by_key(|&i| dirs[i].observation.name);
        let observed_roots: Vec<_> = roots.iter().map(|&i| dirs[i].observation.name).collect();
        self.check_roots(&observed_roots, refreshed, protection)?;
        let mut queue: VecDeque<_> = roots.into();
        let mut visited = 0;
        while let Some(i) = queue.pop_front() {
            visited += 1;
            let mut children = std::mem::take(&mut dirs[i].children);
            children.sort_unstable_by_key(|&c| dirs[c].observation.name);
            queue.extend(children);
            if dirs[i].blocked {
                continue;
            }
            let d = dirs[i].observation;
            let parent = d.parent.map(|token| tokens[&token]);
            let parent_id = parent.and_then(|p| dirs[p].id);
            if parent.is_some() && parent_id.is_none() {
                continue;
            }
            if !dirs[i].included && (!dirs[i].opaque || dirs[i].denied) {
                if let Some(parent) = parent_id {
                    self.edge(parent, ignored(Kind::Dir), d.name, false)?;
                }
                continue;
            }
            let root = parent.map_or_else(
                || {
                    self.old
                        .roots()
                        .find(|(_, path)| *path == d.name)
                        .map(|(id, _)| id.0)
                },
                |p| Some(dirs[p].root),
            );
            let hinted = d
                .token
                .previous_directory()
                .filter(|&id| !self.inodes.contains(id.0));
            let continuing = hinted
                .or_else(|| self.continuing_directory(&d, parent_id, root))
                .filter(|id| !self.inodes.contains(id.0));
            let id = match continuing {
                Some(id) => id.0,
                None => allocate(&mut self.changes.counters[0], u32::MAX - 16)?,
            };
            dirs[i].id = Some(id);
            dirs[i].root = parent.map_or(id, |p| dirs[p].root);
            self.observe_inode(id, Kind::Dir, *d.stat, Content::Unindexed)?;
            self.name_directory(&d, id, parent_id, hinted.is_some(), continuing.is_some())?;
        }
        // A disconnected/cyclic token graph cannot reach publication.
        if visited != dirs.len() {
            return Err(invalid("unreachable directory observation"));
        }
        Ok(())
    }

    /// Fails unless the observed roots are the refreshed roots, less the
    /// protected ones the crawl did not observe.
    fn check_roots(
        &self,
        observed_roots: &[&[u8]],
        refreshed: &[PathBuf],
        protection: &crate::coverage::Protection,
    ) -> Result<(), Error> {
        if observed_roots
            != refreshed
                .iter()
                .filter(|p| {
                    !self.old.roots().any(|(id, path)| {
                        path == p.as_os_str().as_bytes()
                            && protection.directories.contains(&id.0)
                            && !observed_roots.contains(&path)
                    })
                })
                .map(|p| p.as_os_str().as_bytes())
                .collect::<Vec<_>>()
        {
            return Err(invalid("recrawl root observations"));
        }
        Ok(())
    }

    /// The old directory `d` continues, when its token gives no hint: the
    /// same identity under the same name in its parent, or anywhere under
    /// its root; for a root, the root itself.
    fn continuing_directory(
        &self,
        d: &DirectoryObservation<'_>,
        parent_id: Option<u32>,
        root: Option<u32>,
    ) -> Option<InoId> {
        let identity = (d.stat.dev, d.stat.ino);
        let Some(parent) = parent_id else {
            return root
                .map(InoId)
                .filter(|&id| self.old.identity(id) == identity);
        };
        self.old
            .is_live_inode(InoId(parent))
            .then(|| self.old.lookup(InoId(parent), d.name))
            .flatten()
            .map(|id| self.old.name(id).child)
            .filter(|&id| {
                self.old.is_live_inode(id)
                    && self.old.is_directory(id)
                    && self.old.identity(id) == identity
            })
            .or_else(|| root.and_then(|r| self.session.directory_identity(InoId(r), identity)))
    }

    /// Names directory `id` in its parent, keeping a hinted directory's
    /// unchanged name, or puts it as a new root.
    fn name_directory(
        &mut self,
        d: &DirectoryObservation<'_>,
        id: u32,
        parent_id: Option<u32>,
        hinted: bool,
        continuing: bool,
    ) -> Result<(), Error> {
        if let Some(parent) = parent_id {
            let same_edge = hinted
                .then(|| self.old.dir_name(InoId(id)))
                .flatten()
                .filter(|&name| {
                    let edge = self.old.name(name);
                    edge.parent.0 == parent && edge.bytes == d.name
                });
            if let Some(name) = same_edge {
                self.keep(name, d.revealed)?;
            } else {
                self.edge(parent, id, d.name, d.revealed)?;
            }
        } else if !continuing {
            push(
                &self.budget,
                &mut self.changes.records,
                Record::RootPut {
                    id,
                    path: d.name.to_vec(),
                },
            )?;
        }
        Ok(())
    }

    pub(super) fn ignored_entries(
        &mut self,
        tree: &Tree<'_>,
        batches: &[Batch],
        protection: &crate::coverage::Protection,
    ) -> Result<(), Error> {
        let Tree { dirs, tokens } = tree;
        for batch in batches {
            for (parent, name, kind) in batch.ignored_entries() {
                let d = &dirs[tokens[&parent]];
                if d.blocked || d.opaque {
                    continue;
                }
                if !protection.edges.is_empty()
                    && let Some(id) = d.id
                    && self.old.is_live_inode(InoId(id))
                    && let Some(edge) = self.old.lookup(InoId(id), name)
                    && protection.edges.contains(&edge.0)
                {
                    continue;
                }
                if let Some(parent) = dirs[tokens[&parent]].id {
                    self.edge(parent, ignored(kind), name, false)?;
                }
            }
        }
        Ok(())
    }

    /// Removes the refreshed and dropped roots' names that were not seen again.
    pub(super) fn sweep(
        &mut self,
        refreshed: &[PathBuf],
        dropped: &[PathBuf],
        protection: &crate::coverage::Protection,
        kept: &BTreeSet<u32>,
    ) -> Result<(), Error> {
        let swept: Vec<_> = self
            .old
            .roots()
            .filter(|(_, path)| {
                refreshed
                    .iter()
                    .chain(dropped)
                    .any(|p| p.as_os_str().as_bytes() == *path)
            })
            .map(|(id, _)| id)
            .collect();
        for root in swept {
            let mut queue = vec![root];
            while let Some(dir) = queue.pop() {
                if protection.directories.contains(&dir.0) || kept.contains(&dir.0) {
                    continue;
                }
                for name in self.old.children(dir) {
                    let edge = self.old.name(name);
                    if let Target::Inode(child) = edge.target()
                        && self.old.is_directory(child)
                    {
                        queue.push(child);
                    }
                    if !self.names.contains(name.0) {
                        self.budget.charge(1, 48)?;
                        self.removed.insert(name.0);
                    }
                }
            }
            if dropped.iter().any(|p| {
                self.old
                    .roots()
                    .any(|(id, path)| id == root && path == p.as_os_str().as_bytes())
            }) || !self.inodes.contains(root.0)
            {
                push(
                    &self.budget,
                    &mut self.changes.records,
                    Record::RootDelete { id: root.0 },
                )?;
                self.references.entry(root.0).or_default();
            }
        }
        Ok(())
    }

    /// Puts each placed directory's row where it changed, and clears stale
    /// markers.
    pub(super) fn directory_records(
        &mut self,
        tree: &Tree<'_>,
        protection: &crate::coverage::Protection,
    ) -> Result<(), Error> {
        let dirs = &tree.dirs;
        let own_names: BTreeMap<_, _> = self
            .changes
            .records
            .iter()
            .filter_map(|r| match r {
                Record::NamePut { id, child, .. } => Some((*child, *id)),
                _ => None,
            })
            .collect();
        let mut marked = BTreeSet::new();
        for d in dirs {
            let Some(id) = d.id else {
                continue;
            };
            let stale = protection.markers.contains(&id);
            if stale {
                marked.insert(id);
            }
            if let Some(record) = self.directory_record(id, d, &own_names, stale) {
                push(&self.budget, &mut self.changes.records, record)?;
            }
        }
        for &id in protection.markers.difference(&marked) {
            if let Some(record) = self.cleared_marker(InoId(id)) {
                push(&self.budget, &mut self.changes.records, record)?;
            }
        }
        Ok(())
    }

    /// The row for placed directory `d`, or None when the old catalog already
    /// has it. A `stale` marker drops its entry count.
    fn directory_record(
        &self,
        id: u32,
        d: &Directory<'_>,
        own_names: &BTreeMap<u32, u32>,
        stale: bool,
    ) -> Option<Record> {
        let name = if d.observation.parent.is_some() {
            own_names.get(&id).copied().or_else(|| {
                self.old
                    .is_live_inode(InoId(id))
                    .then(|| self.old.dir_name(InoId(id)))
                    .flatten()
                    .map(|n| n.0)
            })
        } else {
            None
        };
        let retained_at = stale.then(|| self.retained_sequence(InoId(id))).flatten();
        let entries = if stale || d.opaque { None } else { d.entries };
        let flags = if retained_at.is_some() {
            8
        } else if stale || d.opaque {
            0
        } else {
            4
        } | if d.observation.traversed { 3 } else { 0 };
        let unchanged = self.old.is_live_inode(InoId(id))
            && self.old.dir_name(InoId(id)).map(|n| n.0) == name
            && self.old.entry_count(InoId(id)) == entries
            && self.old.is_traversed(InoId(id)) == d.observation.traversed
            && self.old.retained_at(InoId(id)) == retained_at;
        (!unchanged).then_some(Record::DirPut {
            id,
            name,
            entries,
            flags,
            retained_at,
        })
    }

    /// The row clearing the entry count of `dir`, a stale marker this run
    /// did not place, or None when the old catalog already has it.
    fn cleared_marker(&self, dir: InoId) -> Option<Record> {
        let retained = self.retained_sequence(dir);
        (self.old.entry_count(dir).is_some() || self.old.retained_at(dir) != retained).then(|| {
            Record::DirPut {
                id: dir.0,
                name: self.old.dir_name(dir).map(|n| n.0),
                entries: None,
                flags: if retained.is_some() { 8 } else { 0 }
                    | if self.old.is_traversed(dir) { 3 } else { 0 },
                retained_at: retained,
            }
        })
    }

    pub(super) fn work_trees(&mut self, tree: &Tree<'_>, batches: &[Batch]) -> Result<(), Error> {
        let Tree { dirs, tokens } = tree;
        let work_trees: BTreeMap<_, _> = batches
            .iter()
            .flat_map(Batch::work_tree_observations)
            .filter_map(|(token, kind, path, identity)| {
                if dirs[tokens[&token]].blocked || dirs[tokens[&token]].opaque {
                    return None;
                }
                dirs[tokens[&token]]
                    .id
                    .map(|id| (id, (kind, path, identity)))
            })
            .collect();
        for d in dirs {
            let Some(id) = d.id else {
                continue;
            };
            let previous = self
                .old
                .is_live_inode(InoId(id))
                .then(|| self.old.work_tree(InoId(id)))
                .flatten();
            match (previous, work_trees.get(&id)) {
                (old, Some(&(kind, path, common_id)))
                    if old.is_none_or(|old| {
                        old.kind != kind || old.common_dir != path || old.common_id != common_id
                    }) =>
                {
                    push(
                        &self.budget,
                        &mut self.changes.records,
                        Record::WorkTreePut {
                            id,
                            kind,
                            common_id,
                            path: path.to_vec(),
                        },
                    )?;
                }
                (Some(_), None) => push(
                    &self.budget,
                    &mut self.changes.records,
                    Record::WorkTreeDelete { id },
                )?,
                _ => {}
            }
        }
        Ok(())
    }
}
