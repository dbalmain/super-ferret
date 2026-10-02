//! Lane A CLI boundaries: inherited streams, actual stdin, PATH refusal and
//! GNU's observable batching. The engine oracle remains in ferret-query.
#![allow(clippy::unwrap_used)] // Fixture/process setup failures are test failures.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const GNU: &str = "/nix/store/i9wgqa0l88aprvpwfaq5hkfa6pklhlv0-findutils-4.11.0/bin/find";
const FERRET: &str = env!("CARGO_BIN_EXE_ferret");

struct Tree(PathBuf);
impl Tree {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("ferret-cli-m2a-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("d")).unwrap();
        Self(path)
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(FERRET);
        command
            .arg("find")
            .args(args)
            .current_dir(&self.0)
            .env("LC_ALL", "C")
            .env("TZ", "UTC");
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn process_stdout_is_interleaved_with_flushed_find_output() {
    let tree = Tree::new("flush");
    let output = tree.run(&[
        "-I",
        "d",
        "-maxdepth",
        "0",
        "-printf",
        "before",
        "-exec",
        "printf",
        "child",
        ";",
        "-printf",
        "after",
    ]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"beforechildafter");
    assert!(output.stderr.is_empty());
    let output = tree.run(&[
        "-I",
        "d",
        "-maxdepth",
        "0",
        "-exec",
        "sh",
        "-c",
        "echo child >&2",
        ";",
    ]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"child\n");
}

#[test]
fn ok_and_okdir_read_one_c_locale_answer_line() {
    let tree = Tree::new("ok");
    for primary in ["-ok", "-okdir"] {
        for (answer, accepted) in [
            ("Yup\n", true),
            ("yes please\n", true),
            (" yes\n", false),
            ("n\n", false),
            ("", false),
        ] {
            let mut child = tree
                .command(&["-I", "d", "-maxdepth", "0", primary, "echo", "{}", ";"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(answer.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert_eq!(output.status.code(), Some(0));
            assert_eq!(
                output.stdout,
                if accepted {
                    if primary == "-okdir" {
                        b"./d\n".as_slice()
                    } else {
                        b"d\n".as_slice()
                    }
                } else {
                    b"".as_slice()
                }
            );
            assert_eq!(output.stderr, b"< echo ... d > ? ");
        }
    }
}

#[test]
fn execdir_rejects_relative_path_even_in_an_unevaluated_branch() {
    let tree = Tree::new("path");
    for path in [".:/usr/bin", "/usr/bin:", "bin:/usr/bin", ""] {
        let output = tree
            .command(&["-I", "d", "-false", "-execdir", "echo", "{}", ";"])
            .env("PATH", path)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn informational_options_exit_zero_on_stdout_without_an_index_or_start() {
    let tree = Tree::new("help");
    for primary in ["-help", "--help", "-version", "--version"] {
        let output = tree.run(&["missing", primary, "-unknown"]);
        assert_eq!(output.status.code(), Some(0));
        assert!(!output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
    let output = tree.run(&["-D", "help"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(!output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn execdir_root_spelling_and_delete_dot_match_observed_gnu_rules() {
    let tree = Tree::new("roots");
    for (start, expected) in [
        (".", "./.\n"),
        ("..", "./..\n"),
        ("d/", "./d/\n"),
        ("d///", "./d/\n"),
        ("d/.", "./.\n"),
        ("/", "/\n"),
        ("///", "/\n"),
    ] {
        let output = tree.run(&["-I", start, "-maxdepth", "0", "-execdir", "echo", "{}", ";"]);
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(output.stdout, expected.as_bytes());
        assert!(output.stderr.is_empty());
    }
    let output = tree.run(&["-I", ".", "-delete"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(tree.0.exists());
    assert!(!tree.0.join("d").exists());
}

#[test]
#[ignore = "development oracle uses the machine-specific pinned GNU binary"]
fn exec_batch_sizes_against_pinned_gnu() {
    if !Path::new(GNU).exists() {
        return;
    }
    let tree = Tree::new("batches");
    for i in 0..1300 {
        fs::write(
            tree.0.join("d").join(format!("{i:05}{}", "x".repeat(225))),
            b"",
        )
        .unwrap();
    }
    let expression = [
        "d",
        "-type",
        "f",
        "-exec",
        "python3",
        "-c",
        "import sys; print(len(sys.argv)-1,sum(len(s)+1 for s in sys.argv[1:]))",
        "{}",
        "+",
    ];
    for stack in ["8192", "512", "256"] {
        for pad in [0, 50000] {
            let execute = |binary: &str, prefix: &[&str]| {
                let mut command = Command::new("sh");
                command
                    .args([
                        "-c",
                        "ulimit -s \"$1\"; shift; exec \"$@\"",
                        "batch",
                        stack,
                        binary,
                    ])
                    .args(prefix)
                    .args(expression)
                    .current_dir(&tree.0)
                    .env("LC_ALL", "C")
                    .env("TZ", "UTC")
                    .env("PAD", "x".repeat(pad));
                command.output().unwrap()
            };
            let expected = execute(GNU, &[]);
            let actual = execute(FERRET, &["find", "-I"]);
            assert_eq!(
                actual.status.code(),
                expected.status.code(),
                "stack {stack}, pad {pad}"
            );
            assert_eq!(actual.stdout, expected.stdout, "stack {stack}, pad {pad}");
            assert_eq!(actual.stderr.is_empty(), expected.stderr.is_empty());
        }
    }
}
