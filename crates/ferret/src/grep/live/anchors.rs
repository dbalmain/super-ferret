//! Where the live policy is anchored: the catalog's configured roots, the
//! operands they do not cover, and the global ignore text.

use std::os::unix::ffi::OsStrExt;
use std::path::Component;
use std::sync::Arc;

use ferret_catalog::{Catalog, Section};
use ferret_policy::{Decision, Entry as PolicyEntry};

use super::*;
use crate::config_file::Contents;

#[derive(Clone)]
pub(in crate::grep) struct Anchors {
    pub(super) roots: Arc<[PathBuf]>,
    uncovered: Arc<[PathBuf]>,
    pub(super) fallback: bool,
    /// `--rg`: the uncovered operands walk under rg's rules; otherwise each
    /// is a policy root of its own, as if it were indexed (D128).
    rg: bool,
    faults: Arc<[(PathBuf, io::Error)]>,
    /// The first configured root whose destination cannot be established:
    /// an unreadable component or a link loop may hide an alias to any
    /// scope, so it is relevant to every walked operand.
    unknown: Option<Arc<str>>,
    global: Arc<str>,
    pub(super) git_global: Arc<ferret_policy::GrepIgnore>,
    index: Arc<Path>,
}

impl Anchors {
    pub fn open(index: Option<&Path>) -> Result<Self, String> {
        Self::load(&index_path(index)?, global()?)
    }
    pub(super) fn load(index: &Path, global: Arc<str>) -> Result<Self, String> {
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
            rg: false,
            faults: faults.into(),
            unknown,
            global,
            git_global: Arc::default(),
            index: index.to_owned().into(),
        }
    }
    /// Resolves coverage before either the engine or the daemon is opened.
    /// `walk_all` leaves every directory operand uncovered.
    pub fn routing(index: Option<&Path>, inputs: &[Input], walk_all: bool) -> Result<Self, String> {
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
                if walk_all
                    || !anchors.roots.iter().any(|root| real.starts_with(root))
                    || !spellings.resolves(path, catalog.as_ref())?
                {
                    uncovered.push(path.clone());
                }
            }
        }
        anchors.uncovered = uncovered.into();
        Ok(anchors)
    }
    /// Loads the global rules for the uncovered operands: git's global
    /// excludes under `--rg`, ferret's global ignore file otherwise. Kept
    /// apart from [`Self::routing`] because its warnings are output: a
    /// request refused after routing must not have printed them.
    pub fn with_globals(mut self, rg: bool) -> Result<Self, String> {
        self.rg = rg;
        if self.uncovered.is_empty() {
            return Ok(self);
        }
        if !rg {
            self.global = global()?;
            return Ok(self);
        }
        self.global = git_global_excludes()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
            .into();
        let (rules, errors) = ferret_policy::GrepIgnore::new(&self.global);
        for error in errors {
            eprintln!("ferret: warning: ignore pattern skipped: {error}");
        }
        self.git_global = Arc::new(rules);
        Ok(self)
    }
    /// The walk applies rg's rules rather than D13.
    pub(super) fn rg_rules(&self) -> bool {
        self.fallback && self.rg
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
        if self.rg_rules() {
            return self.start_rg(path, real, stopped);
        }
        // An uncovered operand is a policy root of its own.
        let root = if self.fallback {
            &real
        } else {
            self.roots
                .iter()
                .find(|root| real.starts_with(root))
                .ok_or_else(|| io::Error::other("no covering policy root"))?
        };
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
    pub(super) fn root(
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

#[cfg(test)]
thread_local! {
    pub(super) static OPENED: std::cell::RefCell<Option<Catalog>> = const { std::cell::RefCell::new(None) };
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
}
