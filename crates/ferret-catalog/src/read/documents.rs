//! The catalog's work trees and its documents' content hashes.

use super::*;

impl Catalog {
    /// Every work tree whose top is at or below a root, by top directory.
    /// Needs [`Section::WorkTrees`].
    pub fn work_trees(&self) -> impl Iterator<Item = WorkTree<'_>> + '_ {
        let base = self
            .section(Section::WorkTrees)
            .chunks_exact(WORK_TREE_ROW)
            .map(|row| self.work_tree_row(row))
            .filter(|w| {
                self.is_live_inode(w.dir)
                    && self
                        .family_record(Family::Aux, overlay::WORKTREE, w.dir.0)
                        .is_none()
            });
        let delta = self
            .overlay
            .iter()
            .flat_map(|o| o.projection(Family::Aux).records(overlay::WORKTREE))
            .filter_map(|r| match r {
                Record::WorkTreePut {
                    id,
                    kind,
                    common_id,
                    path,
                } => Some(WorkTree {
                    dir: InoId(*id),
                    kind: *kind,
                    common_id: *common_id,
                    common_dir: path,
                }),
                _ => None,
            });
        base.chain(delta)
    }

    /// The work tree whose top is `dir`, if it is one. Needs
    /// [`Section::WorkTrees`].
    pub fn work_tree(&self, dir: InoId) -> Option<WorkTree<'_>> {
        match self.family_record(Family::Aux, overlay::WORKTREE, dir.0) {
            Some(Record::WorkTreePut {
                id,
                kind,
                common_id,
                path,
            }) => {
                return Some(WorkTree {
                    dir: InoId(*id),
                    kind: *kind,
                    common_id: *common_id,
                    common_dir: path,
                });
            }
            Some(Record::WorkTreeDelete { .. }) => return None,
            _ => {}
        }
        let rows = self.section(Section::WorkTrees);
        let n = rows.len() / WORK_TREE_ROW;
        let at = partition_point(n, |i| u32_at(rows, i * WORK_TREE_ROW) < dir.0);
        (at < n)
            .then(|| self.work_tree_row(&rows[at * WORK_TREE_ROW..][..WORK_TREE_ROW]))
            .filter(|wt| wt.dir == dir)
    }

    fn work_tree_row(&self, row: &[u8]) -> WorkTree<'_> {
        WorkTree {
            dir: InoId(u32_at(row, 0)),
            // Decoding checked the kind byte.
            kind: WorkTreeKind::from_byte(row[24]).unwrap_or(WorkTreeKind::Main),
            common_id: (u64_at(row, 8), u64_at(row, 16)),
            common_dir: self.string(u32_at(row, 4)),
        }
    }

    /// Writer lookup ordinals address immutable base rows, including deaths.
    pub(crate) fn base_doc_count(&self) -> u32 {
        self.layout.docs as u32
    }
    pub(crate) fn base_doc(&self, row: u32) -> DocId {
        DocId(self.column(Column::DocId).sequence(row as usize) as u32)
    }
    pub(crate) fn base_hash(&self, row: u32) -> Hash {
        self.hash(row as usize)
    }
    /// Document rows the selected overlay replaced since the checkpoint,
    /// each with whether it is live in this view. Bounded by the overlay's
    /// document rows; the checkpoint's table is not read. Requires
    /// [`Section::Docs`].
    pub fn changed_docs(&self) -> impl Iterator<Item = (DocId, bool)> + '_ {
        self.overlay
            .iter()
            .flat_map(|o| o.projection(Family::Docs).records(overlay::DOC))
            .filter_map(|record| match record {
                Record::DocPut { id, .. } => Some((DocId(*id), true)),
                Record::DocDelete { id } => Some((DocId(*id), false)),
                _ => None,
            })
    }
    pub(crate) fn changed_doc_records(&self) -> impl Iterator<Item = &Record> {
        self.overlay
            .iter()
            .flat_map(|o| o.projection(Family::Docs).records(overlay::DOC))
    }
    pub(crate) fn base_doc_hash(&self, doc: DocId) -> Option<Hash> {
        self.column(Column::DocId)
            .sequence_row(u64::from(doc.0), self.layout.docs)
            .map(|row| self.hash(row))
    }

    /// Every live document with its hash: base then replacements, each by id.
    /// Needs [`Section::Docs`].
    pub fn docs(&self) -> impl Iterator<Item = (DocId, Hash)> + '_ {
        let ids = self.column(Column::DocId);
        let base = (0..self.layout.docs)
            .map(move |row| (DocId(ids.sequence(row) as u32), self.hash(row)))
            .filter(|(id, _)| {
                self.family_record(Family::Docs, overlay::DOC, id.0)
                    .is_none()
            });
        let delta = self
            .overlay
            .iter()
            .flat_map(|o| o.projection(Family::Docs).records(overlay::DOC))
            .filter_map(|r| match r {
                Record::DocPut { id, hash, .. } => Some((DocId(*id), *hash)),
                _ => None,
            });
        base.chain(delta)
    }

    /// Calls `f` with each live document id [`Catalog::docs`] yields, in its
    /// order, asking `stop` before every base row and overlay record read,
    /// dead and superseded ones included, so a caller can bound the work
    /// between asks. False once `stop` answers true, the scan unfinished.
    /// Needs [`Section::Docs`].
    pub fn for_each_doc_until(
        &self,
        mut stop: impl FnMut() -> bool,
        mut f: impl FnMut(DocId),
    ) -> bool {
        let ids = self.column(Column::DocId);
        for row in 0..self.layout.docs {
            if stop() {
                return false;
            }
            let id = DocId(ids.sequence(row) as u32);
            if self
                .family_record(Family::Docs, overlay::DOC, id.0)
                .is_none()
            {
                f(id);
            }
        }
        if let Some(o) = &self.overlay {
            let Some(records) = o
                .projection(Family::Docs)
                .records_until(overlay::DOC, &mut stop)
            else {
                return false;
            };
            for r in records {
                if stop() {
                    return false;
                }
                if let Record::DocPut { id, .. } = r {
                    f(DocId(*id));
                }
            }
        }
        true
    }

    /// A live document's hash; `None` if the id is dead or never assigned.
    /// Needs [`Section::Docs`]. Where the generation's ids have no holes, the
    /// row is the id less the first; otherwise a binary search.
    pub fn doc_hash(&self, doc: DocId) -> Option<Hash> {
        match self.family_record(Family::Docs, overlay::DOC, doc.0) {
            Some(Record::DocPut { hash, .. }) => return Some(*hash),
            Some(Record::DocDelete { .. }) => return None,
            _ => {}
        }
        self.base_doc_hash(doc)
    }

    /// Document row `row`'s hash.
    fn hash(&self, row: usize) -> Hash {
        let at = self.layout.hashes + row * HASH_ROW;
        let mut hash = [0; HASH_ROW];
        hash.copy_from_slice(&self.section(Section::Docs)[at..at + HASH_ROW]);
        hash
    }
}
