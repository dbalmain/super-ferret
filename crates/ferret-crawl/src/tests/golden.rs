//! The policy golden corpus, materialised as real trees and walked by [`walk`].
//!
//! Fixture format: `crates/ferret-policy/tests/golden.rs`. Expectations come
//! from those files; the decisions come from the real walker.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use ferret_policy::{Config, DEFAULT_IGNORE, Decision, Entry, Reason};

use super::Scratch;
use crate::{Event, walk};

struct Case {
    title: String,
    config: Config,
    global: String,
    nodes: BTreeMap<PathBuf, Node>,
}

struct Node {
    entry: Entry,
    content: String,
    expect: String,
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../ferret-policy/tests/golden")
}

fn parse(source: &str, file: &Path) -> Vec<Case> {
    let mut cases: Vec<Case> = Vec::new();
    let mut target: Option<Option<PathBuf>> = None;
    for (index, raw) in source.lines().enumerate() {
        let at = || format!("{}:{}", file.display(), index + 1);
        let line = raw.trim_start();
        if let Some(title) = line.strip_prefix("== ") {
            cases.push(Case {
                title: title.to_owned(),
                config: Config::default(),
                global: DEFAULT_IGNORE.to_owned(),
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
                None => &mut case.global,
                Some(path) => &mut case.nodes.get_mut(path).unwrap().content,
            };
            buffer.push_str(text);
            buffer.push('\n');
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["cap", bytes] => case.config.size_cap = bytes.parse().unwrap(),
            ["defaults", "off"] => case.global.clear(),
            ["global"] => target = Some(None),
            [spec, rest @ ..] if !rest.is_empty() && rest.len() <= 2 => {
                let (path, entry) = parse_spec(spec, rest, &at());
                let parent = path.parent().unwrap();
                let listed = parent.as_os_str().is_empty()
                    || case
                        .nodes
                        .get(parent)
                        .is_some_and(|node| node.entry == Entry::Dir);
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

/// Writes the fixture as a tree. Symlinks point at `secret`, a directory
/// outside the walked root that holds `marker.txt`, so a walker that followed
/// a link would report an unexpected path.
fn materialize(tree: &Path, secret: &Path, case: &Case) {
    for (path, node) in &case.nodes {
        let full = tree.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        match node.entry {
            Entry::Dir => fs::create_dir_all(&full).unwrap(),
            Entry::File { size } => {
                let bytes = if node.content.is_empty() {
                    vec![b'x'; usize::try_from(size).unwrap()]
                } else {
                    let declared = usize::try_from(size).unwrap();
                    if declared != 1 && declared != node.content.len() {
                        panic!(
                            "{} declares size {size} and {} bytes of content",
                            path.display(),
                            node.content.len()
                        );
                    }
                    node.content.as_bytes().to_vec()
                };
                fs::write(&full, bytes).unwrap();
            }
            Entry::Symlink => std::os::unix::fs::symlink(secret, &full).unwrap(),
            Entry::Other => {
                let status = Command::new("mkfifo").arg(&full).status().unwrap();
                assert!(status.success(), "mkfifo {}: {status}", full.display());
            }
        }
    }
}

fn mismatches(tree: &Path, case: &Case) -> Vec<String> {
    let mut got = BTreeMap::new();
    let mut faults = Vec::new();
    walk(tree, Some(&case.global), case.config, |event| match event {
        Event::Decided(decided) => {
            got.insert(decided.path.to_path_buf(), label(decided.decision));
        }
        Event::Io { path, error } => {
            faults.push(format!("io {}: {error}", path.display()));
        }
        Event::Pattern(error) => faults.push(format!("pattern {error}")),
    });
    let mut out = faults;
    for (path, node) in &case.nodes {
        let actual = got.get(path).copied().unwrap_or("unvisited");
        if actual != node.expect {
            out.push(format!(
                "{}: expected {}, got {actual}",
                path.display(),
                node.expect
            ));
        }
    }
    for path in got.keys() {
        if !case.nodes.contains_key(path) {
            out.push(format!("unexpected {}", path.display()));
        }
    }
    out
}

#[test]
fn golden_corpus_matches_the_walker() {
    let dir = corpus_dir();
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension() == Some("txt".as_ref()))
        .collect();
    files.sort();
    let mut cases = 0;
    let mut failures = Vec::new();
    for file in &files {
        for case in parse(&fs::read_to_string(file).unwrap(), file) {
            assert!(!case.nodes.is_empty(), "{}: empty case", case.title);
            cases += 1;
            let scratch = Scratch::new(&format!("golden-{cases}"));
            let tree = scratch.join("tree");
            let secret = scratch.join("secret");
            fs::create_dir(&tree).unwrap();
            fs::create_dir(&secret).unwrap();
            fs::write(secret.join("marker.txt"), "followed").unwrap();
            materialize(&tree, &secret, &case);
            for mismatch in mismatches(&tree, &case) {
                failures.push(format!("{} [{}] {mismatch}", file.display(), case.title));
            }
        }
    }
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
