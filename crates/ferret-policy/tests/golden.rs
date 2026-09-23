//! Golden-corpus test: every `tests/golden/*.txt` fixture describes a tree
//! and the decision expected for each of its paths. The driver walks each
//! tree through the public API only — `root`, then `enter` on `Descend` and
//! `traverse` on `Traverse` — so the precedence rules live in the library
//! and nowhere here. A path the walk never reaches is `unvisited`.
//!
//! Fixture format (one case per `== title` header):
//!
//! ```text
//! # comment
//! == title
//! cap 100
//! defaults off
//! global
//!   | *.tmp
//! dir/                     descend
//! dir/file.txt   120       index
//! dir/link@                symlink
//! dir/fifo=                skip
//! dir/.gitignore           index
//!   | *.log
//! ```
//!
//! `cap` sets `Config::size_cap`, `defaults off` clears `Config::defaults`,
//! and `global` starts the global ignore file. An entry line is a path, an
//! optional size in bytes (default 1) and the expectation. A trailing `/`
//! marks a directory, `@` a symlink, `=` a special file. `| ` lines are the
//! contents of the file (or `global`) above them.
//!
//! Expectations: `skip`, `descend`, `traverse`, `index`, `too-large`,
//! `symlink`, `unvisited`. Every path's parent directory must be listed. A
//! directory holding `.git` starts a work tree; `.git/info/exclude` is read
//! from the fixture even though the walk never enters `.git/`, as the crawler
//! reads it directly.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use ferret_policy::{Config, Decision, DirRules, Entry, IgnoreFiles, Reason};

struct Case {
    title: String,
    config: Config,
    global: Option<String>,
    nodes: BTreeMap<PathBuf, Node>,
}

struct Node {
    entry: Entry,
    content: String,
    expect: String,
}

// ── parsing ──

fn parse(source: &str, file: &Path) -> Vec<Case> {
    let mut cases: Vec<Case> = Vec::new();
    // Where `| ` lines go: the global file, or a node's contents.
    let mut target: Option<Option<PathBuf>> = None;
    for (index, raw) in source.lines().enumerate() {
        let at = || format!("{}:{}", file.display(), index + 1);
        let line = raw.trim_start();
        if let Some(title) = line.strip_prefix("== ") {
            cases.push(Case {
                title: title.to_owned(),
                config: Config::default(),
                global: None,
                nodes: BTreeMap::new(),
            });
            target = None;
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let case = cases
            .last_mut()
            .unwrap_or_else(|| panic!("{}: before any `==`", at()));
        if let Some(text) = line.strip_prefix('|') {
            let text = text.strip_prefix(' ').unwrap_or(text);
            let buffer = match target
                .as_ref()
                .unwrap_or_else(|| panic!("{}: stray `|`", at()))
            {
                None => case.global.get_or_insert_with(String::new),
                Some(path) => &mut case.nodes.get_mut(path).unwrap().content,
            };
            buffer.push_str(text);
            buffer.push('\n');
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["cap", bytes] => case.config.size_cap = bytes.parse().unwrap(),
            ["defaults", "off"] => case.config.defaults = false,
            ["global"] => target = Some(None),
            [spec, rest @ ..] if !rest.is_empty() && rest.len() <= 2 => {
                let (path, entry) = parse_spec(spec, rest, &at());
                let parent = path.parent().unwrap();
                let listed = parent.as_os_str().is_empty()
                    || case
                        .nodes
                        .get(parent)
                        .is_some_and(|n| n.entry == Entry::Dir);
                assert!(
                    listed,
                    "{}: parent of {} not listed as a directory",
                    at(),
                    path.display()
                );
                let node = Node {
                    entry,
                    content: String::new(),
                    expect: rest[rest.len() - 1].to_owned(),
                };
                assert!(
                    case.nodes.insert(path.clone(), node).is_none(),
                    "{}: duplicate",
                    at()
                );
                target = Some(Some(path));
            }
            _ => panic!("{}: cannot parse `{raw}`", at()),
        }
    }
    cases
}

fn parse_spec(spec: &str, rest: &[&str], at: &str) -> (PathBuf, Entry) {
    let size = match rest {
        [size, _] => size
            .parse()
            .unwrap_or_else(|_| panic!("{at}: bad size `{size}`")),
        _ => 1,
    };
    if let Some(path) = spec.strip_suffix('/') {
        (PathBuf::from(path), Entry::Dir)
    } else if let Some(path) = spec.strip_suffix('@') {
        (PathBuf::from(path), Entry::Symlink)
    } else if let Some(path) = spec.strip_suffix('=') {
        (PathBuf::from(path), Entry::Other)
    } else {
        (PathBuf::from(spec), Entry::File { size })
    }
}

// ── walking ──

fn label(decision: Decision) -> &'static str {
    match decision {
        Decision::Skip => "skip",
        Decision::Descend => "descend",
        Decision::Traverse => "traverse",
        Decision::Index => "index",
        Decision::Catalog(Reason::TooLarge) => "too-large",
        Decision::Catalog(Reason::Symlink) => "symlink",
    }
}

impl Case {
    /// What the crawler would read in `dir`.
    fn ignore_files(&self, dir: &Path) -> IgnoreFiles<'_> {
        let content = |name: &str| self.nodes.get(&dir.join(name)).map(|n| n.content.as_str());
        IgnoreFiles {
            ferretignore: content(".ferretignore"),
            gitignore: content(".gitignore"),
            git_root: self.nodes.contains_key(&dir.join(".git")),
            git_exclude: content(".git/info/exclude"),
        }
    }

    fn walk(&self, rules: &DirRules, dir: &Path, seen: &mut BTreeMap<PathBuf, &'static str>) {
        let children = self
            .nodes
            .iter()
            .filter(|(path, _)| path.parent() == Some(dir));
        for (path, node) in children {
            let name = path.file_name().unwrap_or(OsStr::new(""));
            let decision = rules.decide(name, node.entry);
            seen.insert(path.clone(), label(decision));
            match decision {
                Decision::Descend => {
                    let (child, errors) = rules.enter(name, self.ignore_files(path));
                    assert!(errors.is_empty(), "{}: {errors:?}", self.title);
                    self.walk(&child, path, seen);
                }
                Decision::Traverse => self.walk(&rules.traverse(name), path, seen),
                _ => {}
            }
        }
    }

    /// Mismatches as `path: expected X, got Y`.
    fn run(&self) -> Vec<String> {
        let root = Path::new("");
        let (rules, errors) = DirRules::root(
            Path::new("/golden"),
            self.global.as_deref(),
            self.ignore_files(root),
            self.config,
        );
        assert!(errors.is_empty(), "{}: {errors:?}", self.title);
        let mut seen = BTreeMap::new();
        self.walk(&rules, root, &mut seen);
        self.nodes
            .iter()
            .filter_map(|(path, node)| {
                let got = seen.get(path).copied().unwrap_or("unvisited");
                (got != node.expect)
                    .then(|| format!("{}: expected {}, got {got}", path.display(), node.expect))
            })
            .collect()
    }
}

#[test]
fn golden_corpus() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension() == Some(OsStr::new("txt")))
        .collect();
    files.sort();
    let mut cases = 0;
    let mut failures = Vec::new();
    for file in &files {
        for case in parse(&fs::read_to_string(file).unwrap(), file) {
            assert!(!case.nodes.is_empty(), "{}: empty case", case.title);
            cases += 1;
            for mismatch in case.run() {
                failures.push(format!("{} [{}] {mismatch}", file.display(), case.title));
            }
        }
    }
    // A corpus that silently stops being collected must not read as a pass.
    assert!(
        cases >= 10,
        "only {cases} golden cases found in {}",
        dir.display()
    );
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
