//! Deterministic regressions for Astra's first whole-find review.
#![allow(clippy::unwrap_used)] // Fixture setup failures identify the failed step.

#[path = "../../../tests/support/gnu_find.rs"]
mod gnu;
mod support {
    pub mod fixture;
}

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

const FERRET: &str = env!("CARGO_BIN_EXE_ferret");

struct Tree(PathBuf);
impl Tree {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("ferret-review-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn command(&self, oracle: bool) -> Command {
        let oracle_command = gnu::command();
        let mut command = support::fixture::bounded_command(
            if oracle {
                oracle_command.get_program()
            } else {
                std::ffi::OsStr::new(FERRET)
            },
            &self.0,
        );
        if !oracle {
            command.args(["find", "-I"]);
        }
        command
    }
    fn index(&self, paths: &[&str]) {
        let output = support::fixture::bounded_command(FERRET, &self.0)
            .arg("index")
            .args(paths)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    fn catalog(&self, args: &[&str]) -> Output {
        let output = support::fixture::bounded_command(FERRET, &self.0)
            .arg("find")
            .args(args)
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "timed out: {output:?}");
        output
    }
    fn run(&self, oracle: bool, args: &[&str]) -> Output {
        let output = gnu::output(self.command(oracle).args(args));
        assert_ne!(output.status.code(), Some(124), "timed out: {output:?}");
        output
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn delete_uses_the_observed_parent_after_an_ancestor_is_replaced() {
    // #1: pathname unlink deleted outside/victim after this synchronous exec.
    for explicit in [false, true] {
        for oracle in [true, false] {
            // GNU also redirects deletion for an explicit file start. The
            // retained-parent safety contract is deliberately stronger.
            if explicit && oracle {
                continue;
            }
            let tree = Tree::new(&format!("delete-{explicit}-{oracle}"));
            for directory in ["tree", "outside"] {
                fs::create_dir(tree.0.join(directory)).unwrap();
                fs::write(tree.0.join(directory).join("victim"), b"x").unwrap();
            }
            let output = tree.run(
                oracle,
                &[
                    if explicit { "tree/victim" } else { "tree" },
                    "-name",
                    "victim",
                    "-exec",
                    "sh",
                    "-c",
                    "mv tree tree.old; ln -s outside tree",
                    ";",
                    "-delete",
                ],
            );
            assert!(output.status.success(), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            assert!(
                tree.0.join("outside/victim").exists(),
                "explicit={explicit}, oracle={oracle}"
            );
            assert!(!tree.0.join("tree.old/victim").exists());
        }
    }
}

#[test]
fn physical_descent_refuses_a_start_replaced_by_a_symlink() {
    // #2: depth zero formerly permitted following, regardless of -P.
    for oracle in [true, false] {
        let tree = Tree::new(&format!("physical-{oracle}"));
        fs::create_dir(tree.0.join("tree")).unwrap();
        fs::create_dir(tree.0.join("outside")).unwrap();
        fs::write(tree.0.join("outside/victim"), b"x").unwrap();
        let output = tree.run(
            oracle,
            &[
                "tree",
                "-exec",
                "sh",
                "-c",
                "if [ \"$1\" = tree ]; then mv tree tree.old; ln -s outside tree; fi",
                "sh",
                "{}",
                ";",
                "-print",
            ],
        );
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert_eq!(output.stdout, b"tree\n");
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn physical_start_with_a_trailing_slash_still_follows_the_link() {
    // #2: no-follow descent must preserve the operand's explicit slash.
    let tree = Tree::new("trailing-slash");
    fs::create_dir(tree.0.join("outside")).unwrap();
    fs::write(tree.0.join("outside/victim"), b"x").unwrap();
    std::os::unix::fs::symlink("outside", tree.0.join("link")).unwrap();
    for args in [
        vec!["link/", "-print"],
        vec!["-H", "link", "-print"],
        vec!["-L", "link", "-print"],
    ] {
        let expected = tree.run(true, &args);
        let actual = tree.run(false, &args);
        assert_eq!(actual.status.code(), expected.status.code());
        assert_eq!(actual.stdout, expected.stdout);
        assert!(actual.stderr.is_empty(), "{actual:?}");
        assert_eq!(actual.stdout.iter().filter(|&&b| b == b'\n').count(), 2);
    }
}

#[test]
fn catalog_delete_retains_the_parent_for_explicit_directory_starts() {
    // #1 also applies to directory unlinkat and the catalog start path.
    for explicit in [false, true] {
        let tree = Tree::new(&format!("catalog-delete-{explicit}"));
        for directory in ["tree/victim", "outside/victim"] {
            fs::create_dir_all(tree.0.join(directory)).unwrap();
        }
        tree.index(&["tree"]);
        let output = tree.catalog(&[
            if explicit { "tree/victim" } else { "tree" },
            "-name",
            "victim",
            "-exec",
            "sh",
            "-c",
            "mv tree tree.old; ln -s outside tree",
            ";",
            "-delete",
        ]);
        assert!(output.status.success(), "{output:?}");
        assert!(tree.0.join("outside/victim").exists());
        assert!(!tree.0.join("tree.old/victim").exists());
    }
}

#[test]
fn failed_child_capture_is_an_output_error_and_reaps_the_child() {
    // #3: spill creation used to masquerade as ENOENT launching head, exit 0.
    let tree = Tree::new("capture-error");
    let output = tree
        .command(false)
        .env("TMPDIR", tree.0.join("nonexistent"))
        .args([
            "/dev/null",
            "-maxdepth",
            "0",
            "-exec",
            "head",
            "-c",
            "131072",
            "/dev/zero",
            ";",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("/dev/null"), "{error}");
    assert!(!error.contains("find: head:"), "{error}");
}

#[test]
fn catalog_resolution_stops_at_an_opaque_boundary_for_dotdot() {
    // #5: the catalog-mode path resolver used to collapse `..` lexically
    // even after crossing an ignored (opaque) directory, so it could walk
    // back out through a symlink the catalog never indexed and land on the
    // wrong inode. `tree/ignored/link` is a symlink into `outside/sub/deep`,
    // kept out of the index by `.ferretignore`; `../../visible` must
    // resolve against the symlink's real target (two levels up is
    // `outside`), not against the lexical `tree/ignored` prefix (two levels
    // up from which is the tree root).
    let tree = Tree::new("opaque-dotdot");
    fs::create_dir_all(tree.0.join("tree/ignored")).unwrap();
    fs::create_dir_all(tree.0.join("outside/sub/deep")).unwrap();
    fs::write(tree.0.join("tree/visible"), b"xxx").unwrap();
    fs::write(tree.0.join("outside/visible"), b"123456789").unwrap();
    std::os::unix::fs::symlink(
        tree.0.join("outside/sub/deep"),
        tree.0.join("tree/ignored/link"),
    )
    .unwrap();
    fs::write(tree.0.join("tree/.ferretignore"), b"ignored/\n").unwrap();
    tree.index(&["."]);

    let args = ["tree/ignored/link/../../visible", "-printf", "%s\n"];
    let expected = tree.run(true, &args);
    assert!(expected.status.success(), "{expected:?}");
    assert_eq!(expected.stdout, b"9\n", "GNU oracle: {expected:?}");

    let actual = tree.catalog(&args);
    assert!(actual.status.success(), "{actual:?}");
    assert_eq!(actual.stdout, expected.stdout, "catalog mode: {actual:?}");
}

#[test]
fn followed_dangling_reference_uses_the_link_itself() {
    // #8: `-L dangling -samefile dangling` must compare the link's own
    // identity, the same fallback live traversal already uses for a
    // dangling link it is asked to follow, rather than failing as though
    // the reference were outside the catalog or stale.
    let tree = Tree::new("dangling-samefile");
    std::os::unix::fs::symlink("missing", tree.0.join("dangling")).unwrap();
    tree.index(&["."]);

    let args = ["-L", "dangling", "-samefile", "dangling", "-print"];
    let expected = tree.run(true, &args);
    assert!(expected.status.success(), "{expected:?}");
    assert_eq!(expected.stdout, b"dangling\n", "GNU oracle: {expected:?}");

    let actual = tree.catalog(&args);
    assert!(actual.status.success(), "{actual:?}");
    assert!(actual.stderr.is_empty(), "{actual:?}");
    assert_eq!(actual.stdout, expected.stdout, "catalog mode: {actual:?}");
}

#[test]
fn deleting_an_explicit_start_counts_against_its_catalog_parent() {
    // #7: an explicit start operand's `Entry` retained `parent: None`, so
    // deleting it never incremented its catalog parent's removed-children
    // count - the same walk's own later `-empty` check on that parent
    // therefore disagreed with the deletion it had just performed.
    // `parent/child` is empty; deleting it as an explicit start should make
    // `parent` empty too, in the same `find` invocation.
    let tree = Tree::new("explicit-start-empty-accounting");
    fs::create_dir_all(tree.0.join("parent/child")).unwrap();
    tree.index(&["."]);

    let args = ["parent/child", "parent", "-empty", "-delete"];
    let expected = tree.run(true, &args);
    assert!(expected.status.success(), "{expected:?}");
    assert!(
        !tree.0.join("parent/child").exists(),
        "GNU oracle: {expected:?}"
    );
    assert!(!tree.0.join("parent").exists(), "GNU oracle: {expected:?}");

    let tree = Tree::new("explicit-start-empty-accounting-ferret");
    fs::create_dir_all(tree.0.join("parent/child")).unwrap();
    tree.index(&["."]);

    let actual = tree.catalog(&args);
    assert!(actual.status.success(), "{actual:?}");
    assert!(
        !tree.0.join("parent/child").exists(),
        "catalog mode: {actual:?}"
    );
    assert!(
        !tree.0.join("parent").exists(),
        "catalog mode must also remove the now-empty parent: {actual:?}"
    );
}

#[test]
fn live_descent_does_not_hold_one_fd_per_ancestor() {
    // #10: a live (non-catalog) directory listing kept its open fd in the
    // traversal's `Level` for the whole subtree beneath it - not just for
    // the listing itself - so a pure query's fd use grew with *depth*
    // rather than staying bounded by its own level. Under a 100-level
    // chain and RLIMIT_NOFILE=64, that aborted with EMFILE partway down,
    // before reaching `leaf`. A plain query (no -delete/-execdir) never
    // asks a child for its parent's fd, so the listing handle can close
    // once its own getdents pass finishes, the same way catalog mode
    // already never opens one at all for a pure query.
    let tree = Tree::new("fd-per-ancestor");
    let mut dir = tree.0.join("chain");
    fs::create_dir(&dir).unwrap();
    for _ in 0..100 {
        dir.push("x");
        fs::create_dir(&dir).unwrap();
    }
    fs::write(dir.join("leaf"), b"").unwrap();

    let output = support::fixture::command("sh", &tree.0)
        .arg("-c")
        .arg(r#"ulimit -n 64 && exec "$0" find -I chain -name leaf"#)
        .arg(FERRET)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .ends_with("leaf"),
        "{output:?}"
    );
}

#[test]
fn reference_observation_and_output_truncation_run_in_expression_order() {
    // #6: `-fprint`/`-fprintf` used to open (and truncate) their target at
    // parse time, unconditionally before any `-newer`-style reference test
    // observed its mtime - regardless of which came first in the
    // expression. `reference` and `-fprint reference` name the same file;
    // whether `-newer reference`'s observation sees its original mtime
    // (1000s) or the fresh mtime truncation gives it depends only on
    // expression order now.
    // `reference` starts far in the past; `candidate` is a few seconds
    // old - newer than the original `reference`, but older than "now",
    // which is what `reference` becomes the instant `-fprint` truncates
    // it. That is exactly what should flip `-newer reference`'s answer
    // depending on which runs first.
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    let new = std::time::SystemTime::now() - std::time::Duration::from_secs(5);
    let seed = |tree: &Tree| {
        fs::write(tree.0.join("reference"), b"ref").unwrap();
        fs::write(tree.0.join("candidate"), b"cand").unwrap();
        fs::File::open(tree.0.join("reference"))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
        fs::File::open(tree.0.join("candidate"))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(new))
            .unwrap();
    };

    for (at, args) in [
        vec!["candidate", "-newer", "reference", "-fprint", "reference"],
        vec!["candidate", "-fprint", "reference", "-newer", "reference"],
    ]
    .into_iter()
    .enumerate()
    {
        let gnu_tree = Tree::new(&format!("reference-output-order-gnu-{at}"));
        seed(&gnu_tree);
        let expected = gnu_tree.run(true, &args);
        assert!(expected.status.success(), "{expected:?}");

        let ferret_tree = Tree::new(&format!("reference-output-order-ferret-{at}"));
        seed(&ferret_tree);
        let actual = ferret_tree.run(false, &args);
        assert!(actual.status.success(), "{actual:?}");
        assert_eq!(actual.stdout, expected.stdout, "args={args:?}: {actual:?}");
        assert_eq!(
            fs::read(ferret_tree.0.join("reference")).unwrap(),
            fs::read(gnu_tree.0.join("reference")).unwrap(),
            "args={args:?}: ferret's reference content must match GNU's"
        );
    }
}
