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
        match &self.parent {
            Some(parent) => parent.path().join(OsStr::from_bytes(&self.basename)),
            None => (*self.root).clone(),
        }
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
    directory: Arc<Directory>,
    subtree: bool,
    cookie: u32,
    from: bool,
    to: bool,
}
#[derive(Debug)]
struct State {
    descriptors: BTreeMap<i32, Arc<Directory>>,
    identities: BTreeMap<(u64, u64), i32>,
    removed: BTreeSet<i32>,
    gaps: BTreeSet<PathBuf>,
    failed: BTreeSet<(u64, u64)>,
    pending: BTreeMap<(i32, Vec<u8>), Hint>,
    bytes: usize,
    first: Option<Instant>,
    last: Option<Instant>,
    backstop: Option<(u64, RefreshReason)>,
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
}
impl Watch {
    pub fn new(config: Config) -> Result<Self, Errno> {
        Ok(Self {
            fd: inotify::init(inotify::CreateFlags::NONBLOCK | inotify::CreateFlags::CLOEXEC)?,
            config,
            state: Mutex::new(State {
                descriptors: BTreeMap::new(),
                identities: BTreeMap::new(),
                removed: BTreeSet::new(),
                gaps: BTreeSet::new(),
                failed: BTreeSet::new(),
                pending: BTreeMap::new(),
                bytes: 0,
                first: None,
                last: None,
                backstop: None,
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
        let parent = parent_identity
            .and_then(|key| state.identities.get(&key))
            .and_then(|wd| state.descriptors.get(wd))
            .cloned();
        if !relative.as_os_str().is_empty() && parent.is_none() {
            state.gaps.insert(root.to_owned());
            state.failed.insert(identity);
            return;
        }
        let basename = relative.file_name().map_or(&b""[..], OsStr::as_bytes);
        if let Some(wd) = state.identities.get(&identity)
            && let Some(old) = state.descriptors.get(wd)
            && old.parent.as_ref().map(|p| p.identity) == parent_identity
            && old.basename == basename
            && old.root.as_path() == root
        {
            return;
        }
        if state.descriptors.len() >= self.config.watch_cap
            && !state.identities.contains_key(&identity)
        {
            state.gaps.insert(root.to_owned());
            state.failed.insert(identity);
            return;
        }
        // /proc/self/fd follows the already observed handle, including after an
        // ancestor rename. ONLYDIR rejects a non-directory without a path race.
        let path = format!("/proc/self/fd/{}", fd.as_raw_fd());
        let mask = WatchFlags::CREATE
            | WatchFlags::DELETE
            | WatchFlags::MOVED_FROM
            | WatchFlags::MOVED_TO
            | WatchFlags::ATTRIB
            | WatchFlags::MODIFY
            | WatchFlags::CLOSE_WRITE
            | WatchFlags::MOVE_SELF
            | WatchFlags::DELETE_SELF
            | WatchFlags::ONLYDIR;
        match inotify::add_watch(&self.fd, path.as_str(), mask) {
            Ok(wd) => {
                if let Some(old) = state.descriptors.get(&wd) {
                    // M5b supplies multiple occurrences. Until then, any alias
                    // or relocation gets complete observation and polling.
                    if old.identity != identity || old.path() != root.join(relative) {
                        let old_root = (*old.root).clone();
                        state.gaps.insert(old_root);
                        state.gaps.insert(root.to_owned());
                        loss(&mut state, RefreshReason::Overflow);
                    }
                }
                let root = parent
                    .as_ref()
                    .map_or_else(|| Arc::new(root.to_owned()), |p| p.root.clone());
                state.descriptors.insert(
                    wd,
                    Arc::new(Directory {
                        identity,
                        parent,
                        basename: basename.to_vec(),
                        root,
                    }),
                );
                state.identities.insert(identity, wd);
                state.failed.remove(&identity);
            }
            Err(_) => {
                state.gaps.insert(root.to_owned());
                state.failed.insert(identity);
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
    fn event(&self, wd: i32, flags: ReadFlags, cookie: u32, name: &[u8]) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
            loss(&mut state, RefreshReason::Overflow);
            return;
        }
        if flags.contains(ReadFlags::IGNORED) {
            if state.removed.remove(&wd) {
                return;
            }
            if let Some(old) = state.descriptors.remove(&wd) {
                state.identities.remove(&old.identity);
            }
            loss(&mut state, RefreshReason::Overflow);
            return;
        }
        let Some(directory) = state.descriptors.get(&wd).cloned() else {
            loss(&mut state, RefreshReason::Overflow);
            return;
        };
        if flags.contains(ReadFlags::UNMOUNT) {
            state.gaps.insert((*directory.root).clone());
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
            if hint.cookie != cookie {
                hint.cookie = 0;
            }
            hint.subtree |=
                flags.intersects(ReadFlags::ISDIR | ReadFlags::MOVE_SELF | ReadFlags::DELETE_SELF);
            hint.from |= flags.contains(ReadFlags::MOVED_FROM);
            hint.to |= flags.contains(ReadFlags::MOVED_TO);
            return;
        }
        let charge =
            std::mem::size_of::<Hint>() + 64 + name.len() + directory.path().as_os_str().len();
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
                directory,
                subtree: flags
                    .intersects(ReadFlags::ISDIR | ReadFlags::MOVE_SELF | ReadFlags::DELETE_SELF),
                cookie,
                from: flags.contains(ReadFlags::MOVED_FROM),
                to: flags.contains(ReadFlags::MOVED_TO),
            },
        );
    }
    pub fn backstop(&self, reason: RefreshReason) {
        loss(
            &mut self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            reason,
        );
    }
    pub fn status(&self) -> Status {
        let s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Status {
            installed: s.descriptors.len(),
            needed: s.descriptors.len() + s.failed.len(),
            failed: s.failed.len(),
            pending: s.pending.len() + usize::from(s.backstop.is_some()),
            oldest: s.first.map(|t| t.elapsed()),
            backstop: s.backstop.map(|(_, r)| r),
            busy: s.running || s.backstop.is_some() || !s.pending.is_empty(),
            uncovered: !s.gaps.is_empty(),
        }
    }
    pub fn take(&self) -> Option<Burst> {
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if s.running
            || (s.backstop.is_none()
                && (s.pending.is_empty()
                    || s.last.is_some_and(|t| t.elapsed() < TRAILING)
                        && s.first.is_some_and(|t| t.elapsed() < MAX_AGE)))
        {
            return None;
        }
        s.running = true;
        s.bytes = 0;
        s.first = None;
        s.last = None;
        Some(Burst {
            pending: std::mem::take(&mut s.pending),
            marker: s.backstop,
        })
    }
    pub fn finish(&self, burst: Burst, success: bool) {
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.running = false;
        if success {
            if burst.marker.is_some() && s.backstop == burst.marker {
                s.backstop = None;
            }
        } else {
            loss(
                &mut s,
                burst.marker.map_or(RefreshReason::Backstop, |(_, r)| r),
            );
        }
    }
    /// Retire watches only after checked catalog observation. Known removals
    /// have an expected IGNORED; every unknown descriptor lifetime is loss.
    pub fn reconcile(&self, view: &Catalog) {
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let gone: Vec<_> = s
            .descriptors
            .iter()
            .filter(|(_, d)| d.resolve(view).is_none())
            .map(|(&wd, _)| wd)
            .collect();
        for wd in gone {
            if let Some(d) = s.descriptors.remove(&wd) {
                s.identities.remove(&d.identity);
            }
            if inotify::remove_watch(&self.fd, wd).is_ok() {
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
impl Burst {
    pub fn reason(&self) -> RefreshReason {
        self.marker.map_or(RefreshReason::Burst, |(_, r)| r)
    }
    pub fn request(&self, view: &Catalog) -> RefreshRequest {
        let mut scopes = Vec::new();
        let mut roots = BTreeSet::new();
        let mut moves: BTreeMap<u32, (Vec<(InoId, Vec<u8>)>, Vec<(InoId, Vec<u8>)>)> =
            BTreeMap::new();
        for ((_, name), hint) in &self.pending {
            if hint.subtree
                || name.is_empty()
                || matches!(name.as_slice(), b".git" | b".gitignore" | b".ferretignore")
            {
                roots.insert((*hint.directory.root).clone());
            } else if let Some(parent) = hint.directory.resolve(view) {
                scopes.push(RefreshScope::Entry {
                    parent,
                    basename: name.clone(),
                });
                if hint.cookie != 0 {
                    let pair = moves.entry(hint.cookie).or_default();
                    if hint.from {
                        pair.0.push((parent, name.clone()));
                    }
                    if hint.to {
                        pair.1.push((parent, name.clone()));
                    }
                }
            } else {
                roots.insert((*hint.directory.root).clone());
            }
        }
        scopes.extend(roots.into_iter().map(RefreshScope::Root));
        let rename_hints = if self.marker.is_some() {
            Vec::new()
        } else {
            moves
                .into_values()
                .filter_map(|(mut old, mut new)| {
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
            reason: self.reason(),
        }
    }
}
