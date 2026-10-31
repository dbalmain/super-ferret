//! The per-worker walk visitor: it records each entry into one batch, reads
//! and hashes the files that changed, and defers names of an inode another
//! worker has in flight.

use super::*;

/// A name of a multiply-linked inode that was in flight on another worker
/// when this one reached it; recorded once that inode's observation is
/// finished ([`Output::defer`]).
pub(crate) struct Deferred {
    pub(crate) parent: DirToken,
    pub(crate) name: Vec<u8>,
    pub(crate) stat: Stat,
    pub(crate) path: PathBuf,
    pub(crate) revealed: bool,
}

/// The per-worker visitor: fills one batch and reads the files it must.
pub(crate) struct Hasher<'a> {
    txn: Source<'a>,
    cache: &'a Cache,
    root: &'a Path,
    pub(crate) out: Output,
    pub(super) reader: Reader,
    pub(super) selection: Option<&'a crate::refresh::Selection>,
    pub(super) watch: Option<&'a crate::watch::Watch>,
    pub(super) priority_error: Option<io::Error>,
    /// Background workers lower themselves. An unpaced explicit command leaves
    /// this false so the worker keeps the nice of the thread that spawned it.
    pub(super) lower_priority: bool,
}

/// What a worker leaves behind.
pub(crate) struct Output {
    budget: Option<std::sync::Arc<ferret_catalog::InputBudget>>,
    pub(crate) batch: ferret_catalog::Batch,
    pub(crate) counts: Counts,
    faults: Vec<CoverageFault>,
    pub(crate) content_faults: Vec<(PathBuf, ContentFault)>,
    pub(super) pattern_errors: Vec<String>,
    pub(crate) deferred: Vec<Deferred>,
    /// The backlog length at which [`Output::defer`] next drains it.
    drain_at: usize,
    pub(super) read_time: Duration,
}

/// The smallest backlog [`Output::defer`] drains. Draining is a cache lookup
/// per entry, and the threshold doubles past what stays, so the cost is
/// amortised constant per alias.
pub(crate) const DRAIN_MIN: usize = 64;

impl<'a> Hasher<'a> {
    #[cfg(test)]
    pub(crate) fn new(txn: &'a Transaction, cache: &'a Cache, root: &'a Path) -> Self {
        Self::with_source(Source::Checkpoint(txn), cache, root)
    }
    pub(super) fn with_source(txn: Source<'a>, cache: &'a Cache, root: &'a Path) -> Self {
        Self {
            txn,
            cache,
            root,
            out: Output {
                budget: txn.budget(),
                batch: txn.batch(),
                counts: Counts::default(),
                faults: Vec::new(),
                content_faults: Vec::new(),
                pattern_errors: Vec::new(),
                deferred: Vec::new(),
                drain_at: DRAIN_MIN,
                read_time: Duration::ZERO,
            },
            reader: Reader::new(),
            selection: None,
            watch: None,
            priority_error: None,
            lower_priority: true,
        }
    }

    pub(super) fn finish(mut self, faults: &mut Vec<CoverageFault>) -> Output {
        self.out.counts.files_read = self.reader.files_read;
        self.out.counts.bytes_read = self.reader.bytes_read;
        self.out.read_time = self.reader.read_time;
        faults.append(&mut self.out.faults);
        self.out
    }

    fn promote_changed_parent(&self, token: DirToken, stat: &Stat, path: &Path) {
        if let Some(selection) = self.selection {
            let continuing = match (token.previous_directory(), self.txn) {
                (Some(id), Source::Bounded(session, _, _) | Source::Rebuild(session, _, _)) => {
                    session.view().inode(id).stat.same_version(stat)
                }
                _ => false,
            };
            if !continuing {
                // Directory ctime can change for ordinary child edits too.
                // Conservatively cover its subtree rather than trust a reused
                // (dev, ino) as proof that the old parent still exists.
                selection.promote(path);
            }
        }
    }

    /// A file the policy sends to the index: carried, taken from another
    /// name's observation, read, or deferred.
    fn index_file(&mut self, decided: &Decided<'_, DirToken>, stat: Stat) {
        let out = &mut self.out;
        out.counts.indexed += 1;
        // An old `Unindexed` means the policy did not send it to the index
        // then; it does now, so it must be read (D37).
        match self.txn.carry(&stat) {
            Some(Content::Unindexed) | None => {}
            Some(content) => {
                out.counts.carried += 1;
                out.batch.revealing(decided.revealed).file(
                    decided.parent,
                    decided.name.as_bytes(),
                    stat,
                    content,
                );
                #[cfg(test)]
                hook(self.root, Probe::Carried(decided.path));
                return;
            }
        }
        let (file, links) = match Reader::open(decided.parent_fd, decided.name, &stat) {
            Opened::Ready { file, links } => (file, links),
            Opened::Fault(fault) => {
                self.record(decided, stat, Err(fault));
                return;
            }
        };
        let key = (stat.dev, stat.ino);
        if links > 1 {
            match self.cache.claim(key) {
                Lookup::Claimed => {}
                Lookup::InFlight => {
                    if !out.reserve(
                        std::mem::size_of::<Deferred>()
                            + decided.name.as_bytes().len()
                            + joined_bytes(self.root, decided.path),
                    ) {
                        return;
                    }
                    let alias = Deferred {
                        parent: decided.parent,
                        name: decided.name.as_bytes().to_vec(),
                        stat,
                        path: self.root.join(decided.path),
                        revealed: decided.revealed,
                    };
                    self.out.defer(alias, self.cache);
                    #[cfg(test)]
                    hook(self.root, Probe::Deferred);
                    return;
                }
                Lookup::Done(stored) => {
                    self.out.counts.aliased += 1;
                    let (stat, content) = observe::consume(stored, &stat);
                    self.record(decided, stat, content);
                    return;
                }
            }
            #[cfg(test)]
            hook(self.root, Probe::Claimed);
        }
        let mut file = file;
        let content = self.reader.read(&mut file, stat.size);
        #[cfg(test)]
        hook(self.root, Probe::Hashed(decided.path));
        let content = content.and_then(|c| observe::bracket(&file, &stat, c));
        if links > 1 {
            let observation = Observation {
                stat,
                content: content.as_ref().ok().copied(),
            };
            self.cache.complete(key, observation);
        }
        #[cfg(test)]
        hook(self.root, Probe::Read(decided.path));
        self.record(decided, stat, content);
    }

    /// Records a read or aliased file.
    fn record(
        &mut self,
        decided: &Decided<'_, DirToken>,
        stat: Stat,
        content: Result<Content, ContentFault>,
    ) {
        let content = self.out.outcome(
            joined_bytes(self.root, decided.path),
            || self.root.join(decided.path),
            content,
        );
        self.out.batch.revealing(decided.revealed).file(
            decided.parent,
            decided.name.as_bytes(),
            stat,
            content,
        );
    }

    fn fault(
        &mut self,
        path: &Path,
        op: IoOp,
        context: FaultContext<'_, DirToken>,
        error: io::Error,
    ) {
        let name_bytes = match context {
            FaultContext::Child { name, .. } => name.as_bytes().len(),
            _ => 0,
        };
        if !self.out.reserve(
            std::mem::size_of::<CoverageFault>()
                + self.root.as_os_str().len()
                + path.as_os_str().len()
                + name_bytes,
        ) {
            return;
        }
        self.out.faults.push(CoverageFault {
            root: self.root.to_owned(),
            path: path.to_owned(),
            op,
            on_root: matches!(context, FaultContext::Root),
            context: match context {
                FaultContext::Root => CoverageContext::Root,
                FaultContext::Dir(token) => CoverageContext::Directory(token),
                FaultContext::Child { parent, name } => CoverageContext::Child {
                    parent,
                    name: name.as_bytes().to_vec(),
                },
            },
            error,
        });
    }
}

// Conservatively include a separator without allocating the joined path.
fn joined_bytes(root: &Path, path: &Path) -> usize {
    root.as_os_str().len() + 1 + path.as_os_str().len()
}

impl Output {
    fn reserve(&self, bytes: usize) -> bool {
        self.budget
            .as_ref()
            .is_none_or(|budget| budget.charge(1, bytes).is_ok())
    }
    /// The content to publish for one name, listing a fault with its path.
    /// These are the faults a worker saw, with their errors; the build may
    /// fault more names, which `index` lists from the published catalog.
    fn outcome(
        &mut self,
        path_bytes: usize,
        path: impl FnOnce() -> PathBuf,
        content: Result<Content, ContentFault>,
    ) -> Content {
        content.unwrap_or_else(|fault| {
            if self.reserve(std::mem::size_of::<(PathBuf, ContentFault)>() + path_bytes) {
                self.content_faults.push((path(), fault));
            }
            // A refused report marks the shared attempt exhausted. The caller
            // discards all output and rewalks; this placeholder never
            // publishes.
            Content::Fault
        })
    }

    /// Sets `alias` aside until its inode's observation is finished. When
    /// the backlog reaches `drain_at`, every alias whose inode has finished
    /// since is recorded, and the rest resolve when the root's walk ends.
    ///
    /// So a worker holds, within one root, the aliases of inodes still in
    /// flight plus at most `max(DRAIN_MIN, 2 × what stayed at its last
    /// drain)` finished ones; nothing survives into the next root. Aliases of
    /// one inode are bounded by its link count on one filesystem, but not
    /// across bind mounts, which show the same inode under several paths.
    pub(crate) fn defer(&mut self, alias: Deferred, cache: &Cache) {
        self.counts.deferred += 1;
        cache.deferred(1);
        self.deferred.push(alias);
        if self.deferred.len() < self.drain_at {
            return;
        }
        for alias in std::mem::take(&mut self.deferred) {
            match cache.finished((alias.stat.dev, alias.stat.ino)) {
                Some(stored) => self.record_alias(alias, Some(stored), cache),
                None => self.deferred.push(alias),
            }
        }
        self.drain_at = (2 * self.deferred.len()).max(DRAIN_MIN);
    }

    /// Records the deferred aliases left once the walk is over.
    pub(crate) fn resolve(&mut self, cache: &Cache) {
        for alias in std::mem::take(&mut self.deferred) {
            let stored = cache.finished((alias.stat.dev, alias.stat.ino));
            self.record_alias(alias, stored, cache);
        }
    }

    /// Records one deferred alias from its inode's observation; `None`, a
    /// claim never completed, is a fault.
    fn record_alias(&mut self, alias: Deferred, stored: Option<Observation>, cache: &Cache) {
        self.counts.aliased += 1;
        cache.deferred(-1);
        let (stat, content) = match stored {
            Some(stored) => observe::consume(stored, &alias.stat),
            None => (alias.stat, Err(ContentFault::Alias)),
        };
        let revealed = alias.revealed;
        let content = self.outcome(alias.path.as_os_str().len(), || alias.path, content);
        self.batch
            .revealing(revealed)
            .file(alias.parent, &alias.name, stat, content);
    }
}

/// Debug-build hold for an explicit command's crawl worker once it has made
/// its priority decision; background crawls never stop here. While
/// `$FERRET_CRAWL_TEST_GATE/worker` exists, each worker records its
/// thread id as `worker.<tid>.reached` and waits, so a test can read the nice
/// of a worker that is known to be past the point where it could lower itself.
#[cfg(debug_assertions)]
fn worker_checkpoint() {
    let Some(gate) = std::env::var_os("FERRET_CRAWL_TEST_GATE").map(std::path::PathBuf::from)
    else {
        return;
    };
    let held = gate.join("worker");
    if !held.exists() {
        return;
    }
    let tid = std::fs::read_link("/proc/thread-self")
        .ok()
        .and_then(|link| link.file_name().map(|name| name.to_owned()));
    let Some(tid) = tid else { return };
    let mut reached = std::ffi::OsString::from("worker.");
    reached.push(tid);
    reached.push(".reached");
    let _ = std::fs::write(gate.join(reached), b"checkpoint");
    while held.exists() {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

impl EventVisitor for Hasher<'_> {
    fn dedicated_worker(&self) -> bool {
        true
    }
    fn worker_started(&mut self) {
        if self.lower_priority {
            self.priority_error = crate::lower_index_priority().err();
        }
        #[cfg(debug_assertions)]
        if !self.lower_priority {
            worker_checkpoint();
        }
    }
    type Dir = DirToken;

    fn policy_path(&mut self, path: &Path) {
        if let Some(watch) = self.watch {
            watch.policy_path(self.root, path);
        }
    }

    fn policy_input(&mut self, fd: std::os::fd::BorrowedFd<'_>, name: &OsStr) {
        if let Some(watch) = self.watch {
            watch.policy_input(self.root, fd, name);
        }
    }

    fn policy_inputs(&mut self, fd: std::os::fd::BorrowedFd<'_>, names: &[&OsStr]) {
        if let Some(watch) = self.watch {
            watch.policy_inputs(self.root, fd, names);
        }
    }

    fn observing(&mut self, fd: std::os::fd::BorrowedFd<'_>, path: &Path) {
        if let Some(watch) = self.watch {
            watch.arm(self.root, path, fd);
        }
    }

    fn root(&mut self, stat: crate::Stat<'_>) -> DirToken {
        self.out.counts.dirs += 1;
        let stat = observe::from_walk(&stat);
        let token = self.out.batch.root(self.root.as_os_str().as_bytes(), stat);
        self.promote_changed_parent(token, &stat, Path::new(""));
        token
    }

    fn consider(&mut self, parent: DirToken, name: &OsStr, path: &Path) -> bool {
        if self.out.batch.input_exceeded() {
            return false;
        }
        let Some(selection) = self.selection else {
            return true;
        };
        if let Some(old) = parent.previous_directory()
            && let Source::Bounded(session, _, _) | Source::Rebuild(session, _, _) = self.txn
            && session.view().entry_count(old).is_none()
        {
            selection.promote(path.parent().unwrap_or_else(|| Path::new("")));
        }
        if selection.includes(path) {
            true
        } else {
            self.out.batch.preserve(parent, name.as_bytes());
            false
        }
    }

    fn visit(&mut self, event: Event<'_, DirToken>) -> Option<DirToken> {
        if self.out.batch.input_exceeded() {
            return None;
        }
        match event {
            Event::Decided(decided) => self.decided(decided),
            Event::Entered {
                dir,
                work_tree,
                entries,
            } => {
                self.entered(dir, work_tree, entries);
                None
            }
            Event::Boundary { .. } => {
                self.out.counts.boundaries += 1;
                None
            }
            Event::Io {
                path,
                op,
                context,
                error,
            } => {
                self.io(path, op, context, error);
                None
            }
            Event::Pattern(error) => {
                self.pattern(error);
                None
            }
        }
    }
}

impl Hasher<'_> {
    /// Records one policy decision; a directory to descend returns its token.
    fn decided(&mut self, decided: Decided<'_, DirToken>) -> Option<DirToken> {
        if let Some(watch) = self.watch {
            watch.file(self.root, &decided);
        }
        if decided.decision == Decision::Skip {
            self.out
                .batch
                .ignored(decided.parent, decided.name.as_bytes(), decided.kind);
            return None;
        }
        if matches!(decided.decision, Decision::Descend | Decision::Traverse)
            && let Some(watch) = self.watch
        {
            watch.arm_entry(self.root, &decided);
        }
        let stat = decided.stat.as_ref().map(observe::from_walk)?;
        let name = decided.name.as_bytes();
        let revealed = decided.revealed;
        match decided.decision {
            Decision::Skip => None,
            Decision::Descend => {
                self.out.counts.dirs += 1;
                let token = self
                    .out
                    .batch
                    .revealing(revealed)
                    .dir(decided.parent, name, stat);
                self.promote_changed_parent(token, &stat, decided.path);
                Some(token)
            }
            Decision::Traverse => {
                self.out.counts.traversed += 1;
                let token =
                    self.out
                        .batch
                        .revealing(revealed)
                        .traversed_dir(decided.parent, name, stat);
                self.promote_changed_parent(token, &stat, decided.path);
                Some(token)
            }
            Decision::Catalog(Reason::Symlink) => {
                let target = decided.stat.and_then(|s| s.link_target)?;
                self.out.counts.symlinks += 1;
                self.out.batch.revealing(revealed).symlink(
                    decided.parent,
                    name,
                    stat,
                    target.as_bytes(),
                );
                None
            }
            Decision::Catalog(Reason::TooLarge) => {
                self.out.counts.files += 1;
                self.out.batch.revealing(revealed).file(
                    decided.parent,
                    name,
                    stat,
                    Content::Unindexed,
                );
                None
            }
            Decision::Catalog(Reason::Special) => {
                self.out.counts.specials += 1;
                self.out.batch.revealing(revealed).file(
                    decided.parent,
                    name,
                    stat,
                    Content::Unindexed,
                );
                None
            }
            Decision::Index => {
                self.out.counts.files += 1;
                self.index_file(&decided, stat);
                None
            }
        }
    }

    /// Records an entered directory's work tree and entry count.
    fn entered(
        &mut self,
        dir: DirToken,
        work_tree: Option<crate::walk::WorkTree<'_>>,
        entries: Option<u32>,
    ) {
        #[cfg(test)]
        hook(self.root, Probe::Entered);
        if let Some(wt) = work_tree {
            let kind = match wt.kind {
                WorkTreeKind::Main => ferret_catalog::WorkTreeKind::Main,
                WorkTreeKind::Linked => ferret_catalog::WorkTreeKind::Linked,
                WorkTreeKind::Submodule => ferret_catalog::WorkTreeKind::Submodule,
            };
            self.out.batch.work_tree(
                dir,
                kind,
                wt.common_dir.as_os_str().as_bytes(),
                wt.common_id,
            );
        }
        if let Some(count) = entries {
            self.out.batch.entry_count(dir, count);
        }
    }

    /// Counts a vanished entry, or records the fault.
    fn io(&mut self, path: &Path, op: IoOp, context: FaultContext<'_, DirToken>, error: io::Error) {
        let on_root = matches!(context, FaultContext::Root);
        if op == IoOp::Lstat && !on_root && error.kind() == io::ErrorKind::NotFound {
            self.out.counts.vanished += 1;
        } else {
            self.fault(path, op, context, error);
        }
    }

    /// Counts an ignore-file pattern error and keeps its message while the
    /// budget allows.
    fn pattern(&mut self, error: ferret_policy::PatternError) {
        self.out.counts.pattern_errors += 1;
        let ferret_policy::PatternError::Line {
            file,
            pattern,
            detail,
            ..
        } = &error;
        let path_bytes = match file {
            ferret_policy::IgnoreFile::Ferret(path)
            | ferret_policy::IgnoreFile::Git(path)
            | ferret_policy::IgnoreFile::GitExclude(path)
            | ferret_policy::IgnoreFile::Dot(path) => path.as_os_str().len(),
            ferret_policy::IgnoreFile::Global => 0,
        };
        // Lossy path display can expand each invalid byte to three;
        // fixed overhead also covers the source label and line number.
        if self.out.reserve(
            std::mem::size_of::<String>() + 64 + path_bytes * 3 + pattern.len() + detail.len(),
        ) {
            self.out.pattern_errors.push(error.to_string());
        }
    }
}

/// Test seam: where a worker is in [`index`], for tests that must act
/// between two steps. Hooks are registered per root path, so parallel tests
/// see only their own walks, on whichever worker the event happens.
#[cfg(test)]
pub(crate) enum Probe<'a> {
    /// Carried content and recorded its observation, before the next entry.
    Carried(&'a Path),
    /// A directory was listed; its children have not been statted.
    Entered,
    /// Claimed an inode in the cache, before reading it.
    Claimed,
    /// Met an inode in flight and deferred this name.
    Deferred,
    /// Hashed a file, before the second `fstat`.
    Hashed(&'a Path),
    /// Read a file (after the cache was completed).
    Read(&'a Path),
}

#[cfg(test)]
pub(crate) type ProbeHook = std::sync::Arc<dyn Fn(Probe<'_>) + Send + Sync>;

#[cfg(test)]
pub(crate) static PROBES: std::sync::Mutex<Vec<(PathBuf, ProbeHook)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn hook(root: &Path, probe: Probe<'_>) {
    let found = crate::lock(&PROBES)
        .iter()
        .find(|(r, _)| r == root)
        .map(|(_, hook)| std::sync::Arc::clone(hook));
    if let Some(hook) = found {
        hook(probe);
    }
}
