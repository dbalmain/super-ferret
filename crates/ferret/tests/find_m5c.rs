//! Common find idioms through the CLI and both real parallel entry sources.
#![allow(clippy::unwrap_used)] // Fixture and subprocess failures identify setup.

#[path = "../../../tests/support/gnu_find.rs"]
mod gnu;
mod support {
    #![allow(dead_code)] // This binary only uses `command`, not `bounded_command`.
    pub mod fixture;
}

use std::fs;
use std::path::PathBuf;
use std::process::Output;

const FERRET: &str = env!("CARGO_BIN_EXE_ferret");

struct Tree(PathBuf);
impl Tree {
    fn new(name: &str) -> Self {
        let tree =
            Self(std::env::temp_dir().join(format!("ferret-m5c-{}-{name}", std::process::id())));
        fs::create_dir_all(&tree.0).unwrap();
        for branch in 0..32 {
            let directory = tree.0.join(format!("root/b{branch}"));
            fs::create_dir_all(&directory).unwrap();
            for file in 0..4 {
                fs::write(
                    directory.join(format!("f{file}")),
                    format!("contents:root/b{branch}/f{file}\nend\n"),
                )
                .unwrap();
            }
        }
        tree.index();
        tree
    }
    fn index(&self) {
        let output = support::fixture::command(FERRET, &self.0)
            .args(["index", "root"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
    }
    fn run(&self, live: bool, args: &[&str]) -> Output {
        let mut command = support::fixture::command(FERRET, &self.0);
        command.arg("find");
        if live {
            command.arg("-I");
        }
        let output = command.args(args).output().unwrap();
        assert!(output.status.success(), "{:?}", output);
        assert!(output.stderr.is_empty(), "{:?}", output);
        output
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn exec_plus_uses_one_total_below_the_argument_limit() {
    // Worker-local batches produce partial totals or omit totals altogether.
    let tree = Tree::new("wc");
    for live in [false, true] {
        let output = tree.run(
            live,
            &["root", "-type", "f", "-exec", "wc", "-l", "{}", "+"],
        );
        let text = String::from_utf8(output.stdout).unwrap();
        let totals: Vec<_> = text
            .lines()
            .filter(|line| line.ends_with(" total"))
            .collect();
        assert_eq!(totals.len(), 1, "{text}");
        assert_eq!(totals[0].split_whitespace().next(), Some("256"));
        assert_eq!(text.lines().count(), 129);
    }
}

#[test]
fn every_print_header_stays_next_to_its_own_child_contents() {
    // Repeated real commands force races between print and child completion.
    let tree = Tree::new("cat");
    for live in [false, true] {
        for _ in 0..16 {
            let output = tree.run(
                live,
                &["root", "-type", "f", "-print", "-exec", "cat", "{}", ";"],
            );
            let text = String::from_utf8(output.stdout).unwrap();
            let lines: Vec<_> = text.lines().collect();
            assert_eq!(lines.len(), 128 * 3);
            let mut seen = std::collections::HashSet::new();
            for entry in lines.chunks_exact(3) {
                assert!(seen.insert(entry[0]));
                assert_eq!(entry[1], format!("contents:{}", entry[0]));
                assert_eq!(entry[2], "end");
            }
        }
    }
}

#[test]
fn quit_commits_exactly_one_matching_entry() {
    // Quit after donation must discard other workers' pending print records.
    let tree = Tree::new("quit");
    for live in [false, true] {
        for _ in 0..64 {
            let output = tree.run(live, &["root", "-type", "f", "-print", "-quit"]);
            let text = String::from_utf8(output.stdout).unwrap();
            let lines: Vec<_> = text.lines().collect();
            assert_eq!(lines.len(), 1, "{text}");
            assert!(tree.0.join(lines[0]).is_file());
        }
    }
}

#[test]
fn overlapping_starts_delete_with_gnus_status_and_empty_stderr() {
    // A ROOT walk must finish before SUB observes names already removed by it.
    for live in [false, true] {
        let tree = Tree::new(if live { "delete-live" } else { "delete-index" });
        let args = ["root", "root/b0", "-type", "f", "-delete"];
        let expected = gnu::output(gnu::command().args(args).current_dir(&tree.0));
        for branch in 0..32 {
            for file in 0..4 {
                fs::write(tree.0.join(format!("root/b{branch}/f{file}")), b"x").unwrap();
            }
        }
        tree.index();
        let actual = tree.run(live, &args);
        assert_eq!(actual.status.code(), expected.status.code());
        assert!(expected.stderr.is_empty(), "{:?}", expected);
        assert_eq!(actual.stdout, expected.stdout);
        assert!(
            fs::read_dir(tree.0.join("root/b0"))
                .unwrap()
                .next()
                .is_none()
        );
    }
}

#[test]
fn quit_in_an_early_start_never_reports_a_later_missing_start() {
    // GNU quits inside `root` and never reaches `missing`. A concurrent second
    // start would report ENOENT and exit 1 before the quit.
    let tree = Tree::new("quit-starts");
    let args = ["root", "missing", "-type", "f", "-print", "-quit"];
    let expected = gnu::output(gnu::command().args(args).current_dir(&tree.0));
    assert!(expected.status.success(), "{:?}", expected);
    assert!(expected.stderr.is_empty(), "{:?}", expected);
    for live in [false, true] {
        for _ in 0..16 {
            let output = tree.run(live, &args);
            assert_eq!(output.stdout.iter().filter(|&&b| b == b'\n').count(), 1);
        }
    }
}

#[test]
fn large_child_output_spills_and_keeps_the_entry_together() {
    let tree = Tree::new("spill");
    let body = vec![b'x'; 256 * 1024];
    for branch in 0..32 {
        fs::write(tree.0.join(format!("root/b{branch}/f0")), &body).unwrap();
    }
    tree.index();
    for live in [false, true] {
        let output = tree.run(
            live,
            &[
                "root",
                "-name",
                "f0",
                "-printf",
                "header:%p\n",
                "-exec",
                "cat",
                "{}",
                ";",
                "-printf",
                "\nend:%p\n",
            ],
        );
        let mut bytes = output.stdout.as_slice();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..32 {
            let at = bytes.iter().position(|&b| b == b'\n').unwrap();
            let path = bytes[..at].strip_prefix(b"header:").unwrap();
            assert!(seen.insert(path.to_vec()));
            bytes = &bytes[at + 1..];
            assert!(bytes[..body.len()] == body, "child body was interrupted");
            bytes = &bytes[body.len()..];
            let end = [b"\nend:".as_slice(), path, b"\n"].concat();
            assert!(bytes.starts_with(&end));
            bytes = &bytes[end.len()..];
        }
        assert!(bytes.is_empty());
    }
}

#[test]
fn quit_discards_later_output_and_finishes_started_commands() {
    // Running children must finish their external effects even when another
    // entry wins quit. Their captured stdout is discarded with their entry.
    for live in [false, true] {
        let tree = Tree::new(if live {
            "quit-child-live"
        } else {
            "quit-child-index"
        });
        let output = tree.run(live, &["root", "-maxdepth", "2", "-name", "f0", "-print", "-exec", "sh", "-c",
            "touch \"$1.started\"; sleep 0.05; touch \"$1.finished\"; printf 'contents:%s\\n' \"$1\"", "sh", "{}", ";", "-quit"]);
        let text = String::from_utf8(output.stdout).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert_eq!(lines[1], format!("contents:{}", lines[0]));
        let mut started = 0;
        for branch in 0..32 {
            let path = tree.0.join(format!("root/b{branch}/f0.started"));
            if path.exists() {
                started += 1;
                assert!(tree.0.join(format!("root/b{branch}/f0.finished")).exists());
            }
        }
        assert!(started > 0);
    }
}

#[test]
fn quit_keeps_file_output_for_the_same_winning_entry() {
    let tree = Tree::new("quit-files");
    for live in [false, true] {
        for _ in 0..16 {
            let output = tree.run(
                live,
                &[
                    "root", "-type", "f", "-print", "-fprintf", "records", "%p\n", "-quit",
                ],
            );
            assert_eq!(fs::read(tree.0.join("records")).unwrap(), output.stdout);
            assert_eq!(output.stdout.iter().filter(|&&b| b == b'\n').count(), 1);
        }
    }
}

#[test]
fn read_only_overlapping_and_repeated_starts_keep_gnus_duplicates() {
    let tree = Tree::new("duplicates");
    let args = ["root", "root/b0", "root/b0", "-type", "f", "-print"];
    let expected = gnu::output(gnu::command().args(args).current_dir(&tree.0));
    assert!(expected.status.success());
    let mut expected: Vec<_> = expected.stdout.split(|&b| b == b'\n').collect();
    expected.sort();
    for live in [false, true] {
        for _ in 0..8 {
            let output = tree.run(live, &args);
            let mut actual: Vec<_> = output.stdout.split(|&b| b == b'\n').collect();
            actual.sort();
            assert_eq!(actual, expected);
        }
    }
}

#[test]
fn staged_arguments_fill_shared_batches_and_flush_the_final_remainder() {
    let tree = Tree::new("large-batch");
    for branch in 0..32 {
        for file in 0..32 {
            fs::write(
                tree.0
                    .join(format!("root/b{branch}/large{file:02}{}", "x".repeat(210))),
                b"x\n",
            )
            .unwrap();
        }
    }
    tree.index();
    let args = [
        "root",
        "-name",
        "large*",
        "-exec",
        "sh",
        "-c",
        "printf 'batch\n'; printf '%s\n' \"$@\"",
        "sh",
        "{}",
        "+",
    ];
    let expected = gnu::output(gnu::command().args(args).current_dir(&tree.0));
    assert!(expected.status.success(), "{expected:?}");
    let expected = String::from_utf8(expected.stdout).unwrap();
    let expected_batches = expected.lines().filter(|&line| line == "batch").count();
    assert_eq!(expected_batches, 2);
    let mut expected_paths: Vec<_> = expected.lines().filter(|&line| line != "batch").collect();
    expected_paths.sort();
    for live in [false, true] {
        let output = tree.run(live, &args);
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            text.lines().filter(|&line| line == "batch").count(),
            expected_batches
        );
        let mut actual_paths: Vec<_> = text.lines().filter(|&line| line != "batch").collect();
        actual_paths.sort();
        assert_eq!(actual_paths, expected_paths);
    }
}
