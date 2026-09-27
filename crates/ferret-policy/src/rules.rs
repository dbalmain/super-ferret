//! Which ignore rules are in force for one directory, as one list, and the
//! decision for each of its entries.
//!
//! For an entry, the first layer with a matching pattern decides, in this
//! order (D13):
//!
//! 1. `.ferretignore` files, closest directory first;
//! 2. inside a git work tree only, `.gitignore` files closest first, then
//!    `.git/info/exclude`;
//! 3. the user's global ignore file, which setup seeds with
//!    [`DEFAULT_IGNORE`](crate::DEFAULT_IGNORE).
//!
//! Within one file the last matching line wins, as in gitignore. So the
//! files concatenated in reverse, lowest precedence first (global, exclude,
//! `.gitignore` root to here, `.ferretignore` root to here) and searched for
//! the last matching line give the same answer (D19). Each directory holds
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
use crate::lists::{List, Lists, Rule};
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
                Layer::compile(IgnoreFile::Global, text, false, &shared.lists, &mut errors).live()
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

    /// Decides the entry at root-relative `path`, which must be a direct child
    /// of this directory. The crawler can pass its existing candidate path so
    /// decisions do not allocate a joined path for each entry; only its last
    /// component is read.
    ///
    /// Special files are always skipped. Otherwise an entry is included when
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
            Entry::Other => Decision::Skip,
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
                .extend(Layer::compile(file, text, true, lists, errors).live());
        }
        if files.git_root {
            self.git = Some(WorkTree {
                exclude: files.git_exclude.and_then(|text| {
                    let file = IgnoreFile::GitExclude(dir.join(".git/info/exclude"));
                    Layer::compile(file, text, false, lists, errors).live()
                }),
                ignores: Vec::new(),
            });
        }
        if let (Some(tree), Some(text)) = (&mut self.git, files.gitignore) {
            let file = IgnoreFile::Git(dir.join(".gitignore"));
            tree.ignores
                .extend(Layer::compile(file, text, false, lists, errors).live());
        }
    }

    fn layers(&self) -> impl Iterator<Item = &Layer> {
        let git = self
            .git
            .iter()
            .flat_map(|tree| tree.exclude.iter().chain(&tree.ignores));
        self.global.iter().chain(git).chain(&self.ferret)
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
        ferret: bool,
        lists: &Lists,
        errors: &mut Vec<PatternError>,
    ) -> Self {
        let (layer, line_errors) = Self::compile_text(text, ferret, lists);
        errors.extend(line_errors.into_iter().map(|error| PatternError::Line {
            file: file.clone(),
            line: error.line,
            pattern: error.pattern,
            detail: error.detail,
        }));
        layer
    }

    fn compile_text(text: &str, ferret: bool, lists: &Lists) -> (Self, Vec<LineError>) {
        let (patterns, errors) = parse(text);
        let mut shared = Vec::new();
        let mut anchored = Vec::new();
        for (at, pattern) in patterns.iter().enumerate() {
            if pattern.is_shared_basename() {
                // One that cannot match anything (an escaped edge) projects
                // to nothing.
                if let Some(Some(projected)) = pattern.projections().into_iter().next() {
                    shared.push((pattern.line(), lists.rule(projected, ferret)));
                }
                continue;
            }
            let reaches = ferret
                && pattern.is_anchored_reinclude()
                && !patterns[at + 1..]
                    .iter()
                    .any(|later| pattern.is_superseded_by(later));
            let projections = pattern
                .projections()
                .into_iter()
                .map(|projected| Some(lists.rule(projected?, ferret)))
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
        let (layer, errors) = Layer::compile_text(text, false, &lists);
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
mod tests {
    use super::*;

    fn root(global: Option<&str>, files: IgnoreFiles<'_>) -> (DirRules, Vec<PatternError>) {
        DirRules::root(Path::new("/r"), global, files, Config::default())
    }

    #[test]
    fn default_ignore_compiles_cleanly() {
        let (_, errors) = root(Some(crate::DEFAULT_IGNORE), IgnoreFiles::default());
        assert_eq!(errors, []);
    }

    #[test]
    fn a_bad_line_is_reported_and_the_rest_of_its_file_applies() {
        let files = IgnoreFiles {
            ferretignore: Some("*.log\nbad\\\n"),
            ..IgnoreFiles::default()
        };
        let (rules, errors) = root(None, files);
        assert!(
            matches!(
                errors.as_slice(),
                [PatternError::Line { file: IgnoreFile::Ferret(path), line: 2, pattern, .. }]
                    if path == Path::new("/r/.ferretignore") && pattern == "bad\\"
            ),
            "{errors:?}"
        );
        let log = rules.decide(Path::new("x.log"), Entry::File { size: 1 });
        assert_eq!(log, Decision::Skip);
    }

    #[test]
    fn global_file_errors_name_the_global_file() {
        let (_, errors) = root(Some("bad\\"), IgnoreFiles::default());
        assert!(
            matches!(
                errors.as_slice(),
                [PatternError::Line {
                    file: IgnoreFile::Global,
                    ..
                }]
            ),
            "{errors:?}"
        );
    }

    #[test]
    fn without_a_global_file_default_names_are_ordinary() {
        let (rules, _) = root(None, IgnoreFiles::default());
        assert_eq!(
            rules.decide(Path::new("node_modules"), Entry::Dir),
            Decision::Descend
        );
    }

    #[test]
    fn normalized_reincludes_drive_traversal_and_decisions() {
        for content in ["\u{feff}!/target/doc/**\n", "!/target/doc\0ignored\n"] {
            let files = IgnoreFiles {
                ferretignore: Some(content),
                ..IgnoreFiles::default()
            };
            let (rules, errors) = root(Some("target/\n"), files);
            assert!(errors.is_empty());
            assert_eq!(
                rules.decide(Path::new("target"), Entry::Dir),
                Decision::Traverse
            );
            let traversed = rules.traverse(OsStr::new("target"));
            let decision = traversed.decide(Path::new("target/doc"), Entry::Dir);
            if decision == Decision::Traverse {
                let doc = traversed.traverse(OsStr::new("doc"));
                assert_eq!(
                    doc.decide(Path::new("target/doc/README.md"), Entry::File { size: 0 }),
                    Decision::Index,
                    "{content:?}"
                );
            } else {
                assert_eq!(decision, Decision::Descend, "{content:?}");
            }
        }
    }

    #[test]
    fn only_fully_superseded_reincludes_are_pruned() {
        // This pins traversal after a negation is canceled, including the
        // directory-only scope and last-match cases.
        let cases = [
            ("!/target/**\ntarget/**\n", Decision::Skip),
            (
                "!/target/**\ntarget/**\n!/target/doc/**\n",
                Decision::Traverse,
            ),
            ("!/target/**\ntarget/**/\n", Decision::Traverse),
            ("!/target/**/\ntarget/**\n", Decision::Skip),
            ("!/target/**\ntarget/**\n!/target/**\n", Decision::Traverse),
        ];
        for (ferretignore, expected) in cases {
            let files = IgnoreFiles {
                ferretignore: Some(ferretignore),
                ..IgnoreFiles::default()
            };
            let (rules, errors) = root(Some("target/\n"), files);
            assert!(errors.is_empty(), "{ferretignore:?}: {errors:?}");
            assert_eq!(
                rules.decide(Path::new("target"), Entry::Dir),
                expected,
                "{ferretignore:?}"
            );
            if expected == Decision::Traverse {
                let traversed = rules.traverse(OsStr::new("target"));
                if ferretignore.ends_with("!/target/doc/**\n") {
                    assert_eq!(
                        traversed.decide(Path::new("target/doc"), Entry::Dir),
                        Decision::Traverse
                    );
                    let doc = traversed.traverse(OsStr::new("doc"));
                    assert_eq!(
                        doc.decide(Path::new("target/doc/README.md"), Entry::File { size: 0 }),
                        Decision::Index
                    );
                } else {
                    assert_eq!(
                        traversed.decide(Path::new("target/file"), Entry::File { size: 0 }),
                        Decision::Index
                    );
                }
            }
        }
    }

    #[test]
    fn in_work_tree_starts_at_git_root_and_is_inherited() {
        let (outside, _) = root(None, IgnoreFiles::default());
        assert!(!outside.in_work_tree());
        let (child, _) = outside.enter(OsStr::new("sub"), IgnoreFiles::default());
        assert!(!child.in_work_tree());

        let (inside, _) = root(
            None,
            IgnoreFiles {
                git_root: true,
                ..IgnoreFiles::default()
            },
        );
        assert!(inside.in_work_tree());
        let (deeper, _) = inside.enter(OsStr::new("sub"), IgnoreFiles::default());
        assert!(deeper.in_work_tree());
        assert!(inside.traverse(OsStr::new("sub")).in_work_tree());
    }

    const FILE: Entry = Entry::File { size: 1 };
    const DIR: Entry = Entry::Dir;

    fn git(text: &str) -> IgnoreFiles<'_> {
        IgnoreFiles {
            gitignore: Some(text),
            git_root: true,
            ..IgnoreFiles::default()
        }
    }

    fn gitignore(text: &str) -> IgnoreFiles<'_> {
        IgnoreFiles {
            gitignore: Some(text),
            ..IgnoreFiles::default()
        }
    }

    /// A directory under test, entered by the walker's calls.
    struct Dir(DirRules);

    impl Dir {
        fn root(global: Option<&str>, files: IgnoreFiles<'_>) -> Self {
            let (rules, errors) = root(global, files);
            assert!(errors.is_empty(), "{errors:?}");
            Self(rules)
        }

        fn enter(&self, name: &str, files: IgnoreFiles<'_>) -> Self {
            assert_eq!(self.decide(name, DIR), Decision::Descend, "entering {name}");
            let (rules, errors) = self.0.enter(OsStr::new(name), files);
            assert!(errors.is_empty(), "{errors:?}");
            Self(rules)
        }

        fn empty(&self, name: &str) -> Self {
            self.enter(name, IgnoreFiles::default())
        }

        fn traverse(&self, name: &str) -> Self {
            assert_eq!(
                self.decide(name, DIR),
                Decision::Traverse,
                "traversing {name}"
            );
            Self(self.0.traverse(OsStr::new(name)))
        }

        fn decide(&self, name: &str, entry: Entry) -> Decision {
            self.0.decide(&self.0.path.join(name), entry)
        }

        fn shares_list_with(&self, other: &Self) -> bool {
            Arc::ptr_eq(&self.0.list, &other.0.list)
        }

        fn layer_count(&self) -> usize {
            self.0.state.layers().count()
        }
    }

    #[test]
    fn empty_and_spent_ignore_files_hold_no_layer() {
        let root = Dir::root(Some(""), git("*.log\n"));
        assert_eq!(root.layer_count(), 1);
        let mut dir = root.enter("a", gitignore(""));
        for depth in 0..50 {
            dir = dir.enter(&format!("d{depth}"), gitignore(""));
            assert_eq!(dir.layer_count(), 1, "depth {depth}");
            assert!(dir.shares_list_with(&root));
        }
        // Still in the work tree, whose rules still apply.
        assert!(dir.0.in_work_tree());
        assert_eq!(dir.decide("x.log", FILE), Decision::Skip);

        // An empty file at the top of a work tree adds no layer either, and
        // still makes the tree.
        let bare = Dir::root(None, git(""));
        assert_eq!(bare.layer_count(), 0);
        assert!(bare.0.in_work_tree());

        // An anchored-only file is dropped once no cursor survives.
        let anchored = root.enter("b", gitignore("/c/d\n"));
        assert_eq!(anchored.layer_count(), 2);
        assert_eq!(anchored.empty("c").layer_count(), 2);
        assert_eq!(anchored.empty("x").layer_count(), 1);
        assert_eq!(anchored.empty("c").decide("d", FILE), Decision::Skip);
    }

    #[test]
    fn a_nearer_gitignore_reincludes_what_a_farther_one_excluded() {
        let root = Dir::root(None, git("*.log\n"));
        let sub = root.enter("sub", gitignore("!keep.log\n"));
        assert_eq!(root.decide("keep.log", FILE), Decision::Skip);
        assert_eq!(sub.decide("keep.log", FILE), Decision::Index);
        assert_eq!(sub.decide("other.log", FILE), Decision::Skip);
        assert_eq!(
            sub.empty("deeper").decide("keep.log", FILE),
            Decision::Index
        );
    }

    #[test]
    fn a_rooted_pattern_matches_only_at_its_own_files_directory() {
        let root = Dir::root(None, git("/build/\n"));
        assert_eq!(root.decide("build", DIR), Decision::Skip);
        let sub = root.enter("sub", gitignore("/build/\n"));
        assert_eq!(root.empty("a").decide("build", DIR), Decision::Descend);
        assert_eq!(sub.decide("build", DIR), Decision::Skip);
        assert_eq!(sub.empty("x").decide("build", DIR), Decision::Descend);
    }

    #[test]
    fn a_globstar_under_a_directory_matches_at_every_depth_below_it() {
        let root = Dir::root(None, git("docs/**/*.tmp\n"));
        assert_eq!(root.decide("a.tmp", FILE), Decision::Index);
        assert_eq!(root.empty("other").decide("a.tmp", FILE), Decision::Index);
        let docs = root.empty("docs");
        assert_eq!(docs.decide("a.tmp", FILE), Decision::Skip);
        assert_eq!(docs.decide("a.txt", FILE), Decision::Index);
        let deep = docs.empty("x").empty("y");
        assert_eq!(deep.decide("a.tmp", FILE), Decision::Skip);
        assert_eq!(deep.decide("a.txt", FILE), Decision::Index);
    }

    #[test]
    fn a_directory_only_pattern_passes_a_file_of_the_same_name() {
        let root = Dir::root(None, git("cache/\n"));
        assert_eq!(root.decide("cache", DIR), Decision::Skip);
        assert_eq!(root.decide("cache", FILE), Decision::Index);
        let sub = root.empty("sub");
        assert_eq!(sub.decide("cache", DIR), Decision::Skip);
        assert_eq!(sub.decide("cache", FILE), Decision::Index);
    }

    #[test]
    fn a_ferret_rule_beats_a_git_rule_in_either_direction() {
        let files = IgnoreFiles {
            ferretignore: Some("!*.log\nkeep\n"),
            gitignore: Some("*.log\n!keep\n"),
            git_root: true,
            git_exclude: None,
        };
        let root = Dir::root(None, files);
        assert_eq!(root.decide("x.log", FILE), Decision::Index);
        assert_eq!(root.decide("keep", FILE), Decision::Skip);
        // A git file nearer than the ferret file still loses to it.
        let sub = root.enter("sub", gitignore("x.log\n"));
        assert_eq!(sub.decide("x.log", FILE), Decision::Index);
    }

    #[test]
    fn the_last_matching_line_wins_between_a_projection_and_a_shared_rule() {
        let root = Dir::root(None, git("docs/*.md\n!readme.md\n"));
        let docs = root.empty("docs");
        assert_eq!(docs.decide("readme.md", FILE), Decision::Index);
        assert_eq!(docs.decide("notes.md", FILE), Decision::Skip);
        let root = Dir::root(None, git("!readme.md\ndocs/*.md\n"));
        assert_eq!(root.empty("docs").decide("readme.md", FILE), Decision::Skip);
    }

    #[test]
    fn the_last_match_wins_over_an_earlier_more_specific_one() {
        // `keep.log` matches all three lines; a leftmost-first or
        // most-specific matcher would stop at `!keep.log`.
        let root = Dir::root(None, git("*.log\n!keep.log\nkeep*\n"));
        assert_eq!(root.decide("keep.log", FILE), Decision::Skip);
        assert_eq!(root.decide("x.log", FILE), Decision::Skip);
        assert_eq!(root.decide("keeper", FILE), Decision::Skip);
        let root = Dir::root(None, git("keep*\n*.log\n!keep.log\n"));
        assert_eq!(root.decide("keep.log", FILE), Decision::Index);
    }

    #[test]
    fn traversal_follows_a_ferret_whitelist_and_ignores_a_git_one() {
        let files = IgnoreFiles {
            ferretignore: Some("!/target/doc/**\n"),
            gitignore: Some("!*.txt\n"),
            git_root: true,
            git_exclude: None,
        };
        let root = Dir::root(Some("target/\n"), files);
        let target = root.traverse("target");
        assert_eq!(target.decide("notes.txt", FILE), Decision::Skip);
        let doc = target.traverse("doc");
        assert_eq!(doc.decide("index.html", FILE), Decision::Index);
        assert_eq!(doc.decide("sub", DIR), Decision::Descend);
    }

    #[test]
    fn a_new_work_tree_drops_the_enclosing_gitignores() {
        let root = Dir::root(Some("*.o\n"), git("*.log\n"));
        let inner = root.enter("inner", git("*.tmp\n"));
        assert_eq!(inner.decide("x.log", FILE), Decision::Index);
        assert_eq!(inner.decide("x.tmp", FILE), Decision::Skip);
        assert_eq!(inner.decide("x.o", FILE), Decision::Skip);
    }

    #[test]
    fn list_position_orders_rules_from_different_files() {
        // Line 1 of the nearer file must beat line 3 of the farther one. By
        // source line, which the index orders by unless renumbered, the
        // farther file's rule would win.
        let root = Dir::root(None, git("a\nb\n*.log\n"));
        let sub = root.enter("sub", gitignore("!keep.log\n"));
        assert_eq!(sub.decide("keep.log", FILE), Decision::Index);
        assert_eq!(sub.decide("other.log", FILE), Decision::Skip);
    }

    #[test]
    fn directory_only_and_whitelist_flags_hold_in_a_merged_list() {
        let root = Dir::root(Some("cache/\n"), git("*.log\n!keep.log\nkeep*\n"));
        assert_eq!(root.decide("cache", DIR), Decision::Skip);
        assert_eq!(root.decide("cache", FILE), Decision::Index);
        assert_eq!(root.decide("keep.log", FILE), Decision::Skip);
        assert_eq!(root.decide("x.log", FILE), Decision::Skip);
        assert_eq!(root.decide("keeper", FILE), Decision::Skip);
        assert_eq!(root.decide("other", FILE), Decision::Index);
    }

    #[test]
    fn directories_with_the_same_rules_share_one_list() {
        let root = Dir::root(Some("*.o\n"), git("/build/\ndocs/*.md\n*.log\n"));
        let a = root.empty("a");
        let b = root.empty("b");
        let c = a.empty("c");
        let docs = root.empty("docs");
        // The root has `/build/`, `docs` has `*.md`, and a, b and a/c share.
        assert!(a.shares_list_with(&b));
        assert!(a.shares_list_with(&c));
        assert!(!a.shares_list_with(&root));
        assert!(!a.shares_list_with(&docs));
        assert_eq!(root.0.shared.lists.list_count(), 3);
    }

    #[test]
    fn identical_text_from_different_files_shares_one_compiled_list() {
        // Two files, same text: one list. And `docs/*.md` projected into
        // `docs` is the same rule as a `*.md` line in `x/.gitignore`, so
        // those directories share too: identity is the text, not the file.
        let root = Dir::root(None, git("docs/*.md\n"));
        let a = root.enter("a", gitignore("*.tmp\n"));
        let b = root.enter("b", gitignore("*.tmp\n"));
        assert!(a.shares_list_with(&b));
        let docs = root.empty("docs");
        let x = root.enter("x", gitignore("*.md\n"));
        assert!(docs.shares_list_with(&x));
        assert_eq!(docs.decide("a.md", FILE), Decision::Skip);
        assert!(!a.shares_list_with(&root));
    }

    #[test]
    fn the_same_line_in_another_band_is_another_rule() {
        // `!*.md` whitelists an entry of a traversed directory from a
        // `.ferretignore` and not from a `.gitignore`, so the two lines
        // cannot share a list even though their text is equal.
        let files = |ferret: bool| IgnoreFiles {
            ferretignore: ferret.then_some("!/target/doc/**\n!*.md\n"),
            gitignore: (!ferret).then_some("!*.md\n"),
            git_root: true,
            git_exclude: None,
        };
        let (with_git, _) = root(Some("target/\n"), files(false));
        let (with_ferret, _) = root(Some("target/\n"), files(true));
        let git_target = with_git.traverse(OsStr::new("target"));
        assert_eq!(
            git_target.decide(Path::new("target/a.md"), FILE),
            Decision::Skip
        );
        let ferret_target = with_ferret.traverse(OsStr::new("target"));
        assert_eq!(
            ferret_target.decide(Path::new("target/a.md"), FILE),
            Decision::Index
        );
        // One table, both bands: a root with both files interns both rules.
        let both = IgnoreFiles {
            ferretignore: Some("!/target/doc/**\n"),
            gitignore: Some("!*.md\n"),
            git_root: true,
            git_exclude: None,
        };
        let root = Dir::root(Some("target/\n"), both);
        let git_x = root.enter("x", gitignore("!*.md\n"));
        let ferret_x = root.enter(
            "y",
            IgnoreFiles {
                ferretignore: Some("!*.md\n"),
                ..IgnoreFiles::default()
            },
        );
        assert!(!git_x.shares_list_with(&ferret_x));
    }

    #[test]
    fn lists_interned_from_many_threads_are_one_list_each() {
        // Sixteen workers, as the walker's default, each entering its own
        // directories, whose rules are textually equal across workers.
        let root = Dir::root(Some("*.o\n"), git("/build/\n*.log\n"));
        let lists: Vec<Vec<Arc<List>>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..16)
                .map(|worker| {
                    let root = &root;
                    scope.spawn(move || {
                        let mut seen = Vec::new();
                        for dir in 0..50 {
                            let name = format!("w{worker}-{dir}");
                            let own = root.enter(&name, gitignore("*.tmp\n!keep.tmp\n"));
                            let deeper = own.empty("sub");
                            seen.push(Arc::clone(&own.0.list));
                            seen.push(Arc::clone(&deeper.0.list));
                            seen.push(Arc::clone(&root.empty(&name).0.list));
                        }
                        seen
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect()
        });
        for seen in &lists {
            for (list, want) in seen.iter().zip(&lists[0]) {
                assert!(Arc::ptr_eq(list, want));
            }
        }
        // The root's, one with `*.tmp`, and the root's without `/build/`.
        assert_eq!(root.0.shared.lists.list_count(), 3);
    }

    #[test]
    fn a_cursor_reached_both_fed_and_unfed_is_fed() {
        // In `**/*/**\/b`, entering `y` below `x` reaches the second
        // globstar twice: from `*` (unfed) and from itself (fed). Only the
        // fed cursor may match zero more directories, so dropping `fed` when
        // merging the two loses `x/y/b`. git 2.54 ignores `x/y/b` and
        // `x/y/z/b`, and not `x/b`.
        let root = Dir::root(None, git("**/*/**\\/b\n"));
        let x = root.empty("x");
        assert_eq!(x.decide("b", FILE), Decision::Index);
        let y = x.empty("y");
        assert_eq!(y.decide("b", FILE), Decision::Skip);
        assert_eq!(y.empty("z").decide("b", FILE), Decision::Skip);
    }

    fn reaches(line: &str, dir: &str) -> bool {
        let lists = Lists::default();
        let (layer, errors) = Layer::compile_text(line, true, &lists);
        assert!(errors.is_empty(), "{errors:?}");
        let mut state = State {
            ferret: vec![layer],
            ..State::default()
        };
        let (parents, name) = dir.rsplit_once('/').unwrap_or(("", dir));
        for parent in parents.split('/').filter(|parent| !parent.is_empty()) {
            state = state.stepped(parent.as_bytes(), false).unwrap_or(state);
        }
        state.reaches_below(name.as_bytes())
    }

    #[test]
    fn only_anchored_bang_patterns_qualify() {
        let cases = [
            ("!/target/doc/**", true),
            ("!target/doc/**", true),
            ("!/target/", false),
            ("!/a/**/b", true),
            ("!/a/***/b", true),
            ("!/a\\/**/b", true),
            ("!*.pdf", false),
            ("!node_modules/", false),
            ("!**/foo/x", false),
            ("!***/foo/x", false),
            ("target/doc/**", false),
            ("\\!/target/doc", false),
        ];
        let lists = Lists::default();
        for (line, want) in cases {
            let (layer, _) = Layer::compile_text(line, true, &lists);
            let qualifies = layer
                .source
                .anchored
                .iter()
                .any(|anchored| anchored.reaches);
            assert_eq!(qualifies, want, "{line}");
        }
    }

    #[test]
    fn reaches_below_follows_the_pattern_components() {
        let cases = [
            ("!/target/doc/**", "target", true),
            ("!/target/doc/**", "target/doc", true),
            ("!/target/doc/**", "target/debug", false),
            ("!/target/doc/**", "other", false),
            ("!/target/doc/x.html", "target/doc", true),
            // The pattern names `target/doc/x.html` itself, not something below it.
            ("!/target/doc/x.html", "target/doc/x.html", false),
            ("!/target/*/x.html", "target/debug", true),
            ("!/target/*/x.html", "target/debug/deps", false),
            ("!/a/**/b", "a/x/y/z", true),
            ("!/a/***/b", "a/x/y/z", true),
            ("!/a\\/**/b", "a/x/y/z", true),
            ("!/a/**/b", "c/x", false),
        ];
        for (line, dir, want) in cases {
            assert_eq!(reaches(line, dir), want, "{line} {dir}");
        }
    }
}
