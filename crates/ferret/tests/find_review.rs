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
