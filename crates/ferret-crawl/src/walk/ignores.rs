//! A directory's ignore files, and the `.git` probe that decides whether it
//! starts a work tree and where that tree's `info/exclude` lives; and
//! [`load_policy`], which reads them for one directory outside a walk.

use super::*;

/// What `.git` is. The crawl leaves a symlink as [`GitProbe::Present`] and
/// opens nothing through it. Grep follows one: a resolved directory is
/// [`GitProbe::Directory`] (an `O_PATH` handle, so missing read permission
/// does not hide `info/exclude`), a resolved file is [`GitProbe::File`],
/// and a target that does not resolve is [`GitProbe::Missing`]. A held
/// directory descriptor is what `info/exclude` is read from, so a later
/// swap of the name cannot redirect it.
enum GitProbe {
    Missing,
    Directory(OwnedFd),
    /// Bytes of a regular `.git` file, already read from the opened inode.
    File(Vec<u8>),
    /// Starts a work tree, so `.gitignore` applies, but contributes no
    /// exclude. The crawl uses this for a symlink. Grep uses it when the
    /// target resolved and is not a directory or a readable gitfile.
    Present,
}

impl GitProbe {
    fn is_root(&self) -> bool {
        !matches!(self, Self::Missing)
    }
}

/// A [`WorkTree`] held until [`Event::Entered`] lends it. One per work tree,
/// so the path allocation is not per entry.
pub(super) struct FoundWorkTree {
    pub(super) kind: WorkTreeKind,
    pub(super) common_dir: PathBuf,
    pub(super) common_id: (u64, u64),
}

pub(super) struct Ignores {
    ferretignore: Option<String>,
    gitignore: Option<String>,
    git_root: bool,
    git_exclude: Option<String>,
    ignore: Option<String>,
    rgignore: Option<String>,
    pub(super) work_tree: Option<FoundWorkTree>,
}

impl Ignores {
    pub(super) fn files(&self) -> IgnoreFiles<'_> {
        IgnoreFiles {
            ferretignore: self.ferretignore.as_deref(),
            gitignore: self.gitignore.as_deref(),
            git_root: self.git_root,
            git_exclude: self.git_exclude.as_deref(),
            ignore: self.ignore.as_deref(),
            rgignore: self.rgignore.as_deref(),
        }
    }
}

enum Opened {
    Missing,
    NotRegular,
    Bytes(Vec<u8>),
}

impl<V: EventVisitor> Walker<'_, V> {
    /// Only names in the listing are opened: a directory without ignore files,
    /// the usual case, costs no failed `open`s.
    ///
    /// `in_work_tree` is the parent directory. The root passes `false`.
    /// `.git`, when it is a directory, is opened here and that descriptor is
    /// what `info/exclude` is read from, including across the `.gitignore`
    /// callback below. `token` is the directory being loaded.
    pub(super) fn load_ignores(
        &mut self,
        dir: BorrowedFd<'_>,
        listed: impl Fn(&str) -> bool,
        in_work_tree: bool,
        token: V::Dir,
    ) -> Ignores {
        let inputs = [
            ".ferretignore",
            ".ignore",
            ".rgignore",
            DOT_GIT,
            ".gitignore",
        ]
        .map(OsStr::new);
        // `.gitignore` counts only inside a work tree; see below for a root.
        let inputs = match in_work_tree {
            true => &inputs[..],
            false => &inputs[..4],
        };
        self.visit.policy_inputs(dir, inputs);
        let [ferretignore, ignore, rgignore] =
            [".ferretignore", ".ignore", ".rgignore"].map(|name| {
                listed(name)
                    .then(|| self.read_named(dir, name, token))
                    .flatten()
            });
        let git = if listed(DOT_GIT) {
            self.probe_git(dir, token)
        } else {
            GitProbe::Missing
        };
        // A `.gitignore` outside a work tree cannot affect decisions, so it is
        // not opened. A FIFO of that name must not stall a walk that is not in
        // a repository.
        if git.is_root() {
            self.visit.policy_input(dir, OsStr::new(".gitignore"));
        }
        let gitignore = if (in_work_tree || git.is_root()) && listed(".gitignore") {
            self.read_named(dir, ".gitignore", token)
        } else {
            None
        };
        let git_root = git.is_root();
        let (work_tree, git_exclude) = match git {
            GitProbe::Directory(fd) => {
                let work_tree = self.main_work_tree(&fd, token);
                (work_tree, self.read_exclude(fd, token))
            }
            GitProbe::File(bytes) => self.read_gitfile(dir, &bytes, in_work_tree, token),
            GitProbe::Missing | GitProbe::Present => (None, None),
        };
        Ignores {
            ferretignore,
            gitignore,
            git_root,
            git_exclude,
            ignore,
            rgignore,
            work_tree,
        }
    }

    fn read_named(&mut self, dir: BorrowedFd<'_>, name: &str, token: V::Dir) -> Option<String> {
        self.visit.policy_input(dir, OsStr::new(name));
        let length = self.push(name);
        let text = match open_ignore(dir, OsStr::new(name)) {
            Ok(Opened::Bytes(bytes)) => Some(decode_lossy(bytes)),
            Ok(Opened::Missing | Opened::NotRegular) => None,
            Err(error) => {
                self.fail_child(IoOp::ReadIgnore, token, OsStr::new(name), error);
                None
            }
        };
        self.pop(length);
        text
    }

    /// `info` is opened `O_NOFOLLOW` relative to the held git directory (or
    /// common directory). `exclude` is an ordinary ignore file: a symlink of
    /// that name is followed.
    fn read_exclude(&mut self, git: OwnedFd, token: V::Dir) -> Option<String> {
        self.visit.policy_input(git.as_fd(), OsStr::new("info"));
        let git_length = self.push(DOT_GIT);
        let info_length = self.push("info");
        let info = match openat(git.as_fd(), "info", child_dir_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => {
                self.pop(git_length);
                return None;
            }
            Err(error) => {
                self.push("exclude");
                self.fail_child(
                    IoOp::ReadIgnore,
                    token,
                    OsStr::new(DOT_GIT),
                    io::Error::from(error),
                );
                self.pop(git_length);
                return None;
            }
        };
        drop(git);
        self.visit.policy_input(info.as_fd(), OsStr::new("exclude"));
        self.push("exclude");
        let opened = match openat(info.as_fd(), "exclude", ignore_flags(), Mode::empty()) {
            Ok(fd) => {
                drop(info);
                read_opened(fd)
            }
            Err(Errno::NOENT) => Ok(Opened::Missing),
            Err(error) => Err(io::Error::from(error)),
        };
        let text = match opened {
            Ok(Opened::Bytes(bytes)) => Some(decode_lossy(bytes)),
            Ok(Opened::Missing | Opened::NotRegular) => None,
            Err(error) => {
                self.fail_child(IoOp::ReadIgnore, token, OsStr::new(DOT_GIT), error);
                None
            }
        };
        self.pop(info_length);
        self.pop(git_length);
        text
    }

    fn probe_git(&mut self, dir: BorrowedFd<'_>, token: V::Dir) -> GitProbe {
        let flags = if self.rg_git {
            // Follow, and do not require read permission on the directory.
            path_dir_flags()
        } else {
            child_dir_flags()
        };
        match openat(dir, DOT_GIT, flags, Mode::empty()) {
            Ok(fd) => GitProbe::Directory(fd),
            Err(Errno::NOENT) => GitProbe::Missing,
            Err(Errno::LOOP) if self.rg_git => GitProbe::Missing,
            Err(Errno::LOOP) => GitProbe::Present,
            Err(Errno::NOTDIR) => self.probe_git_file(dir, token),
            Err(error) => self.probe_git_failed(dir, error, token),
        }
    }

    /// The directory open failed for a reason other than "not a directory"
    /// or a symlink loop. A directory we cannot search still starts a work
    /// tree; a regular file is read below. Grep has already tried `O_PATH`,
    /// so a directory that merely lacks read permission does not arrive
    /// here. A symlink that does is one whose target did not resolve.
    fn probe_git_failed(&mut self, dir: BorrowedFd<'_>, error: Errno, token: V::Dir) -> GitProbe {
        match statat(dir, DOT_GIT, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if file_type(&stat) == FileType::RegularFile => {
                self.probe_git_file(dir, token)
            }
            Ok(stat) if file_type(&stat) == FileType::Directory => {
                self.fail_at_git(io::Error::from(error), token);
                GitProbe::Present
            }
            Ok(stat) if self.rg_git && file_type(&stat) == FileType::Symlink => {
                self.symlink_follow_failed(dir)
            }
            Ok(_) => GitProbe::Present,
            Err(Errno::NOENT) => GitProbe::Missing,
            Err(stat_err) => {
                self.fail_at_git(io::Error::from(stat_err), token);
                GitProbe::Missing
            }
        }
    }

    /// Following a symlinked `.git` failed. `stat` follows, and needs only
    /// search permission, so it distinguishes a target that resolved (an
    /// unreadable file still starts a work tree) from one that did not
    /// (search denied, or a non-directory component). The latter is
    /// absence, as a dangling link is, and is not a fault.
    fn symlink_follow_failed(&self, dir: BorrowedFd<'_>) -> GitProbe {
        if statat(dir, DOT_GIT, AtFlags::empty()).is_ok() {
            GitProbe::Present
        } else {
            GitProbe::Missing
        }
    }

    /// `.git` is not a directory. `O_NOFOLLOW` so a symlink that appeared
    /// since the listing is not a gitdir file.
    fn probe_git_file(&mut self, dir: BorrowedFd<'_>, token: V::Dir) -> GitProbe {
        let flags = if self.rg_git {
            nofollow_file_flags() - OFlags::NOFOLLOW
        } else {
            nofollow_file_flags()
        };
        match openat(dir, DOT_GIT, flags, Mode::empty()) {
            Ok(fd) => match read_regular(fd) {
                Ok(Some(bytes)) => GitProbe::File(bytes),
                Ok(None) => GitProbe::Present,
                Err(error) => {
                    self.fail_at_git(error, token);
                    GitProbe::Present
                }
            },
            Err(Errno::NOENT) => GitProbe::Missing,
            Err(Errno::LOOP) if self.rg_git => GitProbe::Missing,
            Err(Errno::LOOP) => GitProbe::Present,
            Err(error) => match statat(dir, DOT_GIT, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) if file_type(&stat) == FileType::RegularFile => {
                    self.fail_at_git(io::Error::from(error), token);
                    GitProbe::Present
                }
                Ok(stat) if self.rg_git && file_type(&stat) == FileType::Symlink => {
                    self.symlink_follow_failed(dir)
                }
                Ok(_) => GitProbe::Present,
                Err(Errno::NOENT) => GitProbe::Missing,
                Err(stat_err) => {
                    self.fail_at_git(io::Error::from(stat_err), token);
                    GitProbe::Missing
                }
            },
        }
    }

    /// A `.git` directory: the work tree is main and `.git` is its common
    /// directory.
    fn main_work_tree(&mut self, git: &OwnedFd, token: V::Dir) -> Option<FoundWorkTree> {
        let common_id = self.git_identity(git, token)?;
        Some(FoundWorkTree {
            kind: WorkTreeKind::Main,
            common_dir: normalize(&self.here_path().join(DOT_GIT)),
            common_id,
        })
    }

    /// Follow git's `gitdir:` and optional `commondir` from held descriptors.
    /// A relative path may include `..`, as git's format requires. An
    /// absolute gitdir is opened by path because it is outside the tree.
    fn read_gitfile(
        &mut self,
        work: BorrowedFd<'_>,
        bytes: &[u8],
        in_work_tree: bool,
        token: V::Dir,
    ) -> (Option<FoundWorkTree>, Option<String>) {
        let Some(raw) = parse_gitdir(bytes) else {
            self.fail_at_git(
                io::Error::new(io::ErrorKind::InvalidData, "not a `gitdir:` file"),
                token,
            );
            return (None, None);
        };
        let Some(gitdir) = self.open_git_directory(work, raw, token) else {
            return (None, None);
        };
        let gitdir_path = self.here_path().join(raw);
        let Some((common, commondir)) = self.common_dir(gitdir, token) else {
            return (None, None);
        };
        let excluded = commondir.is_some() || !self.rg_git;
        let (kind, common_path) = match commondir {
            Some(text) => (WorkTreeKind::Linked, gitdir_path.join(text)),
            None if in_work_tree => (WorkTreeKind::Submodule, gitdir_path),
            None => (WorkTreeKind::Main, gitdir_path),
        };
        let work_tree = self
            .git_identity(&common, token)
            .map(|common_id| FoundWorkTree {
                kind,
                common_dir: normalize(&common_path),
                common_id,
            });
        let exclude = if excluded {
            self.read_exclude(common, token)
        } else {
            None
        };
        (work_tree, exclude)
    }

    /// The directory being loaded, as a path: the root the caller gave, made
    /// absolute, joined with the root-relative path. Only for work trees.
    fn here_path(&self) -> PathBuf {
        let root = std::path::absolute(&self.root).unwrap_or_else(|_| self.root.clone());
        if self.rel.is_empty() {
            root
        } else {
            root.join(bytes_path(&self.rel))
        }
    }

    fn git_identity(&mut self, git: &OwnedFd, token: V::Dir) -> Option<(u64, u64)> {
        match fstat(git) {
            Ok(stat) => Some((stat.st_dev, stat.st_ino)),
            Err(error) => {
                self.fail_at_git(io::Error::from(error), token);
                None
            }
        }
    }

    /// Symlinks are followed, as git follows them: the gitdir's text can
    /// already name any directory, so `O_NOFOLLOW` here would protect nothing.
    /// What the handle protects is `base`, which a rename cannot redirect.
    /// Grep opens `O_PATH`, so an execute-only directory still yields a
    /// handle `info/exclude` can be read from by name. The crawl opens
    /// `O_RDONLY` and faults when that directory is not readable.
    fn open_git_directory(
        &mut self,
        base: BorrowedFd<'_>,
        raw: &OsStr,
        token: V::Dir,
    ) -> Option<OwnedFd> {
        let path = Path::new(raw);
        let dependency = if path.is_absolute() {
            path.to_owned()
        } else {
            PathBuf::from(format!(
                "/proc/{}/fd/{}",
                std::process::id(),
                base.as_raw_fd()
            ))
            .join(path)
        };
        self.visit.policy_path(&dependency);
        let flags = if self.rg_git {
            path_dir_flags()
        } else {
            root_dir_flags()
        };
        let opened = if path.is_absolute() {
            open_path(path, flags, Mode::empty())
        } else {
            openat(base, path, flags, Mode::empty())
        };
        // A gitdir or common directory that does not exist is a known state:
        // the repository was moved or pruned, so there are no exclude rules
        // and no work-tree record to lose. `policy_path` above already
        // watches where it would appear. Any other failure leaves them
        // unknown, and is a fault.
        match opened {
            Ok(fd) => Some(fd),
            Err(Errno::NOENT) => None,
            Err(error) => {
                self.fail_at_git(io::Error::from(error), token);
                None
            }
        }
    }

    /// The gitdir itself when it has no `commondir` file, with `None` as the
    /// second value; otherwise the directory `commondir` names and its text.
    /// `None`, after a [`IoOp::ProbeGit`] fault, when that file cannot be
    /// read, is not a regular file, names nothing, or names a directory that
    /// cannot be opened; `None` without a fault when the directory it names
    /// does not exist. A linked work tree's own directory is not where
    /// exclude lives, so it is not a fallback. Only a missing `commondir`
    /// means the gitdir is the common directory.
    fn common_dir(
        &mut self,
        gitdir: OwnedFd,
        token: V::Dir,
    ) -> Option<(OwnedFd, Option<OsString>)> {
        self.visit
            .policy_input(gitdir.as_fd(), OsStr::new("commondir"));
        let opened = match openat(
            gitdir.as_fd(),
            "commondir",
            nofollow_file_flags(),
            Mode::empty(),
        ) {
            Ok(fd) => read_opened(fd),
            Err(Errno::NOENT) => Ok(Opened::Missing),
            Err(error) => Err(io::Error::from(error)),
        };
        match opened {
            Ok(Opened::Missing) => Some((gitdir, None)),
            Ok(Opened::NotRegular) => {
                self.fail_at_git(
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "commondir is not a regular file",
                    ),
                    token,
                );
                None
            }
            Ok(Opened::Bytes(bytes)) => {
                let Some(raw) = first_line(&bytes) else {
                    self.fail_at_git(
                        io::Error::new(io::ErrorKind::InvalidData, "commondir is empty"),
                        token,
                    );
                    return None;
                };
                let common = self.open_git_directory(gitdir.as_fd(), raw, token)?;
                drop(gitdir);
                Some((common, Some(raw.to_os_string())))
            }
            Err(error) => {
                self.fail_at_git(error, token);
                None
            }
        }
    }

    fn fail_at_git(&mut self, error: io::Error, token: V::Dir) {
        let length = self.push(DOT_GIT);
        self.fail_child(IoOp::ProbeGit, token, OsStr::new(DOT_GIT), error);
        self.pop(length);
    }
}

/// Loads one directory's policy with the same ignore reads, Git probes and
/// diagnostics as the crawler, without enumerating or reading its contents.
/// `parent` supplies inherited rules; `None` starts a configured root with
/// `global` and `config`. The directory itself is opened without following a
/// symlink, unless `listed` supplies it already open. Policy names are then
/// probed directly; with `listed`, only names its listing held are opened.
///
/// I/O faults within policy inputs are reported as [`Event::Io`], and bad
/// patterns as [`Event::Pattern`]. Valid patterns still apply. The caller
/// decides whether input faults prevent using the returned rules. Failure to
/// open the directory itself is returned directly.
pub fn load_policy(
    path: &Path,
    parent: Option<&DirRules>,
    global: Option<&str>,
    config: Config,
    listed: Option<Listed<'_>>,
    visit: impl FnMut(Event<'_, ()>),
) -> io::Result<DirRules> {
    load_query_policy(path, parent, global, config, false, listed, visit)
}

/// A directory its caller already holds open and has listed, so loading its
/// policy reads through `dir` and opens only the inputs `contains` names,
/// with no open of its path and no probe per input. `contains` answers for
/// names beginning with `.`; every policy input is one.
#[derive(Clone, Copy)]
pub struct Listed<'a> {
    pub dir: BorrowedFd<'a>,
    pub contains: &'a dyn Fn(&OsStr) -> bool,
}

/// Loads rg's git, `.ignore` and `.rgignore` inputs with the crawler's scoped
/// reads. Ferret ignore files are not consulted; content has no size cap.
/// As in rg, a symlinked `.git` is followed. A target that does not resolve
/// — a loop, a dangling link, or a component that cannot be traversed —
/// starts no work tree. An execute-only directory still does: it is opened
/// `O_PATH` and `info/exclude` is read by name. A `.git` file contributes
/// `info/exclude` only through a `commondir`, including when that directory
/// is execute-only. Diagnostics and directory-open failures follow
/// [`load_policy`].
pub fn load_grep_policy(
    path: &Path,
    parent: Option<&DirRules>,
    global: Option<&str>,
    listed: Option<Listed<'_>>,
    visit: impl FnMut(Event<'_, ()>),
) -> io::Result<DirRules> {
    load_query_policy(
        path,
        parent,
        global,
        Config { size_cap: u64::MAX },
        true,
        listed,
        visit,
    )
}

fn load_query_policy(
    path: &Path,
    parent: Option<&DirRules>,
    global: Option<&str>,
    config: Config,
    grep: bool,
    listed: Option<Listed<'_>>,
    visit: impl FnMut(Event<'_, ()>),
) -> io::Result<DirRules> {
    // Without a listing, policy inputs are probed by name, so grep can load
    // an ancestor it may search but not read, as rg does.
    let opened;
    let fd = match listed {
        Some(listed) => listed.dir,
        None => {
            opened = open_path(
                path,
                if grep {
                    OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC
                } else {
                    child_dir_flags()
                },
                Mode::empty(),
            )?;
            opened.as_fd()
        }
    };
    let boundaries = BoundaryIndex::default();
    let mut walker = Walker::new(path, &boundaries, visit);
    walker.rg_git = grep;
    let loaded = walker.load_ignores(
        fd,
        |name| {
            if grep && name == ".ferretignore" {
                return false;
            }
            match listed {
                Some(listed) => (listed.contains)(OsStr::new(name)),
                None => !matches!(
                    statat(fd, name, AtFlags::SYMLINK_NOFOLLOW),
                    Err(Errno::NOENT)
                ),
            }
        },
        parent.is_some_and(DirRules::in_work_tree),
        (),
    );
    let (rules, errors) = if let Some(parent) = parent {
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "policy directory has no name")
        })?;
        parent.enter(name, loaded.files())
    } else {
        DirRules::root(path, global, loaded.files(), config)
    };
    walker.patterns(errors);
    Ok(rules)
}

/// `.` and `..` resolved lexically; `..` at the top stays at the top. Git
/// writes gitdir and `commondir` as real paths, so a symlink that makes `..`
/// mean something else is not expected there; [`WorkTree::common_id`] is the
/// identity either way.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Opens ignore file `name`: `.gitignore` without following a symlink, which
/// then reads as absent; any other name through one.
fn open_ignore(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<Opened> {
    let flags = if name == ".gitignore" {
        gitignore_flags()
    } else {
        ignore_flags()
    };
    match openat(dir, name, flags, Mode::empty()) {
        Ok(fd) => read_opened(fd),
        Err(Errno::NOENT) => Ok(Opened::Missing),
        Err(Errno::LOOP) if flags.contains(OFlags::NOFOLLOW) => Ok(Opened::Missing),
        Err(error) => Err(io::Error::from(error)),
    }
}

fn read_opened(fd: OwnedFd) -> io::Result<Opened> {
    match read_regular(fd)? {
        Some(bytes) => Ok(Opened::Bytes(bytes)),
        None => Ok(Opened::NotRegular),
    }
}

/// Bytes of `fd` when it is a regular file of at most [`MAX_IGNORE_BYTES`].
///
/// `Ok(None)` means the opened inode is not a regular file. The check is
/// `fstat` on the descriptor that was opened, not a prior `lstat`.
fn read_regular(fd: OwnedFd) -> io::Result<Option<Vec<u8>>> {
    let file = File::from(fd);
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Ok(None);
    }
    if meta.len() > MAX_IGNORE_BYTES {
        return Err(ignore_too_large());
    }
    #[cfg(test)]
    if let Some(hook) = AFTER_IGNORE_STAT.with_borrow_mut(Option::take) {
        hook();
    }
    // Size the buffer from the stat. An empty `Vec` makes `read_to_end` grow
    // by small steps, one `read` each, on every ignore file.
    let mut bytes = Vec::new();
    let len = usize::try_from(meta.len()).unwrap_or(0);
    let _ = bytes.try_reserve_exact(len);
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

fn decode_lossy(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}
