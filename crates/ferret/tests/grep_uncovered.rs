//! D112 B compatibility targets for grep operands outside every index.

#[path = "support/grep_uncovered.rs"]
mod fixture;

use fixture::{Fixture, sorted};
use std::fs;

#[test]
fn nested_gitignore_negation_directory_anchor_and_double_star() {
    let f = Fixture::new("nested-gitignore");
    f.repo();
    f.write(
        ".gitignore",
        b"*.log\n!keep.log\ngen/\n/top.txt\n**/cache/**\n",
    );
    f.write("sub/.gitignore", b"*.tmp\n!keep.tmp\n");
    f.write("sub/deep/.gitignore", b"deeper.tmp\n");
    for path in [
        "keep.log",
        "drop.log",
        "top.txt",
        "sub/top.txt",
        "gen/a.txt",
        "sub/gen/a.txt",
        "sub/keep.tmp",
        "sub/drop.tmp",
        "sub/deep/deeper.tmp",
        "sub/deep/cache/a.txt",
        "sub/cache/a.txt",
        "visible.txt",
    ] {
        f.write(path, b"hit\n");
    }
    f.compare(&f.tree, &["-l", "hit", "."], true);
}

#[test]
fn operand_subdirectory_uses_parent_ignore_rules() {
    let f = Fixture::new("parent-rules");
    f.repo();
    f.write(".gitignore", b"*.log\nblocked/\n");
    f.write("src/.gitignore", b"local.tmp\n");
    for path in [
        "src/keep.rs",
        "src/drop.log",
        "src/local.tmp",
        "blocked/ignored.rs",
    ] {
        f.write(path, b"hit\n");
    }
    f.compare(&f.tree, &["-l", "hit", "src"], true);
}

#[test]
fn named_ignored_directory_is_searched_with_parent_rules_inside() {
    let f = Fixture::new("named-ignored-dir");
    f.repo();
    f.write(".gitignore", b"target/\n*.log\n");
    for path in ["target/a.txt", "target/sub/b.txt", "target/c.log"] {
        f.write(path, b"hit\n");
    }
    f.compare(&f.tree, &["-l", "hit", "target"], true);
}

#[test]
fn named_ignored_file_is_searched() {
    let f = Fixture::new("named-ignored-file");
    f.repo();
    f.write(".gitignore", b"secret.log\n");
    f.write("secret.log", b"hit\n");
    f.compare(&f.tree, &["-l", "hit", "secret.log"], true);
}

#[test]
fn cwd_inside_ignored_directory_still_applies_parent_rules() {
    let f = Fixture::new("cwd-ignored-dir");
    f.repo();
    f.write(".gitignore", b"target/\n*.log\n");
    for path in ["target/a.txt", "target/b.log"] {
        f.write(path, b"hit\n");
    }
    f.compare(&f.tree.join("target"), &["-l", "hit"], true);
}

#[test]
fn git_info_exclude_and_default_global_excludes_apply() {
    let f = Fixture::new("exclude-sources");
    f.repo();
    fs::write(f.tree.join(".git/info/exclude"), b"info.txt\n").unwrap();
    let global = f.base.join("home/config/git/ignore");
    fs::create_dir_all(global.parent().unwrap()).unwrap();
    fs::write(&global, b"global.txt\n").unwrap();
    for path in ["info.txt", "global.txt", "visible.txt"] {
        f.write(path, b"hit\n");
    }
    f.compare(&f.tree, &["-l", "hit", "."], true);
}

#[test]
fn ignore_file_precedence_gitignore_ignore_then_rgignore() {
    let f = Fixture::new("ignore-precedence");
    f.repo();
    f.write(".gitignore", b"from-ignore.txt\nfrom-rgignore.txt\n");
    f.write(".ignore", b"!from-ignore.txt\nfrom-rgignore.txt\n");
    f.write(".rgignore", b"!from-rgignore.txt\n");
    for path in ["from-ignore.txt", "from-rgignore.txt", "ordinary.txt"] {
        f.write(path, b"hit\n");
    }
    f.compare(&f.tree, &["-l", "hit", "."], true);
}

#[test]
fn ignore_files_outside_git_repository() {
    let f = Fixture::new("no-repo-ignore-files");
    f.write(".gitignore", b"git.txt\n");
    f.write(".ignore", b"ignore.txt\n");
    f.write(".rgignore", b"rgignore.txt\n");
    for path in ["git.txt", "ignore.txt", "rgignore.txt", "ordinary.txt"] {
        f.write(path, b"hit\n");
    }
    f.compare(&f.tree, &["-l", "hit", "."], true);
}

#[test]
fn nested_git_repository_uses_its_own_rules() {
    let f = Fixture::new("nested-repo");
    f.repo();
    f.write(".gitignore", b"vendor/*.log\n");
    f.write("vendor/.git/HEAD", b"ref: refs/heads/main\n");
    f.write("vendor/.gitignore", b"*.tmp\n");
    for path in ["vendor/outer.log", "vendor/inner.tmp", "vendor/keep.txt"] {
        f.write(path, b"hit\n");
    }
    f.compare(&f.tree, &["-l", "hit", "."], true);
}

#[test]
fn hidden_entries_are_skipped_unless_hidden_is_set() {
    let f = Fixture::new("hidden");
    f.repo();
    for path in [
        "visible.txt",
        ".hidden.txt",
        ".hidden/deep.txt",
        ".git/objects/data",
    ] {
        f.write(path, b"hit\n");
    }
    f.compare_all(
        &f.tree,
        &[
            (&["-l", "hit", "."], true),
            (&["--hidden", "-l", "hit", "."], true),
        ],
    );
}

#[test]
fn ferret_default_skip_directories_are_searched_by_rg() {
    let f = Fixture::new("default-skip-directories");
    f.repo();
    for directory in ["node_modules", "vendor", "build", "dist", "target"] {
        f.write(&format!("{directory}/match.txt"), b"hit\n");
    }
    f.compare(&f.tree, &["-l", "hit", "."], true);
}

#[test]
fn files_over_eight_mib_are_searched() {
    let f = Fixture::new("large-file");
    f.repo();
    let mut contents = vec![b'x'; 8 * 1024 * 1024 + 1];
    contents.extend_from_slice(b"\nhit\n");
    f.write("large.txt", &contents);
    f.compare(&f.tree, &["-l", "hit", "."], true);
}

#[test]
fn default_output_matches_rg_line_output() {
    let f = Fixture::new("default-output");
    f.repo();
    f.write("one.txt", b"before\nhit line\nafter\n");
    f.compare(&f.tree, &["hit", "one.txt"], false);
}

#[test]
fn symlinked_directory_is_skipped_unless_follow_is_set() {
    use std::os::unix::fs::symlink;

    let f = Fixture::new("symlink-directory");
    f.repo();
    f.write("real/match.txt", b"hit\n");
    symlink("real", f.tree.join("linked")).unwrap();
    f.write("real/binary", b"hit\0binary\n");
    symlink("real/match.txt", f.tree.join("file-link")).unwrap();
    f.compare(&f.tree, &["-L", "-l", "hit", "."], true);
    f.compare_all(
        &f.tree,
        &[
            (&["-l", "hit", "."], true),
            (&["-L", "-l", "hit", "."], true),
        ],
    );
}

// An unrelated index must never make an uncovered search open content or
// contact a daemon: a corrupt content manifest would otherwise fail the call.
#[test]
fn uncovered_search_bypasses_unrelated_content_and_daemon() {
    let f = Fixture::new("unrelated-index");
    let indexed = f.base.join("indexed");
    fs::create_dir(&indexed).unwrap();
    fs::write(indexed.join("file"), b"hit\n").unwrap();
    f.index(&indexed);
    fs::write(f.base.join("index/index/manifest"), b"broken").unwrap();
    f.write("visible.txt", b"hit\n");
    f.compare(&f.tree, &["-l", "hit", "."], true);
    let mut command = f.command(std::ffi::OsStr::new(env!("CARGO_BIN_EXE_ferret")), &f.tree);
    let output = command
        .env_remove("FERRET_NO_DAEMON")
        .env("FERRET_DAEMON_BIN", "/does/not/exist")
        .args(["grep", "-l", "hit", "."])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stdout, b"./visible.txt\n");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert!(!f.base.join("home/runtime/ferret").exists());
}

// Covered operands retain snapshot membership while uncovered operands read
// current names. Both orders must preserve operand barriers and output shape.
#[test]
fn mixed_operands_use_snapshot_and_live_in_operand_order() {
    let f = Fixture::new("mixed");
    f.write("covered/old.txt", b"hit\n");
    f.index(&f.tree.join("covered"));
    f.write("covered/new.txt", b"hit\n");
    f.write("uncovered/new.txt", b"hit\n");
    for (args, expected) in [
        (
            ["covered", "uncovered"],
            "covered/old.txt\nuncovered/new.txt\n",
        ),
        (
            ["uncovered", "covered"],
            "uncovered/new.txt\ncovered/old.txt\n",
        ),
    ] {
        let actual = f.ferret(&f.tree, &["-l", "hit", args[0], args[1]]);
        assert_eq!(actual.status.code(), Some(0), "{actual:?}");
        assert_eq!(actual.stdout, expected.as_bytes());
        let actual = f.ferret(&f.tree, &["--files", args[0], args[1]]);
        assert_eq!(actual.status.code(), Some(0), "{actual:?}");
        assert_eq!(actual.stdout, expected.as_bytes());
    }
}

#[test]
fn rgignore_outranks_a_closer_gitignore() {
    let f = Fixture::new("cross-depth-precedence");
    f.repo();
    f.write(".rgignore", b"!*.log\n");
    f.write("sub/.gitignore", b"*.log\n");
    f.write("sub/keep.log", b"hit\n");
    f.compare(&f.tree, &["-l", "hit", "."], true);
}

#[test]
fn overrides_and_ignore_whitelists_reveal_ignored_and_hidden_entries() {
    let f = Fixture::new("override-visibility");
    f.repo();
    f.write(".gitignore", b"*.log\n!.hidden.txt\n!.hidden-dir/\n");
    for path in [
        "drop.log",
        "ordinary.txt",
        ".hidden.txt",
        ".hidden-dir/file",
    ] {
        f.write(path, b"hit\n");
    }
    f.compare(&f.tree, &["-l", "hit", "."], true);
    f.compare(&f.tree, &["-g", "*.log", "-l", "hit", "."], true);
    f.compare(&f.tree, &["-g", "!.hidden.txt", "-l", "hit", "."], true);
}

// Rg applies the global git excludes only in directories inside a work tree,
// whichever ancestor starts it; slash-anchored global patterns stay relative
// to the command's cwd.
#[test]
fn default_global_excludes_apply_only_inside_git_and_keep_the_operand_anchor() {
    let f = Fixture::new("nested-global");
    let global = f.base.join("home/config/git/ignore");
    fs::create_dir_all(global.parent().unwrap()).unwrap();
    fs::write(&global, b"/global.txt\n").unwrap();
    for path in [
        "global.txt",
        "keep.txt",
        "plain/global.txt",
        "nested/global.txt",
        "nested/keep.txt",
    ] {
        f.write(path, b"hit\n");
    }
    f.write("nested/.git/HEAD", b"ref: refs/heads/main\n");
    f.compare(&f.tree, &["-l", "hit", "."], true);
    f.compare(&f.tree, &["-l", "hit", "nested"], true);
    fs::write(&global, b"global.txt\n").unwrap();
    f.compare(&f.tree, &["-l", "hit", "."], true);
    f.compare(&f.tree, &["-l", "hit", "plain"], true);
    f.compare(&f.tree.join("nested"), &["-l", "hit", "."], true);
    // Pinned, so the cases above cannot all agree vacuously: outside the
    // nested repository nothing is excluded.
    let outside = f.ferret(&f.tree, &["-l", "hit", "."]);
    assert_eq!(
        sorted(&outside.stdout),
        [
            "./global.txt",
            "./keep.txt",
            "./nested/keep.txt",
            "./plain/global.txt"
        ]
    );
    // A `.git` above the operand puts it inside a work tree.
    f.repo();
    f.compare(&f.tree.join("plain"), &["-l", "hit", "."], true);
    f.compare(&f.tree, &["-l", "hit", "plain"], true);
}

// Ancestor `.ignore` and `.rgignore` files apply to a named subdirectory,
// anchored at their own directories, whatever the cwd.
#[test]
fn ancestor_ignore_and_rgignore_apply_to_a_named_subdirectory() {
    let f = Fixture::new("ancestor-ignore");
    f.write(".ignore", b"x\n/a/sub/y\nsub/unanchored-miss\n");
    f.write("a/.rgignore", b"z\n");
    for path in ["x", "y", "z", "unanchored-miss", "keep"] {
        f.write(&format!("a/sub/{path}"), b"hit\n");
    }
    f.compare(&f.tree, &["-l", "hit", "a/sub"], true);
    f.compare(&f.tree.join("a"), &["-l", "hit", "sub"], true);
    f.compare(&f.tree.join("a/sub"), &["-l", "hit", "."], true);
    let named = f.ferret(&f.tree, &["-l", "hit", "a/sub"]);
    assert_eq!(
        sorted(&named.stdout),
        ["a/sub/keep", "a/sub/unanchored-miss"]
    );
}

// An operand that starts a nested repository drops the outer `.gitignore`
// rules and keeps the outer `.ignore` and `.rgignore` rules.
#[test]
fn named_nested_repository_keeps_outer_ignore_and_rgignore() {
    let f = Fixture::new("nested-operand");
    f.repo();
    f.write(".gitignore", b"from-gitignore\n");
    f.write(".ignore", b"from-ignore\n");
    f.write(".rgignore", b"from-rgignore\n");
    f.write("nested/.git/HEAD", b"ref: refs/heads/main\n");
    for path in ["from-gitignore", "from-ignore", "from-rgignore", "keep"] {
        f.write(&format!("nested/{path}"), b"hit\n");
    }
    f.compare(&f.tree, &["-l", "hit", "."], true);
    f.compare(&f.tree, &["-l", "hit", "nested"], true);
    f.compare(&f.tree.join("nested"), &["-l", "hit", "."], true);
    let named = f.ferret(&f.tree, &["-l", "hit", "nested"]);
    assert_eq!(
        sorted(&named.stdout),
        ["nested/from-gitignore", "nested/keep"]
    );
}

// A `.git` file without `commondir` (`git init --separate-git-dir`): rg does
// not read that gitdir's `info/exclude`. With a `commondir` (a linked work
// tree) it reads the common directory's.
#[test]
fn git_file_exclude_applies_only_through_commondir() {
    let f = Fixture::new("gitfile-exclude");
    let separate = f.base.join("separate");
    fs::create_dir_all(separate.join("info")).unwrap();
    fs::write(separate.join("info/exclude"), b"excluded\n").unwrap();
    let linked = f.base.join("linked");
    fs::create_dir_all(linked.join("info")).unwrap();
    fs::write(linked.join("commondir"), b"../common\n").unwrap();
    fs::write(linked.join("info/exclude"), b"own\n").unwrap();
    fs::create_dir_all(f.base.join("common/info")).unwrap();
    fs::write(f.base.join("common/info/exclude"), b"excluded\n").unwrap();
    for (tree, gitdir) in [("separate-tree", "separate"), ("linked-tree", "linked")] {
        f.write(
            &format!("{tree}/.git"),
            format!("gitdir: ../../{gitdir}\n").as_bytes(),
        );
        for path in ["excluded", "own", "keep"] {
            f.write(&format!("{tree}/{path}"), b"hit\n");
        }
        f.compare(&f.tree.join(tree), &["-l", "hit", "."], true);
    }
    let separate = f.ferret(&f.tree.join("separate-tree"), &["-l", "hit", "."]);
    assert_eq!(sorted(&separate.stdout), ["./excluded", "./keep", "./own"]);
}

// The catalog resolves an operand by its spelling: an alias into an indexed
// root has none, so it is searched live rather than failing, while the root's
// own spelling keeps snapshot membership.
#[test]
fn symlink_alias_into_an_indexed_root_is_searched_live() {
    let f = Fixture::new("alias-root");
    f.write("real/old", b"hit\n");
    std::os::unix::fs::symlink("real", f.tree.join("alias")).unwrap();
    f.index(&f.tree.join("real"));
    f.write("real/new", b"hit\n");
    f.compare(&f.tree, &["-l", "hit", "alias"], true);
    let snapshot = f.ferret(&f.tree, &["-l", "hit", "real"]);
    assert_eq!(snapshot.status.code(), Some(0), "{snapshot:?}");
    assert_eq!(snapshot.stdout, b"real/old\n");
}

// A directory created under a root after its crawl is covered but has no
// snapshot. The advice must name what works: --live is not in play.
#[test]
fn new_directory_under_a_root_names_working_remedies() {
    let f = Fixture::new("new-under-root");
    f.write("root/old", b"hit\n");
    f.index(&f.tree.join("root"));
    f.write("root/fresh/new", b"hit\n");
    for args in [&["-l", "hit", "root/fresh"][..], &["--files", "root/fresh"]] {
        let output = f.ferret(&f.tree, args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(!stderr.contains("--live"), "{stderr}");
        assert!(
            stderr.contains("ferret index DIR") && stderr.contains("-u"),
            "{stderr}"
        );
    }
}

// Rg does without an ignore input it cannot read, silently, and applies the
// rules that did load: in an ancestor of the operand or inside the tree, for
// `.ignore`, `.gitignore`, `info/exclude`, an unreadable `.git` directory and
// a gitfile naming no gitdir. Each case keeps a readable rule in force so a
// tool that dropped everything would still disagree.
#[test]
fn unreadable_policy_inputs_are_skipped_silently() {
    use std::os::unix::fs::PermissionsExt;

    let f = Fixture::new("unreadable-inputs");
    f.write("repo/.gitignore", b"from-gitignore\n");
    f.write("repo/.ignore", b"from-ignore\n");
    f.write("repo/leaf/inner/.ignore", b"from-inner\n");
    fs::create_dir_all(f.tree.join("repo/.git/info")).unwrap();
    fs::write(f.tree.join("repo/.git/info/exclude"), b"from-exclude\n").unwrap();
    for path in [
        "from-gitignore",
        "from-ignore",
        "from-exclude",
        "keep",
        "inner/from-inner",
        "inner/stay",
    ] {
        f.write(&format!("repo/leaf/{path}"), b"hit\n");
    }
    let leaf = f.tree.join("repo/leaf");
    for (input, mode) in [
        ("repo/.ignore", 0o644),
        ("repo/.gitignore", 0o644),
        ("repo/.git/info/exclude", 0o644),
        ("repo/.git", 0o755),
        ("repo/leaf/inner/.ignore", 0o644),
    ] {
        let path = f.tree.join(input);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(
            fs::read(&path).is_err() && fs::read_dir(&path).is_err(),
            "{input} is still readable; run the tests as an unprivileged user"
        );
        f.compare(&leaf, &["-l", "hit", "."], true);
        f.compare(&f.tree, &["-l", "hit", "repo/leaf"], true);
        let output = f.ferret(&leaf, &["-l", "hit", "."]);
        assert!(output.stderr.is_empty(), "{input}: {output:?}");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    }
    fs::rename(f.tree.join("repo/.git"), f.base.join("moved-git")).unwrap();
    f.write("repo/.git", b"gitdir: ../missing\n");
    f.compare(&leaf, &["-l", "hit", "."], true);
    let output = f.ferret(&leaf, &["-l", "hit", "."]);
    assert!(output.stderr.is_empty(), "{output:?}");
}

// The catalog follows a recorded symlink before applying the `..` after it,
// so `root/link/../..` with `link -> deep/sub` names `root` itself: covered,
// answered from the snapshot as find answers it, not walked live.
#[test]
fn parent_components_after_a_symlink_resolve_as_the_catalog_does() {
    let f = Fixture::new("link-parents");
    f.write("root/deep/sub/.keep", b"");
    std::os::unix::fs::symlink("deep/sub", f.tree.join("root/link")).unwrap();
    f.write("root/old", b"hit\n");
    f.index(&f.tree.join("root"));
    f.write("root/new", b"hit\n");
    for args in [
        &["-l", "hit", "root/link/../.."][..],
        &["--files", "root/link/../.."],
    ] {
        let output = f.ferret(&f.tree, args);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert_eq!(output.stdout, b"root/link/../../old\n", "{args:?}");
    }
    let mut find = f.command(std::ffi::OsStr::new(env!("CARGO_BIN_EXE_ferret")), &f.tree);
    let find = find
        .arg("--index")
        .arg(f.base.join("index"))
        .args(["find", "root/link/../..", "-type", "f", "-name", "old"])
        .output()
        .unwrap();
    assert_eq!(find.stdout, b"root/link/../../old\n", "{find:?}");
}
