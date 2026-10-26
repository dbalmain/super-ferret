//! Current-tree policy over find's live source: D13 for --live, rg rules
//! for uncovered operands. Coverage is resolved before engine/daemon routing.
//! ferret-policy owns rule decisions; ferret-crawl owns scoped input reads.

use super::*;
use crate::config_file::Contents;
use ferret_catalog::{Catalog, Section};
use ferret_policy::{Config, Decision, DirRules, Entry as PolicyEntry};
use ferret_query::find::Entry;
use std::path::Component;
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct Anchors {
    roots: Arc<[PathBuf]>,
    uncovered: Arc<[PathBuf]>,
    fallback: bool,
    faults: Arc<[(PathBuf, io::Error)]>,
    /// The first configured root whose destination cannot be established:
    /// an unreadable component or a link loop may hide an alias to any
    /// scope, so it is relevant to every walked operand.
    unknown: Option<Arc<str>>,
    global: Arc<str>,
    git_global: Arc<ferret_policy::GrepIgnore>,
    index: Arc<Path>,
}

impl Anchors {
    pub fn open(index: Option<&Path>) -> Result<Self, String> {
        Self::load(&index_path(index)?, global()?)
    }
    fn load(index: &Path, global: Arc<str>) -> Result<Self, String> {
        let catalog = Catalog::open(index)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                format!(
                    "no index in {}; index this directory or use --no-ignore",
                    index.display()
                )
            })?;
        catalog.load(&[Section::Roots]).map_err(|e| e.to_string())?;
        #[cfg(test)]
        OPENED.with_borrow_mut(|opened| *opened = Some(catalog.clone()));
        let anchors = Self::from_paths(
            index,
            global,
            catalog.roots().map(|(_, path)| path.to_vec()),
        );
        // `--live` needs every anchor's destination before it walks.
        match anchors.unknown {
            Some(unknown) => Err(unknown.to_string()),
            None => Ok(anchors),
        }
    }
    fn from_paths(
        index: &Path,
        global: Arc<str>,
        paths: impl IntoIterator<Item = Vec<u8>>,
    ) -> Self {
        let mut roots = Vec::new();
        let mut faults = Vec::new();
        let mut unknown = None;
        for root in paths {
            let path = Path::new(std::ffi::OsStr::from_bytes(&root));
            match fs::canonicalize(path) {
                Ok(root) => roots.push(root),
                // Stored roots retain symlink spellings. Even failed anchors
                // need the same namespace as canonicalized operands.
                Err(error) => match failed_scope(path) {
                    Ok(scope) => faults.push((scope, error)),
                    Err(e) => {
                        unknown.get_or_insert_with(|| {
                            format!("cannot resolve policy root {}: {e}", path.display()).into()
                        });
                    }
                },
            }
        }
        roots.sort_by_key(|root| std::cmp::Reverse(root.components().count()));
        roots.dedup();
        Self {
            roots: roots.into(),
            uncovered: Arc::default(),
            fallback: false,
            faults: faults.into(),
            unknown,
            global,
            git_global: Arc::default(),
            index: index.to_owned().into(),
        }
    }
    /// Resolves coverage before either the engine or the daemon is opened.
    pub fn routing(index: Option<&Path>, inputs: &[Input]) -> Result<Self, String> {
        let index = index_path(index)?;
        let catalog = Catalog::open(&index).map_err(|e| e.to_string())?;
        let paths = catalog
            .as_ref()
            .map(Catalog::configured_paths)
            .transpose()
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        // The catalog resolves an operand by its spelling, so a canonical
        // match alone would claim an alias (`alias -> root`) the catalog
        // cannot name. Covered needs both.
        let spellings = Spellings {
            roots: paths
                .iter()
                .map(|path| PathBuf::from(std::ffi::OsStr::from_bytes(path)))
                .collect(),
            crossed: std::cell::Cell::new(false),
        };
        // A root whose destination is unknown can only add coverage, so an
        // operand a resolved root covers keeps its snapshot; it is relevant
        // only to the uncovered operands `validate` checks before walking.
        let mut anchors = Self::from_paths(&index, "".into(), paths);
        anchors.fallback = true;
        let mut uncovered = Vec::new();
        for input in inputs {
            if let Input::Directory(path) = input {
                let real = fs::canonicalize(path).map_err(|e| e.to_string())?;
                if !anchors.roots.iter().any(|root| real.starts_with(root))
                    || !spellings.resolves(path, catalog.as_ref())?
                {
                    uncovered.push(path.clone());
                }
            }
        }
        anchors.uncovered = uncovered.into();
        Ok(anchors)
    }
    /// Loads git's global excludes for the uncovered operands. Kept apart
    /// from [`Self::routing`] because its warnings are output: a request
    /// refused after routing must not have printed them.
    pub fn with_git_global(mut self) -> Self {
        if !self.uncovered.is_empty() {
            self.global = git_global_excludes()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default()
                .into();
            let (rules, errors) = ferret_policy::GrepIgnore::new(&self.global);
            for error in errors {
                eprintln!("ferret: warning: ignore pattern skipped: {error}");
            }
            self.git_global = Arc::new(rules);
        }
        self
    }
    pub fn walks(&self, path: &Path) -> bool {
        !self.fallback || self.uncovered.iter().any(|operand| operand == path)
    }
    pub fn all_covered(&self) -> bool {
        self.uncovered.is_empty()
    }
    pub fn any_covered(&self, inputs: &[Input]) -> bool {
        inputs
            .iter()
            .any(|input| matches!(input, Input::Directory(path) if !self.walks(path)))
    }
    pub fn search_threads(&self) -> usize {
        engine::search_threads_for(&self.index)
    }
    pub fn validate(&self, path: &Path) -> io::Result<PathBuf> {
        let current = fs::canonicalize(path)?;
        if let Some(unknown) = &self.unknown {
            return Err(io::Error::other(unknown.to_string()));
        }
        if let Some((root, error)) = self
            .faults
            .iter()
            .find(|(root, _)| current.starts_with(root) || root.starts_with(&current))
        {
            return Err(io::Error::new(
                error.kind(),
                format!("policy root {}: {error}", root.display()),
            ));
        }
        let covered = self.roots.iter().any(|root| current.starts_with(root))
            || self.fallback && self.walks(path);
        if !covered {
            return Err(io::Error::other(
                "directory is outside configured roots; index it or use --no-ignore",
            ));
        }
        Ok(current)
    }
    pub fn start(&self, path: &Path, stopped: &dyn Fn() -> bool) -> io::Result<Policy> {
        let real = self.validate(path)?;
        if self.fallback {
            return self.start_rg(path, real, stopped);
        }
        let root = self
            .roots
            .iter()
            .find(|root| real.starts_with(root))
            .ok_or_else(|| io::Error::other("no covering policy root"))?;
        let mut state = self.root(root, None, stopped)?;
        let mut at = root.clone();
        for component in real
            .strip_prefix(root)
            .map_err(io::Error::other)?
            .components()
        {
            at.push(component);
            state = if !matches!(state, State::Bypass) && self.roots.contains(&at) {
                self.root(&at, None, stopped)?
            } else {
                match state {
                    State::Rules { root, rules } => {
                        let relative = at.strip_prefix(&root).map_err(io::Error::other)?;
                        match rules.decide(relative, PolicyEntry::Dir) {
                            Decision::Descend => State::Rules {
                                root,
                                rules: enter(&rules, &at, None, stopped)?,
                            },
                            Decision::Traverse => State::Rules {
                                root,
                                rules: rules.traverse(component.as_os_str()),
                            },
                            // An explicit ignored start uses find's opaque
                            // fallback: no nested ignore files are consulted.
                            _ => State::Bypass,
                        }
                    }
                    other => other,
                }
            };
        }
        Ok(Policy {
            anchors: self.clone(),
            spelling: path.to_owned(),
            real: real.clone(),
            stack: vec![(real, state)],
            warnings: None,
        })
    }
    fn start_rg(
        &self,
        path: &Path,
        real: PathBuf,
        stopped: &dyn Fn() -> bool,
    ) -> io::Result<Policy> {
        // Rg reads `.ignore` and `.rgignore` in every ancestor, but git rules
        // only from the nearest work-tree top down. Loading from `/` gives
        // both: entering a directory holding `.git` starts a work tree, which
        // drops the enclosing gitignore rules and keeps the others.
        let root = Path::new("/");
        let mut rules = load_rg(root, None, None, None, stopped)?;
        let mut at = root.to_owned();
        for component in real
            .strip_prefix(root)
            .map_err(io::Error::other)?
            .components()
        {
            at.push(component);
            rules = load_rg(&at, Some(&rules), None, None, stopped)?;
        }
        Ok(Policy {
            anchors: self.clone(),
            spelling: path.to_owned(),
            real: real.clone(),
            stack: vec![(
                real.clone(),
                State::Rules {
                    root: root.to_owned(),
                    rules,
                },
            )],
            warnings: None,
        })
    }
    fn root(
        &self,
        path: &Path,
        warnings: Option<&mut Vec<String>>,
        stopped: &dyn Fn() -> bool,
    ) -> io::Result<State> {
        let rules = load(path, None, Some(&self.global), warnings, stopped)?;
        Ok(State::Rules {
            root: path.to_owned(),
            rules,
        })
    }
}

/// Stored root spellings over the live filesystem: the catalog's namespace
/// for spellings that cross no symlink inside a root, without loading names.
/// [`resolve_in`] walks it as it walks the catalog. A symlink inside a root
/// may sit in an opaque (ignored) directory the catalog stops at, or may
/// have changed since the crawl, so a spelling that crosses one is resolved
/// in the catalog itself.
///
/// [`resolve_in`]: ferret_query::find::resolve_in
struct Spellings {
    roots: Vec<PathBuf>,
    /// Set when a walk met a symlink inside a root.
    crossed: std::cell::Cell<bool>,
}

impl Spellings {
    /// Whether the catalog can resolve `path`'s spelling. Spellings that
    /// cross an in-root symlink load the catalog's name sections (never
    /// content) and ask it.
    fn resolves(&self, path: &Path, catalog: Option<&Catalog>) -> Result<bool, String> {
        self.crossed.set(false);
        let resolves = resolvable(self, path);
        match catalog {
            Some(catalog) if self.crossed.get() => {
                catalog
                    .load(ferret_query::find::CatalogSource::sections())
                    .map_err(|e| e.to_string())?;
                Ok(resolvable(catalog, path))
            }
            _ => Ok(resolves),
        }
    }
}

/// Whether a start spelled `path` resolves, as a start does: following a
/// final symlink, else the link itself. A missing child inside the
/// namespace, or any other fault, still belongs to the snapshot, which
/// reports it.
fn resolvable(namespace: &impl ferret_query::find::Namespace, path: &Path) -> bool {
    [true, false].into_iter().any(|follow| {
        !matches!(
            ferret_query::find::resolve_in(namespace, path, follow),
            Ok(None)
        )
    })
}

impl ferret_query::find::Namespace for Spellings {
    type Node = (PathBuf, fs::FileType);
    fn resolve(&self, path: &Path) -> Option<(Self::Node, bool)> {
        if !self.roots.iter().any(|root| path.starts_with(root)) {
            return None;
        }
        let kind = fs::symlink_metadata(path).ok()?.file_type();
        Some(((path.to_owned(), kind), false))
    }
    fn link_target(&self, (path, kind): &Self::Node) -> Option<PathBuf> {
        if !kind.is_symlink() {
            return None;
        }
        self.crossed.set(true);
        fs::read_link(path).ok()
    }
    fn is_directory(&self, (_, kind): &Self::Node) -> bool {
        kind.is_dir()
    }
}

// read_link expands aliases even when their targets have disappeared.
// Missing components have no aliases to resolve; an unreadable component or
// a link loop leaves the destination unknown and must fail closed.
fn failed_scope(path: &Path) -> io::Result<PathBuf> {
    let mut resolved = PathBuf::new();
    let mut pending = vec![path.to_owned()];
    let mut links = 0;
    while let Some(part) = pending.pop() {
        let mut components = part.components();
        while let Some(component) = components.next() {
            match component {
                Component::RootDir => resolved.push(component.as_os_str()),
                Component::CurDir => {}
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::Normal(name) => {
                    let candidate = resolved.join(name);
                    match fs::read_link(&candidate) {
                        Ok(target) => {
                            links += 1;
                            if links > 40 {
                                return Err(io::Error::other(
                                    "policy root exceeds 40 symlink expansions",
                                ));
                            }
                            pending.push(components.as_path().to_owned());
                            pending.push(target);
                            break;
                        }
                        // readlink's EINVAL means this component is not a link.
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::InvalidInput
                                    | io::ErrorKind::NotFound
                                    | io::ErrorKind::NotADirectory
                            ) =>
                        {
                            resolved.push(name)
                        }
                        Err(error) => return Err(error),
                    }
                }
                Component::Prefix(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "policy root has a non-Unix prefix",
                    ));
                }
            }
        }
    }
    Ok(resolved)
}

fn global() -> Result<Arc<str>, String> {
    let text = if let Ok(dirs) = Dirs::from_env() {
        let path = dirs.ignore_file();
        match crate::config_file::read(&path).map_err(|e| format!("{}: {e}", path.display()))? {
            Contents::Bytes(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Contents::NotRegular => {
                return Err(format!(
                    "{}: global ignore is not a regular file",
                    path.display()
                ));
            }
            Contents::Missing => ferret_policy::DEFAULT_IGNORE.to_owned(),
        }
    } else {
        ferret_policy::DEFAULT_IGNORE.to_owned()
    };
    Ok(text.into())
}

#[derive(Clone)]
enum State {
    Rules { root: PathBuf, rules: DirRules },
    Bypass,
}

#[derive(Clone)]
pub(super) struct Policy {
    anchors: Anchors,
    spelling: PathBuf,
    real: PathBuf,
    stack: Vec<(PathBuf, State)>,
    /// Held for the caller to print, when deferred.
    warnings: Option<Vec<String>>,
}
impl Policy {
    pub fn rg(&self) -> bool {
        self.anchors.fallback
    }
    /// Holds warnings from directories' rules for [`Self::take_warnings`],
    /// so a worker can print them under its own ordering.
    pub fn deferring(mut self) -> Self {
        self.warnings = Some(Vec::new());
        self
    }

    pub fn take_warnings(&mut self) -> Vec<String> {
        self.warnings
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    pub fn selects(
        &mut self,
        entry: &Entry,
        selection: &GrepSelection,
        cwd: &Path,
        content: bool,
        stopped: &dyn Fn() -> bool,
    ) -> io::Result<bool> {
        self.accepts_with(entry, content, stopped, Some((selection, cwd)))
    }

    pub fn accepts(
        &mut self,
        entry: &Entry,
        content: bool,
        stopped: &dyn Fn() -> bool,
    ) -> io::Result<bool> {
        self.accepts_with(entry, content, stopped, None)
    }

    fn accepts_with(
        &mut self,
        entry: &Entry,
        content: bool,
        stopped: &dyn Fn() -> bool,
        selection: Option<(&GrepSelection, &Path)>,
    ) -> io::Result<bool> {
        if stopped() {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        if entry.depth() == 0 {
            return Ok(true);
        }
        let path = self.real.join(
            entry
                .path()
                .strip_prefix(&self.spelling)
                .map_err(io::Error::other)?,
        );
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("live entry has no parent"))?;
        while self
            .stack
            .last()
            .is_some_and(|(directory, _)| directory != parent)
        {
            self.stack.pop();
        }
        let (_, state) = self
            .stack
            .last()
            .ok_or_else(|| io::Error::other("live policy stack lost its parent"))?;
        let kind = entry.kind()?;
        let directory = kind == FileKind::Directory;
        if directory
            && !self.anchors.fallback
            && !matches!(state, State::Bypass)
            && self.anchors.roots.contains(&path)
        {
            let state = self.anchors.root(&path, self.warnings.as_mut(), stopped)?;
            self.stack.push((path, state));
            return Ok(true);
        }
        let item = match kind {
            FileKind::Directory => PolicyEntry::Dir,
            // Rg's rules have no size cap (`load_grep_policy`), so a file's
            // size would go unused; skip the stat.
            FileKind::File if self.anchors.fallback => PolicyEntry::File { size: 0 },
            FileKind::File => PolicyEntry::File {
                size: entry.metadata()?.len(),
            },
            FileKind::Symlink => PolicyEntry::Symlink,
            _ => PolicyEntry::Other,
        };
        let local_rule = match state {
            State::Rules { rules, .. } => {
                rules.ignore_match(std::ffi::OsStr::from_bytes(entry.name()), directory)
            }
            State::Bypass => None,
        };
        // Rg applies the global git excludes only inside a work tree.
        let in_work_tree = matches!(state, State::Rules { rules, .. } if rules.in_work_tree());
        let ignore_rule = local_rule.or_else(|| {
            let (_, cwd) = selection.filter(|_| in_work_tree)?;
            let absolute = cwd.join(entry.path());
            self.anchors.git_global.decision(
                absolute
                    .strip_prefix(cwd)
                    .ok()
                    .map(|path| path.as_os_str().as_bytes()),
                entry.name(),
                directory,
            )
        });
        let override_rule = if let Some((selection, cwd)) = selection {
            let relative = entry
                .path()
                .strip_prefix(cwd)
                .unwrap_or(entry.path())
                .as_os_str()
                .as_bytes();
            let whitelist = ignore_rule == Some(true);
            if !selection.selects(
                entry.path().as_os_str().as_bytes(),
                relative,
                entry.depth(),
                directory,
                whitelist,
            ) {
                return Ok(false);
            }
            selection.override_for(relative, directory)
        } else {
            None
        };
        let decision = if override_rule == Some(true) {
            if directory {
                Decision::Descend
            } else {
                Decision::Index
            }
        } else if selection.is_some() && ignore_rule == Some(false) {
            Decision::Skip
        } else {
            match state {
                State::Rules { root, rules } => {
                    rules.decide(path.strip_prefix(root).map_err(io::Error::other)?, item)
                }
                State::Bypass => match item {
                    PolicyEntry::Dir => Decision::Descend,
                    PolicyEntry::File { size } if size <= Config::default().size_cap => {
                        Decision::Index
                    }
                    _ => Decision::Catalog(ferret_policy::Reason::TooLarge),
                },
            }
        };
        if directory {
            let next = match (decision, state) {
                (Decision::Descend, State::Rules { root, rules }) => State::Rules {
                    root: root.clone(),
                    rules: if self.anchors.fallback {
                        load_rg(&path, Some(rules), None, self.warnings.as_mut(), stopped)?
                    } else {
                        enter(rules, &path, self.warnings.as_mut(), stopped)?
                    },
                },
                (Decision::Traverse, State::Rules { root, rules }) => State::Rules {
                    root: root.clone(),
                    rules: rules.traverse(std::ffi::OsStr::from_bytes(entry.name())),
                },
                (Decision::Descend, State::Bypass) => State::Bypass,
                _ => return Ok(false),
            };
            self.stack.push((path, next));
            Ok(true)
        } else {
            Ok(matches!(decision, Decision::Index)
                || (!content && matches!(decision, Decision::Catalog(_))))
        }
    }
}

fn enter(
    parent: &DirRules,
    path: &Path,
    warnings: Option<&mut Vec<String>>,
    stopped: &dyn Fn() -> bool,
) -> io::Result<DirRules> {
    load(path, Some(parent), None, warnings, stopped)
}

/// Loads a directory's rules. Warnings go to `warnings` when given, else
/// straight to stderr.
fn load(
    path: &Path,
    parent: Option<&DirRules>,
    global: Option<&str>,
    warnings: Option<&mut Vec<String>>,
    stopped: &dyn Fn() -> bool,
) -> io::Result<DirRules> {
    load_kind(path, parent, global, warnings, stopped, false)
}

fn load_rg(
    path: &Path,
    parent: Option<&DirRules>,
    global: Option<&str>,
    warnings: Option<&mut Vec<String>>,
    stopped: &dyn Fn() -> bool,
) -> io::Result<DirRules> {
    load_kind(path, parent, global, warnings, stopped, true)
}
fn load_kind(
    path: &Path,
    parent: Option<&DirRules>,
    global: Option<&str>,
    mut warnings: Option<&mut Vec<String>>,
    stopped: &dyn Fn() -> bool,
    grep: bool,
) -> io::Result<DirRules> {
    if stopped() {
        return Err(io::ErrorKind::ConnectionAborted.into());
    }
    let mut fault = None;
    let mut visit = |event: ferret_crawl::Event<'_, ()>| match event {
        ferret_crawl::Event::Pattern(error) => {
            let warning = format!("ferret: warning: ignore pattern skipped: {error}");
            match warnings.as_deref_mut() {
                Some(warnings) => warnings.push(warning),
                None => eprintln!("{warning}"),
            }
        }
        // Rg silently does without an ignore input it cannot read (an
        // unreadable `.ignore`, `.git` or exclude, a gitfile naming no
        // gitdir) and applies what did load; D13's --live keeps it fatal.
        ferret_crawl::Event::Io {
            path: relative,
            error,
            ..
        } if fault.is_none() && !grep => {
            fault = Some(io::Error::new(
                error.kind(),
                format!("{}: {error}", path.join(relative).display()),
            ));
        }
        _ => {}
    };
    let rules = if grep {
        ferret_crawl::load_grep_policy(path, parent, global, &mut visit)
    } else {
        ferret_crawl::load_policy(path, parent, global, Config::default(), &mut visit)
    }?;
    if stopped() {
        return Err(io::ErrorKind::ConnectionAborted.into());
    }
    if let Some(error) = fault {
        return Err(error);
    }
    Ok(rules)
}

/// Git's global excludes as rg 15.2 reads them (D115 B, measured as a black
/// box). The first line of the form `excludesFile = VALUE` in
/// `$GIT_CONFIG_GLOBAL`, then `~/.gitconfig`, then the git config under the
/// XDG config directory names the file, whatever section holds the line; a
/// file without one passes to the next. With none, git's default `git/ignore`
/// in that directory applies. A named file that cannot be read means no
/// global excludes rather than the default, and nothing here is an error.
fn git_global_excludes() -> Option<Vec<u8>> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".config")));
    let sources = [
        std::env::var_os("GIT_CONFIG_GLOBAL")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from),
        home.as_ref().map(|home| home.join(".gitconfig")),
        config.as_ref().map(|config| config.join("git/config")),
    ];
    let named = sources.into_iter().flatten().find_map(|source| {
        let text = read_global(&source).ok().flatten()?;
        let value = text.split(|&b| b == b'\n').find_map(excludes_file)?;
        // A value that is not UTF-8 discards the whole file, as in rg.
        let value = std::str::from_utf8(value).ok()?;
        Some(match (value.strip_prefix("~/"), &home) {
            (Some(rest), Some(home)) => home.join(rest),
            _ => PathBuf::from(value),
        })
    });
    let path = named.or_else(|| config.map(|config| config.join("git/ignore")))?;
    read_global(&path).ok().flatten()
}

/// The value of an `excludesFile = VALUE` line, as rg recognises one: the
/// key in any case at the start of the line, and one word of value, with
/// either quote optional. Anything after the value, a comment included,
/// means the line does not count. A value that is only quotes counts and
/// names no file, so no global excludes apply.
fn excludes_file(line: &[u8]) -> Option<&[u8]> {
    let line = line.trim_ascii();
    let key = b"excludesfile";
    if line.len() < key.len() || !line[..key.len()].eq_ignore_ascii_case(key) {
        return None;
    }
    let value = line[key.len()..]
        .trim_ascii_start()
        .strip_prefix(b"=")?
        .trim_ascii_start();
    if value.is_empty() || value.iter().any(u8::is_ascii_whitespace) {
        return None;
    }
    let value = value.strip_prefix(b"\"").unwrap_or(value);
    Some(value.strip_suffix(b"\"").unwrap_or(value))
}

// git's configuration and excludes file: anything but a regular file reads as
// absent, and so does any error (see `git_global_excludes`).
fn read_global(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match crate::config_file::read(path)? {
        Contents::Bytes(bytes) => Ok(Some(bytes)),
        Contents::Missing | Contents::NotRegular => Ok(None),
    }
}

#[cfg(test)]
thread_local! {
    static OPENED: std::cell::RefCell<Option<Catalog>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;

    // Exercise the production resolver at both sides of its expansion bound,
    // including absolute targets, relative targets and parent components.
    #[test]
    fn failed_scope_expands_forty_links_but_rejects_forty_one() {
        let base =
            std::env::temp_dir().join(format!("ferret-live-link-limit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("sub")).unwrap();
        for index in 0..40 {
            let target = match index {
                0 => base.join("link-1"),
                39 => PathBuf::from("sub/../missing"),
                _ => PathBuf::from(format!("link-{}", index + 1)),
            };
            std::os::unix::fs::symlink(target, base.join(format!("link-{index}"))).unwrap();
        }
        assert_eq!(
            failed_scope(&base.join("link-0/child")).unwrap(),
            base.join("missing/child")
        );
        std::os::unix::fs::symlink("link-0", base.join("extra")).unwrap();
        assert!(
            failed_scope(&base.join("extra/child"))
                .unwrap_err()
                .to_string()
                .contains("40 symlink expansions")
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn live_roots_and_policy_walk_leave_traversal_and_content_sections_unloaded() {
        let base =
            std::env::temp_dir().join(format!("ferret-live-sections-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("root");
        let index = base.join("index");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("plain"), b"red\n").unwrap();
        fs::write(root.join("sub/new"), b"red\n").unwrap();
        ferret_crawl::index(
            &index,
            std::slice::from_ref(&root),
            ferret_crawl::Refresh::All,
            &ferret_crawl::IndexOptions::default(),
        )
        .unwrap();
        let anchors = Anchors::load(&index, "".into()).unwrap();
        for content in [false, true] {
            let mut policy = anchors.start(&root, &|| false).unwrap();
            let mut source = sources::Directory::new(&root, None, &root, true).unwrap();
            let mut descend = true;
            let mut files = Vec::new();
            while let Some(entry) = source.next(descend) {
                let entry = entry.unwrap();
                let allowed = policy.accepts(entry, content, &|| false).unwrap();
                descend = allowed && entry.kind().unwrap() == FileKind::Directory;
                if allowed && entry.kind().unwrap() == FileKind::File {
                    files.push(entry.path().to_owned());
                }
            }
            files.sort();
            assert_eq!(files, vec![root.join("plain"), root.join("sub/new")]);
        }
        OPENED.with_borrow_mut(|opened| {
            let catalog = opened.take().unwrap();
            assert!(catalog.is_loaded(Section::Roots));
            // Roots' validation dependency chain includes DirNames/Names.
            assert!(catalog.is_loaded(Section::Names));
            for section in [
                Section::Links,
                Section::Entries,
                Section::Doc,
                Section::States,
                Section::Docs,
                Section::DocRefs,
            ] {
                assert!(!catalog.is_loaded(section), "{section:?}");
            }
        });
        fs::remove_dir_all(base).unwrap();
    }
}
