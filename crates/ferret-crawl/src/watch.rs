//! Inotify intake is independent of the writer. Locators own parent/name bytes,
//! never catalog ids; a burst resolves them against the writer's current view.
//! The crawl arms through its observed directory handle before listing. Events
//! remain hints: uncertain lifetimes and queue loss request all-root
//! observation.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferret_catalog::{Catalog, InoId};
use rustix::fs::inotify::{self, ReadFlags, WatchFlags};
use rustix::fs::{Mode, OFlags, fstat, openat};
use rustix::io::Errno;

use crate::{RefreshReason, RefreshRequest, RefreshScope, RenameHint};

mod aliases;
mod policy;
use aliases::FileAliases;

const TRAILING: Duration = Duration::from_millis(200);
const MAX_AGE: Duration = Duration::from_secs(1);

/// Kernel per-user limits, shared with other processes. No sysctl is changed.
#[derive(Clone, Debug)]
pub struct Limits {
    pub watches: usize,
    pub instances: usize,
    pub queued_events: usize,
}
impl Limits {
    /// Reads the three Linux per-user inotify limits.
    pub fn read() -> std::io::Result<Self> {
        let read = |name| {
            std::fs::read_to_string(format!("/proc/sys/fs/inotify/{name}"))?
                .trim()
                .parse()
                .map_err(std::io::Error::other)
        };
        Ok(Self {
            watches: read("max_user_watches")?,
            instances: read("max_user_instances")?,
            queued_events: read("max_queued_events")?,
        })
    }
}

/// Intake bounds. Default watch headroom leaves one eighth for other tools.
#[derive(Clone, Debug)]
pub struct Config {
    pub watch_cap: usize,
    pub scopes: usize,
    pub bytes: usize,
}
impl Config {
    /// Reserves one eighth of the kernel watch limit for other applications.
    pub fn from_limits(limits: &Limits) -> Self {
        Self {
            watch_cap: limits.watches.saturating_sub(limits.watches / 8),
            scopes: 100_000,
            bytes: 16 << 20,
        }
    }
}

#[derive(Debug)]
struct Directory {
    identity: (u64, u64),
    parent: Option<Arc<Directory>>,
    basename: Vec<u8>,
    root: Arc<PathBuf>,
}
impl Directory {
    fn path(&self) -> PathBuf {
        let mut names = Vec::new();
        let mut directory = self;
        while let Some(parent) = &directory.parent {
            names.push(directory.basename.as_slice());
            directory = parent;
        }
        let mut path = (*directory.root).clone();
        for name in names.into_iter().rev() {
            path.push(OsStr::from_bytes(name));
        }
        path
    }
    fn charged_bytes(&self) -> usize {
        let mut bytes = self.root.as_os_str().len();
        let mut directory = Some(self);
        while let Some(node) = directory {
            bytes += std::mem::size_of::<Self>() + 16 + node.basename.len();
            directory = node.parent.as_deref();
        }
        bytes
    }
    fn resolve(&self, view: &Catalog) -> Option<InoId> {
        let path = self.path();
        let resolved = view.resolve(path.as_os_str().as_bytes())?;
        let ferret_catalog::Target::Inode(id) = resolved.target else {
            return None;
        };
        if !resolved.remainder.is_empty() {
            return None;
        }
        let inode = view.inode(id);
        (view.is_directory(id) && (inode.stat.dev, inode.stat.ino) == self.identity).then_some(id)
    }
}
#[derive(Debug)]
struct Hint {
    directories: Vec<Arc<Directory>>,
    subtree: bool,
    cookie: u32,
    ambiguous_cookie: bool,
    from: bool,
    to: bool,
}
#[derive(Debug)]
struct State {
    descriptors: BTreeMap<i32, Vec<Arc<Directory>>>,
    identities: BTreeMap<(u64, u64), i32>,
    file_aliases: BTreeMap<(u64, u64), FileAliases>,
    removed: BTreeSet<i32>,
    gaps: BTreeSet<PathBuf>,
    unreliable: BTreeSet<PathBuf>,
    policies: BTreeMap<i32, BTreeMap<(PathBuf, Vec<u8>), u64>>,
    policy_failed: BTreeMap<(PathBuf, PathBuf), u64>,
    policy_epochs: BTreeMap<PathBuf, u64>,
    policy_refreshed: BTreeSet<PathBuf>,
    failed: BTreeSet<(u64, u64)>,
    pending: BTreeMap<(i32, Vec<u8>), Hint>,
    bytes: usize,
    first: Option<Instant>,
    inflight_first: Option<Instant>,
    last: Option<Instant>,
    backstop: Option<(u64, RefreshReason)>,
    scoped_roots: BTreeSet<PathBuf>,
    serial: u64,
    running: bool,
}

/// Shared safe inotify instance. A host must keep calling `drain` on a separate
/// intake thread, including while its single writer is refreshing/compacting.
#[derive(Debug)]
pub struct Watch {
    fd: OwnedFd,
    config: Config,
    state: Mutex<State>,
}
/// Observable intake coverage and pending work, distinct from crawl coverage.
#[derive(Clone, Debug)]
pub struct Status {
    pub installed: usize,
    pub needed: usize,
    pub failed: usize,
    pub pending: usize,
    pub bytes: usize,
    pub polling_roots: Vec<PathBuf>,
    pub oldest: Option<Duration>,
    pub backstop: Option<RefreshReason>,
    pub busy: bool,
    pub uncovered: bool,
}
/// Detached burst watermark. A backstop marker is acknowledged only on success;
/// a new loss during observation has a different watermark and survives it.
pub struct Burst {
    pending: BTreeMap<(i32, Vec<u8>), Hint>,
    marker: Option<(u64, RefreshReason)>,
    roots: BTreeSet<PathBuf>,
}
impl Watch {
    /// Creates one CLOEXEC, nonblocking instance with the given intake bounds.
    pub fn new(config: Config) -> Result<Self, Errno> {
        Self::new_with_flags(config, true)
    }
    /// Creates a blocking instance for a dedicated daemon intake thread.
    pub fn new_blocking(config: Config) -> Result<Self, Errno> {
        Self::new_with_flags(config, false)
    }
    fn new_with_flags(config: Config, nonblocking: bool) -> Result<Self, Errno> {
        Ok(Self {
            fd: inotify::init(if nonblocking {
                inotify::CreateFlags::NONBLOCK | inotify::CreateFlags::CLOEXEC
            } else {
                inotify::CreateFlags::CLOEXEC
            })?,
            config,
            state: Mutex::new(State {
                descriptors: BTreeMap::new(),
                identities: BTreeMap::new(),
                file_aliases: BTreeMap::new(),
                removed: BTreeSet::new(),
                gaps: BTreeSet::new(),
                unreliable: BTreeSet::new(),
                policies: BTreeMap::new(),
                policy_failed: BTreeMap::new(),
                policy_epochs: BTreeMap::new(),
                policy_refreshed: BTreeSet::new(),
                failed: BTreeSet::new(),
                pending: BTreeMap::new(),
                bytes: 0,
                first: None,
                inflight_first: None,
                last: None,
                backstop: None,
                scoped_roots: BTreeSet::new(),
                serial: 0,
                running: false,
            }),
        })
    }
    /// Arms the observed handle before enumeration. A failure is coverage loss,
    /// never a crawl deletion. Parents are armed before their child is listed.
    pub(crate) fn arm(&self, root: &Path, relative: &Path, fd: BorrowedFd<'_>) {
        let Ok(stat) = fstat(fd) else {
            self.gap(root);
            return;
        };
        let identity = (stat.st_dev, stat.st_ino);
        if relative.as_os_str().is_empty() {
            self.policy_path(root, root);
        }
        if rustix::fs::fstatfs(fd).map_or(true, |s| policy::unreliable(s.f_type as u64)) {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .unreliable
                .insert(root.to_owned());
        }
        let parent_identity = if relative.as_os_str().is_empty() {
            None
        } else {
            openat(
                fd,
                "..",
                OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .and_then(fstat)
            .ok()
            .map(|s| (s.st_dev, s.st_ino))
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = root.join(relative);
        let parent = if relative.as_os_str().is_empty() {
            None
        } else {
            state
                .identities
                .get(&parent_identity.unwrap_or_default())
                .and_then(|wd| state.descriptors.get(wd))
                .and_then(|ds| {
                    ds.iter().find(|d| {
                        Some(d.path().as_path()) == path.parent() && d.root.as_path() == root
                    })
                })
                .cloned()
        };
        if !relative.as_os_str().is_empty() && parent.is_none() {
            state.gaps.insert(root.to_owned());
            state.failed.insert(identity);
            return;
        }
        let basename = relative.file_name().map_or(&b""[..], OsStr::as_bytes);
        if let Some(wd) = state.identities.get(&identity)
            && state
                .descriptors
                .get(wd)
                .is_some_and(|ds| ds.iter().any(|d| d.path() == path))
        {
            return;
        }
        if state.identities.len() >= self.config.watch_cap
            && !state.identities.contains_key(&identity)
        {
            state.gaps.insert(root.to_owned());
            state.failed.insert(identity);
            return;
        }
        // /proc/self/fd follows the already observed handle, including after an
        // ancestor rename. ONLYDIR rejects a non-directory without a path race.
        let path = format!("/proc/self/fd/{}", fd.as_raw_fd());
        // UNMOUNT and IGNORED are automatically reported by inotify; only
        // subscriptions with user-selectable mask bits belong here.
        let mask = WatchFlags::CREATE
            | WatchFlags::DELETE
            | WatchFlags::MOVED_FROM
            | WatchFlags::MOVED_TO
            | WatchFlags::ATTRIB
            | WatchFlags::MODIFY
            | WatchFlags::CLOSE_WRITE
            | WatchFlags::MOVE_SELF
            | WatchFlags::DELETE_SELF
            | WatchFlags::ONLYDIR
            | WatchFlags::MASK_ADD;
        match inotify::add_watch(&self.fd, path.as_str(), mask) {
            Ok(wd) => {
                if state.removed.remove(&wd) {
                    loss(&mut state, RefreshReason::Overflow);
                }
                if state
                    .descriptors
                    .get(&wd)
                    .is_some_and(|ds| ds.iter().any(|d| d.identity != identity))
                {
                    state.descriptors.remove(&wd);
                    state.identities.retain(|_, value| *value != wd);
                    loss(&mut state, RefreshReason::Overflow);
                }
                let root = parent
                    .as_ref()
                    .map_or_else(|| Arc::new(root.to_owned()), |p| p.root.clone());
                state
                    .descriptors
                    .entry(wd)
                    .or_default()
                    .push(Arc::new(Directory {
                        identity,
                        parent,
                        basename: basename.to_vec(),
                        root,
                    }));
                state.identities.insert(identity, wd);
                state.failed.remove(&identity);
            }
            Err(_) => {
                state.gaps.insert(root.to_owned());
                state.failed.insert(identity);
            }
        }
    }
    /// Attempt a watch even when the later readable directory open will be
    /// denied. O_PATH and the observed parent avoid rebuilding a live pathname.
    pub(crate) fn arm_entry(
        &self,
        root: &Path,
        entry: &crate::Decided<'_, ferret_catalog::DirToken>,
    ) {
        let Some(stat) = entry.stat else {
            return;
        };
        match openat(
            entry.parent_fd,
            entry.name,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => {
                if fstat(&fd).is_ok_and(|s| (s.st_dev, s.st_ino) == (stat.dev, stat.ino)) {
                    self.arm(root, entry.path, std::os::fd::AsFd::as_fd(&fd));
                } else {
                    self.gap(root);
                    self.backstop(RefreshReason::Overflow);
                }
            }
            Err(_) => {
                self.gap(root);
            }
        }
    }
    fn gap(&self, root: &Path) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .gaps
            .insert(root.to_owned());
    }
    /// Reads until EAGAIN. Kernel buffers are always consumed even when the
    /// userspace accumulator has collapsed to a complete backstop marker.
    pub fn drain(&self) -> Result<(), Errno> {
        let mut buffer = [MaybeUninit::uninit(); 64 << 10];
        let mut reader = inotify::Reader::new(&self.fd, &mut buffer);
        loop {
            match reader.next() {
                Ok(event) => self.event(
                    event.wd(),
                    event.events(),
                    event.cookie(),
                    event.file_name().map_or(&[], |s| s.to_bytes()),
                ),
                Err(Errno::AGAIN) => return Ok(()),
                Err(Errno::INTR) => continue,
                Err(error) => {
                    self.backstop(RefreshReason::Overflow);
                    return Err(error);
                }
            }
        }
    }
    /// Runs blocking intake and notifies the writer for each observed event.
    pub fn run_intake(&self, mut ready: impl FnMut() -> bool) {
        let mut buffer = [MaybeUninit::uninit(); 64 << 10];
        let mut reader = inotify::Reader::new(&self.fd, &mut buffer);
        loop {
            match reader.next() {
                Ok(event) => {
                    self.event(
                        event.wd(),
                        event.events(),
                        event.cookie(),
                        event.file_name().map_or(&[], |s| s.to_bytes()),
                    );
                    if !ready() {
                        return;
                    }
                }
                Err(Errno::INTR) => continue,
                Err(_) => {
                    self.backstop(RefreshReason::Overflow);
                    if !ready() {
                        return;
                    }
                }
            }
        }
    }
    fn event(&self, wd: i32, flags: ReadFlags, cookie: u32, name: &[u8]) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
            // Overflow may have dropped expected IGNORED records as well. A
            // stale tombstone cannot be trusted at a later descriptor reuse.
            state.removed.clear();
            loss(&mut state, RefreshReason::Overflow);
            return;
        }
        if flags.contains(ReadFlags::IGNORED) {
            if state.removed.remove(&wd) {
                return;
            }
            state.descriptors.remove(&wd);
            state.identities.retain(|_, value| *value != wd);
            if let Some(inputs) = state.policies.remove(&wd) {
                state.gaps.extend(inputs.into_keys().map(|(r, _)| r));
            }
            loss(&mut state, RefreshReason::Overflow);
            return;
        }
        let policy_roots = state
            .policies
            .get(&wd)
            .map(|inputs| {
                inputs
                    .keys()
                    .filter(|(_, n)| n == name || name.is_empty())
                    .map(|(r, _)| r.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !policy_roots.is_empty() {
            state.scoped_roots.extend(policy_roots);
            state.first.get_or_insert_with(Instant::now);
        }
        if !state.descriptors.contains_key(&wd) && state.policies.contains_key(&wd) {
            return;
        }
        let Some(directories) = state.descriptors.get(&wd).cloned() else {
            loss(&mut state, RefreshReason::Overflow);
            return;
        };
        if flags.contains(ReadFlags::UNMOUNT) {
            state
                .gaps
                .extend(directories.iter().map(|d| (*d.root).clone()));
            loss(&mut state, RefreshReason::Overflow);
            return;
        }
        let now = Instant::now();
        state.first.get_or_insert(now);
        state.last = Some(now);
        let key = (wd, name.to_vec());
        if let Some(hint) = state.pending.get_mut(&key) {
            // Multiple moves involving one endpoint are ambiguous. Observation
            // is still required, but it must not manufacture a cookie pair.
            if cookie != 0 && flags.intersects(ReadFlags::MOVED_FROM | ReadFlags::MOVED_TO) {
                if hint.cookie != 0 && hint.cookie != cookie {
                    hint.cookie = 0;
                    hint.ambiguous_cookie = true;
                } else if !hint.ambiguous_cookie {
                    hint.cookie = cookie;
                }
            }
            hint.subtree |=
                flags.intersects(ReadFlags::ISDIR | ReadFlags::MOVE_SELF | ReadFlags::DELETE_SELF);
            hint.from |= flags.contains(ReadFlags::MOVED_FROM);
            hint.to |= flags.contains(ReadFlags::MOVED_TO);
            return;
        }
        let charge = std::mem::size_of::<Hint>()
            + 64
            + name.len()
            + directories.iter().map(|d| d.charged_bytes()).sum::<usize>();
        if state.pending.len() >= self.config.scopes
            || state.bytes.saturating_add(charge) > self.config.bytes
        {
            loss(&mut state, RefreshReason::Overflow);
            return;
        }
        state.bytes += charge;
        state.pending.insert(
            key,
            Hint {
                directories,
                subtree: flags
                    .intersects(ReadFlags::ISDIR | ReadFlags::MOVE_SELF | ReadFlags::DELETE_SELF),
                cookie,
                ambiguous_cookie: false,
                from: flags.contains(ReadFlags::MOVED_FROM),
                to: flags.contains(ReadFlags::MOVED_TO),
            },
        );
    }
    /// Debug-build injection at the exact kernel intake seam. Release hosts
    /// expose no event injection. This exercises loss and cookie ambiguity
    /// without requiring the kernel to generate an impossible event sequence.
    #[cfg(debug_assertions)]
    pub fn inject_overflow(&self) {
        self.event(-1, ReadFlags::QUEUE_OVERFLOW, 0, &[]);
    }
    /// Debug-build move endpoint at an installed directory locator.
    #[cfg(debug_assertions)]
    pub fn inject_move(&self, path: &Path, name: &[u8], cookie: u32, from: bool) {
        let wd = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .descriptors
            .iter()
            .find(|(_, d)| d.iter().any(|d| d.path() == path))
            .map(|(&wd, _)| wd);
        if let Some(wd) = wd {
            self.event(
                wd,
                if from {
                    ReadFlags::MOVED_FROM
                } else {
                    ReadFlags::MOVED_TO
                },
                cookie,
                name,
            );
        }
    }

    /// Collapses pending hints to a new complete all-roots observation marker.
    pub fn backstop(&self, reason: RefreshReason) {
        loss(
            &mut self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            reason,
        );
    }
    /// Conservative live heap plus kernel-watch estimate for bulk reserves.
    /// Shared ancestor Arcs may be counted more than once, deliberately.
    pub fn resource_bytes(&self) -> u64 {
        let s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directories: usize = s
            .descriptors
            .values()
            .flatten()
            .map(|d| d.charged_bytes() + 64)
            .sum();
        let policies: usize = s
            .policies
            .values()
            .flat_map(|m| m.keys())
            .map(|(root, name)| root.as_os_str().len() + name.len() + 128)
            .sum();
        let aliases: usize = s
            .file_aliases
            .values()
            .map(|a| {
                128 + a
                    .names
                    .iter()
                    .map(|((_, name), roots)| {
                        name.len()
                            + 128
                            + roots
                                .iter()
                                .map(|r| r.as_os_str().len() + 64)
                                .sum::<usize>()
                    })
                    .sum::<usize>()
            })
            .sum();
        (s.identities.len() * 1024
            + directories
            + policies
            + aliases
            + s.bytes
            + s.policy_failed
                .keys()
                .map(|(r, p)| r.as_os_str().len() + p.as_os_str().len() + 128)
                .sum::<usize>()) as u64
    }
    /// Snapshots intake coverage and monotonic pending age under a short lock.
    pub fn status(&self) -> Status {
        let s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Status {
            installed: s.identities.len(),
            needed: s.identities.len() + s.failed.len() + s.policy_failed.len(),
            failed: s.failed.len() + s.policy_failed.len(),
            polling_roots: s
                .gaps
                .iter()
                .chain(&s.unreliable)
                .chain(
                    s.file_aliases
                        .values()
                        .flat_map(FileAliases::unproven_roots),
                )
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            bytes: s.bytes
                + s.scoped_roots
                    .iter()
                    .map(|r| r.as_os_str().len() + 64)
                    .sum::<usize>(),
            pending: s.pending.len() + usize::from(s.backstop.is_some()) + s.scoped_roots.len(),
            oldest: s
                .first
                .into_iter()
                .chain(s.inflight_first)
                .min()
                .map(|t| t.elapsed()),
            backstop: s.backstop.map(|(_, r)| r),
            busy: s.running
                || s.backstop.is_some()
                || !s.pending.is_empty()
                || !s.scoped_roots.is_empty(),
            uncovered: !s.gaps.is_empty()
                || !s.unreliable.is_empty()
                || s.file_aliases
                    .values()
                    .any(|a| a.unproven_roots().next().is_some()),
        }
    }
    /// Earliest time at which accumulated hints can be detached.
    pub fn next_due(&self) -> Option<Instant> {
        let s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if s.backstop.is_some() || !s.scoped_roots.is_empty() {
            return Some(Instant::now());
        }
        let (Some(first), Some(last)) = (s.first, s.last) else {
            return None;
        };
        Some((last + TRAILING).min(first + MAX_AGE))
    }
    /// Schedules a known local watch gap for scoped polling.
    pub fn scoped_roots(&self, roots: Vec<PathBuf>) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .scoped_roots
            .extend(roots);
    }
    /// Roots with missing/unreliable watches or unobserved hard-link names.
    pub fn polling_roots(&self, view: &Catalog) -> Vec<PathBuf> {
        let s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let roots = s
            .gaps
            .iter()
            .chain(&s.unreliable)
            .chain(
                s.file_aliases
                    .values()
                    .flat_map(FileAliases::unproven_roots),
            )
            .cloned()
            .collect::<BTreeSet<_>>();
        roots
            .into_iter()
            .filter(|root| view.roots().any(|(_, p)| p == root.as_os_str().as_bytes()))
            .collect()
    }
    /// Detaches at most one due burst. Loss markers bypass debounce; new
    /// arrivals are accumulated independently while the host observes this one.
    pub fn take(&self) -> Option<Burst> {
        self.take_admitted(true)
    }
    /// Leaves bulk markers queued during a controller pause. Intake keeps
    /// coalescing under the same bounds; ordinary scoped hints remain eligible.
    pub fn take_admitted(&self, bulk: bool) -> Option<Burst> {
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if s.running
            || (!bulk && s.backstop.is_some())
            || (s.backstop.is_none()
                && s.scoped_roots.is_empty()
                && (s.pending.is_empty()
                    || s.last.is_some_and(|t| t.elapsed() < TRAILING)
                        && s.first.is_some_and(|t| t.elapsed() < MAX_AGE)))
        {
            return None;
        }
        s.running = true;
        s.bytes = 0;
        s.inflight_first = s.first.take();
        s.last = None;
        Some(Burst {
            pending: std::mem::take(&mut s.pending),
            marker: s.backstop,
            roots: std::mem::take(&mut s.scoped_roots),
        })
    }
    /// Acknowledges this watermark on success, or retains complete work after
    /// failure. A new loss watermark is never cleared by an older observation.
    pub fn finish(&self, burst: Burst, success: bool) {
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.running = false;
        s.inflight_first = None;
        if success {
            if burst.marker.is_some() && s.backstop == burst.marker {
                s.backstop = None;
            }
        } else {
            s.policy_refreshed.clear();
            loss(
                &mut s,
                burst.marker.map_or(RefreshReason::Backstop, |(_, r)| r),
            );
        }
    }
    /// Retire watches only after checked catalog observation. Known removals
    /// have an expected IGNORED; every unknown descriptor lifetime is loss.
    pub fn reconcile(&self, view: &Catalog) {
        let roots: BTreeSet<_> = view
            .roots()
            .map(|(_, p)| PathBuf::from(OsStr::from_bytes(p)))
            .collect();
        let identities: BTreeSet<_> = view
            .dir_ids()
            .map(|id| {
                let stat = view.inode(id).stat;
                (stat.dev, stat.ino)
            })
            .collect();
        let protected = view
            .dir_ids()
            .filter(|&id| view.retained_at(id).is_some())
            .filter_map(|id| crate::refresh::containing_root(view, id).ok())
            .collect::<BTreeSet<_>>();
        let snapshot = {
            let mut s = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.failed.retain(|identity| identities.contains(identity));
            s.file_aliases.retain(|_, aliases| {
                aliases.names.retain(|_, owners| {
                    owners.retain(|r| roots.contains(r));
                    !owners.is_empty()
                });
                !aliases.names.is_empty()
            });
            let refreshed = std::mem::take(&mut s.policy_refreshed);
            let epochs = s.policy_epochs.clone();
            let keep = |r: &PathBuf, epoch: u64| {
                roots.contains(r)
                    && (!refreshed.contains(r)
                        || protected.contains(r)
                        || epochs.get(r).is_some_and(|current| *current == epoch))
            };
            s.policy_epochs.retain(|r, _| roots.contains(r));
            s.policy_failed.retain(|(r, _), epoch| keep(r, *epoch));
            let mut retired = Vec::new();
            for (&wd, inputs) in &mut s.policies {
                inputs.retain(|(r, _), epoch| keep(r, *epoch));
                if inputs.is_empty() {
                    retired.push(wd);
                }
            }
            for wd in retired {
                s.policies.remove(&wd);
                if !s.descriptors.contains_key(&wd) {
                    s.identities.retain(|_, value| *value != wd);
                    if inotify::remove_watch(&self.fd, wd).is_ok() {
                        s.removed.insert(wd);
                    }
                }
            }
            s.unreliable
                .retain(|root| view.roots().any(|(_, p)| p == root.as_os_str().as_bytes()));
            if s.failed.is_empty() {
                s.gaps = s.unreliable.clone();
                let failed = s
                    .policy_failed
                    .keys()
                    .map(|(r, _)| r.clone())
                    .collect::<Vec<_>>();
                s.gaps.extend(failed);
            }
            s.descriptors
                .iter()
                .map(|(&wd, d)| (wd, d.clone()))
                .collect::<Vec<_>>()
        };
        // Resolution may traverse a large watch set. Kernel intake must not
        // wait on its catalog/path work or on a sweep's removal syscalls.
        for (wd, directories) in snapshot {
            let kept = directories
                .iter()
                .filter(|d| d.resolve(view).is_some())
                .cloned()
                .collect::<Vec<_>>();
            let mut s = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if s.descriptors.get(&wd).is_none_or(|ds| {
                ds.len() != directories.len()
                    || ds.iter().zip(&directories).any(|(a, b)| !Arc::ptr_eq(a, b))
            }) {
                continue;
            }
            if !kept.is_empty() {
                s.descriptors.insert(wd, kept);
                continue;
            }
            s.descriptors.remove(&wd);
            if !s.policies.contains_key(&wd) {
                s.identities.retain(|_, value| *value != wd);
            }
            if !s.policies.contains_key(&wd) && inotify::remove_watch(&self.fd, wd).is_ok() {
                s.removed.insert(wd);
            }
        }
    }
}
fn loss(s: &mut State, reason: RefreshReason) {
    s.serial = s.serial.wrapping_add(1);
    let reason = if s
        .backstop
        .is_some_and(|(_, r)| r == RefreshReason::Overflow)
    {
        RefreshReason::Overflow
    } else {
        reason
    };
    s.backstop = Some((s.serial, reason));
    s.pending.clear();
    s.bytes = 0;
    s.first.get_or_insert_with(Instant::now);
    s.last = Some(Instant::now());
}
#[derive(Default)]
struct MovePair {
    old: Vec<(InoId, Vec<u8>)>,
    new: Vec<(InoId, Vec<u8>)>,
}
impl Burst {
    /// Whether this observation can retire directory watches. Ordinary file
    /// bursts do not scan the entire watch set at each publication.
    pub fn reconcile_watches(&self) -> bool {
        self.marker.is_some()
            || !self.roots.is_empty()
            || self.pending.iter().any(|((_, name), h)| {
                h.subtree || matches!(name.as_slice(), b".git" | b".gitignore" | b".ferretignore")
            })
    }
    /// Loss/backstop reason associated with this detached watermark.
    pub fn reason(&self) -> RefreshReason {
        self.marker.map_or(RefreshReason::Burst, |(_, r)| r)
    }
    /// Resolves parent/name locators in the current checked generation. Missing
    /// ancestry or changed root boundaries widen observation conservatively.
    pub fn request(&self, view: &Catalog) -> RefreshRequest {
        let mut scopes = Vec::new();
        let mut roots = self.roots.clone();
        let mut moves: BTreeMap<u32, MovePair> = BTreeMap::new();
        for ((_, name), hint) in &self.pending {
            for directory in &hint.directories {
                if name.is_empty()
                    || matches!(name.as_slice(), b".git" | b".gitignore" | b".ferretignore")
                {
                    roots.insert((*directory.root).clone());
                } else if let Some(parent) = directory.resolve(view) {
                    scopes.push(RefreshScope::Entry {
                        parent,
                        basename: name.clone(),
                    });
                    if hint.cookie != 0 {
                        let pair = moves.entry(hint.cookie).or_default();
                        if hint.from {
                            pair.old.push((parent, name.clone()));
                        }
                        if hint.to {
                            pair.new.push((parent, name.clone()));
                        }
                    }
                } else {
                    roots.insert((*directory.root).clone());
                }
            }
        }
        if self.marker.is_some() {
            return RefreshRequest {
                expected_generation: view.generation(),
                scopes: Vec::new(),
                rename_hints: Vec::new(),
                reason: self.reason(),
            };
        }
        let changed_boundary = roots
            .iter()
            .any(|root| !view.roots().any(|(_, p)| p == root.as_os_str().as_bytes()));
        if changed_boundary {
            scopes.clear();
        } else {
            scopes.retain(|scope| match scope {
                RefreshScope::Entry { parent, .. } => {
                    let mut path = Vec::new();
                    view.dir_path(*parent, &mut path);
                    !roots
                        .iter()
                        .any(|root| Path::new(OsStr::from_bytes(&path)).starts_with(root))
                }
                _ => true,
            });
            scopes.extend(roots.into_iter().map(RefreshScope::Root));
        }
        let rename_hints = if self.marker.is_some() || changed_boundary {
            Vec::new()
        } else {
            moves
                .into_values()
                .filter_map(|MovePair { mut old, mut new }| {
                    if old.len() != 1 || new.len() != 1 {
                        return None;
                    }
                    let (old_parent, old_name) = old.pop()?;
                    let (new_parent, new_name) = new.pop()?;
                    Some(RenameHint {
                        old_parent,
                        old_name,
                        new_parent,
                        new_name,
                    })
                })
                .collect()
        };
        RefreshRequest {
            expected_generation: view.generation(),
            scopes,
            rename_hints,
            reason: if changed_boundary {
                RefreshReason::Backstop
            } else {
                self.reason()
            },
        }
    }
}

#[cfg(test)]
mod tests;
