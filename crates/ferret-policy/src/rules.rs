//! The precedence chain: which ignore rules are in force for one directory,
//! and the decision for each of its entries.
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
//! Within one file the last matching line wins, as in gitignore. Each layer
//! matches paths relative to the directory holding its file. A layer above
//! the walk root (D22) prepends the path from that directory down to the root
//! before matching, so the walk's paths stay root-relative.
//!
//! Seams: `gitignore` compiles and matches each file's patterns; `reinclude`
//! decides whether an excluded directory must be traversed.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::gitignore::{Gitignore, Match};
use crate::reinclude::Reinclude;
use crate::{Config, Decision, Entry, IgnoreFile, IgnoreFiles, PatternError, Reason};

/// Rules in force for one directory: its ancestors' rules plus whatever
/// ignore files it holds.
///
/// Cheap to clone (a handful of `Arc`s and the relative path); the crawler
/// holds one per directory on its stack. Paths are relative to the configured
/// root and never touch the file system.
#[derive(Clone, Debug)]
pub struct DirRules {
    /// This directory, relative to the root; empty at the root.
    path: PathBuf,
    /// The configured root, absolute; used only to name files in errors.
    root: Arc<Path>,
    ferret: Chain,
    /// `None` outside a git work tree.
    git: Option<WorkTree>,
    shared: Arc<Shared>,
    /// Walking an excluded directory only for anchored re-includes: nothing is
    /// included unless a `.ferretignore` `!` pattern says so.
    traversing: bool,
}

/// Linked list of ignore files, closest directory first. Shared by every
/// directory beneath the one that added the head.
type Chain = Option<Arc<Layer>>;

#[derive(Clone, Debug)]
struct WorkTree {
    ignores: Chain,
    exclude: Option<Arc<Layer>>,
}

#[derive(Debug)]
struct Shared {
    global: Option<Layer>,
    config: Config,
}

/// One compiled ignore file.
#[derive(Debug)]
struct Layer {
    /// Directory holding the file, relative to the root. Empty when the
    /// file is at the root, and unused when [`above`](Self::above) is set.
    base: PathBuf,
    /// Path from this file's directory down to the walk root, when the file
    /// sits above the root (D22). Empty for a file at or below the root. A
    /// root-relative path is matched as `above/path`, so the pattern stays
    /// relative to the directory that holds it.
    above: PathBuf,
    matcher: Gitignore,
    /// Anchored `!` patterns; only ever non-empty for a `.ferretignore`.
    reincludes: Vec<Reinclude>,
    parent: Chain,
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

/// Which precedence band a match came from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Band {
    Ferret,
    Other,
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
        let rules = self.with_files(self.path.join(name), files, &mut errors);
        (rules, errors)
    }

    /// Rules for child directory `name`, which [`decide`](Self::decide)
    /// answered [`Decision::Traverse`]. Takes no ignore files, because an
    /// excluded directory's ignore files are never read (D13).
    pub fn traverse(&self, name: &OsStr) -> Self {
        Self {
            path: self.path.join(name),
            traversing: true,
            ..self.clone()
        }
    }

    /// Whether `.gitignore` rules are in force here.
    ///
    /// True when this directory or an ancestor started a work tree
    /// (`git_root`), including a work tree discovered above the root
    /// ([`root_within`](Self::root_within)). The crawler uses it to skip
    /// reading `.gitignore` where those rules cannot apply.
    pub fn in_work_tree(&self) -> bool {
        self.git.is_some()
    }

    /// Rules at a root that sits inside a work tree whose `.git` is above it.
    ///
    /// `ancestors` runs from the work tree's top down to the root's parent.
    /// Each layer matches paths relative to its own directory: `above` is the
    /// path from that directory to the root, prepended to the root-relative
    /// path before matching. So for root `repo/src`, `repo`'s `/src/gen/`
    /// skips `gen` and its `/gen/` does not. Only the top carries
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
        let global = global.map(|text| {
            Layer::compile(
                PathBuf::new(),
                IgnoreFile::Global,
                text,
                None,
                &mut errors,
                false,
            )
        });
        let mut rules = Self {
            path: PathBuf::new(),
            root: Arc::from(root),
            ferret: None,
            git: None,
            shared: Arc::new(Shared { global, config }),
            traversing: false,
        };
        for ancestor in ancestors {
            rules = rules.with_ancestor(ancestor, &mut errors);
        }
        let rules = rules.with_files(PathBuf::new(), files, &mut errors);
        (rules, errors)
    }

    /// Adds one ancestor's git rules. The top replaces any work tree already
    /// in force; a closer directory keeps it and prepends its `.gitignore`.
    fn with_ancestor(mut self, ancestor: &AncestorGit<'_>, errors: &mut Vec<PatternError>) -> Self {
        let mut tree = if ancestor.top {
            WorkTree {
                ignores: None,
                exclude: None,
            }
        } else if let Some(tree) = self.git.take() {
            tree
        } else {
            WorkTree {
                ignores: None,
                exclude: None,
            }
        };
        if ancestor.top
            && let Some(text) = ancestor.git_exclude
        {
            let file = IgnoreFile::GitExclude(ancestor.directory.join(".git/info/exclude"));
            tree.exclude = Some(Arc::new(
                Layer::compile(PathBuf::new(), file, text, None, errors, false)
                    .above(ancestor.above.to_path_buf()),
            ));
        }
        if let Some(text) = ancestor.gitignore {
            let file = IgnoreFile::Git(ancestor.directory.join(".gitignore"));
            let layer = Layer::compile(
                PathBuf::new(),
                file,
                text,
                tree.ignores.take(),
                errors,
                false,
            )
            .above(ancestor.above.to_path_buf());
            tree.ignores = Some(Arc::new(layer));
        }
        self.git = Some(tree);
        self
    }

    /// Decides the entry at root-relative `path`, which must be a direct child
    /// of this directory. The crawler can pass its existing candidate path so
    /// decisions do not allocate a joined path for each entry.
    ///
    /// Special files are always skipped. Otherwise an entry is included when
    /// the first matching layer whitelists it or no layer matches; while
    /// traversing an excluded directory, only a `.ferretignore` whitelist
    /// includes. An included directory descends, a file indexes unless it is
    /// over the size cap, a symlink is catalogued. An excluded directory is
    /// traversed if an anchored `.ferretignore` `!` pattern reaches below it.
    pub fn decide(&self, path: &Path, entry: Entry) -> Decision {
        debug_assert_eq!(path.parent(), Some(self.path.as_path()), "{path:?}");
        let is_dir = entry == Entry::Dir;
        let included = match self.first_match(path, is_dir) {
            Some((band, true)) => !self.traversing || band == Band::Ferret,
            Some((_, false)) => false,
            None => !self.traversing,
        };
        if !included {
            return if is_dir && self.reinclude_below(path) {
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

    /// The first layer that matches `path`, and whether it whitelisted it.
    fn first_match(&self, path: &Path, is_dir: bool) -> Option<(Band, bool)> {
        let ferret = layers(&self.ferret).map(|layer| (Band::Ferret, layer));
        let git = self.git.iter().flat_map(|tree| {
            layers(&tree.ignores)
                .chain(tree.exclude.as_deref())
                .map(|layer| (Band::Other, layer))
        });
        let base = self.shared.global.iter().map(|layer| (Band::Other, layer));
        ferret
            .chain(git)
            .chain(base)
            .find_map(|(band, layer)| Some((band, layer.matched(path, is_dir)?)))
    }

    fn reinclude_below(&self, path: &Path) -> bool {
        layers(&self.ferret).any(|layer| {
            below(path, &layer.base)
                .is_some_and(|rel| layer.reincludes.iter().any(|r| r.reaches_below(rel)))
        })
    }

    /// These rules moved to directory `path`, plus the ignore files it holds.
    fn with_files(
        &self,
        path: PathBuf,
        files: IgnoreFiles<'_>,
        errors: &mut Vec<PatternError>,
    ) -> Self {
        let dir = self.root.join(&path);
        let ferret = match files.ferretignore {
            Some(text) => {
                let file = IgnoreFile::Ferret(dir.join(".ferretignore"));
                Some(Arc::new(Layer::compile(
                    path.clone(),
                    file,
                    text,
                    self.ferret.clone(),
                    errors,
                    true,
                )))
            }
            None => self.ferret.clone(),
        };
        let enclosing = if files.git_root {
            Some(WorkTree {
                ignores: None,
                exclude: files.git_exclude.map(|text| {
                    let file = IgnoreFile::GitExclude(dir.join(".git/info/exclude"));
                    Arc::new(Layer::compile(
                        path.clone(),
                        file,
                        text,
                        None,
                        errors,
                        false,
                    ))
                }),
            })
        } else {
            self.git.clone()
        };
        let git = enclosing.map(|mut tree| {
            if let Some(text) = files.gitignore {
                let file = IgnoreFile::Git(dir.join(".gitignore"));
                let layer =
                    Layer::compile(path.clone(), file, text, tree.ignores.take(), errors, false);
                tree.ignores = Some(Arc::new(layer));
            }
            tree
        });
        Self {
            path,
            root: Arc::clone(&self.root),
            ferret,
            git,
            shared: Arc::clone(&self.shared),
            traversing: false,
        }
    }
}

impl Layer {
    /// Compiles one ignore file, dropping and reporting lines that are not
    /// valid globs, as git does.
    fn compile(
        base: PathBuf,
        file: IgnoreFile,
        text: &str,
        parent: Chain,
        errors: &mut Vec<PatternError>,
        collect_reincludes: bool,
    ) -> Self {
        let (matcher, line_errors, patterns) = if collect_reincludes {
            Gitignore::compile_patterns(text)
        } else {
            let (matcher, errors) = Gitignore::compile(text);
            (matcher, errors, Vec::new())
        };
        errors.extend(line_errors.into_iter().map(|error| PatternError::Line {
            file: file.clone(),
            line: error.line,
            pattern: error.pattern,
            detail: error.detail,
        }));
        Self {
            base,
            above: PathBuf::new(),
            matcher,
            reincludes: if collect_reincludes {
                patterns
                    .iter()
                    .enumerate()
                    .filter_map(|(index, pattern)| {
                        if !pattern.is_anchored_reinclude()
                            || patterns[index + 1..]
                                .iter()
                                .any(|later| pattern.is_superseded_by(later))
                        {
                            return None;
                        }
                        Reinclude::from_pattern(pattern.clone())
                    })
                    .collect()
            } else {
                Vec::new()
            },
            parent,
        }
    }

    /// This layer sits above the walk root. `above` is the path from the
    /// file's directory down to that root.
    fn above(mut self, above: PathBuf) -> Self {
        self.above = above;
        self
    }

    /// `Some(whitelisted)` if a pattern in this file matches `path` (relative
    /// to the root), `None` if none does.
    fn matched(&self, path: &Path, is_dir: bool) -> Option<bool> {
        if self.above.as_os_str().is_empty() {
            let rel = below(path, &self.base)?;
            self.outcome(rel, is_dir)
        } else {
            // `path` is already below this directory. Joining is the
            // layer-relative path the matcher expects; ancestor layers are
            // few, and a root that is not inside a work tree never takes
            // this branch.
            let rel = self.above.join(path);
            self.outcome(&rel, is_dir)
        }
    }

    fn outcome(&self, rel: &Path, is_dir: bool) -> Option<bool> {
        match self.matcher.matched(rel, is_dir) {
            Match::None => None,
            Match::Ignore => Some(false),
            Match::Whitelist => Some(true),
        }
    }
}

/// `path` relative to `base`, both root-relative and built by joining names,
/// or `None` when `path` is not below `base`. A byte comparison: std's
/// `strip_prefix` parses both paths into components on every call, which was
/// a fifth of a walk's user time.
fn below<'a>(path: &'a Path, base: &Path) -> Option<&'a Path> {
    let base = base.as_os_str().as_bytes();
    if base.is_empty() {
        return Some(path);
    }
    let rest = path.as_os_str().as_bytes().strip_prefix(base)?;
    let rest = rest.strip_prefix(b"/")?;
    Some(Path::new(OsStr::from_bytes(rest)))
}

/// The layers of a chain, closest directory first.
fn layers(chain: &Chain) -> impl Iterator<Item = &Layer> {
    std::iter::successors(chain.as_deref(), |layer| layer.parent.as_deref())
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
