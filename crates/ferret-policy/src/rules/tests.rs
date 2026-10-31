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
        ..IgnoreFiles::default()
    };
    let root = Dir::root(None, files);
    assert_eq!(root.decide("x.log", FILE), Decision::Index);
    assert_eq!(root.decide("keep", FILE), Decision::Skip);
    // A git file nearer than the ferret file still loses to it.
    let sub = root.enter("sub", gitignore("x.log\n"));
    assert_eq!(sub.decide("x.log", FILE), Decision::Index);
}

#[test]
fn only_a_repository_whitelist_reveals_a_hidden_name() {
    let reveals = |dir: &Dir, name: &str| dir.0.reveals(OsStr::new(name), false);
    let files = IgnoreFiles {
        gitignore: Some(".env*\n!.env.example\n!.both\n.mine\n"),
        ferretignore: Some("!.kept\n!.both\n!.mine\n"),
        git_root: true,
        ..IgnoreFiles::default()
    };
    let root = Dir::root(Some("!.global\n.g*\n"), files);
    assert!(reveals(&root, ".env.example"));
    assert!(!reveals(&root, ".env.local"));
    assert!(!reveals(&root, ".other"));
    // A .ferretignore rule is ferret's alone: it neither reveals ...
    assert!(!reveals(&root, ".kept"));
    // ... nor hides what a repository rule below it revealed, though it
    // is the last match ...
    assert!(reveals(&root, ".both"));
    // ... and its own whitelist over a repository exclusion is no reveal.
    assert_eq!(root.decide(".mine", FILE), Decision::Index);
    assert!(!reveals(&root, ".mine"));
    // Nor is the global file's.
    assert!(!reveals(&root, ".global"));
    let sub = root.enter("sub", gitignore("!.gk\n"));
    assert!(reveals(&sub, ".gk"));
    // `.ignore` and `.rgignore` count, in a work tree or not.
    let plain = Dir::root(
        None,
        IgnoreFiles {
            ignore: Some("!.a\n"),
            rgignore: Some("!.b\n"),
            ..IgnoreFiles::default()
        },
    );
    assert!(reveals(&plain, ".a") && reveals(&plain, ".b"));
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
        ..IgnoreFiles::default()
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
        ..IgnoreFiles::default()
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
        ..IgnoreFiles::default()
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
    let (layer, errors) = Layer::compile_text(line, Band::Ferret, &lists);
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
        let (layer, _) = Layer::compile_text(line, Band::Ferret, &lists);
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
