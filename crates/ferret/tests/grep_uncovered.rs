//! D112 B compatibility targets for grep operands outside every index.

#[path = "support/grep_uncovered.rs"]
mod fixture;

use fixture::Fixture;
use std::fs;

#[test]
#[ignore = "D112 B: uncovered fallback not built yet"]
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
#[ignore = "D112 B: uncovered fallback not built yet"]
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
#[ignore = "D112 B: uncovered fallback not built yet"]
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
#[ignore = "D112 B: uncovered fallback not built yet"]
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
#[ignore = "D112 B: uncovered fallback not built yet"]
fn git_info_exclude_and_configured_global_excludes_apply() {
    let f = Fixture::new("exclude-sources");
    f.repo();
    fs::write(f.tree.join(".git/info/exclude"), b"info.txt\n").unwrap();
    let global = f.base.join("home/global-ignore");
    fs::write(&global, b"global.txt\n").unwrap();
    fs::write(
        f.base.join("home/.gitconfig"),
        format!("[core]\n\texcludesFile = {}\n", global.display()),
    )
    .unwrap();
    for path in ["info.txt", "global.txt", "visible.txt"] {
        f.write(path, b"hit\n");
    }
    f.compare(&f.tree, &["-l", "hit", "."], true);
}

#[test]
#[ignore = "D112 B: uncovered fallback not built yet"]
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
#[ignore = "D112 B: uncovered fallback not built yet"]
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
#[ignore = "D112 B: uncovered fallback not built yet"]
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
#[ignore = "D112 B: uncovered fallback not built yet"]
fn hidden_entries_are_skipped_unless_hidden_is_set() {
    let f = Fixture::new("hidden");
    f.repo();
    for path in ["visible.txt", ".hidden.txt", ".hidden/deep.txt"] {
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
#[ignore = "D112 B: uncovered fallback not built yet"]
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
    f.compare(&f.tree, &["-l", "hit", "large.txt"], true);
}

#[test]
fn default_output_matches_rg_line_output() {
    let f = Fixture::new("default-output");
    f.repo();
    f.write("one.txt", b"before\nhit line\nafter\n");
    f.compare(&f.tree, &["hit", "one.txt"], false);
}

#[test]
#[ignore = "D112 B: uncovered fallback not built yet"]
fn symlinked_directory_is_skipped_unless_follow_is_set() {
    use std::os::unix::fs::symlink;

    let f = Fixture::new("symlink-directory");
    f.repo();
    f.write("real/match.txt", b"hit\n");
    symlink("real", f.tree.join("linked")).unwrap();
    f.compare_all(
        &f.tree,
        &[
            (&["-l", "hit", "."], true),
            (&["-L", "-l", "hit", "."], true),
        ],
    );
}
