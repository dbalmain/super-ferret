//! D112 B compatibility targets for grep operands outside every index.

#[path = "support/grep_uncovered.rs"]
mod fixture;

use fixture::{Fixture, sorted};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

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

// A walked directory's rules load at its first entry, from the walk's listing
// (D132). Loading them must still look as it did when they loaded on entering
// the directory: a fault names the directory, not the entry that found it,
// and a bad pattern warns once even when no entry of the directory is
// selected (here `-g` and hidden selection skip all of them before policy),
// and once when its listing is split between workers.
#[test]
fn a_directory_s_rules_fault_and_warn_as_the_directory() {
    let f = Fixture::new("lazy-rules");
    f.write("src/keep.rs", b"hit\n");
    f.write("src/sub/a.rs", b"hit\n");
    f.write("src/sub/.ferretignore", b"a.rs\n");
    f.write("wide/sub/.ignore", b"bad\\\n");
    for index in 0..300 {
        f.write(&format!("wide/sub/f{index}.txt"), b"hit\n");
    }
    let denied = f.tree.join("src/sub/.ferretignore");
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o000)).unwrap();
    let faults = [
        f.ferret_own(&f.tree, &["-l", "hit", "src"]),
        f.ferret_own(&f.tree, &["--files", "src"]),
    ];
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o644)).unwrap();
    for output in faults {
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert_eq!(output.stdout, b"src/keep.rs\n", "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.starts_with("ferret: grep: src/sub: ") && stderr.contains(".ferretignore"),
            "{stderr}"
        );
        assert_eq!(stderr.lines().count(), 1, "{stderr}");
    }
    let warning = "warning: ignore pattern skipped:";
    for threads in ["1", "8"] {
        for args in [
            &["--files", "-g", "*.rs", "wide"][..],
            &["-c", "hit", "wide"],
        ] {
            let output = f
                .command(std::ffi::OsStr::new(env!("CARGO_BIN_EXE_ferret")), &f.tree)
                .env("FERRET_SEARCH_THREADS", threads)
                .arg("--index")
                .arg(f.base.join("index"))
                .args(["grep", "--rg"])
                .args(args)
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(
                stderr.matches(warning).count(),
                1,
                "{threads} {args:?}: {stderr}"
            );
        }
    }
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
    // Retargeted after the crawl to the same directory through an outside
    // alias: the catalog's recorded link still decides.
    std::os::unix::fs::symlink("root", f.tree.join("alias")).unwrap();
    fs::remove_file(f.tree.join("root/link")).unwrap();
    std::os::unix::fs::symlink(f.tree.join("alias/deep/sub"), f.tree.join("root/link")).unwrap();
    let output = f.ferret(&f.tree, &["-l", "hit", "root/link/../.."]);
    assert_eq!(output.stdout, b"root/link/../../old\n", "{output:?}");
}

// A symlink inside an opaque (ferret-ignored) directory is not in the
// catalog, which resolves the spelling to that opaque boundary and uses its
// own live fallback. Following the disk link out through an alias, and so
// routing to rg's policy, would apply `.gitignore` and lose `drop`.
#[test]
fn symlink_inside_an_opaque_directory_resolves_as_the_catalog_does() {
    let f = Fixture::new("opaque-link");
    fs::write(f.base.join("home/config/ferret/ignore"), b"target/\n").unwrap();
    f.write("root/.git/HEAD", b"ref: refs/heads/main\n");
    f.write("root/.gitignore", b"drop\n");
    f.write("root/drop", b"hit\n");
    f.write("root/deep/sub/.keep", b"");
    fs::create_dir(f.tree.join("root/target")).unwrap();
    std::os::unix::fs::symlink("root", f.tree.join("alias")).unwrap();
    std::os::unix::fs::symlink(
        f.tree.join("alias/deep/sub"),
        f.tree.join("root/target/link"),
    )
    .unwrap();
    f.index(&f.tree.join("root"));
    let output = f.ferret(&f.tree, &["-l", "hit", "root/target/link/../.."]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stdout, b"root/target/link/../../drop\n");
}

// Rg follows a symlinked `.git`: one that does not resolve (a loop, or a
// dangling link) starts no work tree, so neither `.gitignore` nor the global
// excludes apply; one to a file or directory does, and through a directory
// link its `info/exclude` applies too.
#[test]
fn symlinked_git_counts_only_when_it_resolves() {
    use std::os::unix::fs::symlink;

    let f = Fixture::new("git-symlinks");
    let global = f.base.join("home/config/git/ignore");
    fs::create_dir_all(global.parent().unwrap()).unwrap();
    fs::write(&global, b"global\n").unwrap();
    fs::create_dir_all(f.base.join("gitdir/info")).unwrap();
    fs::write(f.base.join("gitdir/info/exclude"), b"excluded\n").unwrap();
    fs::write(f.base.join("junk"), b"not a gitdir line\n").unwrap();
    for (case, target) in [
        ("loop", PathBuf::from(".git")),
        ("dangling", PathBuf::from("nowhere")),
        ("file", f.base.join("junk")),
        ("directory", f.base.join("gitdir")),
    ] {
        f.write(&format!("{case}/.gitignore"), b"local\n");
        for path in ["keep", "local", "global", "excluded"] {
            f.write(&format!("{case}/{path}"), b"hit\n");
        }
        symlink(target, f.tree.join(case).join(".git")).unwrap();
        f.compare(&f.tree.join(case), &["-l", "hit", "."], true);
    }
    f.compare(&f.tree, &["-l", "hit", "."], true);
    let lone = f.ferret(&f.tree.join("loop"), &["-l", "hit", "."]);
    assert_eq!(
        sorted(&lone.stdout),
        ["./excluded", "./global", "./keep", "./local"]
    );
}

// Mode 0111 is execute-only: the directory cannot be listed, but a name
// inside it can be opened. rg still reads `info/exclude`. A real `.git`, a
// symlink to one, and a chain of symlinks are the same open.
#[test]
fn execute_only_git_directory_exclude_is_read_by_name() {
    use std::os::unix::fs::symlink;

    if running_as_root() {
        return;
    }
    let f = Fixture::new("git-mode-0111");
    write_global(&f);
    for case in ["real", "link", "double"] {
        work_tree(&f, case, HITS);
    }
    f.write("real/.git/info/exclude", b"excluded\n");
    f.write("target-0111/info/exclude", b"excluded\n");
    let _real = ListingDenied::at(&f.tree.join("real/.git"), 0o111);
    let _target = ListingDenied::at(&f.tree.join("target-0111"), 0o111);
    symlink("../target-0111", f.tree.join("link/.git")).unwrap();
    symlink("target-0111", f.tree.join("mid")).unwrap();
    symlink("../mid", f.tree.join("double/.git")).unwrap();
    for case in ["real", "link", "double"] {
        compare_quiet(&f, &f.tree.join(case));
    }
}

// A symlinked `.git` whose target cannot be traversed is not a repository,
// so neither `.gitignore` nor the global excludes apply. A symlink to a
// mode 000 directory does resolve: it is a repository, and `info/exclude`
// cannot be read.
#[test]
fn unresolved_git_symlink_starts_no_work_tree() {
    use std::os::unix::fs::symlink;

    if running_as_root() {
        return;
    }
    let f = Fixture::new("git-unresolved");
    write_global(&f);
    f.write("locked/meta/info/exclude", b"excluded\n");
    f.write("notdir", b"file\n");
    f.write("mode0/info/exclude", b"excluded\n");
    for case in ["through-dir", "through-file", "symlink-000"] {
        work_tree(&f, case, HITS);
    }
    let _locked = ListingDenied::at(&f.tree.join("locked"), 0o000);
    let _mode0 = ListingDenied::at(&f.tree.join("mode0"), 0o000);
    symlink("../locked/meta", f.tree.join("through-dir/.git")).unwrap();
    symlink("../notdir/meta", f.tree.join("through-file/.git")).unwrap();
    symlink("../mode0", f.tree.join("symlink-000/.git")).unwrap();
    for case in ["through-dir", "through-file", "symlink-000"] {
        compare_quiet(&f, &f.tree.join(case));
    }
}

// A gitfile's exclude comes from the `commondir` directory. That directory,
// and the gitdir that names it, may be execute-only; rg still reads the
// common `info/exclude` by name and does not apply the gitdir's own. A
// gitdir path that cannot be traversed still leaves a repository — the
// gitfile was read — and contributes no exclude.
#[test]
fn execute_only_commondir_exclude_is_read_by_name() {
    if running_as_root() {
        return;
    }
    let f = Fixture::new("gitfile-mode-0111");
    write_global(&f);
    let hits = ["keep", "local", "global", "excluded", "own"];
    f.write("common-x/info/exclude", b"excluded\n");
    f.write("gitdir-a/commondir", b"../common-x\n");
    f.write("gitdir-a/info/exclude", b"own\n");
    f.write("common-y/info/exclude", b"excluded\n");
    f.write("gitdir-b/commondir", b"../common-y\n");
    f.write("gitdir-b/info/exclude", b"own\n");
    f.write("reachable/info/exclude", b"excluded\n");
    f.write("locked/gitdir/commondir", b"../../reachable\n");
    f.write("locked/gitdir/info/exclude", b"own\n");
    let _common = ListingDenied::at(&f.tree.join("common-x"), 0o111);
    let _gitdir = ListingDenied::at(&f.tree.join("gitdir-b"), 0o111);
    let _locked = ListingDenied::at(&f.tree.join("locked"), 0o000);
    for (case, gitdir) in [
        ("common-link", "../gitdir-a\n"),
        ("gitdir-link", "../gitdir-b\n"),
        ("unresolved-gitdir", "../locked/gitdir\n"),
    ] {
        work_tree(&f, case, &hits);
        f.write(
            &format!("{case}/.git"),
            format!("gitdir: {gitdir}").as_bytes(),
        );
    }
    for case in ["common-link", "gitdir-link", "unresolved-gitdir"] {
        compare_quiet(&f, &f.tree.join(case));
    }
}

const HITS: &[&str] = &["keep", "local", "global", "excluded"];

// Clippy allows `unwrap` inside `#[test]` and not in the helpers those tests
// call. Fixture setup panics if it cannot write, same as the tests above.
#[allow(clippy::unwrap_used)]
fn write_global(f: &Fixture) {
    let global = f.base.join("home/config/git/ignore");
    fs::create_dir_all(global.parent().unwrap()).unwrap();
    fs::write(&global, b"global\n").unwrap();
}

fn work_tree(f: &Fixture, case: &str, names: &[&str]) {
    f.write(&format!("{case}/.gitignore"), b"local\n");
    for name in names {
        f.write(&format!("{case}/{name}"), b"hit\n");
    }
}

fn compare_quiet(f: &Fixture, cwd: &Path) {
    f.compare(cwd, &["-l", "hit", "."], true);
    for args in [&["-l", "hit", "."][..], &["--files"][..]] {
        let rg = f.oracle(cwd, args);
        let ferret = f.ferret(cwd, args);
        assert!(
            rg.stderr.is_empty() && ferret.stderr.is_empty(),
            "{args:?}\nrg: {rg:?}\nferret: {ferret:?}"
        );
    }
}

/// Restores mode 0755 so the fixture can be removed. Mode 000 and 0111
/// directories cannot be listed or unlinked from by an unprivileged user.
struct ListingDenied {
    path: PathBuf,
}

impl ListingDenied {
    // Clippy allows `unwrap` inside `#[test]` and not in the helpers those
    // tests call.
    #[allow(clippy::unwrap_used)]
    fn at(path: &Path, mode: u32) -> Self {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        let denied = fs::read_dir(path).is_err();
        let guard = Self {
            path: path.to_path_buf(),
        };
        assert!(
            denied,
            "{} mode {mode:04o} is still listable; run the tests as an unprivileged user",
            path.display()
        );
        guard
    }
}

impl Drop for ListingDenied {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o755));
    }
}

// Clippy allows `unwrap` inside `#[test]` and not in the helpers those tests
// call.
#[allow(clippy::unwrap_used)]
fn running_as_root() -> bool {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let root = status.lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next() == Some("Uid:") && fields.nth(1) == Some("0")
    });
    if root {
        eprintln!("skipping: running as root, so chmod 000 does not deny access");
    }
    root
}

// D115 B: `core.excludesFile` chooses the global excludes file as rg 15.2
// does. Each case rewrites the three config sources and compares.
#[test]
fn core_excludes_file_is_read_as_rg_reads_it() {
    let f = Fixture::new("core-excludes");
    f.repo();
    let home = f.base.join("home");
    let xdg = home.join("config/git");
    fs::create_dir_all(&xdg).unwrap();
    fs::write(xdg.join("ignore"), b"default.txt\n").unwrap();
    for (name, pattern) in [
        ("ex-xdg", "xdg.txt"),
        ("ex-home", "home.txt"),
        ("ex-env", "env.txt"),
        ("ex-tilde", "tilde.txt"),
    ] {
        fs::write(home.join(name), format!("{pattern}\n")).unwrap();
    }
    fs::write(f.tree.join("ex-relative"), b"relative.txt\n").unwrap();
    for name in [
        "default", "xdg", "home", "env", "tilde", "relative", "plain",
    ] {
        f.write(&format!("{name}.txt"), b"hit\n");
    }
    let key = |value: &str| format!("[core]\n\texcludesFile = {value}\n");
    let at = |name: &str| home.join(name).display().to_string();
    let env_file = f.base.join("env-config");
    let set = |xdg_config: Option<String>, home_config: Option<String>, env: Option<String>| {
        for (path, text) in [
            (xdg.join("config"), xdg_config),
            (home.join(".gitconfig"), home_config),
            (env_file.clone(), env),
        ] {
            match text {
                Some(text) => fs::write(path, text).unwrap(),
                None => {
                    let _ = fs::remove_file(path);
                }
            }
        }
    };
    // Name, then the XDG config, ~/.gitconfig and $GIT_CONFIG_GLOBAL texts.
    type Case = (&'static str, Option<String>, Option<String>, Option<String>);
    let cases: Vec<Case> = vec![
        ("none", None, None, None),
        ("xdg", Some(key(&at("ex-xdg"))), None, None),
        (
            "home beats xdg",
            Some(key(&at("ex-xdg"))),
            Some(key(&at("ex-home"))),
            None,
        ),
        (
            "env beats home",
            Some(key(&at("ex-xdg"))),
            Some(key(&at("ex-home"))),
            Some(key(&at("ex-env"))),
        ),
        (
            "env without the key passes on",
            None,
            Some(key(&at("ex-home"))),
            Some("[user]\n\tname = x\n".into()),
        ),
        (
            "first file's missing target wins",
            Some(key(&at("ex-xdg"))),
            Some(key(&at("missing"))),
            None,
        ),
        ("tilde", Some(key("~/ex-tilde")), None, None),
        ("relative to cwd", Some(key("ex-relative")), None, None),
        (
            "uppercase",
            Some("[CORE]\n\tEXCLUDESFILE = ~/ex-tilde\n".into()),
            None,
            None,
        ),
        ("quoted", Some(key("\"~/ex-tilde\"")), None, None),
        (
            "no spaces",
            Some("[core]\nexcludesFile=~/ex-tilde\n".into()),
            None,
            None,
        ),
        (
            "any section",
            Some("[user]\n\texcludesFile = ~/ex-tilde\n".into()),
            None,
            None,
        ),
        (
            "first line wins",
            Some(format!("{}{}", key("~/ex-tilde"), key(&at("ex-xdg")))),
            None,
            None,
        ),
        ("trailing comment", Some(key("~/ex-tilde ; c")), None, None),
        (
            "empty value",
            Some("[core]\n\texcludesFile =\n".into()),
            None,
            None,
        ),
        ("value with a space", Some(key("~/ex tilde")), None, None),
        (
            "same-line header",
            Some("[core] excludesFile = ~/ex-tilde\n".into()),
            None,
            None,
        ),
        (
            "commented",
            Some("[core]\n#\texcludesFile = ~/ex-tilde\n".into()),
            None,
            None,
        ),
        ("directory target", Some(key("~/")), None, None),
    ];
    f.env
        .borrow_mut()
        .push(("GIT_CONFIG_GLOBAL", env_file.clone()));
    let mut differences = Vec::new();
    for (name, xdg_config, home_config, env) in cases {
        set(xdg_config, home_config, env);
        let args = ["-l", "hit", "."];
        let rg = f.oracle(&f.tree, &args);
        let ferret = f.ferret(&f.tree, &args);
        if rg.status.code() != ferret.status.code()
            || sorted(&rg.stdout) != sorted(&ferret.stdout)
            || rg.stderr != ferret.stderr
        {
            differences.push(format!("{name}: rg {rg:?}; ferret {ferret:?}"));
        }
    }
    // Byte-level edges: name, then the ~/.gitconfig and XDG config bytes.
    set(None, None, None);
    let tilde: &[u8] = b"[core]\n\texcludesFile = ~/ex-tilde\n";
    type Bytes = (&'static str, Option<&'static [u8]>, &'static [u8]);
    let bytes: [Bytes; 11] = [
        ("quoted empty", None, b"[core]\n\texcludesFile = \"\"\n"),
        ("lone quote", None, b"[core]\n\texcludesFile = \"\n"),
        (
            "quoted empty before a value",
            None,
            b"[core]\n\texcludesFile = \"\"\n\texcludesFile = ~/ex-tilde\n",
        ),
        (
            "quoted empty in home",
            Some(b"[core]\n\texcludesFile = \"\"\n"),
            tilde,
        ),
        (
            "tabs around =",
            None,
            b"[core]\n\texcludesFile\t=\t~/ex-tilde\n",
        ),
        ("CRLF", None, b"[core]\r\n\texcludesFile = ~/ex-tilde\r\n"),
        (
            "invalid UTF-8 comment first",
            None,
            b"# \xff\xfe\n[core]\n\texcludesFile = ~/ex-tilde\n",
        ),
        (
            "NUL in value",
            None,
            b"[core]\n\texcludesFile = ~/ex\0tilde\n",
        ),
        (
            "no-break space in value",
            None,
            b"[core]\n\texcludesFile = ~/ex\xc2\xa0tilde\n",
        ),
        (
            "invalid UTF-8 value before a value",
            None,
            b"[core]\n\texcludesFile = ~/ex-\xfftilde\n\texcludesFile = ~/ex-tilde\n",
        ),
        (
            "invalid UTF-8 value in home",
            Some(b"[core]\n\texcludesFile = ~/ex-\xfftilde\n"),
            tilde,
        ),
    ];
    for (name, home_config, text) in bytes {
        match home_config {
            Some(home_config) => fs::write(home.join(".gitconfig"), home_config).unwrap(),
            None => {
                let _ = fs::remove_file(home.join(".gitconfig"));
            }
        }
        fs::write(xdg.join("config"), text).unwrap();
        let args = ["-l", "hit", "."];
        let rg = f.oracle(&f.tree, &args);
        let ferret = f.ferret(&f.tree, &args);
        if rg.status.code() != ferret.status.code()
            || sorted(&rg.stdout) != sorted(&ferret.stdout)
            || rg.stderr != ferret.stderr
        {
            differences.push(format!("{name}: rg {rg:?}; ferret {ferret:?}"));
        }
    }
    assert!(differences.is_empty(), "{}", differences.join("\n"));
    // rg skips a global excludes file it cannot read, without a message.
    set(None, None, None);
    fs::set_permissions(xdg.join("ignore"), fs::Permissions::from_mode(0o000)).unwrap();
    let args = ["-l", "hit", "."];
    let rg = f.oracle(&f.tree, &args);
    let ferret = f.ferret(&f.tree, &args);
    fs::set_permissions(xdg.join("ignore"), fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(rg.status.code(), ferret.status.code());
    assert_eq!(sorted(&rg.stdout), sorted(&ferret.stdout));
    assert!(rg.stderr.is_empty(), "{rg:?}");
    assert!(ferret.stderr.is_empty(), "{ferret:?}");
}

// D128: without `--rg`, an uncovered operand is walked as if it were an index
// root, so the files selected must not change when it is indexed. (Binary
// detection may: an uncovered read probes 64 KiB for a NUL as rg does, the
// crawl sniffs 8 KiB.) Each named path
// discriminates against rg's rules: rg hides the dot file, applies the
// ancestor `.gitignore`, and ignores ferret's global file. A file over the
// index size cap is searched, as indexed grep reads such files live.
#[test]
fn ferret_rules_walk_an_uncovered_operand_as_its_own_root() {
    let f = Fixture::new("own-root");
    f.repo();
    f.write(".gitignore", b"*.log\n");
    fs::write(f.base.join("home/config/ferret/ignore"), b"*.bak\n").unwrap();
    for path in [
        "src/keep.rs",
        "src/.env",
        "src/.notes/todo.md",
        "src/drop.log",
        "src/old.bak",
    ] {
        f.write(path, b"hit\n");
    }
    let mut big = b"hit\n".to_vec();
    big.resize(9 << 20, b'a');
    f.write("src/big.txt", &big);
    let src = f.tree.join("src");
    let expected = [
        "src/.env",
        "src/.notes/todo.md",
        "src/big.txt",
        "src/drop.log",
        "src/keep.rs",
    ];
    let cases: [&[&str]; 2] = [&["-l", "hit", "src"], &["--files", "src"]];
    let mut walked = Vec::new();
    for args in cases {
        let output = f.ferret_own(&f.tree, args);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
        assert_eq!(sorted(&output.stdout), expected, "{args:?}");
        walked.push(output.stdout);
    }
    let hidden = f.ferret_own(&f.tree, &["--no-hidden", "--files", "src"]);
    assert_eq!(
        sorted(&hidden.stdout),
        ["src/big.txt", "src/drop.log", "src/keep.rs"]
    );
    let rg = f.ferret(&f.tree, &["--files", "src"]);
    assert_eq!(
        sorted(&rg.stdout),
        ["src/big.txt", "src/keep.rs", "src/old.bak"]
    );

    f.index(&src);
    for (args, walked) in cases.into_iter().zip(walked) {
        let output = f.ferret_own(&f.tree, args);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
        assert_eq!(sorted(&output.stdout), sorted(&walked), "indexed {args:?}");
    }
    let rg = f.ferret(&f.tree, &["--files", "src"]);
    assert_eq!(
        sorted(&rg.stdout),
        ["src/big.txt", "src/drop.log", "src/keep.rs"]
    );
}
