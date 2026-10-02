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
fn a_child_runs_after_every_earlier_record_even_past_the_stdout_buffer() {
    // Regression guard for the 64 KiB stdout buffer (m3b): output is flushed
    // before its child, even when other workers interleave whole records and
    // the walk's output far exceeds the buffer.
    let tree = Tree::new("alternate");
    let name = "n".repeat(200);
    for index in 0..1000 {
        fs::write(tree.0.join("d").join(format!("{name}{index}")), b"").unwrap();
    }
    let output = tree.run(&[
        "-I", "d", "-type", "f", "-print", "-exec", "echo", "child", "{}", ";",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let lines: Vec<_> = output.stdout.split(|&b| b == b'\n').collect();
    assert_eq!(lines.len(), 2001);
    let mut printed = std::collections::HashSet::new();
    let mut children = 0;
    for line in &lines[..2000] {
        if let Some(path) = line.strip_prefix(b"child ") {
            assert!(printed.contains(path), "child ran before its own record");
            children += 1;
        } else {
            assert!(line.starts_with(b"d/n"));
            assert!(printed.insert(*line));
        }
    }
    assert_eq!(printed.len(), 1000);
    assert_eq!(children, 1000);
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
                Some(0),
                "stack {stack}, pad {pad}: {:?}",
                actual.stderr
            );
            // Shared batches keep GNU's 128 KiB string budget. Constrained
            // stacks also reserve Linux's argv-pointer space. Argument order is
            // free.
            let totals = |bytes: &[u8]| {
                std::str::from_utf8(bytes).unwrap().lines().fold(
                    (0usize, 0usize),
                    |(count, size), line| {
                        let fields: Vec<usize> = line
                            .split_whitespace()
                            .map(|field| field.parse().unwrap())
                            .collect();
                        assert_eq!(fields.len(), 2);
                        assert!(fields[1] <= 128 * 1024);
                        (count + fields[0], size + fields[1])
                    },
                )
            };
            assert_eq!(
                totals(&actual.stdout),
                (1300, 1300 * 233),
                "stack {stack}, pad {pad}"
            );
            assert!(actual.stderr.is_empty());
            if expected.status.success() {
                assert_eq!(totals(&actual.stdout), totals(&expected.stdout));
            } else {
                // GNU's larger batch can exceed the kernel's argument limit
                // at a low stack limit; reserving pointer space avoids E2BIG.
                assert!(!expected.stderr.is_empty());
            }
        }
    }
}

#[test]
fn interactive_commands_have_closed_stdin_after_the_answer() {
    let tree = Tree::new("closed-stdin");
    let mut child = tree
        .command(&[
            "-I",
            "d",
            "-maxdepth",
            "0",
            "-ok",
            "cat",
            ";",
            "-o",
            "-print",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"y\nremaining\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"d\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("Bad file descriptor"));
}

mod regex_cases {
    include!("support/regex_cases.rs");
}

#[test]
#[ignore = "development oracle uses the machine-specific pinned GNU binary"]
fn regex_dialects_against_pinned_gnu() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    if !Path::new(GNU).exists() {
        return;
    }
    let tree = Tree::new("regex-m3a");
    for name in regex_cases::NAMES {
        fs::write(tree.0.join(name), b"").unwrap();
    }
    fs::create_dir_all(tree.0.join("same/same")).unwrap();
    for name in [b"\xff".as_slice(), b"\xff\xff", b"\xc0\xe0"] {
        fs::write(tree.0.join(OsStr::from_bytes(name)), b"").unwrap();
    }
    let records = |bytes: &[u8]| {
        let mut records = bytes
            .split(|byte| *byte == 0)
            .map(<[u8]>::to_vec)
            .collect::<Vec<_>>();
        records.sort();
        records
    };
    let cases = regex_cases::cases();
    for (dialect, pattern, fold) in &cases {
        let args = [
            ".",
            "-regextype",
            dialect,
            if *fold { "-iregex" } else { "-regex" },
            pattern,
            "-print0",
        ];
        let expected = Command::new(GNU)
            .args(args)
            .current_dir(&tree.0)
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        let actual = tree.command(&["-I"]).args(args).output().unwrap();
        assert_eq!(
            actual.status.code(),
            expected.status.code(),
            "status {dialect} {pattern:?}: {:?}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            records(&actual.stdout),
            records(&expected.stdout),
            "stdout {dialect} {pattern:?}"
        );
        assert_eq!(
            actual.stderr.is_empty(),
            expected.stderr.is_empty(),
            "stderr {dialect} {pattern:?}"
        );
    }
    eprintln!(
        "milestone 3a CLI regex differential: {} expressions",
        cases.len()
    );
}

#[test]
fn backreference_budget_failure_reports_an_error_and_the_walk_continues() {
    let tree = Tree::new("regex-budget");
    let long_name = "a".repeat(32);
    fs::write(tree.0.join(&long_name), b"").unwrap();
    fs::write(tree.0.join("after"), b"").unwrap();
    let output = tree.run(&[
        "-I",
        ".",
        "-regextype",
        "posix-extended",
        "-regex",
        r".*/(a|aa)*\1b",
        ",",
        "-print0",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("matching budget"));
    let records = output.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
    assert!(records.contains(&b"./after".as_slice()));
    // An evaluation error aborts this entry's expression, then the walk
    // continues.
    assert!(!records.contains(&format!("./{long_name}").as_bytes()));
}

#[test]
fn concurrent_print_printf_files_and_child_output_are_whole_records() {
    let tree = Tree::new("whole-records");
    for index in 0..64 {
        fs::write(tree.0.join("d").join(format!("f{index}")), b"").unwrap();
    }
    let format = format!("%p|{}|%p\n", "x".repeat(20000));
    for primary in ["-printf", "-fprintf"] {
        let mut args = vec!["-I", "d", "-type", "f", primary];
        if primary == "-fprintf" {
            args.push("records");
        }
        args.push(&format);
        let output = tree.run(&args);
        assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
        let bytes = if primary == "-fprintf" {
            fs::read(tree.0.join("records")).unwrap()
        } else {
            output.stdout
        };
        let records: Vec<_> = bytes
            .split(|&b| b == b'\n')
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(records.len(), 64);
        for record in records {
            let fields: Vec<_> = record.split(|&b| b == b'|').collect();
            assert_eq!(fields.len(), 3);
            assert_eq!(fields[0], fields[2]);
            assert_eq!(fields[1], vec![b'x'; 20000]);
        }
    }
    let output = tree.run(&[
        "-I",
        "d",
        "-type",
        "f",
        "-exec",
        "sh",
        "-c",
        "printf '%s:' \"$1\"; sleep 0.001; printf '%s\\n' \"$1\"",
        "sh",
        "{}",
        ";",
    ]);
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    let records: Vec<_> = output
        .stdout
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(records.len(), 64);
    for record in records {
        let fields: Vec<_> = record.split(|&b| b == b':').collect();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0], fields[1]);
    }
}

#[test]
fn concurrent_ok_and_okdir_prompts_are_serialized() {
    let tree = Tree::new("serial-prompts");
    for index in 0..64 {
        fs::write(tree.0.join("d").join(format!("f{index}")), b"").unwrap();
    }
    for primary in ["-ok", "-okdir"] {
        let mut child = tree
            .command(&[
                "-I", "d", "-type", "f", primary, "true", "{}", ";", "-print",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all("yes\n".repeat(64).as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(0));
        let prompts = std::str::from_utf8(&output.stderr).unwrap();
        let prompts: Vec<_> = prompts.split(" > ? ").collect();
        assert_eq!(prompts.len(), 65);
        assert_eq!(prompts[64], "");
        for prompt in &prompts[..64] {
            assert!(prompt.starts_with("< true ... d/f"));
            assert_eq!(prompt.matches('<').count(), 1);
        }
        assert_eq!(
            output
                .stdout
                .split(|&b| b == b'\n')
                .filter(|line| !line.is_empty())
                .count(),
            64
        );
    }
}

#[test]
fn catalog_empty_accounts_for_concurrent_deletion_and_ignored_children() {
    for ignored in [false, true] {
        let tree = Tree::new(if ignored {
            "delete-ignored"
        } else {
            "delete-all"
        });
        for branch in 0..32 {
            let path = tree.0.join("d").join(format!("b{branch}/one/two/three"));
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("file"), b"x").unwrap();
            if ignored {
                fs::create_dir(path.join(".git")).unwrap();
                fs::write(path.join(".git/keep"), b"x").unwrap();
            }
        }
        let index = tree.0.join("index");
        let mut command = Command::new(FERRET);
        let output = command
            .args(["--index"])
            .arg(&index)
            .arg("index")
            .arg(tree.0.join("d"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
        let output = tree
            .command(&[
                "d", "-depth", "(", "-type", "f", "-o", "-type", "d", "-empty", ")", "-delete",
            ])
            .env("FERRET_INDEX", &index)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
        assert_eq!(tree.0.join("d").exists(), ignored);
        if ignored {
            for branch in 0..32 {
                let path = tree.0.join("d").join(format!("b{branch}/one/two/three"));
                assert!(!path.join("file").exists());
                assert!(path.join(".git/keep").exists());
            }
        }
    }
}
