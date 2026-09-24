//! The precedence chain: which ignore rules are in force for one directory,
//! and the decision for each of its entries.
//!
//! For an entry, the first layer with a matching pattern decides, in this
//! order (D13):
//!
//! 1. `.ferretignore` files, closest directory first;
//! 2. inside a git work tree only, `.gitignore` files closest first, then
//!    `.git/info/exclude`;
//! 3. the user's global ignore file;
//! 4. the built-in defaults.
//!
//! Within one file the last matching line wins, as in gitignore. Each layer
//! matches paths relative to the directory holding its file.
//!
//! Seams: `gitignore` compiles and matches each file's patterns; `reinclude`
//! decides whether an excluded directory must be traversed.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::gitignore::{Gitignore, Match};
use crate::reinclude::Reinclude;
use crate::{Config, Decision, Entry, IgnoreFile, IgnoreFiles, PatternError, Reason};

/// The M1 list, measured on Dave's tree (research M1). `result` has no slash:
/// Nix's `result` is a symlink.
pub(crate) const DEFAULTS: &str = "\
.git/
node_modules/
target/
.venv/
__pycache__/
.cache/
dist/
build/
.next/
vendor/
.direnv/
result
";

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
    defaults: Option<Layer>,
    config: Config,
}

/// One compiled ignore file.
#[derive(Debug)]
struct Layer {
    /// Directory holding the file, relative to the root.
    base: PathBuf,
    matcher: Gitignore,
    /// Anchored `!` patterns; only ever non-empty for a `.ferretignore`.
    reincludes: Vec<Reinclude>,
    parent: Chain,
}

/// Which precedence band a match came from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Band {
    Ferret,
    Other,
}

impl DirRules {
    /// Rules at a configured root: its own ignore files, the global ignore
    /// file's contents (if any) and, when `config.defaults`, the built-ins.
    ///
    /// Returns every pattern that could not be used alongside the rules,
    /// which apply without them. `root` is only used to name files in those
    /// errors. A work tree begins at `root` only if `files.git_root`; a root
    /// inside a work tree whose `.git` is above it is treated as outside one.
    pub fn root(
        root: &Path,
        global: Option<&str>,
        files: IgnoreFiles<'_>,
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
        let defaults = config.defaults.then(|| {
            Layer::compile(
                PathBuf::new(),
                IgnoreFile::Builtin,
                DEFAULTS,
                None,
                &mut errors,
                false,
            )
        });
        let bare = Self {
            path: PathBuf::new(),
            root: Arc::from(root),
            ferret: None,
            git: None,
            shared: Arc::new(Shared {
                global,
                defaults,
                config,
            }),
            traversing: false,
        };
        let rules = bare.with_files(PathBuf::new(), files, &mut errors);
        (rules, errors)
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

    /// Decides entry `name` of this directory.
    ///
    /// Special files are always skipped. Otherwise an entry is included when
    /// the first matching layer whitelists it or no layer matches; while
    /// traversing an excluded directory, only a `.ferretignore` whitelist
    /// includes. An included directory descends, a file indexes unless it is
    /// over the size cap, a symlink is catalogued. An excluded directory is
    /// traversed if an anchored `.ferretignore` `!` pattern reaches below it.
    pub fn decide(&self, name: &OsStr, entry: Entry) -> Decision {
        let path = self.path.join(name);
        let is_dir = entry == Entry::Dir;
        let included = match self.first_match(&path, is_dir) {
            Some((band, true)) => !self.traversing || band == Band::Ferret,
            Some((_, false)) => false,
            None => !self.traversing,
        };
        if !included {
            return if is_dir && self.reinclude_below(&path) {
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
        let base = [&self.shared.global, &self.shared.defaults]
            .into_iter()
            .flatten()
            .map(|layer| (Band::Other, layer));
        ferret
            .chain(git)
            .chain(base)
            .find_map(|(band, layer)| Some((band, layer.matched(path, is_dir)?)))
    }

    fn reinclude_below(&self, path: &Path) -> bool {
        layers(&self.ferret).any(|layer| {
            path.strip_prefix(&layer.base)
                .is_ok_and(|rel| layer.reincludes.iter().any(|r| r.reaches_below(rel)))
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
            matcher,
            reincludes: if collect_reincludes {
                patterns
                    .into_iter()
                    .filter_map(Reinclude::from_pattern)
                    .collect()
            } else {
                Vec::new()
            },
            parent,
        }
    }

    /// `Some(whitelisted)` if a pattern in this file matches `path` (relative
    /// to the root), `None` if none does.
    fn matched(&self, path: &Path, is_dir: bool) -> Option<bool> {
        let rel = path.strip_prefix(&self.base).ok()?;
        match self.matcher.matched(rel, is_dir) {
            Match::None => None,
            Match::Ignore => Some(false),
            Match::Whitelist => Some(true),
        }
    }
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
    fn builtin_defaults_compile_cleanly() {
        let (_, errors) = root(None, IgnoreFiles::default());
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
        let log = rules.decide(OsStr::new("x.log"), Entry::File { size: 1 });
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
    fn defaults_off_leaves_default_names_alone() {
        let config = Config {
            defaults: false,
            ..Config::default()
        };
        let (rules, _) = DirRules::root(Path::new("/r"), None, IgnoreFiles::default(), config);
        assert_eq!(
            rules.decide(OsStr::new("node_modules"), Entry::Dir),
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
            let (rules, errors) = root(None, files);
            assert!(errors.is_empty());
            assert_eq!(
                rules.decide(OsStr::new("target"), Entry::Dir),
                Decision::Traverse
            );
            let traversed = rules.traverse(OsStr::new("target"));
            let decision = traversed.decide(OsStr::new("doc"), Entry::Dir);
            if decision == Decision::Traverse {
                let doc = traversed.traverse(OsStr::new("doc"));
                assert_eq!(
                    doc.decide(OsStr::new("README.md"), Entry::File { size: 0 }),
                    Decision::Index,
                    "{content:?}"
                );
            } else {
                assert_eq!(decision, Decision::Descend, "{content:?}");
            }
        }
    }
}
