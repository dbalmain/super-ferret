//! Placing a walk's files: untouched scopes' rows kept as they were, then
//! each identity's observations grouped, matched to an inode to reuse or a
//! new one, and given their edges.

use super::*;

impl Final<'_> {
    /// Keeps untouched scopes' rows, returning the kept directories.
    pub(super) fn preserved_files(
        &mut self,
        tree: &Tree<'_>,
        batches: &[Batch],
    ) -> Result<BTreeSet<u32>, Error> {
        let Tree { dirs, tokens } = tree;
        let mut kept = BTreeSet::new();
        for batch in batches {
            for (token, name) in batch.preserved_files() {
                let d = &dirs[tokens[&token]];
                if d.blocked || d.opaque {
                    continue;
                }
                if d.id != token.previous_directory().map(|id| id.0) {
                    return Err(invalid("untouched scope parent changed"));
                }
                self.names.insert(name.0);
                if let Target::Inode(child) = self.old.name(name).target() {
                    self.inodes.insert(child.0);
                    if self.old.is_directory(child) {
                        kept.insert(child.0);
                    }
                }
            }
        }
        Ok(kept)
    }

    /// Groups file observations by identity and places each group's inode and
    /// edges.
    pub(super) fn place_files(
        &mut self,
        tree: &Tree<'_>,
        batches: &[Batch],
        protection: &crate::coverage::Protection,
    ) -> Result<(), Error> {
        let Tree { dirs, tokens } = tree;
        let mut files = self.observed_files(tree, batches, protection);
        self.reuse_files(tree, batches, protection, &mut files)?;
        let file = |loc: File| loc.observation(batches);
        files.sort_unstable_by(|&a, &b| {
            let a = file(a);
            let b = file(b);
            (a.stat.dev, a.stat.ino)
                .cmp(&(b.stat.dev, b.stat.ino))
                .then_with(|| {
                    (dirs[tokens[&a.parent]].id, a.name).cmp(&(dirs[tokens[&b.parent]].id, b.name))
                })
        });
        let same = |a: File, b: File| {
            let (a, b) = (file(a).stat, file(b).stat);
            (a.dev, a.ino) == (b.dev, b.ino)
        };
        for group in files.chunk_by(|&a, &b| same(a, b)) {
            self.place_inode(tree, batches, group)?;
        }
        Ok(())
    }

    /// The batches' file observations, less those under a blocked or opaque
    /// directory or behind a protected name edge.
    fn observed_files(
        &self,
        tree: &Tree<'_>,
        batches: &[Batch],
        protection: &crate::coverage::Protection,
    ) -> Vec<File> {
        let Tree { dirs, tokens } = tree;
        batches
            .iter()
            .enumerate()
            .flat_map(|(b, batch)| {
                (0..batch.file_count()).map(move |f| File::Full(b as u32, f as u32))
            })
            .filter(|&loc| {
                let f = loc.observation(batches);
                let dir = &dirs[tokens[&f.parent]];
                if dir.blocked || dir.opaque {
                    return false;
                }
                if !protection.edges.is_empty()
                    && let Some(id) = dir.id
                    && self.old.is_live_inode(InoId(id))
                    && let Some(name) = self.old.lookup(InoId(id), f.name)
                    && protection.edges.contains(&name.0)
                {
                    return false;
                }
                true
            })
            .collect()
    }

    /// Carries forward each reused file whose directory continues and which is
    /// no alias of an observed file; adds the rest to `files` to place.
    fn reuse_files(
        &mut self,
        tree: &Tree<'_>,
        batches: &[Batch],
        protection: &crate::coverage::Protection,
        files: &mut Vec<File>,
    ) -> Result<(), Error> {
        let Tree { dirs, tokens } = tree;
        // An unmatched observation may be another alias of a compacted row,
        // even with st_nlink == 1 (bind mounts). Expand that residue's
        // equal aliases so conflicts and canonical first-observation
        // selection remain unchanged.
        let mut aliases = Seen::new(self.old.next_inode().0);
        for &loc in files.iter() {
            let st = loc.observation(batches).stat;
            if let Some(id) = self.session.identity((st.dev, st.ino)) {
                aliases.insert(id.0);
            }
        }
        for (b, batch) in batches.iter().enumerate() {
            let mut parent = None;
            let mut continuing = false;
            let mut active = false;
            if files.is_empty()
                && protection.edges.is_empty()
                && batch.reused_parents().all(|token| {
                    let d = &dirs[tokens[&token]];
                    !d.blocked && !d.opaque && d.id == token.previous_directory().map(|id| id.0)
                })
            {
                let (names, inodes) = batch.reused_words();
                self.names.extend(names);
                self.inodes.extend(inodes);
                continue;
            }
            for (token, name) in batch.reused_files() {
                if parent != Some(token) {
                    parent = Some(token);
                    let d = &dirs[tokens[&token]];
                    active = !d.blocked && !d.opaque;
                    continuing = d.id == token.previous_directory().map(|id| id.0);
                }
                if !active || protection.edges.contains(&name.0) {
                    continue;
                }
                let child = self.old.name(name).child;
                if continuing && !aliases.contains(child.0) {
                    self.names.insert(name.0);
                    self.inodes.insert(child.0);
                } else {
                    self.budget.charge(1, std::mem::size_of::<File>())?;
                    files.push(File::Reused(b as u32, name.0));
                }
            }
        }
        Ok(())
    }

    /// Places one identity's inode, its link target, and an edge per name.
    fn place_inode(
        &mut self,
        tree: &Tree<'_>,
        batches: &[Batch],
        group: &[File],
    ) -> Result<(), Error> {
        let Tree { dirs, tokens } = tree;
        let first = group[0].observation(batches);
        let kind = Kind::from_mode(first.stat.mode);
        let id = match self
            .session
            .identity((first.stat.dev, first.stat.ino))
            .filter(|&id| self.old.kind(id) == kind)
        {
            Some(id) => id.0,
            None => allocate(&mut self.changes.counters[0], u32::MAX - 16)?,
        };
        let conflict = group.iter().any(|&loc| {
            let other = loc.observation(batches);
            other.stat != first.stat
                || other.content != first.content
                || other.target != first.target
        });
        self.observe_inode(
            id,
            kind,
            first.stat,
            if conflict {
                Content::Fault
            } else {
                first.content
            },
        )?;
        if kind == Kind::Symlink
            && (!self.old.is_live_inode(InoId(id))
                || self.old.link_target(InoId(id)) != first.target)
        {
            push(
                &self.budget,
                &mut self.changes.records,
                Record::LinkPut {
                    id,
                    target: first.target.unwrap_or_default().to_vec(),
                },
            )?;
        }
        for &loc in group {
            let f = loc.observation(batches);
            let parent = dirs[tokens[&f.parent]]
                .id
                .ok_or_else(|| invalid("collapsed file parent"))?;
            self.edge(parent, id, f.name, f.revealed)?;
        }
        Ok(())
    }
}
