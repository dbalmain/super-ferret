//! Current-tree policy over find's live source: D13 for --live, rg rules
//! for uncovered operands. Coverage is resolved before engine/daemon routing.
//! ferret-policy owns rule decisions; ferret-crawl owns scoped input reads.

use std::os::unix::ffi::OsStrExt;
use std::sync::{Arc, OnceLock};

use ferret_policy::{Config, Decision, DirRules, Entry as PolicyEntry};
use ferret_query::find::{Entry, FileKind};

use super::*;
use crate::config_file::Contents;

mod anchors;
pub(super) use anchors::Anchors;

/// The override `selection` from `cwd` gives `entry`, or `None` when it
/// does not select the entry at all.
fn select(
    selection: &GrepSelection,
    cwd: &Path,
    entry: &Entry,
    directory: bool,
    ignore_rule: Option<bool>,
) -> Option<Option<bool>> {
    let relative = super::visibility::relative_to(entry.path(), cwd)
        .as_os_str()
        .as_bytes();
    let whitelist = ignore_rule == Some(true);
    selection
        .selects(
            entry.path().as_os_str().as_bytes(),
            relative,
            entry.depth(),
            directory,
            whitelist,
        )
        .then(|| selection.override_for(relative, directory))
}

/// The decision for an entry no policy root covers.
fn decide_uncovered(item: PolicyEntry) -> Decision {
    match item {
        PolicyEntry::Dir => Decision::Descend,
        PolicyEntry::File { size } if size <= Config::default().size_cap => Decision::Index,
        _ => Decision::Catalog(ferret_policy::Reason::TooLarge),
    }
}

/// The state a directory enters under `decision`, with its parent's root
/// and rules, or `None` when the walk does not enter it.
fn descend(
    decision: Decision,
    covered: Option<(&PathBuf, &DirRules)>,
    name: &std::ffi::OsStr,
) -> Option<State> {
    Some(match (decision, covered) {
        (Decision::Descend, Some((root, rules))) => State::Entering {
            root: root.clone(),
            parent: rules.clone(),
            rules: Arc::default(),
        },
        (Decision::Traverse, Some((root, rules))) => State::Rules {
            root: root.clone(),
            rules: rules.traverse(name),
        },
        (Decision::Descend, None) => State::Bypass,
        _ => return None,
    })
}

#[derive(Clone)]
enum State {
    Rules {
        root: PathBuf,
        rules: DirRules,
    },
    /// A descended directory whose rules load at its first entry
    /// ([`Policy::reach`]), when the walk has listed it and holds it open:
    /// its ignore files are then read through that handle, and only those
    /// the listing held are opened. The walk yields a directory's first
    /// entry in the step that lists it, before any loan can split the
    /// listing, so clones never meet this state today; they share the load
    /// anyway, so that a scheduler that lends earlier still loads, and
    /// warns, once.
    Entering {
        root: PathBuf,
        parent: DirRules,
        rules: Arc<OnceLock<Result<DirRules, Arc<io::Error>>>>,
    },
    /// Its rules failed to load; the failure was reported once, at the entry
    /// that tried, and every entry of it is skipped.
    Refused,
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
    /// Entries are selected by rg's rules rather than D13.
    pub fn rg(&self) -> bool {
        self.anchors.rg_rules()
    }
    /// The walk is of an uncovered operand, whose files are read as rg reads
    /// them in either mode: no size cap, rg's binary probe, one read (D123).
    pub fn uncovered(&self) -> bool {
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

    /// Loads the rules of the directory `entry` was listed in, if the walk
    /// entered it without them. A walk calls this for every entry it yields,
    /// before any selection that may skip it, so a directory's ignore files
    /// load (and warn) whenever it has an entry at all, as eagerly loading
    /// them did. An `Err` is this directory's failure, reported once.
    pub fn reach(&mut self, entry: &Entry, stopped: &dyn Fn() -> bool) -> io::Result<()> {
        if entry.depth() == 0 {
            return Ok(());
        }
        // The walk is depth first and each accepted directory pushed one
        // level, so the entry's parent is the level at its depth less one. A
        // lent walk's first entry unwinds its copy of the lender's stack.
        self.stack.truncate(entry.depth());
        let Some((
            path,
            State::Entering {
                root,
                parent,
                rules,
            },
        )) = self.stack.last()
        else {
            return Ok(());
        };
        let (path, root, parent, cell) =
            (path.clone(), root.clone(), parent.clone(), rules.clone());
        let listing = entry.listing();
        let contains = |name: &std::ffi::OsStr| {
            listing.is_none_or(|listing| listing.holds(name.as_bytes()) != Some(false))
        };
        let listed = listing.map(|listing| ferret_crawl::Listed {
            dir: listing.dir(),
            contains: &contains,
        });
        let mut loaded = false;
        let rules = cell.get_or_init(|| {
            loaded = true;
            let rg = self.anchors.rg_rules();
            load_kind(
                &path,
                Some(&parent),
                None,
                self.warnings.as_mut(),
                stopped,
                rg,
                listed,
            )
            .map_err(Arc::new)
        });
        let (state, result) = match rules {
            Ok(rules) => (
                State::Rules {
                    root,
                    rules: rules.clone(),
                },
                Ok(()),
            ),
            Err(error) if loaded => (
                State::Refused,
                Err(io::Error::new(error.kind(), error.to_string())),
            ),
            Err(_) => (State::Refused, Ok(())),
        };
        if let Some(top) = self.stack.last_mut() {
            top.1 = state;
        }
        result
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
        self.reach(entry, stopped)?;
        let (parent, state) = self
            .stack
            .last()
            .filter(|_| self.stack.len() == entry.depth())
            .ok_or_else(|| io::Error::other("live policy stack lost its parent"))?;
        debug_assert_eq!(
            Some(parent.as_path()),
            entry
                .path()
                .strip_prefix(&self.spelling)
                .ok()
                .and_then(|relative| self.real.join(relative).parent().map(Path::to_owned))
                .as_deref(),
        );
        let name = std::ffi::OsStr::from_bytes(entry.name());
        let kind = entry.kind()?;
        let directory = kind == FileKind::Directory;
        // Only a directory needs its real path: to match a nested root, and
        // to load its rules.
        let path = directory.then(|| parent.join(name));
        let (root, rules) = match state {
            State::Rules { root, rules } => (Some(root), Some(rules)),
            State::Bypass => (None, None),
            // `reach` resolved an entering directory; a refused one's
            // entries are skipped.
            State::Entering { .. } | State::Refused => return Ok(false),
        };
        if let Some(path) = path.as_ref().filter(|path| {
            !self.anchors.rg_rules() && rules.is_some() && self.anchors.roots.contains(path)
        }) {
            let path = path.clone();
            let state = self.anchors.root(&path, self.warnings.as_mut(), stopped)?;
            self.stack.push((path, state));
            return Ok(true);
        }
        let item = match kind {
            FileKind::Directory => PolicyEntry::Dir,
            // An uncovered walk accepts a file over the size cap as readily
            // as one under it (below), so its size would go unused; skip the
            // stat.
            FileKind::File if self.anchors.fallback => PolicyEntry::File { size: 0 },
            FileKind::File => PolicyEntry::File {
                size: entry.metadata()?.size(),
            },
            FileKind::Symlink => PolicyEntry::Symlink,
            _ => PolicyEntry::Other,
        };
        let ignore_rule = self.ignore_rule(rules, entry, directory, selection.map(|(_, cwd)| cwd));
        let override_rule = match selection {
            Some((selection, cwd)) => match select(selection, cwd, entry, directory, ignore_rule) {
                Some(rule) => rule,
                None => return Ok(false),
            },
            None => None,
        };
        let decision = match (override_rule, ignore_rule, rules) {
            (Some(true), ..) if directory => Decision::Descend,
            (Some(true), ..) => Decision::Index,
            (_, Some(false), _) if selection.is_some() => Decision::Skip,
            (.., Some(rules)) => rules.decide_child(name, item),
            (.., None) => decide_uncovered(item),
        };
        if let Some(path) = path {
            let Some(next) = descend(decision, root.zip(rules), name) else {
                return Ok(false);
            };
            self.stack.push((path, next));
            Ok(true)
        } else {
            // An uncovered operand stands in for an index, whose content
            // search reads files over the size cap live.
            Ok(matches!(decision, Decision::Index)
                || (!content && matches!(decision, Decision::Catalog(_)))
                || (self.anchors.fallback
                    && matches!(decision, Decision::Catalog(ferret_policy::Reason::TooLarge))))
        }
    }

    /// The ignore-file rule for `entry`: its directory's, else, under a
    /// selection from `cwd` inside a work tree, the global git excludes'.
    fn ignore_rule(
        &self,
        rules: Option<&DirRules>,
        entry: &Entry,
        directory: bool,
        cwd: Option<&Path>,
    ) -> Option<bool> {
        let name = std::ffi::OsStr::from_bytes(entry.name());
        let local_rule = rules.and_then(|rules| rules.ignore_match(name, directory));
        // Rg applies the global git excludes only inside a work tree.
        let in_work_tree = rules.is_some_and(DirRules::in_work_tree);
        local_rule.or_else(|| {
            let cwd = cwd.filter(|_| in_work_tree)?;
            let absolute = cwd.join(entry.path());
            self.anchors.git_global.decision(
                absolute
                    .strip_prefix(cwd)
                    .ok()
                    .map(|path| path.as_os_str().as_bytes()),
                entry.name(),
                directory,
            )
        })
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
    load_kind(path, parent, global, warnings, stopped, false, None)
}

fn load_rg(
    path: &Path,
    parent: Option<&DirRules>,
    global: Option<&str>,
    warnings: Option<&mut Vec<String>>,
    stopped: &dyn Fn() -> bool,
) -> io::Result<DirRules> {
    load_kind(path, parent, global, warnings, stopped, true, None)
}
fn load_kind(
    path: &Path,
    parent: Option<&DirRules>,
    global: Option<&str>,
    mut warnings: Option<&mut Vec<String>>,
    stopped: &dyn Fn() -> bool,
    grep: bool,
    listed: Option<ferret_crawl::Listed<'_>>,
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
        ferret_crawl::load_grep_policy(path, parent, global, listed, &mut visit)
    } else {
        ferret_crawl::load_policy(path, parent, global, Config::default(), listed, &mut visit)
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
mod tests {
    use ferret_catalog::Section;

    use super::anchors::OPENED;
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
            let mut source = sources::Directory::new(&root, None, &root, false, true).unwrap();
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
