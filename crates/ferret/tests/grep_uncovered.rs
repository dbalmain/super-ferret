//! D112 B compatibility targets for grep operands outside every index.

#[path = "support/grep_uncovered.rs"]
mod fixture;

use fixture::Fixture;
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

#[test]
fn default_global_excludes_apply_outside_git_and_keep_the_operand_anchor() {
    let f = Fixture::new("nested-global");
    let global = f.base.join("home/config/git/ignore");
    fs::create_dir_all(global.parent().unwrap()).unwrap();
    fs::write(&global, b"/global.txt\n").unwrap();
    f.write("global.txt", b"hit\n");
    f.write("nested/.git/HEAD", b"ref: refs/heads/main\n");
    f.write("nested/global.txt", b"hit\n");
    f.write("nested/keep.txt", b"hit\n");
    f.compare(&f.tree, &["-l", "hit", "."], true);
    f.compare(&f.tree, &["-l", "hit", "nested"], true);
    fs::write(&global, b"global.txt\n").unwrap();
    f.compare(&f.tree, &["-l", "hit", "."], true);
}
