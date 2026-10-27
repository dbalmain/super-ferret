//! Which entries a walk searches: the per-entry filter, and the serial operand
//! source that feeds [`run`](super::run).

use super::super::visibility::{Visibility, Visit};
use super::job::Job;
use super::*;
use std::sync::Mutex;

/// Index candidates: the documents that may hold a match.
pub(in super::super) trait Candidates: Sync {
    fn contains(&self, doc: u32, stopped: &dyn Fn() -> bool) -> bool;
}

impl Candidates for ferret_index::DocSet {
    fn contains(&self, doc: u32, _stopped: &dyn Fn() -> bool) -> bool {
        ferret_index::DocSet::contains(self, doc)
    }
}

impl Candidates for engine::GrepCandidates<'_> {
    fn contains(&self, doc: u32, stopped: &dyn Fn() -> bool) -> bool {
        engine::GrepCandidates::contains(self, doc, stopped)
    }
}

/// The per-entry selection every walk applies: depth, glob, hidden and
/// ignore filters, live policy, then the catalog's view of the file.
pub(in super::super) struct Filter<'a> {
    catalog: Option<&'a ferret_catalog::Catalog>,
    candidates: Option<&'a dyn Candidates>,
    emit_absent: bool,
    pub(super) traversal: line_grep::Traversal,
    text: bool,
    selection: &'a GrepSelection,
    pub(super) cwd: PathBuf,
}

impl Filter<'_> {
    /// The job a walked entry yields, if any. Sets `descend` for a directory
    /// and `searched` for a selected file (D99). `publication`, when given,
    /// is held while printing a deferred policy's warnings.
    pub(super) fn entry(
        &self,
        entry: &ferret_query::find::Entry,
        descend: &mut bool,
        policy: Option<&mut live::Policy>,
        searched: &mut bool,
        publication: Option<&Mutex<()>>,
        stopped: &dyn Fn() -> bool,
    ) -> Option<(u64, Job)> {
        let visibility = Visibility {
            selection: self.selection,
            revealed: self.catalog.filter(|_| self.traversal.rg),
            cwd: &self.cwd,
            max_depth: self.traversal.max_depth,
            content: true,
        };
        let uncovered = policy.as_ref().is_some_and(|policy| policy.uncovered());
        // A `-u` walk never consults the index, so it reads once as an
        // uncovered scan does; `--live` keeps the checked read (D123).
        let once = uncovered || (self.traversal.unrestricted && !self.traversal.live);
        let mut policy = policy;
        let visit = visibility.select(entry, policy.as_deref_mut(), stopped);
        if let Some(policy) = policy {
            let warnings = policy.take_warnings();
            if !warnings.is_empty() {
                // Not inside another file's group.
                let _publication = publication.map(lock);
                for warning in warnings {
                    eprintln!("{warning}");
                }
            }
        }
        match visit {
            Ok(Visit::File) => {}
            Ok(Visit::Descend) => {
                *descend = true;
                return None;
            }
            Ok(Visit::Skip) => {
                *descend = false;
                return None;
            }
            Err(refusal) => {
                *descend = false;
                return Some((0, Job::Failed(refusal.path, refusal.error.to_string())));
            }
        }
        *searched = true;
        if let (Some(catalog), Some(name)) = (self.catalog, entry.catalog_name()) {
            let inode = catalog.name(name).child;
            let Some(doc) = catalog.doc(inode) else {
                if catalog.inode(inode).stat.size > ferret_policy::DEFAULT_SIZE_CAP
                    || self.text && catalog.state(inode) == ferret_catalog::ContentState::Binary
                {
                    return Some((
                        catalog.inode(inode).stat.size,
                        Job::Unindexed(entry.path().to_owned(), name),
                    ));
                }
                return None;
            };
            if self
                .candidates
                .is_some_and(|candidates| !candidates.contains(doc.0, stopped))
            {
                if self.emit_absent {
                    return Some((0, Job::Absent(entry.path().to_owned())));
                }
                return None;
            }
            return Some((
                catalog.inode(inode).stat.size,
                Job::Checked(entry.path().to_owned(), name, doc.0),
            ));
        }
        if self.traversal.follow {
            return Some((
                0,
                Job::Listed(
                    entry.path().to_owned(),
                    self.cwd.join(entry.path()),
                    engine::reader::Parent::Followed,
                    true,
                    once,
                ),
            ));
        }
        // Content walks retain parents or, past their budget, the parent's
        // identity; every name a catalogued listing, which opens no
        // directory, yields is catalogued.
        let parent = match (entry.directory_handle(), entry.directory_identity()) {
            (Some(directory), _) => engine::reader::Parent::Open(directory),
            (None, Some((dev, ino))) => engine::reader::Parent::Closed(dev, ino),
            (None, None) => {
                return Some((
                    0,
                    Job::Failed(
                        entry.path().to_owned(),
                        "walked without its directory".into(),
                    ),
                ));
            }
        };
        Some((
            0,
            Job::Listed(
                entry.path().to_owned(),
                self.cwd.join(entry.path()),
                parent,
                uncovered,
                // Uncovered scans read once, as rg does (D123).
                once,
            ),
        ))
    }
}

#[cfg(test)]
type OnEntry = dyn FnMut(&Path) + Send;

/// Workers advance the source before reserving input bytes for their next file.
/// It retains find's prune feedback and live fallback, without a path backlog.
pub(in super::super) struct Source<'a> {
    pub(super) inputs: std::vec::IntoIter<Input>,
    source: Option<sources::Directory>,
    descend: bool,
    pub(super) filter: Filter<'a>,
    pub(super) live: Option<&'a live::Anchors>,
    policy: Option<live::Policy>,
    searched: bool,
    #[cfg(test)]
    pub(super) on_entry: Option<Box<OnEntry>>,
    #[cfg(test)]
    pub(super) on_listing_poll: Option<Box<dyn FnMut() + Send>>,
}
impl<'a> Source<'a> {
    pub fn new(
        inputs: Vec<Input>,
        catalog: Option<&'a ferret_catalog::Catalog>,
        candidates: Option<&'a dyn Candidates>,
        selection: &'a GrepSelection,
        cwd: PathBuf,
    ) -> Self {
        Self {
            inputs: inputs.into_iter(),
            source: None,
            descend: true,
            filter: Filter {
                catalog,
                candidates,
                emit_absent: false,
                traversal: Default::default(),
                text: false,
                selection,
                cwd,
            },
            live: None,
            policy: None,
            searched: false,
            #[cfg(test)]
            on_entry: None,
            #[cfg(test)]
            on_listing_poll: None,
        }
    }

    pub(in super::super) fn with_live(mut self, live: Option<&'a live::Anchors>) -> Self {
        self.live = live;
        self
    }

    pub(in super::super) fn with_traversal(
        mut self,
        traversal: line_grep::Traversal,
        text: bool,
    ) -> Self {
        self.filter.traversal = traversal;
        self.filter.text = text;
        self
    }

    pub(in super::super) fn with_absence(mut self, emit: bool) -> Self {
        self.filter.emit_absent = emit;
        self
    }

    /// Whether a directory operand walks the live tree, which [`walk`] can
    /// spread over workers.
    pub(in super::super) fn walks_live(&self) -> bool {
        self.inputs.as_slice().iter().any(|input| {
            matches!(input, Input::Directory(path) if self.filter.traversal.unrestricted || self.live.is_some_and(|anchors| anchors.walks(path)))
        })
    }

    /// Whether the walk selected any file: it passed depth, glob, hidden and
    /// ignore filters. Index candidate pruning and binary or oversized
    /// documents still count, as rg searches them (D99).
    pub(in super::super) fn searched(&self) -> bool {
        self.searched
    }

    pub fn next(&mut self, stopped: &dyn Fn() -> bool) -> Option<(u64, Job)> {
        loop {
            if stopped() {
                return None;
            }
            if let Some(source) = &mut self.source {
                loop {
                    if stopped() {
                        return None;
                    }
                    let mut cancelled = || {
                        #[cfg(test)]
                        if let Some(hook) = &mut self.on_listing_poll {
                            hook();
                        }
                        stopped()
                    };
                    let entry =
                        source.next_with_cancel(self.descend, &mut || Ok(()), &mut cancelled);
                    if stopped() {
                        return None;
                    }
                    let Some(entry) = entry else {
                        break;
                    };
                    self.descend = true;
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error) => {
                            return Some((0, Job::Failed(error.path, error.error.to_string())));
                        }
                    };
                    #[cfg(test)]
                    if let Some(hook) = &mut self.on_entry {
                        hook(entry.path());
                    }
                    if let Some(job) = self.filter.entry(
                        entry,
                        &mut self.descend,
                        self.policy.as_mut(),
                        &mut self.searched,
                        None,
                        stopped,
                    ) {
                        return Some(job);
                    }
                }
                self.source = None;
            }
            match self.inputs.next()? {
                Input::File(path, size) => {
                    return Some((size, Job::Direct(path.clone(), self.filter.cwd.join(path))));
                }
                Input::Failed(path, error) => {
                    return Some((0, Job::Failed(path, error.to_string())));
                }
                Input::Directory(path) => {
                    self.policy = match self
                        .live
                        .filter(|anchors| anchors.walks(&path))
                        .map(|anchors| anchors.start(&path, stopped))
                        .transpose()
                    {
                        Ok(policy) => policy,
                        Err(error) => return Some((0, Job::Failed(path, error.to_string()))),
                    };
                    match sources::Directory::new(
                        &path,
                        self.filter.catalog,
                        &self.filter.cwd,
                        self.filter.traversal.unrestricted,
                        self.policy.is_some(),
                    ) {
                        Ok(source) => {
                            self.source = Some(
                                source
                                    .following_links(self.filter.traversal.follow)
                                    .retaining_parents(),
                            )
                        }
                        Err(error) => return Some((0, Job::Failed(path, error.to_string()))),
                    }
                    self.descend = true;
                }
                // The parent splits operands at stdin barriers before scheduling.
                Input::Stream(_) => return None,
            }
        }
    }
}
