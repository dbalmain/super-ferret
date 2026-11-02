//! Reading a batch back at reconciliation: its directory and file
//! observations borrowed in place, the names it reused or preserved from
//! the pinned view, and its counts.

use super::*;
use crate::Generation;

/// Borrowed directory observation. Tokens remain local to the producing run.
#[derive(Clone, Copy)]
pub struct DirectoryObservation<'a> {
    pub token: DirToken,
    pub parent: Option<DirToken>,
    pub name: &'a [u8],
    pub stat: &'a Stat,
    pub traversed: bool,
    /// A repository ignore file's `!` line reveals this hidden name (D127).
    pub revealed: bool,
    pub retained_at: Option<u64>,
}

/// Borrowed file observation, including symlinks and special files.
#[derive(Clone, Copy)]
pub struct FileObservation<'a> {
    pub parent: DirToken,
    pub name: &'a [u8],
    pub stat: Stat,
    pub content: Content,
    pub target: Option<&'a [u8]>,
    /// A repository ignore file's `!` line reveals this hidden name (D127).
    pub revealed: bool,
}

impl Batch {
    /// Directory observations, without copying their strings or stat columns.
    pub fn directories(&self) -> impl Iterator<Item = DirectoryObservation<'_>> {
        self.dirs
            .iter()
            .enumerate()
            .map(|(i, dir)| DirectoryObservation {
                token: DirToken {
                    batch: self.id,
                    index: i as u32,
                    old: dir.old,
                },
                parent: dir.parent,
                name: dir.name.of(&self.names),
                stat: &self.dir_stats[i],
                traversed: dir.traversed,
                revealed: dir.revealed,
                retained_at: dir.retained_at,
            })
    }
    /// Number of full file observations, including an unfinished local listing.
    /// Equal compact references are exposed separately by `reused_files`.
    pub fn file_count(&self) -> usize {
        self.files.len() + self.pending.len()
    }
    /// Borrows one file observation; `index` must be below `file_count`.
    pub fn file_observation(&self, index: usize) -> FileObservation<'_> {
        if index >= self.files.len() {
            let f = &self.pending[index - self.files.len()];
            return FileObservation {
                parent: f.parent,
                name: f.name.of(&self.pending_bytes),
                stat: f.stat,
                content: f.content,
                target: f.target.map(|span| span.of(&self.pending_bytes)),
                revealed: f.revealed,
            };
        }
        let file = &self.files[index];
        FileObservation {
            parent: file.parent,
            name: file.name.of(&self.names),
            stat: self.file_stats[index],
            content: self.contents[index],
            target: self.target(index),
            revealed: self.revealed_files.binary_search(&(index as u32)).is_ok(),
        }
    }
    /// Equal rows proven against this batch's pinned generation. The token
    /// still needs to resolve to its hinted old directory before reuse.
    pub fn reused_files(&self) -> impl Iterator<Item = (DirToken, NameId)> + '_ {
        self.reused
            .ids()
            .map(|name| (self.reused_parent(name), name))
    }
    fn reused_parent(&self, name: NameId) -> DirToken {
        let Some(old) = &self.previous else {
            unreachable!("reused names require a pinned generation");
        };
        self.reused.parents[&old.name(name).parent].0
    }
    /// Included parents, without retaining one reference per equal file.
    pub fn reused_directories(&self) -> impl Iterator<Item = DirToken> + '_ {
        self.reused.included()
    }
    /// Parents whose continuing hints must validate before bulk reuse.
    pub fn reused_parents(&self) -> impl Iterator<Item = DirToken> + '_ {
        self.reused.parents.values().map(|&(parent, _)| parent)
    }
    /// Equal-name/inode seen words, valid only after parent validation.
    pub fn reused_words(&self) -> (&[u64], &[u64]) {
        (&self.reused.names, &self.reused.inodes)
    }
    /// Number of compact equal observations.
    pub fn reused_file_count(&self) -> usize {
        self.reused.count
    }
    /// Reconstructs an equal observation by its seen NameId, for alias
    /// grouping.
    pub fn reused_name_observation(&self, name: NameId) -> FileObservation<'_> {
        let parent = self.reused_parent(name);
        let Some(old) = &self.previous else {
            unreachable!("reused observations require a pinned generation");
        };
        let edge = old.name(name);
        FileObservation {
            parent,
            name: edge.bytes,
            stat: old.inode(edge.child).stat,
            content: content(old, edge.child),
            target: old.link_target(edge.child),
            revealed: old.is_revealed(name),
        }
    }
    /// Preserves an untouched edge under a checked same-path parent. This is
    /// scope retention, not a fresh observation or proof of child identity.
    /// Batches with preserved inode edges require resident log reconciliation;
    /// checkpoint transactions refuse them rather than dropping old subtrees.
    pub fn preserve(&mut self, parent: DirToken, name: &[u8]) {
        let Some(old) = &self.previous else {
            return;
        };
        let Some(id) = parent
            .previous_directory()
            .and_then(|p| old.lookup(p, name))
        else {
            return;
        };
        let child = match old.name(id).target() {
            crate::Target::Inode(id) => Some(id),
            crate::Target::Ignored(_) => None,
        };
        self.scoped |= child.is_some();
        self.preserved.insert(parent, id, child);
    }
    /// Untouched edges and their validated parent tokens.
    pub fn preserved_files(&self) -> impl Iterator<Item = (DirToken, NameId)> + '_ {
        self.preserved.ids().map(|name| {
            let Some(old) = &self.previous else {
                unreachable!("preserved edges require a pinned view")
            };
            (self.preserved.parents[&old.name(name).parent].0, name)
        })
    }
    /// Parent scopes containing preserved included rows (D29).
    pub fn preserved_parents(&self) -> impl Iterator<Item = DirToken> + '_ {
        self.preserved.included()
    }
    /// Peak local observation rows/bytes, excluding changed rows and seen bits.
    pub fn observation_peak(&self) -> (usize, usize) {
        (
            self.pending_peak,
            self.pending_peak * std::mem::size_of::<PendingFile>() + self.pending_bytes_peak,
        )
    }
    /// Generation against which compact observations were checked.
    pub fn observation_generation(&self) -> Option<Generation> {
        self.previous.as_ref().map(Catalog::generation)
    }
    /// Opaque ignored edges, without allocating inode observations.
    pub fn ignored_entries(&self) -> impl Iterator<Item = (DirToken, &[u8], Kind)> {
        self.ignored
            .iter()
            .map(|e| (e.parent, e.name.of(&self.names), e.kind))
    }
    /// Raw listing counts; a token without a count has incomplete coverage.
    pub fn entry_counts(&self) -> impl Iterator<Item = (DirToken, u32)> + '_ {
        self.entry_counts.iter().copied()
    }
    /// Work-tree observations, borrowing repository paths.
    pub fn work_tree_observations(
        &self,
    ) -> impl Iterator<Item = (DirToken, WorkTreeKind, &[u8], (u64, u64))> {
        self.work_trees
            .iter()
            .map(|w| (w.dir, w.kind, w.common_dir.of(&self.strings), w.common_id))
    }
    /// Whether an observation buffer exceeded the wire-format span limit.
    pub fn overflowed(&self) -> bool {
        self.overflow
    }
}
