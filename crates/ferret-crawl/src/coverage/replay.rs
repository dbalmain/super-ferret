//! Checkpoint replay: complete rewalk observations and the retained
//! boundaries [`resolve`] protected, carried through the
//! full builder.

use super::*;

/// Replays complete rewalk observations and checked retained boundaries through
/// the full builder. Numeric ids select old rows only; new tokens own all
/// edges. None refuses unrepresentable coverage before publishing anything.
pub(crate) fn checkpoint_observations(
    session: &WriterSession,
    batches: Vec<Batch>,
    protection: &Protection,
    configured: &[PathBuf],
) -> Option<Vec<Batch>> {
    if protection.is_empty() && protection.denied.is_empty() {
        return Some(batches);
    }
    let old = session.view();
    let dirs: BTreeMap<_, _> = batches
        .iter()
        .flat_map(Batch::directories)
        .map(|d| (d.token, d))
        .collect();
    let mut children: BTreeMap<_, Vec<_>> = BTreeMap::new();
    let mut queue = VecDeque::new();
    for d in dirs.values() {
        if let Some(parent) = d.parent {
            children.entry(parent).or_default().push(d.token);
        } else {
            queue.push_back(d.token);
        }
    }
    let mut resolver = Resolver {
        session,
        old: old.clone(),
        dirs,
        resolved: BTreeMap::new(),
        active: BTreeSet::new(),
        out: Protection::default(),
    };
    let mut replay = Replay {
        old: &old,
        protection,
        carried: session.checkpoint_batch(0),
        live: BTreeSet::new(),
        coverage: BTreeMap::new(),
        overridden: BTreeMap::new(),
        represented: BTreeSet::new(),
    };
    while let Some(token) = queue.pop_front() {
        if replay.visit(&mut resolver, token)? {
            queue.extend(children.remove(&token).unwrap_or_default());
        }
    }
    replay.fill_roots(&resolver.dirs, configured)?;
    replay.check_represented()?;
    drop(resolver);
    drop(children);
    Some(replay.apply(batches))
}

/// A checkpoint replay's state: the batch of old rows it carries, and per new
/// token whether it is live, its coverage, and the old names it overrides.
struct Replay<'a> {
    old: &'a Catalog,
    protection: &'a Protection,
    carried: Batch,
    live: BTreeSet<DirToken>,
    coverage: BTreeMap<DirToken, Option<u64>>,
    overridden: BTreeMap<DirToken, BTreeSet<Vec<u8>>>,
    represented: BTreeSet<u32>,
}

impl Replay<'_> {
    /// Replays directory `token`: copies a protected subtree whole, records
    /// an opaque directory's coverage, or carries its protected old edges.
    /// `Some(true)` when its children are to be replayed too; `None` refuses.
    fn visit(&mut self, resolver: &mut Resolver<'_>, token: DirToken) -> Option<bool> {
        let (old, protection) = (self.old, self.protection);
        let d = *resolver.dirs.get(&token)?;
        let parent = match d.parent {
            Some(p) => self.live.contains(&p).then_some(Some(p))?,
            None => None,
        };
        if d.parent.is_some_and(|p| {
            self.overridden
                .get(&p)
                .is_some_and(|names| names.contains(d.name))
        }) {
            return Some(false);
        }
        if protection.tokens.contains(&token) {
            let id = resolver.directory(token)?;
            copy_subtree(
                old,
                &mut self.carried,
                id,
                parent,
                d.name,
                d.revealed,
                &protection.markers,
            )?;
            self.represented.insert(id.0);
            return Some(false);
        }
        self.live.insert(token);
        let opaque = protection.denied.contains(&token) || protection.opaque.contains(&token);
        if opaque {
            self.coverage.insert(token, None);
            return Some(false);
        }
        if let Some(id) = resolver.directory(token) {
            if protection.markers.contains(&id.0) {
                self.coverage.insert(token, retained_sequence(old, id));
            }
            for name in old.children(id) {
                let edge = old.name(name);
                let retained_dir = matches!(edge.target(), ferret_catalog::Target::Inode(child) if old.is_directory(child) && protection.directories.contains(&child.0));
                if protection.edges.contains(&name.0) || retained_dir {
                    self.overridden
                        .entry(token)
                        .or_default()
                        .insert(edge.bytes.to_vec());
                    copy_edge(old, &mut self.carried, token, name, &protection.markers)?;
                    self.represented.insert(edge.child.0);
                }
            }
        }
        Some(true)
    }

    /// A failed root open can have no token. Only a checked old configured
    /// root is allowed to fill that absence; new unanchored roots abort.
    fn fill_roots(
        &mut self,
        dirs: &BTreeMap<DirToken, DirectoryObservation<'_>>,
        configured: &[PathBuf],
    ) -> Option<()> {
        for root in configured {
            if !dirs
                .values()
                .any(|d| d.parent.is_none() && d.name == root.as_os_str().as_bytes())
            {
                let (id, path) = self
                    .old
                    .roots()
                    .find(|(_, path)| *path == root.as_os_str().as_bytes())?;
                if !self.protection.directories.contains(&id.0) {
                    return None;
                }
                copy_subtree(
                    self.old,
                    &mut self.carried,
                    id,
                    None,
                    path,
                    false,
                    &self.protection.markers,
                )?;
                self.represented.insert(id.0);
            }
        }
        Some(())
    }

    /// Every protected directory has a represented ancestor, or is one.
    fn check_represented(&self) -> Option<()> {
        for &id in &self.protection.directories {
            let mut dir = InoId(id);
            while !self.represented.contains(&dir.0) {
                dir = self.old.name(self.old.dir_name(dir)?).parent;
            }
        }
        Some(())
    }

    /// Maps the replay onto `batches`, then appends the carried batch when it
    /// holds anything.
    fn apply(self, mut batches: Vec<Batch>) -> Vec<Batch> {
        let Self {
            protection,
            mut carried,
            live,
            coverage,
            overridden,
            ..
        } = self;
        let mut mapping: BTreeMap<_, _> = batches
            .iter()
            .flat_map(|batch| batch.checkpoint_tokens(|token| live.contains(&token)))
            .collect();
        mapping.extend(carried.checkpoint_tokens(|_| true));
        for batch in &mut batches {
            batch.checkpoint_coverage(&coverage);
            batch.retain_checkpoint(&mapping, |parent, name| {
                !protection.denied.contains(&parent)
                    && !protection.opaque.contains(&parent)
                    && !overridden
                        .get(&parent)
                        .is_some_and(|names| names.contains(name))
            });
        }
        carried.retain_checkpoint(&mapping, |_, _| true);
        carried.seal();
        if carried.directories().next().is_some()
            || carried.file_count() > 0
            || carried.ignored_entries().next().is_some()
        {
            batches.push(carried);
        }
        batches
    }
}

fn retained_sequence(old: &Catalog, id: InoId) -> Option<u64> {
    old.retained_at(id)
        .or_else(|| old.entry_count(id).map(|_| old.generation().sequence))
}

fn copy_subtree(
    old: &Catalog,
    batch: &mut Batch,
    id: InoId,
    parent: Option<DirToken>,
    name: &[u8],
    revealed: bool,
    markers: &BTreeSet<u32>,
) -> Option<()> {
    // The legacy full builder cannot express an independently suppressed old
    // directory. Refuse that rare protected case instead of changing D29.
    if old.is_traversed(id) != old.is_search_suppressed(id) {
        return None;
    }
    let stat = old.inode(id).stat;
    let token = match parent {
        None => batch.root(name, stat),
        Some(p) if old.is_traversed(id) => batch.revealing(revealed).traversed_dir(p, name, stat),
        Some(p) => batch.revealing(revealed).dir(p, name, stat),
    };
    if markers.contains(&id.0) {
        if let Some(seq) = retained_sequence(old, id) {
            batch.retained_at(token, Some(seq));
        }
    } else {
        if let Some(count) = old.entry_count(id) {
            batch.entry_count(token, count);
        }
        if let Some(seq) = old.retained_at(id) {
            batch.retained_at(token, Some(seq));
        }
    }
    if let Some(wt) = old.work_tree(id) {
        batch.work_tree(token, wt.kind, wt.common_dir, wt.common_id);
    }
    for name in old.children(id) {
        copy_edge(old, batch, token, name, markers)?;
    }
    Some(())
}

fn copy_edge(
    old: &Catalog,
    batch: &mut Batch,
    parent: DirToken,
    name: ferret_catalog::NameId,
    markers: &BTreeSet<u32>,
) -> Option<()> {
    use ferret_catalog::{Content, ContentState, Target};
    let edge = old.name(name);
    let Target::Inode(id) = edge.target() else {
        if let Target::Ignored(kind) = edge.target() {
            batch.ignored(parent, edge.bytes, kind);
        }
        return Some(());
    };
    if old.is_directory(id) {
        let revealed = old.is_revealed(name);
        return copy_subtree(old, batch, id, Some(parent), edge.bytes, revealed, markers);
    }
    let inode = old.inode(id);
    if old.kind(id) == Kind::Symlink {
        batch.retained_file(
            parent,
            edge.bytes,
            inode.stat,
            Content::Unindexed,
            Some(old.link_target(id)?),
            old.is_revealed(name),
        );
    } else {
        let content = match inode.state {
            ContentState::Unindexed => Content::Unindexed,
            ContentState::Binary => Content::Binary,
            ContentState::Fault => Content::Fault,
            ContentState::Hashed => Content::Hashed(old.doc_hash(inode.doc?)?),
        };
        batch.retained_file(
            parent,
            edge.bytes,
            inode.stat,
            content,
            None,
            old.is_revealed(name),
        );
    }
    Some(())
}
