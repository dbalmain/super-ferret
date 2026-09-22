//! The crate graph in docs/DESIGN.md § Crates is the source of truth: every
//! workspace crate's `[dependencies]` must match its line there. Workspace
//! dependencies must match exactly; external ones must be listed. Changing
//! the layering therefore means changing the design document in the same
//! commit.

// Test-only helpers outside `#[test]` fns: clippy's allow-unwrap-in-tests
// does not reach them, and a panic is the right failure here.
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

type Graph = BTreeMap<String, BTreeSet<String>>;

fn root() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

/// Lines of the form `crate → dep, dep (note), …` inside DESIGN.md's first
/// `text` block. `(std only)`, `anything` and `later` list no dependencies.
fn designed() -> Graph {
    let doc = fs::read_to_string(root().join("docs/DESIGN.md")).unwrap();
    let block = doc
        .split("```text\n")
        .nth(1)
        .unwrap()
        .split("```")
        .next()
        .unwrap();
    block
        .lines()
        .filter_map(|line| line.split_once('→'))
        .map(|(name, deps)| {
            let mut plain = String::new();
            let mut depth = 0;
            for c in deps.chars() {
                match c {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ if depth == 0 => plain.push(c),
                    _ => {}
                }
            }
            let deps = plain
                .split([',', ';'])
                .map(str::trim)
                .filter(|d| !d.is_empty() && !d.contains(' ') && *d != "later")
                .map(String::from)
                .collect();
            (name.trim().to_string(), deps)
        })
        .collect()
}

/// Names under `[dependencies]` in each `crates/*/Cargo.toml`.
fn actual() -> Graph {
    fs::read_dir(root().join("crates"))
        .unwrap()
        .map(|entry| {
            let manifest = fs::read_to_string(entry.unwrap().path().join("Cargo.toml")).unwrap();
            let name = manifest
                .lines()
                .find_map(|l| l.strip_prefix("name = "))
                .unwrap()
                .trim_matches('"')
                .to_string();
            let deps = manifest
                .split("\n[dependencies]\n")
                .nth(1)
                .unwrap_or("")
                .lines()
                .take_while(|l| !l.starts_with('['))
                .filter_map(|l| l.split(['.', '=', ' ']).next())
                .filter(|d| !d.is_empty())
                .map(String::from)
                .collect();
            (name, deps)
        })
        .collect()
}

#[test]
fn crate_graph_matches_design() {
    let designed = designed();
    let actual = actual();
    for (name, deps) in &actual {
        let Some(allowed) = designed.get(name) else {
            panic!("crate `{name}` is missing from docs/DESIGN.md § Crates");
        };
        let internal = |set: &BTreeSet<String>| -> BTreeSet<String> {
            set.iter()
                .filter(|d| actual.contains_key(*d))
                .cloned()
                .collect()
        };
        assert_eq!(
            internal(deps),
            internal(allowed),
            "`{name}`'s workspace dependencies differ from docs/DESIGN.md § Crates"
        );
        let unlisted: Vec<_> = deps.difference(allowed).collect();
        assert!(
            unlisted.is_empty(),
            "`{name}` depends on {unlisted:?}, not listed in docs/DESIGN.md § Crates"
        );
    }
}

#[test]
fn design_graph_parses() {
    // Guards the parser: if the block's format changes, fail loudly rather
    // than compare against an empty graph.
    let designed = designed();
    assert!(designed["ferret-query"].contains("ferret-index"));
    assert!(designed["ferret-catalog"].is_empty());
}
