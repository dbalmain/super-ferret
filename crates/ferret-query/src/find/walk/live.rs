//! The live source: a depth-first walk over each directory's getdents
//! listing, held level by level so a walk can split between workers.

use std::mem::MaybeUninit;

use rustix::fs::{Mode, OFlags, open};

use super::*;
use crate::find::Follow;
use crate::find::schedule::Share;

mod listing;
use listing::Children;

/// An open directory. Its children occupy `start..end` of the walk's child
/// stack, so levels pop in the order they were pushed.
#[derive(Clone)]
struct Level {
    catalogued: bool,
    pending: Option<Arc<AtomicUsize>>,
    handle: Option<DirectoryHandle>,
    /// The directory's `(dev, ino)` when `handle` was closed for want of
    /// budget.
    identity: Option<(u64, u64)>,
    /// The directory's own path is `entry.path[..path_len]`.
    path_len: usize,
    separator: bool,
    start: usize,
    next: usize,
    end: usize,
    names_start: usize,
    /// The first child's metadata, observed while listing when it is a
    /// directory.
    first: Option<Result<Metadata, Arc<io::Error>>>,
    /// A live listing's hidden names, for a walk [keeping hidden
    /// names](LiveWalk::keeping_hidden_names). A loan clones it with the
    /// level, so a borrower's entries see the whole listing, not just their
    /// half.
    hidden: Hidden,
    own: Saved,
}

impl Level {
    /// Makes this level own the first `children` of a fresh child stack,
    /// none of them visited, with their names at the start of its buffer.
    fn rebase(&mut self, children: usize) {
        (self.start, self.next, self.end, self.names_start) = (0, 0, children, 0);
    }
}

/// What the lent entry holds between fetches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    /// Nothing owed; advance to the next name.
    Empty,
    /// A pre-order entry was yielded; descend if the evaluator allows.
    Pending,
    /// An entry whose directory stat failed is still to be evaluated.
    Visit,
    /// A post-order directory that could not be opened is still to be yielded.
    After,
}

enum Loaded {
    Entry,
    Directory,
}

/// Sequential DFS. The unrestricted source uses readdir without ignore rules;
/// the catalog source shares the stack and substitutes catalog child listings.
/// It holds one directory handle per ancestor only when it retains parents,
/// and never sorts child names.
/// A descriptor exhaustion error is reported just like a listing failure.
pub struct LiveWalk {
    pub(super) catalog: Option<Arc<Catalog>>,
    /// Counts only successful catalog-child removals made by this walk.
    pub(super) removed_children: Option<Arc<Mutex<Removals>>>,
    pub(super) nested_roots: Vec<(InoId, InoId, Vec<u8>)>,
    paths: std::vec::IntoIter<PathBuf>,
    /// The catalog target of each start, where the caller already knows it,
    /// in step with `paths`; empty when none is known.
    pub(super) known: std::vec::IntoIter<Option<Target>>,
    pub(super) options: Options,
    entry: Entry,
    slot: Slot,
    root_dev: u64,
    levels: Vec<Level>,
    boundary: usize,
    suspended: bool,
    children: Children,
    buffer: Vec<MaybeUninit<u8>>,
    cancelled: bool,
    #[cfg(test)]
    pub(in crate::find) force_unknown: bool,
    #[cfg(test)]
    pub(in crate::find) on_listed: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl LiveWalk {
    /// An unrestricted physical walk, retaining each operand's spelling.
    pub fn paths(paths: Vec<PathBuf>, cwd: PathBuf) -> Self {
        Self::new(paths, Options::at(cwd))
    }

    /// Leaves each directory unstatted until it is opened. find stats a
    /// directory before visiting it, as GNU does, so that a vanished one
    /// faults even at a depth limit; a host that reports only failed opens,
    /// as rg does, saves a stat per directory.
    pub fn lazy_directories(mut self) -> Self {
        self.options.lazy_directories = true;
        self
    }

    /// Follows descendant symlinks, diagnosing ancestor cycles.
    pub fn following_links(mut self, follow: bool) -> Self {
        if follow {
            self.options.follow = Follow::All;
        }
        self
    }

    /// Keeps each listed directory open while its entries are visited, so
    /// that a host can open a listed name beneath the directory it was listed
    /// in ([`Entry::directory_handle`]) instead of resolving its path again,
    /// which a name swapped for a symlink would redirect (D21). Costs one
    /// descriptor per directory on the walk's stack, plus one per start for
    /// the start's parent; the syscalls are the same.
    ///
    /// Those descriptors are charged to a process-wide budget (`Charge`).
    /// Past it a listed directory is closed as an unretaining walk closes
    /// it, keeping only its `(dev, ino)` from one `fstat`
    /// ([`Entry::directory_identity`]), so a deep tree under a low open-file
    /// limit walks as it would without retaining.
    pub fn retaining_parents(mut self) -> Self {
        self.options.retain_parent = true;
        self.options.bounded_parents = true;
        self
    }

    /// Keeps each listed directory's hidden names for its entries'
    /// [`Entry::listing`]. Costs an allocation per directory holding any.
    pub fn keeping_hidden_names(mut self) -> Self {
        self.options.hidden_names = true;
        self
    }

    pub(in crate::find) fn new(paths: Vec<PathBuf>, options: Options) -> Self {
        Self {
            catalog: None,
            removed_children: None,
            nested_roots: Vec::new(),
            paths: paths.into_iter(),
            known: Vec::new().into_iter(),
            options,
            entry: Entry::new(PathBuf::new(), 0, FileKind::File),
            slot: Slot::Empty,
            root_dev: 0,
            levels: Vec::new(),
            boundary: 0,
            suspended: false,
            children: Children::default(),
            buffer: Vec::new(),
            cancelled: false,
            #[cfg(test)]
            force_unknown: false,
            #[cfg(test)]
            on_listed: None,
        }
    }

    // Five warm samples on the 300k fixture: 16 workers win live stat-heavy
    // rows (48 ms versus 69 ms at 8); 8 win typical catalog scans (21–23 ms
    // versus 23–24). Shallow live traversal also avoids the extra startup.
    // Catalogued starts at depth zero, a name test answered from postings,
    // read no directory: one worker beats handing each start to another
    // (`-name '*.md'` over 3.5k hits on ~/w: 6.2 ms versus 12.8 ms).
    pub(in crate::find) fn worker_limit(&self, workers: usize) -> usize {
        if self.catalog.is_some() && self.options.max_depth == Some(0) {
            1
        } else if self.catalog.is_some() || self.options.max_depth.is_some_and(|depth| depth <= 2) {
            workers.min(8)
        } else {
            workers
        }
    }

    pub(in crate::find) fn has_starts(&self) -> bool {
        !self.paths.as_slice().is_empty()
    }

    pub(in crate::find) fn in_live_directory(&self) -> bool {
        self.levels.last().is_some_and(|level| !level.catalogued)
    }

    pub(in crate::find) fn suspended(&self) -> bool {
        self.suspended
    }

    pub(in crate::find) fn waiting(&self) -> bool {
        self.slot == Slot::Empty
            && self.levels.last().is_some_and(|level| {
                level.next == level.end
                    && level
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.load(Ordering::Acquire) != 0)
            })
    }

    // Read-only starts may overlap. Effectful starts stay in the donor and
    // advance only after all donated descendants have completed.
    pub(in crate::find) fn split_start(&mut self) -> Option<Self> {
        let path = self.paths.next()?;
        let mut walk = Self::new(vec![path], self.options.clone());
        walk.known = vec![self.known.next().flatten()].into_iter();
        walk.catalog = self.catalog.clone();
        walk.removed_children = self.removed_children.clone();
        walk.nested_roots = self.nested_roots.clone();
        Some(walk)
    }

    // Donate siblings already observed by readdir. Ancestor levels remain in
    // the donated walk solely for loop detection and start/device identity.
    // Its boundary excludes their evaluation; the donor owns their completion,
    // so the loan carries none of their children. A loan copies only the lent
    // siblings and their names, rebased to start the borrower's child stack:
    // it runs under the scheduler's queue lock, and copying the whole stack
    // there serialised every worker on one wide directory. A whole level is
    // lent only under `Share::Idle`, when each lent sibling has an idle worker.
    pub(in crate::find) fn split(&mut self, share: Share) -> Option<(Self, Arc<AtomicUsize>)> {
        let fewest = match share {
            Share::Half => 2,
            Share::Idle(_) => 1,
        };
        // At depth two, catalog donation costs 7.4 ms versus 6.6 ms on the
        // caller. Live fallback levels still donate, even in this mode.
        let index = self
            .levels
            .iter()
            .enumerate()
            .skip(self.boundary.saturating_sub(1))
            .find_map(|(index, level)| {
                (level.end - level.next >= fewest
                    && !(level.catalogued && self.options.max_depth.is_some_and(|max| max <= 2)))
                .then_some(index)
            })?;
        let pending = self.levels[index]
            .pending
            .get_or_insert_with(|| Arc::new(AtomicUsize::new(0)))
            .clone();
        let level = &self.levels[index];
        let unvisited = level.end - level.next;
        let middle = match share {
            Share::Idle(idle) if unvisited <= idle => level.next,
            Share::Half | Share::Idle(_) => level.next + unvisited / 2,
        };
        let children = self.children.copy(middle..level.end);
        let mut levels: Vec<_> = self.levels[..=index].to_vec();
        // Ancestors own no children here; the donated level owns them all.
        for ancestor in &mut levels[..index] {
            ancestor.rebase(0);
        }
        let donated = &mut levels[index];
        donated.rebase(children.len());
        donated.pending = None;
        donated.first = None;
        let mut walk = Self::new(Vec::new(), self.options.clone());
        walk.catalog = self.catalog.clone();
        walk.removed_children = self.removed_children.clone();
        walk.nested_roots = self.nested_roots.clone();
        walk.entry = self.entry.clone();
        walk.root_dev = self.root_dev;
        walk.levels = levels;
        walk.boundary = index + 1;
        walk.children = children;
        walk.cancelled = self.cancelled;
        #[cfg(test)]
        {
            walk.force_unknown = self.force_unknown;
            walk.on_listed = self.on_listed.clone();
        }
        self.levels[index].end = middle;
        pending.fetch_add(1, Ordering::Relaxed);
        Some((walk, pending))
    }

    /// Loads the next start operand, child or finished post-order directory
    /// into the lent entry.
    fn advance(&mut self) -> Option<Result<Loaded, WalkError>> {
        loop {
            if self.waiting() {
                self.suspended = true;
                return None;
            }
            if self.boundary != 0
                && self.levels.len() == self.boundary
                && self
                    .levels
                    .last()
                    .is_some_and(|level| level.next == level.end)
            {
                return None;
            }
            if let Some(level) = self.levels.pop_if(|level| level.next == level.end) {
                self.children.truncate(level.start, level.names_start);
                if self.options.depth_first {
                    self.entry.path.truncate(level.path_len);
                    level.own.restore(&mut self.entry);
                    return Some(Ok(Loaded::Directory));
                }
                continue;
            }
            if self.levels.is_empty() {
                let path = self.paths.next()?;
                let known = self.known.next().flatten();
                return Some(self.load_operand(path, known));
            }
            return Some(self.load_child());
        }
    }

    /// Loads start operand `path` into the lent entry: through the catalog
    /// when it resolves there, else by a live stat. `known` is the catalog
    /// target the caller already resolved, if any.
    fn load_operand(&mut self, path: PathBuf, known: Option<Target>) -> Result<Loaded, WalkError> {
        let follow = self.options.follow != Follow::Physical;
        self.entry = Entry::new(path, 0, FileKind::File);
        self.entry.cwd = self.options.cwd.clone();
        self.entry.cwd_handle = self.options.cwd_handle.clone();
        self.entry.inherit_cwd = self.options.inherit_cwd;
        self.entry.removed_children = self.removed_children.clone();
        if self.options.retain_parent && (self.catalog.is_none() || self.options.live_checks) {
            match parent_handle(&self.entry) {
                Ok(handle) => self.entry.state.directory = Some(DirectoryHandle::uncharged(handle)),
                Err(error) => return Err(self.entry.error(error)),
            }
        }
        if let Some(catalog) = &self.catalog {
            if self.options.delete {
                self.entry.state.id = None;
                self.entry.state.parent = std::path::absolute(self.entry.lookup_path().as_ref())
                    .ok()
                    .as_deref()
                    .and_then(Path::parent)
                    .and_then(|parent| resolve(catalog, parent, true).ok().flatten())
                    .and_then(|(target, remainder)| match target {
                        Target::Inode(id) if !remainder => Some(id),
                        _ => None,
                    });
            }
            let follow = follow || self.entry.path.ends_with(b"/");
            let resolved = match known.filter(|_| !follow).map_or_else(
                || resolve(catalog, self.entry.lookup_path().as_ref(), follow),
                |target| Ok(Some((target, false))),
            ) {
                Ok(Some(resolved)) => resolved,
                Ok(None) => {
                    return Err(self.entry.error(io::Error::other(OutsideCatalog)));
                }
                Err(error) => return Err(self.entry.error(error)),
            };
            if !resolved.1
                && let Target::Inode(id) = resolved.0
            {
                self.entry.state.target = Some(resolved.0);
                self.entry.state.catalog = Some(catalog.clone());
                self.entry.state.kind = Some(catalog_kind(catalog.kind(id)));
                self.entry.state.follow = follow;
                // A physical resolution followed no final symlink.
                self.entry.state.followed_symlink = follow
                    && resolve(catalog, self.entry.lookup_path().as_ref(), false)
                        .ok()
                        .flatten()
                        .is_some_and(|(target, _)| target != resolved.0);
                if let Err(error) = self.entry.follow_catalog() {
                    return Err(self.entry.error(error));
                }
                if self.options.live_checks
                    && let Err(error) = self.entry.metadata()
                {
                    return Err(self.entry.error(error));
                }
                if self.options.xdev {
                    self.root_dev = catalog.identity(id).0;
                }
                return Ok(Loaded::Entry);
            }
        }
        if self.options.retain_parent && self.entry.state.directory.is_none() {
            match parent_handle(&self.entry) {
                Ok(handle) => self.entry.state.directory = Some(DirectoryHandle::uncharged(handle)),
                Err(error) => return Err(self.entry.error(error)),
            }
        }
        let follow = follow || self.entry.path.ends_with(b"/");
        let stat = match self
            .entry
            .with_observed_at(|dir, path| metadata_at(dir, path, follow))
        {
            Ok(stat) => stat,
            Err(error) => return Err(self.entry.error(error)),
        };
        self.root_dev = stat.dev();
        self.entry.state.kind = Some(stat.kind());
        self.entry.state.follow = follow;
        self.entry.state.metadata = OnceCell::from(Ok(stat));
        Ok(Loaded::Entry)
    }

    /// Loads the innermost level's next child into the lent entry.
    fn load_child(&mut self) -> Result<Loaded, WalkError> {
        let Some(level) = self.levels.last_mut() else {
            unreachable!("load_child with no open level")
        };
        let index = level.next;
        level.next += 1;
        let child = self.children[index];
        let entry = &mut self.entry;
        entry.path.truncate(level.path_len);
        if level.separator {
            entry.path.push(b'/');
        }
        entry.path.extend_from_slice(self.children.name(&child));
        entry.state.follow = self.options.follow == Follow::All;
        entry.state.followed_symlink = false;
        entry.state.directory = level.handle.clone();
        entry.state.listed = level.identity;
        entry.state.hidden = level.hidden.clone();
        entry.state.depth = level.own.depth + 1;
        entry.state.kind = child.kind;
        entry.check_directory = false;
        entry.state.metadata = OnceCell::new();
        entry.state.target = child.target;
        entry.state.parent = child.parent;
        entry.state.id = child.id;
        // Deletion accounting is walk-wide and stays in the reusable entry;
        // re-cloning its shared counter here contends across workers.
        if child.target.is_some() {
            if entry.state.catalog.is_none() {
                entry.state.catalog = self.catalog.clone();
            }
        } else {
            entry.state.catalog = None;
        }
        if let Err(error) = entry.follow_catalog() {
            return Err(entry.error(error));
        }
        if child.kind == Some(FileKind::Directory)
            && (child.target.is_none() || self.options.live_checks)
            && !self.options.lazy_directories
        {
            match level.first.take() {
                Some(stat) if index == level.start => entry.state.metadata = OnceCell::from(stat),
                _ => entry.check_directory = true,
            }
        }
        Ok(Loaded::Entry)
    }

    /// Applies visit-time checks to the lent entry. `Ok(true)` yields it;
    /// `Ok(false)` means a post-order directory was entered instead.
    fn visit(
        &mut self,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<bool, WalkError> {
        let entry = &mut self.entry;
        if entry.check_directory {
            entry.check_directory = false;
            if let Err(error) = entry.metadata() {
                // GNU still evaluates cheap name/type fields after this
                // directory stat error, including at maxdepth.
                let error = entry.error(error);
                self.slot = Slot::Visit;
                return Err(error);
            }
        }
        if self.options.follow != Follow::Physical
            && entry.state.catalog.is_some()
            && entry.state.kind == Some(FileKind::Directory)
        {
            let stat = entry.stat().map_err(|error| entry.error(error))?;
            if self.levels.iter().any(|level| {
                level
                    .own
                    .catalog
                    .as_ref()
                    .zip(level.own.target)
                    .is_some_and(|(catalog, target)| {
                        let Target::Inode(id) = target else {
                            return false;
                        };
                        catalog.identity(id) == (stat.dev(), stat.ino())
                    })
            }) {
                return Err(entry.error(io::Error::other("File system loop detected")));
            }
        }
        if entry.state.follow
            && entry.state.catalog.is_none()
            && entry.kind().map_err(|error| entry.error(error))? == FileKind::Directory
        {
            let stat = entry.metadata().map_err(|error| entry.error(error))?;
            if self.levels.iter().any(|level| {
                // Live ancestors own an observed directory descriptor. Pure
                // catalog ancestors have a stored identity and need no stat.
                if let Some(cached) = level.own.metadata.get() {
                    // Pure live walks close directory descriptors after
                    // listing. Their already observed
                    // metadata still proves ancestry.
                    cached.as_ref().is_ok_and(|ancestor| {
                        ancestor.dev() == stat.dev() && ancestor.ino() == stat.ino()
                    })
                } else if let Some(handle) = &level.handle {
                    level
                        .own
                        .metadata
                        .get_or_init(|| {
                            stat_at(handle.as_fd(), Path::new(""), AtFlags::EMPTY_PATH)
                                .map_err(Arc::new)
                        })
                        .as_ref()
                        .is_ok_and(|ancestor| {
                            ancestor.dev() == stat.dev() && ancestor.ino() == stat.ino()
                        })
                } else {
                    level
                        .own
                        .catalog
                        .as_ref()
                        .zip(level.own.target)
                        .is_some_and(|(catalog, target)| {
                            matches!(target,
                            Target::Inode(id) if catalog.identity(id) == (stat.dev(), stat.ino()))
                        })
                }
            }) {
                return Err(WalkError {
                    path: entry.path().to_owned(),
                    error: io::Error::other("File system loop detected"),
                });
            }
        }
        if !self.options.depth_first {
            self.slot = Slot::Pending;
            return Ok(true);
        }
        if self
            .options
            .max_depth
            .is_some_and(|max| entry.state.depth >= max)
            || entry.kind().map_err(|error| entry.error(error))? != FileKind::Directory
        {
            return Ok(true);
        }
        // xdev directories still evaluate, even when not descended.
        if self.options.xdev && entry.stat().is_ok_and(|stat| stat.dev() != self.root_dev) {
            return Ok(true);
        }
        self.descend(before_directory, cancelled)?;
        Ok(false)
    }

    fn next_entry(
        &mut self,
        descend: bool,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Option<Result<&Entry, WalkError>> {
        if self.cancelled {
            return None;
        }
        self.suspended = false;
        let mut loaded = match std::mem::replace(&mut self.slot, Slot::Empty) {
            Slot::Pending if descend => {
                if let Err(error) = self.descend(before_directory, cancelled) {
                    return Some(Err(error));
                }
                if self.cancelled {
                    return None;
                }
                None
            }
            Slot::After => return Some(Ok(&self.entry)),
            Slot::Visit => Some(Loaded::Entry),
            Slot::Empty | Slot::Pending => None,
        };
        loop {
            if self.cancelled {
                return None;
            }
            let current = match loaded.take() {
                Some(current) => current,
                None => match self.advance()? {
                    Ok(current) => current,
                    Err(error) => return Some(Err(error)),
                },
            };
            match current {
                Loaded::Directory => return Some(Ok(&self.entry)),
                Loaded::Entry => match self.visit(before_directory, cancelled) {
                    Ok(true) => return Some(Ok(&self.entry)),
                    Ok(false) => {}
                    Err(error) => return Some(Err(error)),
                },
            }
        }
    }
}

impl EntrySource for LiveWalk {
    fn catalog(&self) -> Option<&Catalog> {
        self.catalog.as_deref()
    }
    fn next(&mut self, descend: bool) -> Option<Result<&Entry, WalkError>> {
        self.next_entry(descend, &mut || Ok(()), &mut || false)
    }
    fn next_with(
        &mut self,
        descend: bool,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
    ) -> Option<Result<&Entry, WalkError>> {
        self.next_entry(descend, before_directory, &mut || false)
    }
    fn next_with_cancel(
        &mut self,
        descend: bool,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Option<Result<&Entry, WalkError>> {
        self.next_entry(descend, before_directory, cancelled)
    }
}

fn parent_handle(entry: &Entry) -> io::Result<Arc<File>> {
    let (parent, _) = crate::find::action::exec_path(entry.observed_path().as_ref());
    Ok(Arc::new(File::from(open(
        &parent,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)))
}
