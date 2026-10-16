//! Current-tree D13 policy over find's live source. Catalog roots anchor the
//! policy; names and postings never exclude live input. Pure rule decisions
//! remain in ferret-policy; ferret-crawl owns scoped policy loading.

use super::*;
use ferret_catalog::{Catalog, Section};
use ferret_policy::{Config, Decision, DirRules, Entry as PolicyEntry};
use ferret_query::find::Entry;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;

// Linux open flags, as in the other std-only CLI filesystem hosts. Opening
// non-regular global ignore entries must not block on a FIFO.
const NONBLOCK: i32 = 0x800;

#[derive(Clone)]
pub(super) struct Anchors {
    roots: Arc<[PathBuf]>,
    global: Arc<str>,
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
        let mut roots = Vec::new();
        for (_, root) in catalog.roots() {
            // Only existing anchors can cover a current directory. An unrelated
            // vanished root does not prevent querying another configured root.
            match fs::canonicalize(Path::new(std::ffi::OsStr::from_bytes(root))) {
                Ok(root) => roots.push(root),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        roots.sort_by_key(|root| std::cmp::Reverse(root.components().count()));
        roots.dedup();
        Ok(Self {
            roots: roots.into(),
            global,
            index: index.to_owned().into(),
        })
    }
    pub fn search_threads(&self) -> usize {
        engine::search_threads_for(&self.index)
    }
    pub fn validate(&self, path: &Path) -> io::Result<PathBuf> {
        let current = fs::canonicalize(path)?;
        if !self.roots.iter().any(|root| current.starts_with(root)) {
            return Err(io::Error::other(
                "directory is outside configured roots; index it or use --no-ignore",
            ));
        }
        Ok(current)
    }
    pub fn start(&self, path: &Path, stopped: &dyn Fn() -> bool) -> io::Result<Policy> {
        let real = self.validate(path)?;
        let root = self
            .roots
            .iter()
            .find(|root| real.starts_with(root))
            .ok_or_else(|| io::Error::other("no covering policy root"))?;
        let mut state = self.root(root, stopped)?;
        let mut at = root.clone();
        for component in real
            .strip_prefix(root)
            .map_err(io::Error::other)?
            .components()
        {
            at.push(component);
            state = if !matches!(state, State::Bypass) && self.roots.contains(&at) {
                self.root(&at, stopped)?
            } else {
                match state {
                    State::Rules { root, rules } => {
                        let relative = at.strip_prefix(&root).map_err(io::Error::other)?;
                        match rules.decide(relative, PolicyEntry::Dir) {
                            Decision::Descend => State::Rules {
                                root,
                                rules: enter(&rules, &at, stopped)?,
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
        })
    }
    fn root(&self, path: &Path, stopped: &dyn Fn() -> bool) -> io::Result<State> {
        let rules = load(path, None, Some(&self.global), stopped)?;
        Ok(State::Rules {
            root: path.to_owned(),
            rules,
        })
    }
}

fn global() -> Result<Arc<str>, String> {
    let text = if let Ok(dirs) = Dirs::from_env() {
        let path = dirs.ignore_file();
        match read_global(&path).map_err(|e| format!("{}: {e}", path.display()))? {
            Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            None if fs::symlink_metadata(&path).is_ok() => {
                return Err(format!(
                    "{}: global ignore is not a regular file",
                    path.display()
                ));
            }
            None => ferret_policy::DEFAULT_IGNORE.to_owned(),
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

pub(super) struct Policy {
    anchors: Anchors,
    spelling: PathBuf,
    real: PathBuf,
    stack: Vec<(PathBuf, State)>,
}
impl Policy {
    pub fn accepts(
        &mut self,
        entry: &Entry,
        content: bool,
        stopped: &dyn Fn() -> bool,
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
        if directory && !matches!(state, State::Bypass) && self.anchors.roots.contains(&path) {
            let state = self.anchors.root(&path, stopped)?;
            self.stack.push((path, state));
            return Ok(true);
        }
        let item = match kind {
            FileKind::Directory => PolicyEntry::Dir,
            FileKind::File => PolicyEntry::File {
                size: entry.metadata()?.len(),
            },
            FileKind::Symlink => PolicyEntry::Symlink,
            _ => PolicyEntry::Other,
        };
        let decision = match state {
            State::Rules { root, rules } => {
                rules.decide(path.strip_prefix(root).map_err(io::Error::other)?, item)
            }
            State::Bypass => match item {
                PolicyEntry::Dir => Decision::Descend,
                PolicyEntry::File { size } if size <= Config::default().size_cap => Decision::Index,
                _ => Decision::Catalog(ferret_policy::Reason::TooLarge),
            },
        };
        if directory {
            let next = match (decision, state) {
                (Decision::Descend, State::Rules { root, rules }) => State::Rules {
                    root: root.clone(),
                    rules: enter(rules, &path, stopped)?,
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

fn enter(parent: &DirRules, path: &Path, stopped: &dyn Fn() -> bool) -> io::Result<DirRules> {
    load(path, Some(parent), None, stopped)
}

fn load(
    path: &Path,
    parent: Option<&DirRules>,
    global: Option<&str>,
    stopped: &dyn Fn() -> bool,
) -> io::Result<DirRules> {
    if stopped() {
        return Err(io::ErrorKind::ConnectionAborted.into());
    }
    let mut fault = None;
    let rules =
        ferret_crawl::load_policy(
            path,
            parent,
            global,
            Config::default(),
            |event| match event {
                ferret_crawl::Event::Pattern(error) => {
                    eprintln!("ferret: warning: ignore pattern skipped: {error}");
                }
                ferret_crawl::Event::Io {
                    path: relative,
                    error,
                    ..
                } if fault.is_none() => {
                    fault = Some(io::Error::new(
                        error.kind(),
                        format!("{}: {error}", path.join(relative).display()),
                    ));
                }
                _ => {}
            },
        )?;
    if stopped() {
        return Err(io::ErrorKind::ConnectionAborted.into());
    }
    if let Some(error) = fault {
        return Err(error);
    }
    Ok(rules)
}

// Global configuration is not a scoped crawl input: it has no 1 MiB limit
// and a dangling link is an error. Queries must not create configuration.
fn read_global(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e)
            if e.kind() == io::ErrorKind::NotFound
                && fs::symlink_metadata(path)
                    .is_err_and(|e| e.kind() == io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    if !file.metadata()?.is_file() {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        bytes.try_reserve(n).map_err(io::Error::other)?;
        bytes.extend_from_slice(&chunk[..n]);
    }
    Ok(Some(bytes))
}

#[cfg(test)]
thread_local! {
    static OPENED: std::cell::RefCell<Option<Catalog>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;

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
