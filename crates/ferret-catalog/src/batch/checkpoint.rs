//! Narrowing a full checkpoint batch to the directories a checkpoint keeps:
//! marking its coverage, remapping the kept tokens densely, then retaining
//! their rows and edges.

use super::*;

impl Batch {
    /// Dense token remapping for a subset of a full checkpoint batch. All
    /// batches' mappings must be combined before retaining cross-worker edges.
    pub fn checkpoint_tokens<'a>(
        &'a self,
        mut keep: impl FnMut(DirToken) -> bool + 'a,
    ) -> impl Iterator<Item = (DirToken, DirToken)> + 'a {
        self.directories()
            .filter(move |d| keep(d.token))
            .enumerate()
            .map(|(index, d)| {
                (
                    d.token,
                    DirToken {
                        index: index as u32,
                        ..d.token
                    },
                )
            })
    }

    /// Prunes full checkpoint columns in place and remaps surviving parents.
    /// The map must contain every retained directory across all worker batches.
    /// `keep_edge` receives original parent tokens and borrowed basenames.
    /// No file/name/stat column is copied into another batch.
    pub fn retain_checkpoint(
        &mut self,
        mapping: &std::collections::BTreeMap<DirToken, DirToken>,
        keep_edge: impl Fn(DirToken, &[u8]) -> bool,
    ) {
        assert!(self.full, "checkpoint pruning requires full observations");
        self.seal();
        self.retain_dirs(mapping);
        self.retain_files(mapping, &keep_edge);
        self.retain_edges(mapping, &keep_edge);
    }

    /// Keeps the mapped directories and their stats, remapping parents.
    fn retain_dirs(&mut self, mapping: &std::collections::BTreeMap<DirToken, DirToken>) {
        let id = self.id;
        let mut read = 0;
        let mut write = 0;
        self.dirs.retain_mut(|dir| {
            let token = DirToken {
                batch: id,
                index: read as u32,
                old: dir.old,
            };
            let index = read;
            read += 1;
            if !mapping.contains_key(&token) {
                return false;
            }
            dir.parent = dir.parent.map(|parent| mapping[&parent]);
            self.dir_stats[write] = self.dir_stats[index];
            write += 1;
            true
        });
        self.dir_stats.truncate(write);
    }

    /// Keeps the files under mapped directories whose edges `keep_edge`
    /// accepts, with their stats, contents, targets and retained and
    /// revealed marks renumbered to their new rows.
    fn retain_files(
        &mut self,
        mapping: &std::collections::BTreeMap<DirToken, DirToken>,
        keep_edge: &impl Fn(DirToken, &[u8]) -> bool,
    ) {
        let retained = std::mem::take(&mut self.retained_files);
        let revealed = std::mem::take(&mut self.revealed_files);
        let mut revealed = revealed.into_iter().peekable();
        let mut read = 0;
        let mut write = 0;
        let mut target_read = 0;
        let mut target_write = 0;
        self.files.retain_mut(|file| {
            let index = read;
            read += 1;
            let target = self
                .targets
                .get(target_read)
                .filter(|&&(i, _)| i as usize == index)
                .copied();
            if target.is_some() {
                target_read += 1;
            }
            let was_revealed = revealed.next_if_eq(&(index as u32)).is_some();
            let Some(&parent) = mapping.get(&file.parent) else {
                return false;
            };
            if !keep_edge(file.parent, file.name.of(&self.names)) {
                return false;
            }
            file.parent = parent;
            self.file_stats[write] = self.file_stats[index];
            self.contents[write] = self.contents[index];
            if let Some((_, span)) = target {
                self.targets[target_write] = (write as u32, span);
                target_write += 1;
            }
            if retained
                .get(index / 64)
                .is_some_and(|word| word & (1 << (index % 64)) != 0)
            {
                self.retained_files
                    .resize(self.retained_files.len().max(write / 64 + 1), 0);
                self.retained_files[write / 64] |= 1 << (write % 64);
            }
            if was_revealed {
                self.revealed_files.push(write as u32);
            }
            write += 1;
            true
        });
        self.file_stats.truncate(write);
        self.contents.truncate(write);
        self.targets.truncate(target_write);
    }

    /// Keeps the ignored edges, work trees and entry counts under mapped
    /// directories, remapped.
    fn retain_edges(
        &mut self,
        mapping: &std::collections::BTreeMap<DirToken, DirToken>,
        keep_edge: &impl Fn(DirToken, &[u8]) -> bool,
    ) {
        self.ignored.retain_mut(|edge| {
            let Some(&parent) = mapping.get(&edge.parent) else {
                return false;
            };
            if !keep_edge(edge.parent, edge.name.of(&self.names)) {
                return false;
            }
            edge.parent = parent;
            true
        });
        self.work_trees.retain_mut(|work| {
            let Some(&dir) = mapping.get(&work.dir) else {
                return false;
            };
            if !keep_edge(work.dir, b"") {
                return false;
            }
            work.dir = dir;
            true
        });
        self.entry_counts.retain_mut(|(token, _)| {
            let Some(&mapped) = mapping.get(token) else {
                return false;
            };
            *token = mapped;
            true
        });
    }

    /// Sets unknown counts and retention markers for original directory tokens.
    /// None represents covered opacity; absent tokens keep their observations.
    /// Apply this before token remapping.
    pub fn checkpoint_coverage(
        &mut self,
        markers: &std::collections::BTreeMap<DirToken, Option<u64>>,
    ) {
        for (index, dir) in self.dirs.iter_mut().enumerate() {
            let token = DirToken {
                batch: self.id,
                index: index as u32,
                old: dir.old,
            };
            if let Some(&marker) = markers.get(&token) {
                dir.retained_at = marker;
            }
        }
        self.entry_counts
            .retain(|(token, _)| !markers.contains_key(token));
    }
}
