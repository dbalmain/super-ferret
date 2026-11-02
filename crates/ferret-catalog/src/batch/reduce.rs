//! Reducing a batch's file observations against its pinned view: equal
//! rows become seen bits and reused names, changed rows stay complete, and
//! a checkpoint fallback materializes the reused rows again.

use super::*;

/// Whether `old`'s row `id`, named by `edge`, proves the observed `file`
/// unchanged: the same non-directory inode, kind, content state and hash,
/// link target and `!` flag.
fn unchanged(
    old: &Catalog,
    id: NameId,
    edge: crate::Name<'_>,
    file: &PendingFile,
    target: Option<&[u8]>,
) -> bool {
    let crate::Target::Inode(child) = edge.target() else {
        return false;
    };
    if old.is_directory(child) {
        return false;
    }
    let inode = old.inode(child);
    let kind = old.kind(child);
    inode.stat == file.stat
        && kind == Kind::from_mode(file.stat.mode)
        && inode.state == file.content.state()
        && match file.content {
            Content::Hashed(hash) => inode.doc.and_then(|doc| old.doc_hash(doc)) == Some(hash),
            _ => true,
        }
        && if kind == Kind::Symlink {
            old.link_target(child) == target
        } else {
            target.is_none()
        }
        // A changed `!` rule changes only the flag (D127).
        && old.is_revealed(id) == file.revealed
}

impl Batch {
    pub(super) fn observe_file(
        &mut self,
        parent: DirToken,
        name: &[u8],
        stat: Stat,
        content: Content,
        target: Option<&[u8]>,
        revealed: bool,
    ) {
        if !self.full && self.previous.is_some() && parent.old != NONE {
            if self.pending.first().is_some_and(|f| f.parent != parent) {
                self.finish_observations();
                self.chunked = false;
            }
            if self.pending.len() >= OBSERVATION_ROWS
                || self.pending_bytes.len() + name.len() + target.map_or(0, <[u8]>::len)
                    > OBSERVATION_BYTES
            {
                self.finish_observations();
                self.chunked = true;
            }
            let name = push(&mut self.pending_bytes, name, &mut self.overflow);
            let target =
                target.map(|target| push(&mut self.pending_bytes, target, &mut self.overflow));
            self.pending.push(PendingFile {
                parent,
                name,
                stat,
                content,
                target,
                revealed,
            });
            self.pending_peak = self.pending_peak.max(self.pending.len());
            self.pending_bytes_peak = self.pending_bytes_peak.max(self.pending_bytes.len());
        } else {
            self.push_file(parent, name, stat, content, target, revealed);
        }
    }

    /// Reduces a completed local listing against sorted old children. Equal
    /// rows retain seen bits and a checked parent hint. Relevant aliases are
    /// reconstructed for final-set reconciliation; changed rows stay complete.
    /// Unflushed observations remain visible through the borrowed accessors.
    pub fn finish_observations(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let mut pending = std::mem::take(&mut self.pending);
        let mut bytes = std::mem::take(&mut self.pending_bytes);
        pending.sort_unstable_by(|a, b| a.name.of(&bytes).cmp(b.name.of(&bytes)));
        let old = self.previous.clone();
        if let Some(old) = old {
            let names = old.name_reader();
            let dir = InoId(pending[0].parent.old);
            // Merging walks the directory's children from the first; over an
            // overlay, `children` also sorts all of them. A few files of a
            // large directory, which a walk delivers in many small groups,
            // are looked up instead. `/nix/store` holds 62K files in one
            // directory: merging each group was quadratic (D116).
            let lookup = self.chunked
                || old
                    .entry_count(dir)
                    .is_some_and(|count| count as usize / 8 > pending.len());
            let mut children = (!lookup)
                .then(|| old.children(dir))
                .into_iter()
                .flatten()
                .map(|id| (id, names.get(id)))
                .peekable();
            // Duplicate observations cannot become a single equal-row proof.
            let duplicate = pending
                .windows(2)
                .any(|p| p[0].name.of(&bytes) == p[1].name.of(&bytes));
            for file in &pending {
                let name = file.name.of(&bytes);
                let target = file.target.map(|span| span.of(&bytes));
                let matched = if lookup {
                    old.lookup(InoId(file.parent.old), name)
                        .map(|id| (id, names.get(id)))
                } else {
                    while children.peek().is_some_and(|(_, edge)| edge.bytes < name) {
                        children.next();
                    }
                    children
                        .peek()
                        .copied()
                        .filter(|(_, edge)| edge.bytes == name)
                };
                let reusable = !duplicate
                    && matched.is_some_and(|(id, edge)| unchanged(&old, id, edge, file, target));
                if reusable {
                    if let Some((id, edge)) = matched {
                        self.reused.insert(file.parent, id, Some(edge.child));
                    }
                } else {
                    self.push_file(
                        file.parent,
                        name,
                        file.stat,
                        file.content,
                        target,
                        file.revealed,
                    );
                }
            }
        }
        pending.clear();
        bytes.clear();
        self.pending = pending;
        self.pending_bytes = bytes;
    }

    /// Seals a worker's output and releases its temporary buffers. Metrics
    /// survive; equal rows have already become seen bits and changed rows
    /// remain available to the final reconciler.
    pub fn seal(&mut self) {
        self.finish_observations();
        self.pending = Vec::new();
        self.pending_bytes = Vec::new();
    }

    pub(crate) fn materialize(&mut self) {
        self.finish_observations();
        let old = self.previous.take();
        if let Some(old) = old {
            let preserved = std::mem::take(&mut self.preserved);
            for id in preserved.ids() {
                let edge = old.name(id);
                let parent = preserved.parents[&edge.parent].0;
                if let crate::Target::Ignored(kind) = edge.target() {
                    self.ignored(parent, edge.bytes, kind);
                }
            }
            let reused = std::mem::take(&mut self.reused);
            for id in reused.ids() {
                let edge = old.name(id);
                let parent = reused.parents[&edge.parent].0;
                let child = edge.child;
                self.push_file(
                    parent,
                    edge.bytes,
                    old.inode(child).stat,
                    content(&old, child),
                    old.link_target(child),
                    old.is_revealed(id),
                );
            }
        }
    }
}
