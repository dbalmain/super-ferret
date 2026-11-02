//! Listing a directory onto the walk's child stack: getdents for a live
//! directory, stored entries for a catalogued one.

use std::fs;
use std::os::unix::fs::MetadataExt;

use ferret_catalog::Contents;
use rustix::fs::{FileType as RawType, RawDir, openat, statat};

use super::*;

/// getdents64 buffer. One call reads a typical directory whole; 8 KiB took
/// three calls per directory on the timing tree.
const LISTING_BUFFER: usize = 64 * 1024;

/// A child read from a directory: its name's range in [`Children`]' buffer.
#[derive(Clone, Copy, Debug)]
pub(super) struct Child {
    name: (usize, usize),
    pub(super) kind: Option<FileKind>,
    pub(super) target: Option<Target>,
    pub(super) parent: Option<InoId>,
    pub(super) id: Option<NameId>,
}

/// The walk's child stack: every open level's children in order, their names
/// in one buffer.
#[derive(Default)]
pub(super) struct Children {
    rows: Vec<Child>,
    names: Vec<u8>,
}

impl Children {
    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }

    fn names_len(&self) -> usize {
        self.names.len()
    }

    fn push(
        &mut self,
        name: &[u8],
        kind: Option<FileKind>,
        target: Option<Target>,
        parent: Option<InoId>,
        id: Option<NameId>,
    ) {
        let start = self.names.len();
        self.names.extend_from_slice(name);
        self.rows.push(Child {
            name: (start, self.names.len()),
            kind,
            target,
            parent,
            id,
        });
    }

    pub(super) fn name(&self, child: &Child) -> &[u8] {
        &self.names[child.name.0..child.name.1]
    }

    /// Drops every child from row `rows` and every name from byte `names`.
    pub(super) fn truncate(&mut self, rows: usize, names: usize) {
        self.rows.truncate(rows);
        self.names.truncate(names);
    }

    /// Sorts the children from row `start` by name.
    fn sort_from(&mut self, start: usize) {
        let names = &self.names;
        self.rows[start..]
            .sort_unstable_by(|a, b| names[a.name.0..a.name.1].cmp(&names[b.name.0..b.name.1]));
    }

    /// The children in `rows` as a fresh stack, names and all.
    pub(super) fn copy(&self, rows: std::ops::Range<usize>) -> Self {
        let mut copy = Self::default();
        for child in &self.rows[rows] {
            copy.push(
                self.name(child),
                child.kind,
                child.target,
                child.parent,
                child.id,
            );
        }
        copy
    }
}

/// Where one directory's children begin on the stack, and how many entries
/// its listing has read.
struct Listing {
    start: usize,
    names_start: usize,
    seen: usize,
}

impl Listing {
    fn begin(children: &Children) -> Self {
        Listing {
            start: children.len(),
            names_start: children.names_len(),
            seen: 0,
        }
    }

    /// Counts one listed entry and, every `CANCELLATION_INTERVAL` entries,
    /// asks whether the walk was cancelled.
    fn cancelled_at(&mut self, cancelled: &mut dyn FnMut() -> bool) -> bool {
        self.seen += 1;
        self.seen.is_multiple_of(CANCELLATION_INTERVAL) && cancelled()
    }

    /// Takes this listing's children back off the stack.
    fn undo(&self, children: &mut Children) {
        children.truncate(self.start, self.names_start);
    }
}

impl std::ops::Index<usize> for Children {
    type Output = Child;

    fn index(&self, index: usize) -> &Child {
        &self.rows[index]
    }
}

impl LiveWalk {
    /// Lists the lent entry's directory and makes it the innermost level.
    pub(super) fn descend(
        &mut self,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<(), WalkError> {
        let entry = &mut self.entry;
        if self
            .options
            .max_depth
            .is_some_and(|max| entry.state.depth >= max)
        {
            return Ok(());
        }
        if entry.kind().map_err(|error| entry.error(error))? != FileKind::Directory {
            return Ok(());
        }
        if self.options.xdev
            && entry.stat().map_err(|error| entry.error(error))?.dev() != self.root_dev
        {
            return Ok(());
        }
        if let Some(catalog) = &self.catalog
            && let Some(target @ Target::Inode(dir)) = entry.state.target
            && catalog.contents(target) == Some(Contents::Catalogued)
        {
            return self.list_catalogued(dir, before_directory, cancelled);
        }
        self.list_directory(before_directory, cancelled)
    }

    /// Opens the lent entry's directory to list it. On failure a depth-first
    /// walk still visits the entry after its (unlisted) children.
    fn open_listing(&mut self) -> Result<Arc<File>, WalkError> {
        match open_directory(&self.entry) {
            Ok(handle) => Ok(handle),
            Err(error) => {
                if self.options.depth_first {
                    self.slot = Slot::After;
                }
                Err(self.entry.error(error))
            }
        }
    }

    /// Lists catalogued directory `dir` from its stored entries, checking
    /// each name live where the walk's actions can remove one.
    fn list_catalogued(
        &mut self,
        dir: InoId,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<(), WalkError> {
        let handle = if self.options.live_checks {
            Some(self.open_listing()?)
        } else {
            None
        };
        let entry = &mut self.entry;
        let Some(catalog) = &self.catalog else {
            unreachable!("descend found the catalog")
        };
        let mut listing = Listing::begin(&self.children);
        for child in catalog.entries(dir) {
            #[cfg(test)]
            if let Some(listed) = &self.on_listed {
                listed();
            }
            if listing.cancelled_at(cancelled) {
                // `catalog` is still borrowed here, so cancel_listing's
                // `&mut self` is out of reach.
                listing.undo(&mut self.children);
                self.cancelled = true;
                return Ok(());
            }
            if matches!(child.target, Target::Ignored(_)) {
                continue;
            }
            if child.kind != Kind::Dir
                && !(self.options.follow == Follow::All && child.kind == Kind::Symlink)
                && self
                    .options
                    .guard
                    .as_ref()
                    .is_some_and(|guard| !guard.matches(catalog_kind(child.kind), child.bytes))
            {
                continue;
            }
            // Test membership before child actions. A later sibling
            // removal must not erase a name already observed here. This
            // applies to a cataloged directory child too (#7's repro):
            // an earlier explicit start's own `-delete` can remove a
            // directory another start is about to descend into, and the
            // catalog's stored listing has no way to know that already
            // happened - GNU's live readdir simply never lists it.
            if self.options.live_checks {
                let missing = missing_now(entry, handle.as_ref(), child.bytes);
                let removed_here = matches!(child.target, Target::Inode(id) if self.removed_children.as_ref().is_some_and(|removed| lock(removed).entries.contains(&id)));
                if missing && (child.kind != Kind::Dir || removed_here) {
                    continue;
                }
            }
            self.children.push(
                child.bytes,
                Some(catalog_kind(child.kind)),
                Some(child.target),
                Some(dir),
                Some(child.name),
            );
        }
        // Nested roots have no name row in the outer crawl (D34).
        let first = self
            .nested_roots
            .partition_point(|(parent, _, _)| *parent < dir);
        let roots = self.nested_roots[first..]
            .iter()
            .take_while(|(parent, _, _)| *parent == dir);
        let mut added = false;
        for (_, id, name) in roots {
            #[cfg(test)]
            if let Some(listed) = &self.on_listed {
                listed();
            }
            if listing.cancelled_at(cancelled) {
                return self.cancel_listing(&listing);
            }
            added = true;
            self.children.push(
                name,
                Some(FileKind::Directory),
                Some(Target::Inode(*id)),
                Some(dir),
                None,
            );
        }
        if cancelled() {
            return self.cancel_listing(&listing);
        }
        if added {
            self.children.sort_from(listing.start);
        }
        if catalog.has_children(dir) == Some(true) || added {
            before_directory().map_err(|error| entry.error(error))?;
        }
        let path_len = entry.path.len();
        let separator = !entry.path.ends_with(b"/");
        let own = Saved::take(entry);
        self.levels.push(Level {
            catalogued: true,
            pending: None,
            handle: if self.options.retain_parent {
                handle.map(DirectoryHandle::uncharged)
            } else {
                None
            },
            identity: None,
            path_len,
            separator,
            start: listing.start,
            next: listing.start,
            end: self.children.len(),
            names_start: listing.names_start,
            first: None,
            hidden: Hidden::Unrecorded,
            own,
        });
        Ok(())
    }

    /// Lists the lent entry's directory with getdents.
    fn list_directory(
        &mut self,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<(), WalkError> {
        let handle = self.open_listing()?;
        let entry = &mut self.entry;
        let path_len = entry.path.len();
        let separator = !entry.path.ends_with(b"/");
        let follow = self.options.follow == Follow::All;
        let mut listing = Listing::begin(&self.children);
        let mut first = None;
        let mut hidden = self.options.hidden_names.then(Vec::new);
        if self.buffer.is_empty() {
            self.buffer.resize(LISTING_BUFFER, MaybeUninit::uninit());
        }
        let mut reader = RawDir::new(&*handle, &mut self.buffer);
        while let Some(item) = reader.next() {
            let item = match item {
                Ok(item) => item,
                Err(error) => {
                    listing.undo(&mut self.children);
                    return Err(WalkError {
                        path: entry.path().to_owned(),
                        error: error.into(),
                    });
                }
            };
            #[cfg(test)]
            if let Some(listed) = &self.on_listed {
                listed();
            }
            if listing.cancelled_at(cancelled) {
                return self.cancel_listing(&listing);
            }
            let name = item.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            let kind = raw_kind(item.file_type());
            let target = None;
            #[cfg(test)]
            let kind = if self.force_unknown { None } else { kind };
            // GNU observes the first child's directory metadata before a cwd
            // batch flush, but later directories are checked when visited.
            // Removed directories can therefore print and still fault at a
            // depth limit; regular file metadata remains lazy.
            if target.is_none()
                && kind == Some(FileKind::Directory)
                && self.children.len() == listing.start
                && !self.options.lazy_directories
            {
                // Observed relative to the handle just opened for this
                // listing, not the entry's pathname: an action run while
                // listing (e.g. `-exec` renaming an ancestor) can make the
                // pathname stale before this first child is even visited.
                let name = Path::new(OsStr::from_bytes(name));
                first = Some(metadata_at(handle.as_fd(), name, follow).map_err(Arc::new));
            }
            if let Some(hidden) = hidden.as_mut().filter(|_| name.starts_with(b".")) {
                hidden.extend_from_slice(name);
                hidden.push(0);
            }
            self.children.push(name, kind, target, None, None);
        }
        if cancelled() {
            return self.cancel_listing(&listing);
        }
        if self.children.len() > listing.start
            && let Err(error) = before_directory()
        {
            listing.undo(&mut self.children);
            return Err(WalkError {
                path: entry.path().to_owned(),
                error,
            });
        }
        let own = Saved::take(entry);
        let (handle, identity) = match retain_listed(&self.options, handle) {
            Ok(kept) => kept,
            Err(error) => {
                listing.undo(&mut self.children);
                own.restore(entry);
                return Err(entry.error(error));
            }
        };
        self.levels.push(Level {
            catalogued: false,
            pending: None,
            handle,
            identity,
            path_len,
            separator,
            start: listing.start,
            next: listing.start,
            end: self.children.len(),
            names_start: listing.names_start,
            first,
            hidden: match hidden {
                None => Hidden::Unrecorded,
                Some(names) if names.is_empty() => Hidden::Empty,
                Some(names) => Hidden::Names(Arc::from(names)),
            },
            own,
        });
        Ok(())
    }

    /// Abandons a listing the walk was cancelled during.
    fn cancel_listing(&mut self, listing: &Listing) -> Result<(), WalkError> {
        listing.undo(&mut self.children);
        self.cancelled = true;
        Ok(())
    }
}

fn raw_kind(value: RawType) -> Option<FileKind> {
    Some(match value {
        RawType::RegularFile => FileKind::File,
        RawType::Directory => FileKind::Directory,
        RawType::Symlink => FileKind::Symlink,
        RawType::Fifo => FileKind::Fifo,
        RawType::Socket => FileKind::Socket,
        RawType::BlockDevice => FileKind::Block,
        RawType::CharacterDevice => FileKind::Character,
        RawType::Unknown => return None,
    })
}

/// Whether `name`, a catalogued child of the entry's directory, is gone:
/// looked up through `handle` when the listing opened one, else by path.
fn missing_now(entry: &mut Entry, handle: Option<&Arc<File>>, name: &[u8]) -> bool {
    let own_len = entry.path.len();
    if !entry.path.ends_with(b"/") {
        entry.path.push(b'/');
    }
    entry.path.extend_from_slice(name);
    let missing = match handle {
        Some(directory) => statat(
            directory,
            OsStr::from_bytes(name),
            AtFlags::SYMLINK_NOFOLLOW,
        )
        .is_err_and(|error| error == rustix::io::Errno::NOENT),
        None => fs::symlink_metadata(entry.lookup_path().as_ref())
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound),
    };
    entry.path.truncate(own_len);
    missing
}

/// A listed directory's kept handle, or the `(dev, ino)` to reopen it by.
type Retained = (Option<DirectoryHandle>, Option<(u64, u64)>);

/// What a listed directory's level keeps of its `handle`: the handle while
/// the walk retains parents and, when they are bounded, a charge is free;
/// else its identity, to reopen it by; else nothing.
///
/// A pure, action-free walk never needs this directory open past its own
/// listing - no -delete/-execdir will ask a child for its parent's fd.
/// Dropping it here, rather than carrying it for the whole subtree the way
/// an effectful walk must, bounds live fd use by width instead of depth
/// (#10): a 100-level chain under a tight RLIMIT_NOFILE no longer exhausts
/// descriptors one per ancestor.
fn retain_listed(options: &Options, handle: Arc<File>) -> io::Result<Retained> {
    Ok(if !options.retain_parent {
        drop(handle);
        (None, None)
    } else if !options.bounded_parents {
        (Some(DirectoryHandle::uncharged(handle)), None)
    } else if let Some(charge) = Charge::take() {
        (Some(DirectoryHandle::charged(handle, charge)), None)
    } else {
        let stat = handle.metadata()?;
        (None, Some((stat.dev(), stat.ino())))
    })
}

fn open_directory(entry: &Entry) -> io::Result<Arc<File>> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let flags = if entry.state.follow {
        flags
    } else {
        flags | OFlags::NOFOLLOW
    };
    let opened = entry.with_observed_at(|dir, path| openat(dir, path, flags, Mode::empty()))?;
    Ok(Arc::new(File::from(opened)))
}
