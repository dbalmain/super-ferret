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
//! directory holding its file; a layer above the walk root (D22) is stepped
//! through the path down to the root before the walk starts.
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

/// Rules in force for one directory: its ancestors' rules plus whatever
/// ignore files it holds.
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

/// A position in one anchored pattern; see [`Pattern::advance`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cursor {
    slot: usize,
    pos: usize,
    fed: bool,
}

/// One directory above the walk root, carrying that directory's git rules.
///
/// The crawler discovers these from the work tree's top down to the root's
/// parent. [`DirRules`] does not look at the file system.
#[derive(Clone, Copy, Debug)]
pub struct AncestorGit<'a> {
    /// Path from this directory to the walk root. `src` when this directory
    /// is the parent of root `repo/src`.
    pub above: &'a Path,
    /// Absolute path of this directory, used only to name pattern errors.
    pub directory: &'a Path,
    /// `.gitignore` in this directory, if it had one.
    pub gitignore: Option<&'a str>,
    /// `.git/info/exclude` of the work tree. Set only when [`top`](Self::top)
    /// is true.
    pub git_exclude: Option<&'a str>,
    /// This directory holds the work tree's `.git`.
    pub top: bool,
}

impl DirRules {
    /// Rules at a configured root: its own ignore files and the global ignore
    /// file's contents, if any. With no global file nothing is excluded by
    /// default.
    ///
    /// Returns every pattern that could not be used alongside the rules,
    /// which apply without them. `root` is only used to name files in those
    /// errors. This does not look above `root`. A root inside a work tree is
    /// built with [`root_within`](Self::root_within), which the crawler calls
    /// after discovering that tree (D22).
    pub fn root(
        root: &Path,
        global: Option<&str>,
        files: IgnoreFiles<'_>,
        config: Config,
    ) -> (Self, Vec<PatternError>) {
        Self::root_within(root, global, files, &[], config)
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
    /// True when this directory or an ancestor started a work tree
    /// (`git_root`), including a work tree discovered above the root
    /// ([`root_within`](Self::root_within)). The crawler uses it to skip
    /// reading `.gitignore` where those rules cannot apply.
    pub fn in_work_tree(&self) -> bool {
        self.state.git.is_some()
    }

    /// Rules at a root that sits inside a work tree whose `.git` is above it.
    ///
    /// `ancestors` runs from the work tree's top down to the root's parent.
    /// Each layer matches paths relative to its own directory: its cursors
    /// are stepped through `above`, the path from that directory to the
    /// root, before the walk starts. So for root `repo/src`, `repo`'s
    /// `/src/gen/` skips `gen` and its `/gen/` does not. Only the top carries
    /// `git_exclude`. No `.ferretignore` above the root is consulted; pass
    /// those only in `files`, which are the root's own.
    ///
    /// An empty `ancestors` is [`root`](Self::root). A `.git` in `files`
    /// (`git_root`) still starts a new work tree at the root and drops these
    /// ancestors, as a nested repository does.
    pub fn root_within(
        root: &Path,
        global: Option<&str>,
        files: IgnoreFiles<'_>,
        ancestors: &[AncestorGit<'_>],
        config: Config,
    ) -> (Self, Vec<PatternError>) {
        let mut errors = Vec::new();
        let shared = Arc::new(Shared {
            root: root.to_path_buf(),
            config,
            lists: Lists::default(),
        });
        let mut state = State {
            global: global.map(|text| {
                Layer::compile(IgnoreFile::Global, text, false, &shared.lists, &mut errors)
            }),
            ..State::default()
        };
        for ancestor in ancestors {
            state.add_ancestor(ancestor, &shared.lists, &mut errors);
        }
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
        let next = Self {
            global: self.global.as_ref().map(&mut step),
            git: self.git.as_ref().map(|tree| WorkTree {
                exclude: tree.exclude.as_ref().map(&mut step),
                ignores: tree.ignores.iter().map(&mut step).collect(),
            }),
            ferret: self.ferret.iter().map(&mut step).collect(),
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
                .push(Layer::compile(file, text, true, lists, errors));
        }
        if files.git_root {
            self.git = Some(WorkTree {
                exclude: files.git_exclude.map(|text| {
                    let file = IgnoreFile::GitExclude(dir.join(".git/info/exclude"));
                    Layer::compile(file, text, false, lists, errors)
                }),
                ignores: Vec::new(),
            });
        }
        if let (Some(tree), Some(text)) = (&mut self.git, files.gitignore) {
            let file = IgnoreFile::Git(dir.join(".gitignore"));
            tree.ignores
                .push(Layer::compile(file, text, false, lists, errors));
        }
    }

    /// Adds one ancestor's git rules, stepped down to the root. The top
    /// replaces any work tree already in force; a closer directory keeps it
    /// and adds its `.gitignore` nearer.
    fn add_ancestor(
        &mut self,
        ancestor: &AncestorGit<'_>,
        lists: &Lists,
        errors: &mut Vec<PatternError>,
    ) {
        let mut tree = if ancestor.top {
            WorkTree::default()
        } else {
            self.git.take().unwrap_or_default()
        };
        if ancestor.top
            && let Some(text) = ancestor.git_exclude
        {
            let file = IgnoreFile::GitExclude(ancestor.directory.join(".git/info/exclude"));
            let layer = Layer::compile(file, text, false, lists, errors);
            tree.exclude = Some(layer.stepped_through(ancestor.above));
        }
        if let Some(text) = ancestor.gitignore {
            let file = IgnoreFile::Git(ancestor.directory.join(".gitignore"));
            let layer = Layer::compile(file, text, false, lists, errors);
            tree.ignores.push(layer.stepped_through(ancestor.above));
        }
        self.git = Some(tree);
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
                anchored
                    .pattern
                    .advance(cursor.pos, cursor.fed, name, &mut step);
                !step.is_empty()
            })
        })
    }
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
                if let Some(projected) = pattern.project_basename(0, false) {
                    shared.push((pattern.line(), lists.rule(projected, ferret)));
                }
                continue;
            }
            let reaches = ferret
                && pattern.is_anchored_reinclude()
                && !patterns[at + 1..]
                    .iter()
                    .any(|later| pattern.is_superseded_by(later));
            let projections = (0..pattern.component_count())
                .flat_map(|pos| [(pos, false), (pos, true)])
                .map(|(pos, fed)| {
                    let projected = pattern.project_basename(pos, fed)?;
                    Some(lists.rule(projected, ferret))
                })
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

    /// This layer in child `name`, or `None` when its cursors do not change.
    fn stepped(&self, name: &[u8]) -> Option<Self> {
        if self.cursors.is_empty() {
            return None;
        }
        let mut out: Vec<Cursor> = Vec::with_capacity(self.cursors.len());
        let mut step = Vec::new();
        for cursor in self.cursors.iter() {
            let anchored = &self.source.anchored[cursor.slot];
            step.clear();
            anchored
                .pattern
                .advance(cursor.pos, cursor.fed, name, &mut step);
            for (pos, fed) in step.drain(..) {
                if let Some(existing) = out
                    .iter_mut()
                    .find(|item| item.slot == cursor.slot && item.pos == pos)
                {
                    existing.fed |= fed;
                } else {
                    out.push(Cursor {
                        slot: cursor.slot,
                        pos,
                        fed,
                    });
                }
            }
        }
        if *out == *self.cursors {
            return None;
        }
        Some(Self {
            source: Arc::clone(&self.source),
            cursors: out.into(),
        })
    }

    /// This layer stepped through each component of `path`.
    fn stepped_through(self, path: &Path) -> Self {
        path.iter().fold(self, |layer, name| {
            layer.stepped(name.as_bytes()).unwrap_or(layer)
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
        list.last_match(name, is_dir).map(|(whitelist, _)| whitelist)
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

    #[test]
    fn an_ancestor_layer_matches_relative_to_its_own_directory() {
        let directory = Path::new("/repo");
        let above = Path::new("src");
        let positive = [AncestorGit {
            above,
            directory,
            gitignore: Some("*.o\n/src/gen/\n"),
            git_exclude: None,
            top: true,
        }];
        let (rules, errors) = DirRules::root_within(
            Path::new("/repo/src"),
            None,
            IgnoreFiles::default(),
            &positive,
            Config::default(),
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert!(rules.in_work_tree());
        assert_eq!(
            rules.decide(Path::new("a.o"), Entry::File { size: 1 }),
            Decision::Skip
        );
        assert_eq!(rules.decide(Path::new("gen"), Entry::Dir), Decision::Skip);
        assert_eq!(
            rules.decide(Path::new("keep.c"), Entry::File { size: 1 }),
            Decision::Index
        );

        // `/gen/` anchored at `repo` does not name `repo/src/gen`. Anchoring
        // it at the walk root would skip `gen`, which is the bug.
        let anchored = [AncestorGit {
            above,
            directory,
            gitignore: Some("/gen/\n"),
            git_exclude: None,
            top: true,
        }];
        let (rules, errors) = DirRules::root_within(
            Path::new("/repo/src"),
            None,
            IgnoreFiles::default(),
            &anchored,
            Config::default(),
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            rules.decide(Path::new("gen"), Entry::Dir),
            Decision::Descend
        );
    }

    #[test]
    fn a_closer_gitignore_reincludes_over_an_ancestor() {
        let ancestors = [AncestorGit {
            above: Path::new("src"),
            directory: Path::new("/repo"),
            gitignore: Some("*.o\n"),
            git_exclude: None,
            top: true,
        }];
        let files = IgnoreFiles {
            gitignore: Some("!keep.o\n"),
            ..IgnoreFiles::default()
        };
        let (rules, errors) = DirRules::root_within(
            Path::new("/repo/src"),
            None,
            files,
            &ancestors,
            Config::default(),
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            rules.decide(Path::new("keep.o"), Entry::File { size: 1 }),
            Decision::Index
        );
        assert_eq!(
            rules.decide(Path::new("drop.o"), Entry::File { size: 1 }),
            Decision::Skip
        );
    }

    #[test]
    fn the_root_git_file_drops_ancestor_rules() {
        let ancestors = [AncestorGit {
            above: Path::new("src"),
            directory: Path::new("/repo"),
            gitignore: Some("*.o\n"),
            git_exclude: None,
            top: true,
        }];
        let files = IgnoreFiles {
            git_root: true,
            ..IgnoreFiles::default()
        };
        let (rules, _) = DirRules::root_within(
            Path::new("/repo/src"),
            None,
            files,
            &ancestors,
            Config::default(),
        );
        assert_eq!(
            rules.decide(Path::new("a.o"), Entry::File { size: 1 }),
            Decision::Index
        );
    }
}
