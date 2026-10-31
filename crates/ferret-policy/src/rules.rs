//! Which ignore rules are in force for one directory, as one list, and the
//! decision for each of its entries.
//!
//! For an entry, the first layer with a matching pattern decides, in this
//! order (D13):
//!
//! 1. `.ferretignore` files, closest directory first;
//! 2. `.rgignore` files, then `.ignore` files, each closest first and in or out
//!    of a work tree, as rg ranks them (D126);
//! 3. inside a git work tree only, `.gitignore` files closest first, then
//!    `.git/info/exclude`;
//! 4. the user's global ignore file, which setup seeds with
//!    [`DEFAULT_IGNORE`](crate::DEFAULT_IGNORE).
//!
//! Within one file the last matching line wins, as in gitignore. So the
//! files concatenated in reverse, lowest precedence first (global, exclude,
//! `.gitignore`, `.ignore`, `.rgignore`, `.ferretignore`, each root to here)
//! and searched for the last matching line give the same answer (D19). Each
//! directory holds
//! that concatenation as one list of *basename* rules: every rule that can
//! match one of its entries, rewritten to match the entry's name alone.
//!
//! A rule without a slash before its last character matches the same names
//! everywhere and goes into every list below its file. An anchored rule
//! (`/build/`, `docs/**/*.tmp`) is followed by cursors: positions in the
//! pattern, stepped one component per directory entered. A cursor on the
//! pattern's last component, or on a trailing `**`, puts that component into
//! the list as a basename rule. Each layer matches paths relative to the
//! directory holding its file; only files at or below the configured root
//! contribute rules (D22).
//!
//! Lists are interned by their rules' text (`lists`): a directory holds a
//! handle to its compiled list and the cursors to derive its children's.
//! Seams: `gitignore` parses patterns, steps cursors and indexes a list;
//! `lists` interns rules and lists.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::gitignore::{LineError, Pattern, parse};
use crate::lists::{Band, List, Lists, Rule};
use crate::{Config, Decision, Entry, IgnoreFile, IgnoreFiles, PatternError, Reason};

/// Rules in force for one directory: the configured root's rules and its
/// descendants' rules.
///
/// Cheap to clone (a few `Arc`s and the relative path); the crawler holds
/// one per directory on its stack. A directory whose rules are its parent's
/// shares its parent's state and list. Paths are relative to the configured
/// root and never touch the file system.
#[derive(Clone, Debug)]
pub struct DirRules {
    /// This directory, relative to the root; empty at the root.
    path: PathBuf,
    shared: Arc<Shared>,
    /// What derives the children's lists; read only by `enter` and
    /// `traverse`, and by `decide` for an excluded directory.
    state: Arc<State>,
    /// Every rule that can match an entry here, compiled.
    list: Arc<List>,
    /// Walking an excluded directory only for anchored re-includes: nothing is
    /// included unless a `.ferretignore` `!` pattern says so.
    traversing: bool,
}

/// Per root: shared by every directory below it.
#[derive(Debug)]
struct Shared {
    /// The configured root, absolute; used only to name files in errors.
    root: PathBuf,
    config: Config,
    lists: Lists,
}

/// The ignore files in force and their cursors, lowest precedence first.
#[derive(Clone, Debug, Default)]
struct State {
    global: Option<Layer>,
    /// `None` outside a git work tree.
    git: Option<WorkTree>,
    /// Root first.
    ferret: Vec<Layer>,
    ignore: Vec<Layer>,
    rgignore: Vec<Layer>,
}

#[derive(Clone, Debug, Default)]
struct WorkTree {
    exclude: Option<Layer>,
    /// Work tree top first.
    ignores: Vec<Layer>,
}

/// One ignore file and its live cursors in one directory.
#[derive(Clone, Debug)]
struct Layer {
    source: Arc<Source>,
    /// In slot order, so in source order.
    cursors: Arc<[Cursor]>,
}

/// One compiled ignore file.
#[derive(Debug)]
struct Source {
    /// Rules that match the same names in every directory, with their line.
    shared: Box<[(usize, Arc<Rule>)]>,
    /// Anchored at the file's directory, in source order.
    anchored: Box<[Anchored]>,
}

#[derive(Debug)]
struct Anchored {
    pattern: Pattern,
    /// The rule each cursor position projects to, at `2 * pos + fed`.
    projections: Box<[Option<Arc<Rule>>]>,
    /// A `.ferretignore` re-include that no later line supersedes: traversal
    /// of an excluded directory may follow it.
    reaches: bool,
}

/// A position in one anchored pattern; see [`Pattern::step`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cursor {
    slot: usize,
    pos: usize,
    fed: bool,
}

impl DirRules {
    /// Rules at a configured root: its own ignore files and the global ignore
    /// file's contents, if any. With no global file nothing is excluded by
    /// default.
    ///
    /// Returns every pattern that could not be used alongside the rules,
    /// which apply without them. `root` is only used to name files in those
    /// errors. This does not look above `root` (D22).
    pub fn root(
        root: &Path,
        global: Option<&str>,
        files: IgnoreFiles<'_>,
        config: Config,
    ) -> (Self, Vec<PatternError>) {
        let mut errors = Vec::new();
        let shared = Arc::new(Shared {
            root: root.to_path_buf(),
            config,
            lists: Lists::default(),
        });
        let mut state = State {
            global: global.and_then(|text| {
                Layer::compile(
                    IgnoreFile::Global,
                    text,
                    Band::Global,
                    &shared.lists,
                    &mut errors,
                )
                .live()
            }),
            ..State::default()
        };
        state.add_files(root, files, &shared.lists, &mut errors);
        let list = shared.list(&state, None);
        let rules = Self {
            path: PathBuf::new(),
            shared,
            state: Arc::new(state),
            list,
            traversing: false,
        };
        (rules, errors)
    }

    /// Rules for child directory `name`, which [`decide`](Self::decide)
    /// answered [`Decision::Descend`]. `files` are that directory's ignore
    /// files. Returns the patterns that could not be used, as for
    /// [`root`](Self::root).
    pub fn enter(&self, name: &OsStr, files: IgnoreFiles<'_>) -> (Self, Vec<PatternError>) {
        let mut errors = Vec::new();
        let path = self.path.join(name);
        let stepped = self.state.stepped(name.as_bytes(), files.git_root);
        let adds_files = files.ferretignore.is_some()
            || files.ignore.is_some()
            || files.rgignore.is_some()
            || files.git_root
            || (files.gitignore.is_some() && self.state.git.is_some());
        let (state, list) = if stepped.is_none() && !adds_files {
            (Arc::clone(&self.state), Arc::clone(&self.list))
        } else {
            let mut state = stepped.unwrap_or_else(|| State::clone(&self.state));
            let dir = self.shared.root.join(&path);
            state.add_files(&dir, files, &self.shared.lists, &mut errors);
            let list = self.shared.list(&state, Some(&self.list));
            (Arc::new(state), list)
        };
        let rules = Self {
            path,
            shared: Arc::clone(&self.shared),
            state,
            list,
            traversing: false,
        };
        (rules, errors)
    }

    /// Rules for child directory `name`, which [`decide`](Self::decide)
    /// answered [`Decision::Traverse`]. Takes no ignore files, because an
    /// excluded directory's ignore files are never read (D13).
    pub fn traverse(&self, name: &OsStr) -> Self {
        let (state, list) = match self.state.stepped(name.as_bytes(), false) {
            None => (Arc::clone(&self.state), Arc::clone(&self.list)),
            Some(state) => {
                let list = self.shared.list(&state, Some(&self.list));
                (Arc::new(state), list)
            }
        };
        Self {
            path: self.path.join(name),
            shared: Arc::clone(&self.shared),
            state,
            list,
            traversing: true,
        }
    }

    /// Whether `.gitignore` rules are in force here.
    ///
    /// True when this directory or a descendant of the configured root
    /// started a work tree (`git_root`). The crawler uses it to skip reading
    /// `.gitignore` where those rules cannot apply.
    pub fn in_work_tree(&self) -> bool {
        self.state.git.is_some()
    }

    /// Last matching ignore rule for this entry: true includes, false
    /// excludes, and None leaves the decision to lower-priority sources.
    pub fn ignore_match(&self, name: &OsStr, directory: bool) -> Option<bool> {
        self.list
            .last_match(name.as_bytes(), directory)
            .map(|(include, _)| include)
    }

    /// Whether rg would show this hidden entry for its ignore files' sake:
    /// the last rule matching it in a `.gitignore`, `.git/info/exclude`,
    /// `.ignore` or `.rgignore` is a `!` line. Rules from `.ferretignore`
    /// and the global ignore file are ferret's alone and do not count (D127).
    /// The crawler asks only for dot-named entries.
    pub fn reveals(&self, name: &OsStr, directory: bool) -> bool {
        self.list.repo_whitelists(name.as_bytes(), directory)
    }

    /// Decides the entry at root-relative `path`, which must be a direct child
    /// of this directory. The crawler can pass its existing candidate path so
    /// decisions do not allocate a joined path for each entry; only its last
    /// component is read.
    ///
    /// An entry is included when
    /// the last matching rule whitelists it or no rule matches; while
    /// traversing an excluded directory, only a `.ferretignore` whitelist
    /// includes. An included directory descends, a file indexes unless it is
    /// over the size cap, a symlink is catalogued. An excluded directory is
    /// traversed if an anchored `.ferretignore` `!` pattern reaches below it.
    pub fn decide(&self, path: &Path, entry: Entry) -> Decision {
        debug_assert_eq!(path.parent(), Some(self.path.as_path()), "{path:?}");
        let bytes = path.as_os_str().as_bytes();
        let name = bytes
            .iter()
            .rposition(|byte| *byte == b'/')
            .map_or(bytes, |slash| &bytes[slash + 1..]);
        self.decide_child(OsStr::from_bytes(name), entry)
    }

    /// [`DirRules::decide`] for the child `name` of this directory, for a
    /// walker that has no root-relative path at hand.
    pub fn decide_child(&self, name: &OsStr, entry: Entry) -> Decision {
        let name = name.as_bytes();
        let is_dir = entry == Entry::Dir;
        let included = match self.list.last_match(name, is_dir) {
            Some((true, ferret)) => !self.traversing || ferret,
            Some((false, _)) => false,
            None => !self.traversing,
        };
        if !included {
            return if is_dir && self.state.reaches_below(name) {
                Decision::Traverse
            } else {
                Decision::Skip
            };
        }
        match entry {
            Entry::Dir => Decision::Descend,
            Entry::File { size } if size > self.shared.config.size_cap => {
                Decision::Catalog(Reason::TooLarge)
            }
            Entry::File { .. } => Decision::Index,
            Entry::Symlink => Decision::Catalog(Reason::Symlink),
            Entry::Other => Decision::Catalog(Reason::Special),
        }
    }
}

impl Shared {
    /// The interned list for `state`. A child whose list has its parent's
    /// key reuses the parent's list without taking the table's lock.
    fn list(&self, state: &State, parent: Option<&Arc<List>>) -> Arc<List> {
        let mut rules = Vec::new();
        state.rules(&mut rules);
        let key: Vec<usize> = rules.iter().map(|rule| rule.id()).collect();
        match parent {
            Some(parent) if parent.key() == key.as_slice() => Arc::clone(parent),
            _ => self.lists.list(&key, &rules),
        }
    }
}

impl State {
    /// This state moved into child `name`, or `None` when that changes
    /// nothing. A child that starts a work tree drops the enclosing one's
    /// `.gitignore` files and exclude.
    fn stepped(&self, name: &[u8], git_root: bool) -> Option<Self> {
        if git_root && self.git.is_some() {
            let mut next = self.stepped(name, false).unwrap_or_else(|| self.clone());
            next.git = None;
            return Some(next);
        }
        let mut changed = false;
        let mut step = |layer: &Layer| match layer.stepped(name) {
            Some(next) => {
                changed = true;
                next
            }
            None => layer.clone(),
        };
        // A layer whose last cursor died here, with no shared rules, is
        // dropped; the work tree it belonged to stays in force.
        let next = Self {
            global: self.global.as_ref().map(&mut step).and_then(Layer::live),
            git: self.git.as_ref().map(|tree| WorkTree {
                exclude: tree.exclude.as_ref().map(&mut step).and_then(Layer::live),
                ignores: tree
                    .ignores
                    .iter()
                    .map(&mut step)
                    .filter_map(Layer::live)
                    .collect(),
            }),
            ignore: self
                .ignore
                .iter()
                .map(&mut step)
                .filter_map(Layer::live)
                .collect(),
            rgignore: self
                .rgignore
                .iter()
                .map(&mut step)
                .filter_map(Layer::live)
                .collect(),
            ferret: self
                .ferret
                .iter()
                .map(&mut step)
                .filter_map(Layer::live)
                .collect(),
        };
        changed.then_some(next)
    }

    /// Adds the ignore files of directory `dir` (absolute, for errors).
    fn add_files(
        &mut self,
        dir: &Path,
        files: IgnoreFiles<'_>,
        lists: &Lists,
        errors: &mut Vec<PatternError>,
    ) {
        if let Some(text) = files.ferretignore {
            let file = IgnoreFile::Ferret(dir.join(".ferretignore"));
            self.ferret
                .extend(Layer::compile(file, text, Band::Ferret, lists, errors).live());
        }
        if files.git_root {
            self.git = Some(WorkTree {
                exclude: files.git_exclude.and_then(|text| {
                    let file = IgnoreFile::GitExclude(dir.join(".git/info/exclude"));
                    Layer::compile(file, text, Band::Repo, lists, errors).live()
                }),
                ignores: Vec::new(),
            });
        }
        if let (Some(tree), Some(text)) = (&mut self.git, files.gitignore) {
            let file = IgnoreFile::Git(dir.join(".gitignore"));
            tree.ignores
                .extend(Layer::compile(file, text, Band::Repo, lists, errors).live());
        }
        for (name, text, layers) in [
            (".ignore", files.ignore, &mut self.ignore),
            (".rgignore", files.rgignore, &mut self.rgignore),
        ] {
            if let Some(text) = text {
                let file = IgnoreFile::Dot(dir.join(name));
                layers.extend(Layer::compile(file, text, Band::Repo, lists, errors).live());
            }
        }
    }

    fn layers(&self) -> impl Iterator<Item = &Layer> {
        let git = self
            .git
            .iter()
            .flat_map(|tree| tree.exclude.iter().chain(&tree.ignores));
        self.global
            .iter()
            .chain(git)
            .chain(&self.ignore)
            .chain(&self.rgignore)
            .chain(&self.ferret)
    }

    /// Every layer's rules for this directory, lowest precedence first.
    fn rules<'a>(&'a self, out: &mut Vec<&'a Rule>) {
        for layer in self.layers() {
            layer.rules(out);
        }
    }

    /// Whether a `.ferretignore` re-include can match something below the
    /// excluded directory `name`: one of its cursors steps into `name`.
    fn reaches_below(&self, name: &[u8]) -> bool {
        let mut step = Vec::new();
        self.ferret.iter().any(|layer| {
            layer.cursors.iter().any(|cursor| {
                let anchored = &layer.source.anchored[cursor.slot];
                if !anchored.reaches {
                    return false;
                }
                step.clear();
                let work = anchored
                    .pattern
                    .step([(cursor.pos, cursor.fed)], name, &mut step);
                count_step_work(work);
                !step.is_empty()
            })
        })
    }
}

/// Adds one step's work to the test counter; nothing outside tests.
#[cfg(not(test))]
fn count_step_work(_work: usize) {}

#[cfg(test)]
thread_local! {
    /// Component visits by [`Pattern::step`] on this thread, for tests that
    /// bound the work of a walk rather than its wall-clock time.
    static STEP_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn count_step_work(work: usize) {
    STEP_WORK.with(|total| total.set(total.get() + work));
}

/// Takes the step work counted on this thread since the last take.
#[cfg(test)]
pub(crate) fn take_step_work() -> usize {
    STEP_WORK.with(|total| total.replace(0))
}

impl Layer {
    /// Compiles one ignore file, reporting lines that are not valid globs,
    /// as git does, and applying the rest.
    fn compile(
        file: IgnoreFile,
        text: &str,
        band: Band,
        lists: &Lists,
        errors: &mut Vec<PatternError>,
    ) -> Self {
        let (layer, line_errors) = Self::compile_text(text, band, lists);
        errors.extend(line_errors.into_iter().map(|error| PatternError::Line {
            file: file.clone(),
            line: error.line,
            pattern: error.pattern,
            detail: error.detail,
        }));
        layer
    }

    fn compile_text(text: &str, band: Band, lists: &Lists) -> (Self, Vec<LineError>) {
        let (patterns, errors) = parse(text);
        let mut shared = Vec::new();
        let mut anchored = Vec::new();
        for (at, pattern) in patterns.iter().enumerate() {
            if pattern.is_shared_basename() {
                // One that cannot match anything (an escaped edge) projects
                // to nothing.
                if let Some(Some(projected)) = pattern.projections().into_iter().next() {
                    shared.push((pattern.line(), lists.rule(projected, band)));
                }
                continue;
            }
            let reaches = band == Band::Ferret
                && pattern.is_anchored_reinclude()
                && !patterns[at + 1..]
                    .iter()
                    .any(|later| pattern.is_superseded_by(later));
            let projections = pattern
                .projections()
                .into_iter()
                .map(|projected| Some(lists.rule(projected?, band)))
                .collect();
            anchored.push(Anchored {
                pattern: pattern.clone(),
                projections,
                reaches,
            });
        }
        let cursors = (0..anchored.len())
            .map(|slot| Cursor {
                slot,
                pos: 0,
                fed: false,
            })
            .collect();
        let source = Source {
            shared: shared.into_boxed_slice(),
            anchored: anchored.into_boxed_slice(),
        };
        let layer = Self {
            source: Arc::new(source),
            cursors,
        };
        (layer, errors)
    }

    /// This layer, unless it can no longer contribute anything: no rule
    /// shared by every directory and no live cursor. An empty ignore file is
    /// such a layer from the start. Keeping one would cost a slot in every
    /// descendant's state, so a chain of d empty `.gitignore` files would
    /// hold Θ(d²) slots between its directories.
    fn live(self) -> Option<Self> {
        (!self.source.shared.is_empty() || !self.cursors.is_empty()).then_some(self)
    }

    /// This layer in child `name`, or `None` when its cursors do not change.
    fn stepped(&self, name: &[u8]) -> Option<Self> {
        if self.cursors.is_empty() {
            return None;
        }
        let mut out: Vec<Cursor> = Vec::with_capacity(self.cursors.len());
        let mut step = Vec::new();
        // Cursors are kept grouped by slot, each group in position order, so
        // one pattern's cursors step together and come back canonical.
        for group in self.cursors.chunk_by(|a, b| a.slot == b.slot) {
            let slot = group[0].slot;
            step.clear();
            let work = self.source.anchored[slot].pattern.step(
                group.iter().map(|cursor| (cursor.pos, cursor.fed)),
                name,
                &mut step,
            );
            count_step_work(work);
            out.extend(step.iter().map(|&(pos, fed)| Cursor { slot, pos, fed }));
        }
        if *out == *self.cursors {
            return None;
        }
        Some(Self {
            source: Arc::clone(&self.source),
            cursors: out.into(),
        })
    }

    /// This file's rules for this directory in source order: the shared
    /// rules merged by line with the live cursors' projections. Cursors are
    /// in slot order, and slots in source order, so both inputs are sorted.
    fn rules<'a>(&'a self, out: &mut Vec<&'a Rule>) {
        let source = &*self.source;
        let mut shared = source.shared.iter().peekable();
        for cursor in self.cursors.iter() {
            let anchored = &source.anchored[cursor.slot];
            let line = anchored.pattern.line();
            while let Some((_, rule)) = shared.next_if(|(at, _)| *at < line) {
                out.push(rule);
            }
            if let Some(rule) = &anchored.projections[2 * cursor.pos + usize::from(cursor.fed)] {
                out.push(rule);
            }
        }
        out.extend(shared.map(|(_, rule)| &**rule));
    }
}

/// One ignore file at a root, asked about one path at a time through the
/// same compile, cursor steps and list index as a walk. The gitignore tests
/// compare it with git; there is no other matcher to compare.
#[cfg(test)]
pub(crate) struct OneFile {
    lists: Lists,
    state: State,
}

#[cfg(test)]
impl OneFile {
    pub(crate) fn compile(text: &str) -> (Self, Vec<LineError>) {
        let lists = Lists::default();
        let (layer, errors) = Layer::compile_text(text, Band::Repo, &lists);
        let state = State {
            global: Some(layer),
            ..State::default()
        };
        (Self { lists, state }, errors)
    }

    /// Whether the last line matching root-relative `path` whitelists it;
    /// `None` when no line does. As in a walk, the path's parents are only
    /// stepped through, never matched.
    pub(crate) fn matched(&self, path: &[u8], is_dir: bool) -> Option<bool> {
        let (parents, name) = match path.iter().rposition(|byte| *byte == b'/') {
            Some(slash) => (&path[..slash], &path[slash + 1..]),
            None => (&path[..0], path),
        };
        let mut state = self.state.clone();
        for parent in parents.split(|byte| *byte == b'/') {
            if !parent.is_empty() {
                state = state.stepped(parent, false).unwrap_or(state);
            }
        }
        let mut rules = Vec::new();
        state.rules(&mut rules);
        let key: Vec<usize> = rules.iter().map(|rule| rule.id()).collect();
        let list = self.lists.list(&key, &rules);
        list.last_match(name, is_dir)
            .map(|(whitelist, _)| whitelist)
    }
}

#[cfg(test)]
mod tests;
