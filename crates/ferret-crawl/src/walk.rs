//! One root, walked depth-first, with one [`DirRules`] per directory.
//!
//! Seam: [`ferret_policy`] decides what to do with each entry. This module
//! reads the tree and reports. It does not touch the catalog.

use std::ffi::{OsStr, OsString};
use std::fs::{self, FileType, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use ferret_policy::{Config, Decision, DirRules, Entry, IgnoreFiles, PatternError, Reason};

/// `lstat` fields for an entry the walk did not skip.
///
/// Times are Unix `st_mtim` and `st_ctim` (seconds plus nanoseconds). `mode`
/// is `st_mode`, type bits included. `uid` and `gid` are `st_uid` and
/// `st_gid`: the catalog's inode row stores them, and they come from the same
/// `lstat` as the rest.
///
/// `link_target` is set only for a catalogued symlink. It is the raw
/// `readlink` bytes, not resolved and not re-encoded (D18).
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
/// absent for [`Decision::Skip`], and also when `lstat` failed after the
/// decision (an [`Event::Io`] for the same path was emitted as well).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decided<'a> {
    /// Root-relative path of the entry.
    pub path: &'a Path,
    /// What the policy said to do with `path`.
    pub decision: Decision,
    /// `lstat` of `path`, when the entry was not skipped and the stat
    /// succeeded.
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
/// Ignore files (`.ferretignore`, `.gitignore`, `.git`, and
/// `.git/info/exclude` when `.git` is a real directory) are read when the
/// walk enters a directory ([`Decision::Descend`]). A directory reached only
/// so a `.ferretignore` `!` pattern can re-include beneath it
/// ([`Decision::Traverse`]) is listed, and its ignore files are not read
/// (D13). Bytes that are not UTF-8 are converted lossily. A missing file is
/// absent, not a fault; a file that exists but cannot be read is a fault, and
/// that file is then treated as absent.
///
/// File types come from `read_dir` (`d_type`, or `lstat` when the filesystem
/// leaves the type unknown). `d_type` does not follow symlinks. Regular files
/// are `lstat`ed so `decide` can see the size. Anything that is not skipped
/// is `lstat`ed for the catalog fields. A catalogued symlink gets one
/// `readlink`; the link is never opened.
///
/// Mount points are crossed: the walk does not compare `st_dev` with the
/// root. The walk is single-threaded.
pub fn walk(root: &Path, global: Option<&str>, config: Config, visit: impl FnMut(Event<'_>)) {
    let mut walker = Walker::new(root, visit);
    let Some(children) = walker.list() else {
        return;
    };
    let loaded = walker.load_ignores();
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
/// directory, so exclude is never read through a link.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GitKind {
    Missing,
    /// A real directory: `.git/info/exclude` may be read.
    Directory,
    /// A file or a symlink. Still starts a work tree (submodule, linked work
    /// tree).
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
        let decision = rules.decide(&self.rel, classified.entry);
        if decision == Decision::Skip {
            self.emit(decision, None, None);
            return;
        }
        let Some(meta) = self.metadata_for(classified.meta) else {
            self.emit(decision, None, None);
            self.follow(rules, name, decision);
            return;
        };
        let target = self.link_target(decision);
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
                    entry: Entry::File { size: meta.len() },
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

    fn link_target(&mut self, decision: Decision) -> Option<PathBuf> {
        if decision != Decision::Catalog(Reason::Symlink) {
            return None;
        }
        match fs::read_link(&self.abs) {
            Ok(target) => Some(target),
            Err(error) => {
                self.fail(error);
                None
            }
        }
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
        let loaded = self.load_ignores();
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

    fn load_ignores(&mut self) -> Ignores {
        let ferretignore = self.read_named(".ferretignore");
        let gitignore = self.read_named(".gitignore");
        let git = self.probe_git();
        let git_exclude = if git == GitKind::Directory {
            self.read_exclude()
        } else {
            None
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

    /// `Ok` text, `None` if the file is absent. Any other error is reported
    /// and the file is treated as absent so the rest of the directory still
    /// walks.
    fn read_at_cursor(&mut self) -> Option<String> {
        match fs::read(&self.abs) {
            Ok(bytes) => Some(decode_lossy(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
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
}

fn decode_lossy(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}
