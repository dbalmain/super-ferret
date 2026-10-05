//! `.git` files against `git check-ignore`. Git is the oracle only: these
//! tests never read its source.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use ferret_policy::{Config, Decision, Reason};

use super::{decision, walked};

struct Tree {
    path: PathBuf,
}

impl Tree {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir()
            .join("ferret-git-oracle")
            .join(format!("{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn join(&self, rel: &str) -> PathBuf {
        self.path.join(rel)
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(super) fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "ferret")
        .env("GIT_AUTHOR_EMAIL", "ferret@example.com")
        .env("GIT_COMMITTER_NAME", "ferret")
        .env("GIT_COMMITTER_EMAIL", "ferret@example.com")
        .output()
        .unwrap()
}

pub(super) fn git_ok(dir: &Path, args: &[&str]) {
    let output = git(dir, args);
    assert!(
        output.status.success(),
        "git {args:?} in {}: {}\n{}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr),
        output.status,
    );
}

/// `git check-ignore` exits 0 when the path is ignored and 1 when it is not.
pub(super) fn ignored(dir: &Path, rel: &str) -> bool {
    let output = git(dir, &["check-ignore", "-q", "--", rel]);
    match output.status.code() {
        Some(0) => true,
        Some(1) => false,
        code => panic!(
            "git check-ignore {rel} in {}: {code:?}\n{}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr),
        ),
    }
}

pub(super) fn init(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git_ok(dir, &["init", "-q", "-b", "main"]);
}

fn commit(dir: &Path) {
    git_ok(dir, &["add", "-A"]);
    git_ok(dir, &["commit", "-q", "-m", "init"]);
}

fn write(path: &Path, bytes: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, bytes).unwrap();
}

#[test]
fn a_gitfile_matches_check_ignore_for_a_worktree_and_a_submodule() {
    let tree = Tree::new("layouts");

    let main = tree.join("main");
    init(&main);
    write(&main.join("README"), "hi\n");
    commit(&main);
    let exclude = main.join(".git/info/exclude");
    let mut text = fs::read_to_string(&exclude).unwrap();
    text.push_str("secret.txt\n");
    fs::write(&exclude, text).unwrap();

    let linked = tree.join("linked");
    let linked_arg = linked.to_string_lossy().into_owned();
    git_ok(
        &main,
        &["worktree", "add", "-q", "-b", "linked", &linked_arg],
    );
    let gitfile = fs::read_to_string(linked.join(".git")).unwrap();
    let gitdir = gitfile
        .strip_prefix("gitdir: ")
        .unwrap()
        .trim_end_matches(['\n', '\r']);
    assert!(
        Path::new(gitdir).is_absolute(),
        "worktree gitdir: {gitfile}"
    );
    assert!(fs::symlink_metadata(linked.join(".git")).unwrap().is_file());
    let commondir = fs::read_to_string(Path::new(gitdir).join("commondir")).unwrap();
    assert_eq!(commondir, "../..\n");

    write(&linked.join("secret.txt"), "x\n");
    write(&linked.join("kept.txt"), "x\n");
    // A per-worktree exclude is not the common dir. Git ignores it.
    let local = Path::new(gitdir).join("info/exclude");
    write(&local, "decoy.txt\n");
    write(&linked.join("decoy.txt"), "x\n");

    let linked_walk = walked(linked.as_path(), None, Config::default());
    assert!(linked_walk.io.is_empty(), "{:?}", linked_walk.io);
    for rel in ["secret.txt", "kept.txt", "decoy.txt"] {
        let skip = decision(&linked_walk, rel) == Decision::Skip;
        assert_eq!(skip, ignored(&linked, rel), "{rel}");
    }

    let remote = tree.join("remote");
    init(&remote);
    write(&remote.join("lib.txt"), "lib\n");
    commit(&remote);
    let super_repo = tree.join("super");
    init(&super_repo);
    write(&super_repo.join("README"), "top\n");
    commit(&super_repo);
    let remote_arg = remote.to_string_lossy().into_owned();
    git_ok(
        &super_repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &remote_arg,
            "sub",
        ],
    );
    let sub_git = fs::read_to_string(super_repo.join("sub/.git")).unwrap();
    let sub_rel = sub_git
        .strip_prefix("gitdir: ")
        .unwrap()
        .trim_end_matches(['\n', '\r']);
    assert!(
        !Path::new(sub_rel).is_absolute(),
        "submodule gitdir: {sub_git}"
    );
    assert!(!super_repo.join(".git/modules/sub/commondir").exists());

    let module_exclude = super_repo.join(".git/modules/sub/info/exclude");
    let mut text = fs::read_to_string(&module_exclude).unwrap();
    text.push_str("mod-secret.txt\n");
    fs::write(&module_exclude, text).unwrap();
    let super_exclude = super_repo.join(".git/info/exclude");
    let mut text = fs::read_to_string(&super_exclude).unwrap();
    text.push_str("parent-secret.txt\n");
    fs::write(&super_exclude, text).unwrap();

    write(&super_repo.join("parent-secret.txt"), "x\n");
    write(&super_repo.join("sub/mod-secret.txt"), "x\n");
    write(&super_repo.join("sub/mod-kept.txt"), "x\n");
    write(&super_repo.join("sub/parent-secret.txt"), "x\n");

    let sub = super_repo.join("sub");
    assert!(ignored(&super_repo, "parent-secret.txt"));
    assert!(ignored(&sub, "mod-secret.txt"));
    assert!(!ignored(&sub, "mod-kept.txt"));
    assert!(!ignored(&sub, "parent-secret.txt"));

    let super_walk = walked(super_repo.as_path(), None, Config::default());
    assert!(super_walk.io.is_empty(), "{:?}", super_walk.io);
    assert_eq!(decision(&super_walk, "parent-secret.txt"), Decision::Skip);
    assert_eq!(decision(&super_walk, "sub/mod-secret.txt"), Decision::Skip);
    assert_eq!(decision(&super_walk, "sub/mod-kept.txt"), Decision::Index);
    assert_eq!(
        decision(&super_walk, "sub/parent-secret.txt"),
        Decision::Index
    );
}

#[test]
fn a_symlinked_dot_git_does_not_contribute_exclude() {
    // Git follows the symlink and applies exclude. The walker must not open
    // anything through it; `.gitignore` still applies because the entry exists.
    let tree = Tree::new("symlink");
    let repo = tree.join("repo");
    init(&repo);
    let exclude = repo.join(".git/info/exclude");
    let mut text = fs::read_to_string(&exclude).unwrap();
    text.push_str("secret.txt\n");
    fs::write(&exclude, text).unwrap();
    fs::rename(repo.join(".git"), repo.join(".gitreal")).unwrap();
    symlink(".gitreal", repo.join(".git")).unwrap();
    write(&repo.join(".gitignore"), "*.o\n");
    write(&repo.join("secret.txt"), "x\n");
    write(&repo.join("a.o"), "x\n");
    write(&repo.join("a.c"), "y\n");

    assert!(ignored(&repo, "secret.txt"));

    let walked = walked(repo.as_path(), None, Config::default());
    assert!(walked.io.is_empty(), "{:?}", walked.io);
    assert_eq!(decision(&walked, "secret.txt"), Decision::Index);
    assert_eq!(decision(&walked, "a.o"), Decision::Skip);
    assert_eq!(decision(&walked, "a.c"), Decision::Index);
    assert_eq!(
        decision(&walked, ".git"),
        Decision::Catalog(Reason::Symlink)
    );
}

#[test]
fn a_commondir_that_is_not_a_file_is_a_fault_not_the_gitdir() {
    let tree = Tree::new("commondir-dir");
    let gitdir = tree.join("gitdir");
    write(&gitdir.join("info/exclude"), "excluded.txt\n");
    fs::create_dir(gitdir.join("commondir")).unwrap();
    let work = tree.join("work");
    write(&work.join(".git"), "gitdir: ../gitdir\n");
    write(&work.join("excluded.txt"), "x\n");

    let walk = walked(work.as_path(), None, Config::default());
    // The gitdir's own exclude is not the common dir's; git fails here
    // rather than apply it.
    assert_eq!(decision(&walk, "excluded.txt"), Decision::Index);
    assert_eq!(
        walk.io,
        [(PathBuf::from(".git"), std::io::ErrorKind::InvalidData)]
    );
}

#[test]
fn a_symlinked_exclude_applies_with_both_git_layouts() {
    let tree = Tree::new("symlinked-exclude");
    let directory = tree.join("directory");
    write(&directory.join(".git/info/rules"), "secret.txt\n");
    symlink("rules", directory.join(".git/info/exclude")).unwrap();
    write(&directory.join("secret.txt"), "x");

    let file = tree.join("file");
    write(&file.join(".git"), "gitdir: ../gitdir\n");
    write(&tree.join("gitdir/info/rules"), "secret.txt\n");
    symlink("rules", tree.join("gitdir/info/exclude")).unwrap();
    write(&file.join("secret.txt"), "x");

    for root in [directory, file] {
        let result = walked(&root, None, Config::default());
        assert!(result.io.is_empty(), "{}: {:?}", root.display(), result.io);
        assert_eq!(decision(&result, "secret.txt"), Decision::Skip);
    }
}

#[test]
fn core_excludes_file_in_git_config_does_not_exclude_entries() {
    let tree = Tree::new("core-excludes-file-ignored");
    let repo = tree.join("repo");
    write(
        &repo.join(".git/config"),
        "[core]\nexcludesFile = ../rules\n",
    );
    write(&tree.join("rules"), "secret.txt\n");
    write(&repo.join("secret.txt"), "visible to ferret\n");

    let walk = walked(&repo, None, Config::default());
    assert!(walk.io.is_empty(), "{:?}", walk.io);
    assert_eq!(decision(&walk, "secret.txt"), Decision::Index);
}

/// A `gitdir:` path is followed as git follows it, symlinks included: the
/// gitdir's text can already name any directory, so refusing a symlink as
/// its last component protects nothing and breaks a working layout.
#[test]
fn a_gitdir_reached_through_a_symlink_applies_its_exclude() {
    let tree = Tree::new("symlinked-gitdir");
    let main = tree.join("main");
    init(&main);
    write(&main.join("README"), "hi\n");
    commit(&main);
    let exclude = main.join(".git/info/exclude");
    let mut text = fs::read_to_string(&exclude).unwrap();
    text.push_str("secret.txt\n");
    fs::write(&exclude, text).unwrap();

    let linked = tree.join("linked");
    let linked_arg = linked.to_string_lossy().into_owned();
    git_ok(&main, &["worktree", "add", "-q", &linked_arg]);
    symlink(main.join(".git/worktrees/linked"), tree.join("alias")).unwrap();
    write(&linked.join("secret.txt"), "x");

    let absolute = format!("gitdir: {}\n", tree.join("alias").display());
    for gitfile in [absolute.as_str(), "gitdir: ../alias\n"] {
        fs::write(linked.join(".git"), gitfile).unwrap();
        assert!(ignored(&linked, "secret.txt"), "git: {gitfile}");
        let result = walked(&linked, None, Config::default());
        assert!(result.io.is_empty(), "{gitfile}: {:?}", result.io);
        assert_eq!(decision(&result, "secret.txt"), Decision::Skip, "{gitfile}");
    }
}
