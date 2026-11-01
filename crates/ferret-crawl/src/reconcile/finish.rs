//! Settling a walk's placements: edges, inodes and documents become the
//! records of the final change set.

use super::*;

impl Final<'_> {
    pub(super) fn finish_edges(&mut self) -> Result<(), Error> {
        self.pending
            .sort_unstable_by(|a, b| (a.parent, &a.name).cmp(&(b.parent, &b.name)));
        if self
            .pending
            .windows(2)
            .any(|e| (e[0].parent, &e[0].name) == (e[1].parent, &e[1].name))
        {
            return Err(invalid("duplicate recrawl name"));
        }
        let mut deleted_by_inode: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        let mut added_by_inode: BTreeMap<u32, usize> = BTreeMap::new();
        for &name in &self.removed {
            if let Target::Inode(child) = self.old.name(NameId(name)).target() {
                deleted_by_inode.entry(child.0).or_default().push(name);
            }
        }
        for edge in &self.pending {
            *added_by_inode.entry(edge.child).or_default() += 1;
        }
        let pending = std::mem::take(&mut self.pending);
        for edge in pending {
            let rename = deleted_by_inode
                .get(&edge.child)
                .filter(|ids| {
                    ids.len() == 1
                        && added_by_inode[&edge.child] == 1
                        && self.inodes.contains(edge.child)
                })
                .map(|ids| ids[0]);
            let id = match rename {
                Some(id) => {
                    self.removed.remove(&id);
                    id
                }
                None => {
                    self.changes.counts[1] += 1;
                    allocate(&mut self.changes.counters[1], u32::MAX)?
                }
            };
            if rename.is_none() && edge.child < u32::MAX - 16 {
                *self.references.entry(edge.child).or_default() += 1;
            }
            push(
                &self.budget,
                &mut self.changes.records,
                Record::NamePut {
                    id,
                    parent: edge.parent,
                    child: edge.child,
                    name: edge.name,
                    revealed: edge.revealed,
                },
            )?;
        }
        for &id in &self.removed {
            if let Target::Inode(child) = self.old.name(NameId(id)).target() {
                *self.references.entry(child.0).or_default() -= 1;
            }
            self.changes.counts[1] -= 1;
            push(
                &self.budget,
                &mut self.changes.records,
                Record::NameDelete { id },
            )?;
        }
        Ok(())
    }
    pub(super) fn finish_inodes(&mut self) -> Result<(), Error> {
        let roots = self.live_roots();
        self.settle_inodes(&roots)?;
        self.move_documents();
        for record in std::mem::take(&mut self.puts).into_values() {
            push(&self.budget, &mut self.changes.records, record)?;
        }
        self.settle_documents()
    }
    /// The roots once this crawl's changes apply.
    fn live_roots(&self) -> BTreeSet<u32> {
        let mut roots: BTreeSet<_> = self.old.roots().map(|(id, _)| id.0).collect();
        for record in &self.changes.records {
            match record {
                Record::RootPut { id, .. } => {
                    roots.insert(*id);
                }
                Record::RootDelete { id } => {
                    roots.remove(id);
                }
                _ => {}
            }
        }
        roots
    }
    /// Deletes each inode this crawl left with no names and no root, and
    /// writes the new name count of each other inode it touched.
    fn settle_inodes(&mut self, roots: &BTreeSet<u32>) -> Result<(), Error> {
        for (&id, &delta) in &self.references {
            let live = self.old.is_live_inode(InoId(id));
            let names = i64::from(self.session.name_references(InoId(id))) + delta;
            let names = u32::try_from(names).map_err(|_| invalid("recrawl name references"))?;
            let root = roots.contains(&id);
            if names == 0 && !root {
                if !live {
                    return Err(invalid("unreachable inode birth"));
                }
                self.puts.remove(&id);
                push(
                    &self.budget,
                    &mut self.changes.records,
                    Record::InodeDelete { id },
                )?;
                self.changes.counts[0] -= 1;
                if self.old.is_directory(InoId(id)) {
                    self.changes.counts[2] -= 1;
                    if self.old.work_tree(InoId(id)).is_some() {
                        push(
                            &self.budget,
                            &mut self.changes.records,
                            Record::WorkTreeDelete { id },
                        )?;
                    }
                } else if self.old.kind(InoId(id)) == Kind::Symlink {
                    push(
                        &self.budget,
                        &mut self.changes.records,
                        Record::LinkDelete { id },
                    )?;
                }
                if let Some(doc) = self.old.doc(InoId(id)) {
                    *self.document_refs.entry(doc.0).or_default() -= 1;
                }
            } else if !live || delta != 0 {
                let kind = self
                    .puts
                    .get(&id)
                    .and_then(|r| {
                        if let Record::InodePut { kind, .. } = r {
                            Some(*kind)
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| self.old.kind(InoId(id)));
                if !live {
                    self.changes.counts[0] += 1;
                    if kind == Kind::Dir {
                        self.changes.counts[2] += 1;
                    }
                }
                push(
                    &self.budget,
                    &mut self.changes.records,
                    Record::LifePut {
                        id,
                        kind,
                        flags: 0,
                        names,
                    },
                )?;
            }
        }
        Ok(())
    }
    /// Moves document references from each inode's old document to the one
    /// its put names.
    fn move_documents(&mut self) {
        for (&id, record) in &self.puts {
            let Record::InodePut { doc, .. } = record else {
                continue;
            };
            let before = self
                .old
                .is_live_inode(InoId(id))
                .then(|| self.old.doc(InoId(id)))
                .flatten()
                .map(|id| id.0);
            if before != *doc {
                if let Some(id) = before {
                    *self.document_refs.entry(id).or_default() -= 1;
                }
                if let Some(id) = doc {
                    *self.document_refs.entry(*id).or_default() += 1;
                }
            }
        }
    }
    /// Deletes each document this crawl left unreferenced, and writes the new
    /// reference count of each other document it touched.
    fn settle_documents(&mut self) -> Result<(), Error> {
        let hashes: BTreeMap<_, _> = self
            .new_docs
            .iter()
            .map(|(&hash, &id)| (id, hash))
            .collect();
        for (&id, &delta) in &self.document_refs {
            if delta == 0 {
                continue;
            }
            let old = self.old.doc_references(DocId(id));
            let refs = i64::from(old.unwrap_or(0)) + delta;
            let refs = u32::try_from(refs).map_err(|_| invalid("recrawl document references"))?;
            if refs == 0 {
                self.changes.counts[3] -= 1;
                push(
                    &self.budget,
                    &mut self.changes.records,
                    Record::DocDelete { id },
                )?;
            } else {
                let hash = self
                    .old
                    .doc_hash(DocId(id))
                    .or_else(|| hashes.get(&id).copied())
                    .ok_or_else(|| invalid("recrawl document hash"))?;
                if old.is_none() {
                    self.changes.counts[3] += 1;
                }
                push(
                    &self.budget,
                    &mut self.changes.records,
                    Record::DocPut {
                        id,
                        references: refs,
                        hash,
                    },
                )?;
            }
        }
        Ok(())
    }
}
