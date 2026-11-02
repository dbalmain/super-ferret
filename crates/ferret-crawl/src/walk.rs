//! One root, walked by directory handles, with one [`DirRules`] per directory.
//!
//! Seam: [`ferret_policy`] decides what to do with each entry. This module
//! reads the tree and reports. It does not touch the catalog.
//!
//! The root is opened by the path the caller gave, following a symlink there
//! (that path is the user's), with `O_DIRECTORY`. Every later open, stat,
//! `readlink` and ignore-file read is relative to a directory descriptor.
//! Each worker keeps parent continuations in a local LIFO stack. When another
//! worker is idle it shares the oldest continuation through a mutex and
//! `Condvar`. Jobs in local stacks and the shared queue hold at most 128
//! directory descriptors combined. Beyond that, a continuation closes its
//! descriptor and reopens by checked `openat` steps from the root when resumed.
//! Each worker holds one active job and at most three extra descriptors while
//! opening a child and reading `.git/info/exclude`. Thus a parallel walk with N
//! workers holds at most 128 + 4N descriptors, independent of depth and width.
//! The git directory closes as soon as `info` opens, and `info` closes as soon
//! as `exclude` opens, so five worker descriptors never overlap.
//! `EMFILE` opening a child is an [`Event::Io`] and the walk continues. There
//! is no path-based fallback below the root.
//! Listings store names in one byte buffer with offsets, and a worker reuses
//! one root-relative byte path, restoring its length after each entry.
//!
//! A catalogued symlink is one observation: `openat` with `O_PATH |
//! O_NOFOLLOW`, then `fstat` and `readlinkat` on that descriptor (an empty
//! path, which on Linux reads the link the descriptor refers to). The stat
//! in the event and the stored target cannot disagree.

use std::ffi::OsStr;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::{io, thread};

use ferret_policy::{Config, Decision, DirRules, PatternError};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, fstat, open as open_path, statat};
use rustix::io::Errno;

mod children;
mod enter;
mod events;
mod ignores;
mod listing;
mod schedule;

pub use events::{
    Boundary, Decided, Event, FaultContext, IoOp, Stat, WalkOptions, WorkTree, WorkTreeKind,
};
use ignores::FoundWorkTree;
pub use ignores::{Listed, load_grep_policy, load_policy};
use listing::Children;
use schedule::{Shared, run_guarded, run_worker};

/// An ignore file larger than this is an [`Event::Io`] and counts as absent.
/// One mebibyte is past any ignore file this tree has seen; the bound is what
/// stops a hostile or accidental giant from being pulled into the walk.
const MAX_IGNORE_BYTES: u64 = 1 << 20;

/// Walks one configured root on the calling thread, with no boundaries.
///
/// `global` is the text of the global ignore file, or `None` when there is no
/// such file (nothing is excluded by default). `visit` is called for every
/// entry and every fault as the walk goes, so a catalog can record about a
/// million entries without this function retaining them. A callback is the
/// shape that allows that: the root-relative path is one reused buffer, and a
/// lending iterator is not expressible in stable Rust. Its directory token is
/// `()`, and it enters every directory the policy enters; a visitor that needs
/// tokens or boundaries uses [`walk_parallel`] with one worker.
///
/// The root directory itself is not a [`Decided`] event. It is where the walk
/// starts, not an entry `decide` sees; [`EventVisitor::root`] receives its
/// stat. Faults that belong to the root use an empty path.
///
/// Ignore files are read when the walk enters a directory
/// ([`Decision::Descend`]). A directory reached only so a `.ferretignore` `!`
/// pattern can re-include beneath it ([`Decision::Traverse`]) is listed, and
/// its ignore files are not read (D13). `.gitignore` is read only inside a
/// work tree, where its rules can apply. A configured root is self-contained:
/// `.gitignore` and `info/exclude` above it do not apply (D22). A `.gitignore`
/// applies at the root only when the root itself starts a work tree.
/// `.git/info/exclude` applies when a work tree starts at or below the root.
/// A `.git` file's first line (`gitdir: <path>`, relative to that directory)
/// points to the gitdir. Exclude comes from that gitdir or its `commondir`,
/// when present, as in a linked work tree. Nothing is opened through a
/// symlinked `.git`.
///
/// `.ferretignore`, `.gitignore` and `info/exclude` are opened without blocking
/// (`O_NONBLOCK`) and read only when that open file is a regular file of at
/// most 1 MiB. A larger file is an [`Event::Io`] and is then treated as
/// absent. A missing file, or an opened inode that is not a regular file, is
/// absent and not a fault: a FIFO or directory of one of these names must not
/// stall the walk. A name that cannot be opened at all (a socket, say) is a
/// fault. A regular file that exists but cannot be read is a fault, and is
/// then treated as absent. Bytes that are not UTF-8 are converted lossily. A
/// symlinked `.gitignore` is absent in descended directories, as git
/// disregards it; a symlinked `.ferretignore` or `exclude` is followed; `info`
/// is not, and neither is `.git`.
///
/// The listing is a snapshot. An ignore file created after the directory was
/// listed is not seen until the next crawl, so it does not apply to the
/// siblings listed with it. A listed name that is gone by the time it is
/// opened is absent.
///
/// File types come from the directory entry (`d_type`, or `statat` with
/// `AT_SYMLINK_NOFOLLOW` when the filesystem leaves the type unknown).
/// `d_type` does not follow symlinks. Once a stat has run, its type is the
/// truth and the entry is classified again when the listing disagreed: the
/// catalog stores that inode, so the decision should describe it. A name
/// skipped from `d_type` alone is not statted. Regular files are statted so
/// `decide` can see the size. Anything that is not skipped is statted for the
/// catalog fields. A catalogued symlink is opened `O_PATH | O_NOFOLLOW` and
/// both the stat and the target come from that descriptor. If the stat or
/// `readlink` fails, the walk emits [`Event::Io`] and no [`Event::Decided`].
///
/// A directory that is descended or traversed is opened with `O_DIRECTORY |
/// O_NOFOLLOW` after its `Decided` event and `fstat`ed. A different `(dev,
/// ino)` from the stat `decide` was given, or a symlink where the directory
/// was, is an [`Event::Io`] and the directory is not listed.
///
/// Mount points are crossed: the walk does not compare `st_dev` with the
/// root. See the module docs for the descriptor bound.
pub fn walk(root: &Path, global: Option<&str>, config: Config, visit: impl FnMut(Event<'_, ()>)) {
    let boundaries = BoundaryIndex::default();
    let mut walker = Walker::new(root, &boundaries, visit);
    let Some(root_job) = walker.root_job(root, global, config) else {
        return;
    };
    run_worker(walker, &Shared::new(root_job));
}

/// A worker-local visitor. The returned visitors retain their accumulated
/// state, so callers can merge it after the walk without locking per entry.
///
/// # Directory lifecycle
///
/// Each directory the walk enters is named by a token of the visitor's
/// choosing, [`EventVisitor::Dir`] (`Copy + Send`; D29). The walk stores the
/// token in its per-directory job and hands it back with every event about
/// that directory's entries, so a visitor can attach a child to its parent
/// without a path map, whichever worker reports the child.
///
/// ```text
/// root:   open, fstat -> root(stat) = token
///         list, read ignore files, probe .git -> Entered { dir: token, work_tree }
/// child:  Decided { parent, .. }  (before any open)
///           returns Some(token) for Descend / Traverse; None prunes it
///         Descend:  open, fstat, list, ignore files, .git -> Entered { dir, work_tree }
///         Traverse: open, fstat, list                     -> Entered { dir, work_tree: None }
///         its children: Decided { parent: token, .. }, Io, Boundary, ...
/// ```
///
/// - A directory's [`Decided`] fires before it is opened, as for every entry.
///   The value the visitor returns is that directory's token. For a directory
///   the policy would enter ([`Decision::Descend`] or [`Decision::Traverse`])
///   `None` prunes it: it is not opened and nothing below it is reported. For
///   every other event the return value is ignored.
/// - [`Event::Entered`] fires once the directory is open, listed, and, for
///   `Descend`, its ignore files are read and `.git` probed. A directory that
///   has a token but never gets `Entered` was not listed: its coverage is
///   uncertain, and an [`Event::Io`] says why.
/// - The root gets its token from [`EventVisitor::root`], called after the root
///   is opened and statted and before it is listed; then `Entered`.
/// - Every child event carries its parent's token: [`Decided::parent`],
///   [`Event::Boundary`], and [`Event::Io`]'s [`FaultContext`]. Tokens travel
///   with the job across workers, so a child can be reported on a different
///   worker from the one that minted its parent's token.
pub trait EventVisitor {
    /// A directory's token: small, chosen by the visitor, carried by the walk.
    type Dir: Copy + Send;

    /// The root, opened and statted; returns its token. Called once per walk,
    /// on the first visitor, before any other event except a root fault.
    fn root(&mut self, stat: Stat<'_>) -> Self::Dir;

    /// Receives one event; borrowed paths are valid only during this call.
    /// Returns the token for a directory's [`Event::Decided`] (see the
    /// lifecycle above); ignored otherwise.
    fn visit(&mut self, event: Event<'_, Self::Dir>) -> Option<Self::Dir>;

    /// Observed directory handle, before its complete listing. A watch host
    /// arms here so notifications during observation survive into its next run.
    /// Policy input consulted relative to the held parent, before reading.
    /// A path-valued policy dependency outside the observed directory handle.
    fn policy_path(&mut self, _path: &Path) {}

    /// Requires a separate thread even for one worker, so lowering an index
    /// worker's priority cannot affect the caller's query or intake work.
    fn dedicated_worker(&self) -> bool {
        false
    }

    /// Called on the executing worker, including a single-worker walk.
    fn worker_started(&mut self) {}

    fn policy_input(&mut self, _parent: BorrowedFd<'_>, _name: &OsStr) {}

    /// [`EventVisitor::policy_input`] for each of `names`, all in `parent`.
    fn policy_inputs(&mut self, parent: BorrowedFd<'_>, names: &[&OsStr]) {
        for name in names {
            self.policy_input(parent, name);
        }
    }

    fn observing(&mut self, _fd: BorrowedFd<'_>, _path: &Path) {}

    /// Selects work before child stat/open. A false result deliberately keeps
    /// this untouched scope; it is not an ignored or vanished observation.
    fn consider(&mut self, _parent: Self::Dir, _name: &OsStr, _path: &Path) -> bool {
        true
    }
}

/// A closure sees every event, has no tokens (`()`), and enters every
/// directory the policy enters.
impl<F: FnMut(Event<'_, ()>)> EventVisitor for F {
    type Dir = ();

    fn root(&mut self, _stat: Stat<'_>) {}

    fn visit(&mut self, event: Event<'_, ()>) -> Option<()> {
        self(event);
        Some(())
    }
}

/// Most workers [`default_workers`] picks. Warm on a 16-core machine, 16
/// workers walked `$HOME` in 0.097 s against 0.154 s for 8 and 0.090 s for 32,
/// and cold in 2.50 s against 3.08 s and 2.60 s; 32 cost 39% more CPU than 16
/// for no gain (D24).
const MAX_DEFAULT_WORKERS: usize = 16;

/// The worker count to use when the user sets none: the machine's available
/// parallelism, capped at 16 (D24).
pub fn default_workers() -> usize {
    std::thread::available_parallelism().map_or(1, |threads| threads.get().min(MAX_DEFAULT_WORKERS))
}

/// Walks with `options.workers` worker-local visitors and returns them for
/// merging. Event order is unspecified across workers. When the root fails
/// before threads start, only its fault visitor is returned. Directories in
/// `options.boundaries` are reported as [`Event::Boundary`] and not entered.
/// See [`walk`] for what is read and when, and [`EventVisitor`] for the
/// directory lifecycle.
pub fn walk_parallel<V: EventVisitor + Send>(
    root: &Path,
    global: Option<&str>,
    config: Config,
    options: &WalkOptions,
    make_visitor: impl Fn() -> V + Sync,
) -> Vec<V> {
    let count = options.workers.max(1);
    let index = BoundaryIndex::new(&options.boundaries);
    let boundaries = &index;
    let mut first = Walker::new(root, boundaries, make_visitor());
    let Some(root_job) = first.root_job(root, global, config) else {
        return vec![first.visit];
    };
    let root_id = first.root_id;
    let shared = Shared::new(root_job);
    if count == 1 && !first.visit.dedicated_worker() {
        return vec![run_worker(first, &shared)];
    }
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(count);
        handles.push(scope.spawn(|| run_guarded(first, &shared)));
        for _ in 1..count {
            let visitor = make_visitor();
            let mut walker = Walker::new(root, boundaries, visitor);
            walker.root_id = root_id;
            handles.push(scope.spawn(|| run_guarded(walker, &shared)));
        }
        handles
            .into_iter()
            .map(|handle| match handle.join() {
                Ok(visitor) => visitor,
                Err(panic) => std::panic::resume_unwind(panic),
            })
            .collect()
    })
}

impl<V: EventVisitor> Walker<'_, V> {
    fn root_job(
        &mut self,
        root: &Path,
        global: Option<&str>,
        config: Config,
    ) -> Option<Job<V::Dir>> {
        #[cfg(test)]
        if let Some((op, error)) = inject(root, IoPoint::Root, Path::new("")) {
            self.fail(op, FaultContext::Root, error);
            return None;
        }
        let fd = match open_path(root, root_dir_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(error) => {
                if error == Errno::ACCESS {
                    // A root denied at open still has a catalog row, just as
                    // a denied child does. Follow the user's root symlink.
                    let observed = match open_path(
                        root,
                        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                        Mode::empty(),
                    ) {
                        Ok(fd) => {
                            fstat(&fd).inspect(|_| self.visit.observing(fd.as_fd(), Path::new("")))
                        }
                        Err(_) => statat(rustix::fs::CWD, root, AtFlags::empty()),
                    };
                    match observed {
                        Ok(stat) if file_type(&stat) == FileType::Directory => {
                            self.visit.root(public_stat(&stat, None));
                        }
                        Ok(_) => {}
                        Err(error) => {
                            self.fail(IoOp::Lstat, FaultContext::Root, io::Error::from(error))
                        }
                    }
                }
                self.fail(IoOp::OpenDir, FaultContext::Root, io::Error::from(error));
                return None;
            }
        };
        let root_stat = match fstat(&fd) {
            Ok(stat) => stat,
            Err(error) => {
                self.fail(IoOp::OpenDir, FaultContext::Root, io::Error::from(error));
                return None;
            }
        };
        self.root_id = Some((root_stat.st_dev, root_stat.st_ino));
        let token = self.visit.root(public_stat(&root_stat, None));
        let children = self.list(fd.as_fd(), FaultContext::Root)?;
        let loaded = self.load_ignores(fd.as_fd(), |name| children.contains(name), false, token);
        let (rules, errors) = DirRules::root(root, global, loaded.files(), config);
        self.patterns(errors);
        self.entered(token, loaded.work_tree.as_ref(), children.count());
        Some(Job::new(
            fd,
            rules,
            children,
            self.rel.clone(),
            (root_stat.st_dev, root_stat.st_ino),
            token,
        ))
    }
}

fn root_dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC
}

/// A directory location, with no read of the directory itself. Opening needs
/// no permission on the final directory, so an execute-only `.git` or gitdir
/// still yields a handle; named reads then need search permission. Grep uses
/// this so `info/exclude` can be read by name. The crawl uses
/// [`root_dir_flags`], which requires the directory to be readable.
fn path_dir_flags() -> OFlags {
    OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC
}

fn child_dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn nofollow_file_flags() -> OFlags {
    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// `.ferretignore` and `info/exclude` follow a final symlink, as git does for
/// `exclude`.
fn ignore_flags() -> OFlags {
    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC
}

/// Git disregards a `.gitignore` that is a symlink (it warns "unable to
/// access"), so it is opened `O_NOFOLLOW` and a symlink reads as absent.
fn gitignore_flags() -> OFlags {
    ignore_flags() | OFlags::NOFOLLOW
}

fn link_flags() -> OFlags {
    OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn bytes_path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

const DOT_GIT: &str = ".git";

// ── walk state ──

struct Walker<'b, F> {
    root: PathBuf,
    root_id: Option<(u64, u64)>,
    boundaries: &'b BoundaryIndex,
    /// Root-relative path bytes. Empty at the root. Reused for every entry.
    rel: Vec<u8>,
    /// The `getdents` buffer, reused for every listing: empty, with capacity
    /// [`DENTS_BYTES`].
    dents: Vec<u8>,
    visit: F,
    /// Rg's reading of `.git`, for grep's policy loads. A symlinked `.git`
    /// is followed: one that does not resolve (a loop, a dangling link, or
    /// a component that cannot be traversed) starts no work tree, while an
    /// execute-only directory is opened `O_PATH` so `info/exclude` can be
    /// read by name. A `.git` file's `info/exclude` applies only through a
    /// `commondir`. D13 opens nothing through a symlinked `.git`, counts
    /// any `.git` entry, and reads a gitdir's own exclude.
    rg_git: bool,
}

/// A worker's `getdents` buffer. 32 KiB holds several hundred entries of
/// typical name length, so most directories list in one call.
const DENTS_BYTES: usize = 32 << 10;

/// The directory whose entries are being considered.
struct Here<'a, D> {
    fd: BorrowedFd<'a>,
    rules: &'a DirRules,
    token: D,
}

struct Job<D> {
    dir: Option<OwnedFd>,
    rules: DirRules,
    children: Children,
    next: usize,
    rel: Vec<u8>,
    id: (u64, u64),
    token: D,
}

impl<D> Job<D> {
    fn new(
        dir: OwnedFd,
        rules: DirRules,
        children: Children,
        rel: Vec<u8>,
        id: (u64, u64),
        token: D,
    ) -> Self {
        Self {
            dir: Some(dir),
            rules,
            children,
            next: 0,
            rel,
            id,
            token,
        }
    }
}

/// [`WalkOptions::boundaries`] keyed by root-relative path bytes, so a
/// directory's check is one hash lookup however many boundaries there are.
/// Paths are joined from their normal components with `/`, the form of the
/// walker's own path buffer; one with a root, `..` or a prefix is not
/// root-relative and can never match, so it is left out.
#[derive(Default)]
struct BoundaryIndex {
    by_path: std::collections::HashMap<Vec<u8>, Vec<Option<(u64, u64)>>>,
}

impl BoundaryIndex {
    fn new(boundaries: &[Boundary]) -> Self {
        let mut by_path: std::collections::HashMap<_, Vec<_>> = std::collections::HashMap::new();
        'next: for boundary in boundaries {
            let mut key = Vec::new();
            for component in boundary.path.components() {
                match component {
                    Component::Normal(name) => {
                        if !key.is_empty() {
                            key.push(b'/');
                        }
                        key.extend_from_slice(name.as_bytes());
                    }
                    Component::CurDir => {}
                    Component::RootDir | Component::ParentDir | Component::Prefix(_) => {
                        continue 'next;
                    }
                }
            }
            if !key.is_empty() {
                by_path.entry(key).or_default().push(boundary.id);
            }
        }
        Self { by_path }
    }

    fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }

    /// The identities wanted at `rel`, `None` inside meaning any. Free when
    /// there are no boundaries.
    fn ids(&self, rel: &[u8]) -> Option<&[Option<(u64, u64)>]> {
        if self.by_path.is_empty() {
            return None;
        }
        self.by_path.get(rel).map(Vec::as_slice)
    }
}

#[cfg(test)]
pub(crate) type AfterOpen = Box<dyn FnMut(&Path)>;

#[cfg(test)]
thread_local! {
    /// Runs after the opened policy file's stat and before its bounded read.
    pub(crate) static AFTER_IGNORE_STAT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
    /// Test seam: runs with a child directory's root-relative path after it is
    /// opened and before it is listed, on the worker that opened it. No
    /// visitor callback falls in that window, and it is the one a listing
    /// fault below the root needs. Compiled only into this crate's tests.
    pub(crate) static AFTER_OPEN: std::cell::RefCell<Option<AfterOpen>> =
        const { std::cell::RefCell::new(None) };
}

/// Shared, root-keyed syscall seam for coverage tests across real workers.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IoPoint {
    Root,
    Directory,
    OpenDirectory,
    Child,
    Readlink,
    Listing(usize),
}
#[cfg(test)]
pub(crate) type IoHook =
    std::sync::Arc<dyn Fn(IoPoint, &Path) -> Option<(IoOp, io::Error)> + Send + Sync>;
#[cfg(test)]
pub(crate) static IO_HOOKS: std::sync::Mutex<Vec<(PathBuf, IoHook)>> =
    std::sync::Mutex::new(Vec::new());
#[cfg(test)]
fn inject(root: &Path, point: IoPoint, rel: &Path) -> Option<(IoOp, io::Error)> {
    let hook = crate::lock(&IO_HOOKS)
        .iter()
        .find(|(r, _)| r == root)
        .map(|(_, h)| h.clone());
    hook.and_then(|h| h(point, rel))
}

#[cfg(test)]
fn after_open(rel: &[u8]) {
    AFTER_OPEN.with_borrow_mut(|hook| {
        if let Some(hook) = hook {
            hook(bytes_path(rel));
        }
    });
}

/// A parent's continuation, and the child directory to walk next.
type Descent<D> = (Job<D>, Job<D>);

impl<'b, V: EventVisitor> Walker<'b, V> {
    fn new(root: &Path, boundaries: &'b BoundaryIndex, visit: V) -> Self {
        Self {
            root: root.to_path_buf(),
            root_id: None,
            boundaries,
            rel: Vec::with_capacity(256),
            dents: Vec::with_capacity(DENTS_BYTES),
            visit,
            rg_git: false,
        }
    }

    fn push(&mut self, name: impl AsRef<OsStr>) -> usize {
        let length = self.rel.len();
        if length != 0 {
            self.rel.push(b'/');
        }
        self.rel.extend_from_slice(name.as_ref().as_bytes());
        length
    }

    fn pop(&mut self, length: usize) {
        self.rel.truncate(length);
    }

    fn fail(&mut self, op: IoOp, context: FaultContext<'_, V::Dir>, error: io::Error) {
        self.visit.visit(Event::Io {
            path: bytes_path(&self.rel),
            op,
            context,
            error,
        });
    }

    /// A fault on entry `name` of the directory `parent`.
    fn fail_child(&mut self, op: IoOp, parent: V::Dir, name: &OsStr, error: io::Error) {
        self.fail(op, FaultContext::Child { parent, name }, error);
    }

    fn patterns(&mut self, errors: Vec<PatternError>) {
        for error in errors {
            self.visit.visit(Event::Pattern(error));
        }
    }

    fn entered(&mut self, dir: V::Dir, work_tree: Option<&FoundWorkTree>, entries: Option<u32>) {
        let work_tree = work_tree.map(|found| WorkTree {
            kind: found.kind,
            common_dir: &found.common_dir,
            common_id: found.common_id,
        });
        self.visit.visit(Event::Entered {
            dir,
            work_tree,
            entries,
        });
        #[cfg(test)]
        if let Some((op, error)) = inject(&self.root, IoPoint::Directory, bytes_path(&self.rel)) {
            self.fail(op, FaultContext::Dir(dir), error);
        }
    }

    fn emit(
        &mut self,
        here: &Here<'_, V::Dir>,
        name: &OsStr,
        decision: Decision,
        stat: Option<Stat<'_>>,
        kind: FileType,
    ) -> Option<V::Dir> {
        // Only a dot name can be revealed, so others never ask the rules.
        let revealed = decision != Decision::Skip
            && name.as_bytes().starts_with(b".")
            && here.rules.reveals(name, kind == FileType::Directory);
        self.visit.visit(Event::Decided(Decided {
            parent: here.token,
            parent_fd: here.fd,
            name,
            path: bytes_path(&self.rel),
            decision,
            stat,
            kind: catalog_kind(kind),
            revealed,
        }))
    }

    fn emit_skip(&mut self, here: &Here<'_, V::Dir>, name: &OsStr, kind: FileType) {
        self.emit(here, name, Decision::Skip, None, kind);
    }

    fn emit_stat(
        &mut self,
        here: &Here<'_, V::Dir>,
        name: &OsStr,
        decision: Decision,
        stat: &rustix::fs::Stat,
        target: Option<&OsStr>,
    ) -> Option<V::Dir> {
        self.emit(
            here,
            name,
            decision,
            Some(public_stat(stat, target)),
            file_type(stat),
        )
    }

    fn process(&mut self, mut job: Job<V::Dir>) -> Option<Descent<V::Dir>> {
        self.rel.clone_from(&job.rel);
        if job.dir.is_none() {
            job.dir = self.reopen(bytes_path(&job.rel), job.id, job.token);
        }
        let here = Here {
            fd: job.dir.as_ref()?.as_fd(),
            rules: &job.rules,
            token: job.token,
        };
        while job.next < job.children.entries.len() {
            let child = &job.children.entries[job.next];
            let name = job.children.name(child);
            job.next += 1;
            let length = self.push(name);
            let descended = if self.visit.consider(here.token, name, bytes_path(&self.rel)) {
                self.consider(&here, name, child.kind)
            } else {
                None
            };
            self.pop(length);
            if let Some(descended) = descended {
                return Some((job, descended));
            }
        }
        None
    }
}

fn file_type(stat: &rustix::fs::Stat) -> FileType {
    FileType::from_raw_mode(stat.st_mode)
}

fn public_stat<'a>(stat: &rustix::fs::Stat, target: Option<&'a OsStr>) -> Stat<'a> {
    Stat {
        size: u64::try_from(stat.st_size).unwrap_or(0),
        mtime_sec: stat.st_mtime,
        mtime_nsec: i64::try_from(stat.st_mtime_nsec).unwrap_or(0),
        ctime_sec: stat.st_ctime,
        ctime_nsec: i64::try_from(stat.st_ctime_nsec).unwrap_or(0),
        dev: stat.st_dev,
        ino: stat.st_ino,
        mode: stat.st_mode,
        uid: stat.st_uid,
        gid: stat.st_gid,
        nlink: stat.st_nlink,
        link_target: target,
    }
}

fn catalog_kind(kind: FileType) -> ferret_catalog::Kind {
    match kind {
        FileType::Directory => ferret_catalog::Kind::Dir,
        FileType::Symlink => ferret_catalog::Kind::Symlink,
        FileType::Fifo => ferret_catalog::Kind::Fifo,
        FileType::Socket => ferret_catalog::Kind::Socket,
        FileType::BlockDevice => ferret_catalog::Kind::Block,
        FileType::CharacterDevice => ferret_catalog::Kind::Character,
        _ => ferret_catalog::Kind::File,
    }
}
