//! A root inside a git work tree (D22), checked against `git check-ignore`.
//!
//! Git is the oracle only: these tests never read its source. `check-ignore`
//! is run from the directory that contains the path, so a nested repository
//! is asked from inside itself. From the outer repository, `check-ignore`
//! still reports the outer pattern for a path under the nested `.git`.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use ferret_policy::{Config, Decision};

use super::gitfile::{git, git_ok, ignored, init};
use super::{Scratch, decision, mkfifo, walked, write};

#[test]
fn a_fifo_commondir_does_not_block_discovery_and_reports_a_fault() {
    let scratch = Scratch::new("ancestor-fifo-commondir");
    let repo = scratch.join("repo");
    write(&repo.join(".git/HEAD"), "ref: refs/heads/main\n");
    fs::create_dir_all(repo.join(".git/refs")).unwrap();
    mkfifo(&repo.join(".git/commondir"));
    write(&repo.join("sub/file.txt"), "x");

    let (sender, receiver) = mpsc::channel();
    let root = repo.join("sub");
    std::thread::spawn(move || {
        let result = walked(&root, None, Config::default());
        let _ = sender.send(result);
    });
    let result = receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("discovery blocked on the commondir FIFO");
    assert_eq!(decision(&result, "file.txt"), Decision::Index);
    assert!(
        result
            .io
            .iter()
            .any(|(_, kind)| *kind == std::io::ErrorKind::InvalidData),
        "discovery fault was lost: {:?}",
        result.io
    );
}

fn assert_same(root: &Path, rel: &str) {
    let walked = walked(root, None, Config::default());
    assert!(walked.io.is_empty(), "{rel}: {:?}", walked.io);
    let skip = decision(&walked, rel) == Decision::Skip;
    assert_eq!(skip, ignored(root, rel), "{rel}");
}

#[test]
fn an_ancestor_gitignore_matches_relative_to_its_directory() {
    let scratch = Scratch::new("ancestor-base");
    let repo = scratch.join("repo");
    init(&repo);
    write(&repo.join(".gitignore"), "*.o\n/src/gen/\n/gen/\n");
    fs::create_dir_all(repo.join("src/gen")).unwrap();
    write(&repo.join("src/a.o"), "x");
    write(&repo.join("src/keep.c"), "y");
    write(&repo.join("src/gen/out.c"), "z");
    // A directory `gen` directly under the work tree, which `/gen/` names.
    // It is not inside the walk root.
    fs::create_dir(repo.join("gen")).unwrap();

    let root = repo.join("src");
    let walked = walked(&root, None, Config::default());
    assert!(walked.io.is_empty(), "{:?}", walked.io);
    for rel in ["a.o", "gen", "keep.c"] {
        let skip = decision(&walked, rel) == Decision::Skip;
        assert_eq!(skip, ignored(&root, rel), "{rel}");
    }
    assert!(!walked.rows.contains_key(Path::new("gen/out.c")));

    // `/src/gen/` also skips `gen`, so the line above does not show that
    // `/gen/` is anchored at `repo`. A tree whose only rule is `/gen/` does.
    let plain = scratch.join("plain");
    init(&plain);
    write(&plain.join(".gitignore"), "/gen/\n");
    fs::create_dir_all(plain.join("src/gen")).unwrap();
    write(&plain.join("src/gen/f"), "f");
    let plain_root = plain.join("src");
    let plain_walk = super::walked(&plain_root, None, Config::default());
    assert!(plain_walk.io.is_empty(), "{:?}", plain_walk.io);
    assert!(
        !ignored(&plain_root, "gen"),
        "git anchored /gen/ at the walk root"
    );
    assert_eq!(decision(&plain_walk, "gen"), Decision::Descend);
    assert_eq!(decision(&plain_walk, "gen/f"), Decision::Index);
}

#[test]
fn a_closer_gitignore_reincludes_over_an_ancestor() {
    let scratch = Scratch::new("ancestor-bang");
    let repo = scratch.join("repo");
    init(&repo);
    write(&repo.join(".gitignore"), "*.o\n");
    write(&repo.join("src/.gitignore"), "!keep.o\n");
    write(&repo.join("src/keep.o"), "k");
    write(&repo.join("src/drop.o"), "d");

    let root = repo.join("src");
    let walked = walked(&root, None, Config::default());
    assert!(walked.io.is_empty(), "{:?}", walked.io);
    for rel in ["keep.o", "drop.o"] {
        let skip = decision(&walked, rel) == Decision::Skip;
        assert_eq!(skip, ignored(&root, rel), "{rel}");
    }
}

#[test]
fn the_work_tree_exclude_applies_above_the_root() {
    let scratch = Scratch::new("ancestor-exclude");
    let repo = scratch.join("repo");
    init(&repo);
    let exclude = repo.join(".git/info/exclude");
    let mut text = fs::read_to_string(&exclude).unwrap();
    text.push_str("secret.txt\n");
    fs::write(&exclude, text).unwrap();
    write(&repo.join("src/secret.txt"), "s");
    write(&repo.join("src/kept.txt"), "k");

    let root = repo.join("src");
    let walked = walked(&root, None, Config::default());
    assert!(walked.io.is_empty(), "{:?}", walked.io);
    for rel in ["secret.txt", "kept.txt"] {
        let skip = decision(&walked, rel) == Decision::Skip;
        assert_eq!(skip, ignored(&root, rel), "{rel}");
    }
}

#[test]
fn a_ferretignore_above_the_root_does_not_apply() {
    let scratch = Scratch::new("ancestor-ferret");
    let repo = scratch.join("repo");
    init(&repo);
    write(&repo.join(".ferretignore"), "secret.txt\n");
    write(&repo.join("src/secret.txt"), "s");
    write(&repo.join("src/.ferretignore"), "local.log\n");
    write(&repo.join("src/local.log"), "l");

    let root = repo.join("src");
    let walked = walked(&root, None, Config::default());
    assert!(walked.io.is_empty(), "{:?}", walked.io);
    // Git does not know `.ferretignore`. The ancestor file must not skip
    // `secret.txt`; the root's own file still skips `local.log`.
    assert!(!ignored(&root, "secret.txt"));
    assert_eq!(decision(&walked, "secret.txt"), Decision::Index);
    assert_eq!(decision(&walked, "local.log"), Decision::Skip);
}

#[test]
fn a_nested_repository_resets_ancestor_git_rules() {
    let scratch = Scratch::new("ancestor-nested");
    let repo = scratch.join("repo");
    init(&repo);
    write(&repo.join(".gitignore"), "*.o\n");
    let sub = repo.join("src/sub");
    init(&sub);
    write(&repo.join("src/a.o"), "a");
    write(&sub.join("a.o"), "b");
    write(&sub.join("keep.c"), "c");

    let root = repo.join("src");
    let walked = walked(&root, None, Config::default());
    assert!(walked.io.is_empty(), "{:?}", walked.io);
    assert_eq!(
        decision(&walked, "a.o") == Decision::Skip,
        ignored(&root, "a.o")
    );
    // Inside the nested repository the outer `*.o` does not apply. Asking
    // `check-ignore` from `root` still reports it; git's own view from `sub`
    // does not.
    assert_eq!(
        decision(&walked, "sub/a.o") == Decision::Skip,
        ignored(&sub, "a.o")
    );
    assert_eq!(
        decision(&walked, "sub/keep.c") == Decision::Skip,
        ignored(&sub, "keep.c")
    );
}

#[test]
fn a_root_with_no_work_tree_above_it_ignores_a_parent_gitignore() {
    let scratch = Scratch::new("ancestor-none");
    // An empty directory named `.git` is not a repository. Git agrees
    // (`rev-parse` fails); this machine has one at `/tmp`.
    fs::create_dir(scratch.join(".git")).unwrap();
    write(&scratch.join(".gitignore"), "*.o\n");
    write(&scratch.join("src/.ferretignore"), "*.log\n");
    write(&scratch.join("src/a.o"), "o");
    write(&scratch.join("src/a.log"), "l");
    write(&scratch.join("src/a.c"), "c");

    let root = scratch.join("src");
    let walked = walked(&root, None, Config::default());
    assert!(walked.io.is_empty(), "{:?}", walked.io);
    assert_eq!(decision(&walked, "a.o"), Decision::Index);
    assert_eq!(decision(&walked, "a.log"), Decision::Skip);
    assert_eq!(decision(&walked, "a.c"), Decision::Index);
}

#[test]
fn discovery_follows_the_canonical_path() {
    let scratch = Scratch::new("ancestor-link");
    let repo = scratch.join("canon/repo");
    init(&repo);
    write(&repo.join(".gitignore"), "*.o\n/src/gen/\n");
    fs::create_dir_all(repo.join("src/gen")).unwrap();
    write(&repo.join("src/a.o"), "x");
    fs::create_dir_all(scratch.join("logic")).unwrap();
    symlink(repo.join("src"), scratch.join("logic/src")).unwrap();

    let root = scratch.join("logic/src");
    assert_same(&root, "a.o");
    let walked = walked(&root, None, Config::default());
    assert_eq!(
        decision(&walked, "gen") == Decision::Skip,
        ignored(&root, "gen")
    );
}

#[test]
fn a_linked_worktree_above_the_root_uses_the_common_exclude() {
    let scratch = Scratch::new("ancestor-worktree");
    let main = scratch.join("main");
    init(&main);
    write(&main.join("README"), "hi\n");
    git_ok(&main, &["add", "-A"]);
    git_ok(&main, &["commit", "-q", "-m", "init"]);
    let linked = scratch.join("linked");
    let linked_arg = linked.to_string_lossy().into_owned();
    git_ok(
        &main,
        &["worktree", "add", "-q", "-b", "linked", &linked_arg],
    );
    let exclude = main.join(".git/info/exclude");
    let mut text = fs::read_to_string(&exclude).unwrap();
    text.push_str("secret.txt\n");
    fs::write(&exclude, text).unwrap();
    fs::create_dir_all(linked.join("src")).unwrap();
    write(&linked.join("src/secret.txt"), "s");
    write(&linked.join("src/kept.txt"), "k");

    let root = linked.join("src");
    let walked = walked(&root, None, Config::default());
    assert!(walked.io.is_empty(), "{:?}", walked.io);
    for rel in ["secret.txt", "kept.txt"] {
        let skip = decision(&walked, rel) == Decision::Skip;
        assert_eq!(skip, ignored(&root, rel), "{rel}");
    }
}

#[test]
fn discovery_stops_at_a_filesystem_boundary() {
    if std::env::var_os("FERRET_XDEV_CHILD").is_some() {
        xdev_child();
        return;
    }
    let scratch = Scratch::new("ancestor-xdev");
    let marker = scratch.join("result");
    let output = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "--"])
        .arg(std::env::current_exe().unwrap())
        .arg("tests::ancestor::discovery_stops_at_a_filesystem_boundary")
        .arg("--exact")
        .env("FERRET_XDEV_CHILD", "1")
        .env("FERRET_XDEV_BASE", &scratch.path)
        .env("FERRET_XDEV_MARKER", &marker)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "status {}\n{}{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(fs::read_to_string(&marker).unwrap(), "index\n");
}

fn xdev_child() {
    let base = PathBuf::from(std::env::var_os("FERRET_XDEV_BASE").unwrap());
    let marker = PathBuf::from(std::env::var_os("FERRET_XDEV_MARKER").unwrap());
    let tree = base.join("tree");
    init(&tree);
    write(&tree.join(".gitignore"), "*.o\n");
    let sub = tree.join("sub");
    fs::create_dir(&sub).unwrap();
    let mounted = Command::new("mount")
        .args(["-t", "tmpfs", "tmpfs"])
        .arg(&sub)
        .status()
        .unwrap();
    assert!(mounted.success(), "mount tmpfs");
    write(&sub.join("a.o"), "x");

    let git = git(&sub, &["check-ignore", "-q", "--", "a.o"]);
    assert_ne!(
        git.status.code(),
        Some(0),
        "git applied the parent exclude across the mount:\n{}",
        String::from_utf8_lossy(&git.stderr)
    );

    let walked = walked(&sub, None, Config::default());
    let text = if decision(&walked, "a.o") == Decision::Skip {
        "skip\n"
    } else {
        "index\n"
    };
    fs::write(&marker, text).unwrap();
}

/// D25: a configured root is always walked, even when the enclosing work
/// tree's rules exclude it, as they exclude everything below it for git. The
/// same rules still apply below the root.
#[test]
fn a_root_that_git_ignores_is_still_walked() {
    let scratch = Scratch::new("ancestor-ignored-root");
    let repo = scratch.join("repo");
    init(&repo);
    write(&repo.join(".gitignore"), "/src/\n*.o\n");
    write(&repo.join("src/file.txt"), "x");
    write(&repo.join("src/a.o"), "y");
    assert!(ignored(&repo, "src/file.txt"), "git no longer ignores it");

    let walked = walked(&repo.join("src"), None, Config::default());
    assert!(walked.io.is_empty(), "{:?}", walked.io);
    assert_eq!(decision(&walked, "file.txt"), Decision::Index);
    assert_eq!(decision(&walked, "a.o"), Decision::Skip);
}
