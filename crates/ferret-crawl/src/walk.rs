//! One root, walked depth-first, with one [`DirRules`] per directory.
//!
//! Seam: [`ferret_policy`] decides what to do with each entry. This module
//! reads the tree and reports. It does not touch the catalog.

use std::ffi::{OsStr, OsString};
use std::fs::{self, FileType, Metadata, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use ferret_policy::{Config, Decision, DirRules, Entry, IgnoreFiles, PatternError, Reason};

/// `O_NONBLOCK` from Linux `<fcntl.h>` (`04000`). std does not export it.
const O_NONBLOCK: i32 = 0o4000;

/// `O_NOFOLLOW` from Linux `<fcntl.h>` (`0400000`). Opening `.git` with this
/// set refuses a symlink instead of reading through it.
const O_NOFOLLOW: i32 = 0o400000;

/// An ignore file larger than this is an [`Event::Io`] and counts as absent.
/// One mebibyte is past any ignore file this tree has seen; the bound is what
/// stops a hostile or accidental giant from being pulled into the walk.
const MAX_IGNORE_BYTES: u64 = 1 << 20;

/// `lstat` fields for an entry the walk did not skip.
///
/// Times are Unix `st_mtim` and `st_ctim` (seconds plus nanoseconds). `mode`
/// is `st_mode`, type bits included. `uid` and `gid` are `st_uid` and
/// `st_gid`: the catalog's inode row stores them, and they come from the same
/// `lstat` as the rest.
///
/// `link_target` is the raw `readlink` bytes of a catalogued symlink, not
/// resolved and not re-encoded (D18). It is `Some` exactly then. A catalogued
/// symlink is not emitted until `readlink` succeeds; a failure is
/// [`Event::Io`] and no [`Decided`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat<'a> {
    /// `st_size`. For a symlink, the length of the target path.
    pub size: u64,
    /// `st_mtim` seconds.
    pub mtime_sec: i64,
    /// `st_mtim` nanoseconds.
    pub mtime_nsec: i64,
    /// `st_ctim` seconds.
    pub ctime_sec: i64,
    /// `st_ctim` nanoseconds.
    pub ctime_nsec: i64,
    /// `st_dev`.
    pub dev: u64,
    /// `st_ino`.
    pub ino: u64,
    /// `st_mode`.
    pub mode: u32,
    /// `st_uid`.
    pub uid: u32,
    /// `st_gid`.
    pub gid: u32,
    /// Raw `readlink` text when this entry is a catalogued symlink.
    pub link_target: Option<&'a OsStr>,
}

/// One entry [`DirRules::decide`] classified.
///
/// `path` is relative to the walk's root and borrows the walker's path
/// buffer. It is valid only for the callback that receives it. `stat` is
/// absent for [`Decision::Skip`] and present for every other decision. If
/// `lstat` fails, or `readlink` fails for a catalogued symlink, the walk
/// emits [`Event::Io`] and does not emit `Decided`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decided<'a> {
    /// Root-relative path of the entry.
    pub path: &'a Path,
    /// What the policy said to do with `path`.
    pub decision: Decision,
    /// `lstat` of `path`. Present for every decision other than
    /// [`Decision::Skip`].
    pub stat: Option<Stat<'a>>,
}

/// What the walk reports, in the order it happens.
///
/// A directory is reported before its children. Siblings come out in
/// directory order, which is not sorted. Paths borrow the walker's buffers
/// and must be copied to be kept.
#[derive(Debug)]
pub enum Event<'a> {
    /// An entry `decide` classified.
    Decided(Decided<'a>),
    /// A directory listing, `lstat`, `readlink` or ignore-file read failed.
    ///
    /// `path` is root-relative. It is empty when the root itself cannot be
    /// listed, and it is the directory being listed when `read_dir` yields an
    /// error with no name. The walk continues with the next entry.
    Io {
        /// Root-relative path the operation was about.
        path: &'a Path,
        /// The OS error.
        error: io::Error,
    },
    /// A pattern [`DirRules::root`] or [`DirRules::enter`] dropped. The rest
    /// of that ignore file still applies, and the walk continues.
    Pattern(PatternError),
}

/// Walks one configured root.
///
/// `global` is the text of the global ignore file, or `None` when there is no
/// such file (nothing is excluded by default). `visit` is called for every
/// entry and every fault as the walk goes, so a catalog can record about a
/// million entries without this function retaining them. A callback is the
/// shape that allows that: the root-relative path is one reused buffer, and a
/// lending iterator is not expressible in stable Rust.
///
/// The root directory itself is not an event. It is where the walk starts,
/// not an entry `decide` sees; the caller stats it if the roots table needs
/// its inode. Faults that belong to the root use an empty path.
///
/// Ignore files are read when the walk enters a directory
/// ([`Decision::Descend`]). A directory reached only so a `.ferretignore` `!`
/// pattern can re-include beneath it ([`Decision::Traverse`]) is listed, and
/// its ignore files are not read (D13). `.gitignore` is read only inside a
/// work tree, where its rules can apply; the root counts as outside one unless
/// it contains `.git` (D22). `.git/info/exclude` is read when `.git` is a real
/// directory. When `.git` is a regular file whose first line is `gitdir:
/// <path>` (relative to the directory that holds the file), exclude is read
/// from that gitdir, or from the directory named by a `commondir` file there:
/// one line, relative to the gitdir, which is how a linked work tree points at
/// the main repository. Nothing is opened through a symlinked `.git`.
///
/// `.ferretignore`, `.gitignore` and `info/exclude` are opened without blocking
/// (`O_NONBLOCK`) and read only when that open file is a regular file of at
/// most 1 MiB. A larger file is an [`Event::Io`] and is then treated as
/// absent. A missing file, or anything that is not a regular file, is absent
/// and not a fault: a FIFO or directory of one of these names must not stall
/// the walk. A regular file that exists but cannot be read is a fault, and is
/// then treated as absent. Bytes that are not UTF-8 are converted lossily.
///
/// The listing is a snapshot. An ignore file created after the directory was
/// listed is not seen until the next crawl, so it does not apply to the
/// siblings listed with it. A listed name that is gone by the time it is
/// opened is absent.
///
/// File types come from `read_dir` (`d_type`, or `lstat` when the filesystem
/// leaves the type unknown). `d_type` does not follow symlinks. Once `lstat`
/// has run, its type is the truth and the entry is classified again when the
/// listing disagreed: the catalog stores that inode, so the decision should
/// describe it. A name skipped from `d_type` alone is not statted. Regular
/// files are `lstat`ed so `decide` can see the size. Anything that is not
/// skipped is `lstat`ed for the catalog fields. A catalogued symlink gets one
/// `readlink`; the link is never opened. If that `lstat` or `readlink` fails,
/// the walk emits [`Event::Io`] and no [`Event::Decided`].
///
/// Mount points are crossed: the walk does not compare `st_dev` with the
/// root. The walk is single-threaded.
pub fn walk(root: &Path, global: Option<&str>, config: Config, visit: impl FnMut(Event<'_>)) {
    let mut walker = Walker::new(root, visit);
    let Some(children) = walker.list() else {
        return;
    };
    // The root is outside a work tree unless it contains `.git` (D22).
    let loaded = walker.load_ignores(&children, false);
    let (rules, errors) = DirRules::root(root, global, loaded.files(), config);
    walker.patterns(errors);
    walker.walk_listed(&rules, children);
}

// ── walk state ──

struct Walker<F> {
    /// Absolute or caller-supplied path of the directory under consideration.
    /// Kept in lockstep with [`Self::rel`]: every push and pop touches both.
    abs: PathBuf,
    /// Root-relative path. Empty at the root. Reused for every entry.
    rel: PathBuf,
    visit: F,
}

struct Child {
    name: OsString,
    kind: io::Result<FileType>,
}

struct Classified {
    entry: Entry,
    /// Already `lstat`ed for a regular file, so a kept file is not statted
    /// twice.
    meta: Option<Metadata>,
}

struct Ignores {
    ferretignore: Option<String>,
    gitignore: Option<String>,
    git_root: bool,
    git_exclude: Option<String>,
}

impl Ignores {
    fn files(&self) -> IgnoreFiles<'_> {
        IgnoreFiles {
            ferretignore: self.ferretignore.as_deref(),
            gitignore: self.gitignore.as_deref(),
            git_root: self.git_root,
            git_exclude: self.git_exclude.as_deref(),
        }
    }
}

/// What `.git` is, from `lstat`. A symlink is [`GitKind::Present`], not a
/// directory and not a file, so nothing is opened through it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GitKind {
    Missing,
    /// A real directory: `.git/info/exclude` may be read.
    Directory,
    /// A regular file. May hold `gitdir: <path>`; exclude comes from there.
    File,
    /// A symlink or other non-regular entry. Still starts a work tree
    /// (so `.gitignore` applies) but contributes no exclude.
    Present,
}

impl<F> Walker<F>
where
    F: FnMut(Event<'_>),
{
    fn new(root: &Path, visit: F) -> Self {
        let mut abs = PathBuf::from(root);
        abs.reserve(256);
        let mut rel = PathBuf::new();
        rel.reserve(256);
        Self { abs, rel, visit }
    }

    fn push(&mut self, name: impl AsRef<OsStr>) {
        let name = name.as_ref();
        self.rel.push(name);
        self.abs.push(name);
    }

    fn pop(&mut self) {
        let rel = self.rel.pop();
        let abs = self.abs.pop();
        debug_assert!(rel && abs, "pop past the walk root");
    }

    fn fail(&mut self, error: io::Error) {
        (self.visit)(Event::Io {
            path: &self.rel,
            error,
        });
    }

    fn patterns(&mut self, errors: Vec<PatternError>) {
        for error in errors {
            (self.visit)(Event::Pattern(error));
        }
    }

    fn emit(&mut self, decision: Decision, meta: Option<&Metadata>, target: Option<&OsStr>) {
        let stat = meta.map(|meta| Stat {
            size: meta.len(),
            mtime_sec: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
            ctime_sec: meta.ctime(),
            ctime_nsec: meta.ctime_nsec(),
            dev: meta.dev(),
            ino: meta.ino(),
            mode: meta.mode(),
            uid: meta.uid(),
            gid: meta.gid(),
            link_target: target,
        });
        (self.visit)(Event::Decided(Decided {
            path: &self.rel,
            decision,
            stat,
        }));
    }

    /// Lists the current directory, or reports one fault and returns `None`.
    ///
    /// Names are collected so the listing fd is closed before recursion
    /// (otherwise a deep tree pins one fd per level) and so a directory that
    /// cannot be listed is a single fault, before any ignore file is probed.
    fn list(&mut self) -> Option<Vec<Child>> {
        match self.read_children() {
            Ok(children) => Some(children),
            Err(error) => {
                self.fail(error);
                None
            }
        }
    }

    fn read_children(&mut self) -> io::Result<Vec<Child>> {
        let iter = fs::read_dir(&self.abs)?;
        let mut children = Vec::new();
        for item in iter {
            match item {
                Ok(entry) => children.push(Child {
                    name: entry.file_name(),
                    kind: entry.file_type(),
                }),
                Err(error) => self.fail(error),
            }
        }
        Ok(children)
    }

    fn walk_listed(&mut self, rules: &DirRules, children: Vec<Child>) {
        for child in children {
            self.push(&child.name);
            match child.kind {
                Ok(kind) => self.consider(rules, &child.name, kind),
                Err(error) => self.fail(error),
            }
            self.pop();
        }
    }

    fn consider(&mut self, rules: &DirRules, name: &OsStr, kind: FileType) {
        let Some(classified) = self.classify(kind) else {
            return;
        };
        // A regular file was lstat'd in classify. That stat's type wins over
        // the listing's d_type before the first decision.
        let entry = match &classified.meta {
            Some(meta) => entry_from_meta(meta),
            None => classified.entry,
        };
        let mut decision = rules.decide(&self.rel, entry);
        if decision == Decision::Skip {
            self.emit(decision, None, None);
            return;
        }
        let Some(meta) = self.metadata_for(classified.meta) else {
            return;
        };
        let observed = entry_from_meta(&meta);
        if !same_type(entry, observed) {
            decision = rules.decide(&self.rel, observed);
            if decision == Decision::Skip {
                self.emit(decision, None, None);
                return;
            }
        }
        let target = match self.link_target(decision) {
            Ok(target) => target,
            Err(error) => {
                self.fail(error);
                return;
            }
        };
        self.emit(
            decision,
            Some(&meta),
            target.as_deref().map(Path::as_os_str),
        );
        self.follow(rules, name, decision);
    }

    /// Symlink first: `d_type` of a link is `DT_LNK`, including a link to a
    /// directory, and `file_type` does not follow it. When the filesystem
    /// reports `DT_UNKNOWN`, std fills the type in with `lstat`, which also
    /// does not follow.
    fn classify(&mut self, kind: FileType) -> Option<Classified> {
        if kind.is_symlink() {
            Some(Classified {
                entry: Entry::Symlink,
                meta: None,
            })
        } else if kind.is_dir() {
            Some(Classified {
                entry: Entry::Dir,
                meta: None,
            })
        } else if kind.is_file() {
            match fs::symlink_metadata(&self.abs) {
                Ok(meta) => Some(Classified {
                    entry: entry_from_meta(&meta),
                    meta: Some(meta),
                }),
                Err(error) => {
                    self.fail(error);
                    None
                }
            }
        } else {
            Some(Classified {
                entry: Entry::Other,
                meta: None,
            })
        }
    }

    fn metadata_for(&mut self, have: Option<Metadata>) -> Option<Metadata> {
        if let Some(meta) = have {
            return Some(meta);
        }
        match fs::symlink_metadata(&self.abs) {
            Ok(meta) => Some(meta),
            Err(error) => {
                self.fail(error);
                None
            }
        }
    }

    /// `Ok(None)` when `decision` is not a catalogued symlink. A `readlink`
    /// error is returned to the caller, which emits it and no `Decided`.
    fn link_target(&mut self, decision: Decision) -> io::Result<Option<PathBuf>> {
        if decision != Decision::Catalog(Reason::Symlink) {
            return Ok(None);
        }
        fs::read_link(&self.abs).map(Some)
    }

    fn follow(&mut self, rules: &DirRules, name: &OsStr, decision: Decision) {
        match decision {
            Decision::Descend => self.enter_and_list(rules, name),
            Decision::Traverse => self.traverse_and_list(rules, name),
            Decision::Skip | Decision::Catalog(_) | Decision::Index => {}
        }
    }

    fn enter_and_list(&mut self, parent: &DirRules, name: &OsStr) {
        let Some(children) = self.list() else {
            return;
        };
        let loaded = self.load_ignores(&children, parent.in_work_tree());
        let (rules, errors) = parent.enter(name, loaded.files());
        self.patterns(errors);
        self.walk_listed(&rules, children);
    }

    fn traverse_and_list(&mut self, parent: &DirRules, name: &OsStr) {
        let Some(children) = self.list() else {
            return;
        };
        let rules = parent.traverse(name);
        self.walk_listed(&rules, children);
    }

    // ── ignore files ──

    /// Only names in the listing are opened: a directory without ignore files,
    /// the usual case, costs no failed `open`s.
    ///
    /// `in_work_tree` is the parent directory. The root passes `false`.
    fn load_ignores(&mut self, children: &[Child], in_work_tree: bool) -> Ignores {
        let listed = |name: &str| children.iter().any(|child| child.name == name);
        let ferretignore = listed(".ferretignore")
            .then(|| self.read_named(".ferretignore"))
            .flatten();
        let git = if listed(".git") {
            self.probe_git()
        } else {
            GitKind::Missing
        };
        // A `.gitignore` outside a work tree cannot affect decisions, so it is
        // not opened. A FIFO of that name must not stall a walk that is not in
        // a repository.
        let gitignore = if (in_work_tree || git != GitKind::Missing) && listed(".gitignore") {
            self.read_named(".gitignore")
        } else {
            None
        };
        let git_exclude = match git {
            GitKind::Directory => self.read_exclude(),
            GitKind::File => self.read_gitfile_exclude(),
            GitKind::Missing | GitKind::Present => None,
        };
        Ignores {
            ferretignore,
            gitignore,
            git_root: git != GitKind::Missing,
            git_exclude,
        }
    }

    fn read_named(&mut self, name: &str) -> Option<String> {
        self.push(name);
        let text = self.read_at_cursor();
        self.pop();
        text
    }

    fn read_exclude(&mut self) -> Option<String> {
        self.push(".git");
        self.push("info");
        self.push("exclude");
        let text = self.read_at_cursor();
        self.pop();
        self.pop();
        self.pop();
        text
    }

    /// Text of the file at the cursor, or `None` when it is absent or not a
    /// regular file. Any other error is reported and the file is treated as
    /// absent so the rest of the directory still walks.
    fn read_at_cursor(&mut self) -> Option<String> {
        match open_regular(&self.abs, false) {
            Ok(Some(bytes)) => Some(decode_lossy(bytes)),
            Ok(None) => None,
            Err(error) => {
                self.fail(error);
                None
            }
        }
    }

    fn probe_git(&mut self) -> GitKind {
        self.push(".git");
        let kind = match fs::symlink_metadata(&self.abs) {
            Ok(meta) if meta.is_dir() => GitKind::Directory,
            Ok(meta) if meta.is_file() => GitKind::File,
            Ok(_) => GitKind::Present,
            Err(error) if error.kind() == io::ErrorKind::NotFound => GitKind::Missing,
            Err(error) => {
                self.fail(error);
                GitKind::Missing
            }
        };
        self.pop();
        kind
    }

    /// Exclude for a regular `.git` file: `gitdir:` then an optional
    /// `commondir`, then `<common>/info/exclude`. A fault is reported against
    /// the `.git` file. The gitdir may sit outside the walk root (a linked
    /// work tree, a submodule's module directory).
    fn read_gitfile_exclude(&mut self) -> Option<String> {
        self.push(".git");
        let bytes = match open_regular(&self.abs, true) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.fail(error);
                self.pop();
                return None;
            }
        };
        self.pop();
        let bytes = bytes?;
        let raw = parse_gitdir(&bytes)?;
        let gitdir = resolve_git_path(&self.abs, raw);
        let common = self.common_dir(&gitdir)?;
        self.read_external(&common.join("info").join("exclude"))
    }

    /// The gitdir itself when it has no `commondir` file. `None` when that
    /// file cannot be read or names nothing: a linked work tree's own
    /// directory is not where exclude lives, so it is not a fallback.
    fn common_dir(&mut self, gitdir: &Path) -> Option<PathBuf> {
        match open_regular(&gitdir.join("commondir"), false) {
            Ok(None) => Some(gitdir.to_path_buf()),
            Ok(Some(bytes)) => {
                let raw = first_line(&bytes)?;
                Some(resolve_git_path(gitdir, raw))
            }
            Err(error) => {
                self.fail_at_git(error);
                None
            }
        }
    }

    fn read_external(&mut self, path: &Path) -> Option<String> {
        match open_regular(path, false) {
            Ok(Some(bytes)) => Some(decode_lossy(bytes)),
            Ok(None) => None,
            Err(error) => {
                self.fail_at_git(io::Error::new(
                    error.kind(),
                    format!("{}: {error}", path.display()),
                ));
                None
            }
        }
    }

    fn fail_at_git(&mut self, error: io::Error) {
        self.push(".git");
        self.fail(error);
        self.pop();
    }
}

/// Opens `path` for reading without blocking, and returns its bytes when the
/// open file is a regular file of at most [`MAX_IGNORE_BYTES`].
///
/// `Ok(None)` is absence: nothing was there, or the opened inode is not a
/// regular file (a FIFO, directory, or device of an ignore-file's name). The
/// open uses `O_NONBLOCK` so a FIFO does not wait for a writer. `nofollow`
/// adds `O_NOFOLLOW`, for the `.git` file only. The check is `fstat` on that
/// file, not a prior `lstat`, so a name that changes between the listing and
/// the open is classified from what was actually opened.
fn open_regular(path: &Path, nofollow: bool) -> io::Result<Option<Vec<u8>>> {
    let mut flags = O_NONBLOCK;
    if nofollow {
        flags |= O_NOFOLLOW;
    }
    let file = match OpenOptions::new().read(true).custom_flags(flags).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Ok(None);
    }
    if meta.len() > MAX_IGNORE_BYTES {
        return Err(ignore_too_large());
    }
    let mut bytes = Vec::new();
    file.take(MAX_IGNORE_BYTES + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_IGNORE_BYTES {
        return Err(ignore_too_large());
    }
    Ok(Some(bytes))
}

fn ignore_too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::FileTooLarge,
        "ignore file is larger than 1 MiB",
    )
}

/// First line of a gitdir or commondir file, without its newline. Spaces are
/// part of the path: git does not trim them.
fn first_line(bytes: &[u8]) -> Option<&OsStr> {
    let line = bytes.split(|byte| *byte == b'\n').next()?;
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.is_empty() {
        None
    } else {
        Some(OsStr::from_bytes(line))
    }
}

/// `gitdir: <path>` from the first line. The path is relative to the directory
/// that holds the `.git` file. Anything else is not a gitdir pointer.
fn parse_gitdir(bytes: &[u8]) -> Option<&OsStr> {
    let raw = first_line(bytes)?.as_bytes().strip_prefix(b"gitdir: ")?;
    if raw.is_empty() {
        None
    } else {
        Some(OsStr::from_bytes(raw))
    }
}

fn resolve_git_path(base: &Path, raw: &OsStr) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn entry_from_meta(meta: &Metadata) -> Entry {
    let kind = meta.file_type();
    if kind.is_symlink() {
        Entry::Symlink
    } else if kind.is_dir() {
        Entry::Dir
    } else if kind.is_file() {
        Entry::File { size: meta.len() }
    } else {
        Entry::Other
    }
}

fn same_type(left: Entry, right: Entry) -> bool {
    matches!(
        (left, right),
        (Entry::Dir, Entry::Dir)
            | (Entry::Symlink, Entry::Symlink)
            | (Entry::Other, Entry::Other)
            | (Entry::File { .. }, Entry::File { .. })
    )
}

fn decode_lossy(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}
