//! Real effects and filesystem fixtures; the ignored differential extension
//! compares the same tree, including tree mutations and output-file contents.
#![allow(clippy::unwrap_used)] // Test setup failures should point at the fixture.

use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::find::{Outcome, Plan};

const GNU: &str = "/nix/store/i9wgqa0l88aprvpwfaq5hkfa6pklhlv0-findutils-4.11.0/bin/find";
static NEXT: AtomicU64 = AtomicU64::new(0);

pub(in crate::find) struct Tree(pub PathBuf);
impl Tree {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ferret-m2a-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let tree = Self(path);
        tree.reset();
        tree
    }
    fn reset(&self) {
        let _ = fs::remove_dir_all(&self.0);
        fs::create_dir_all(self.0.join("dir/sub")).unwrap();
        fs::create_dir(self.0.join("empty")).unwrap();
        for name in [
            "a",
            "b",
            "m.py",
            "n.pyc",
            "dir/sub/file",
            "a+b",
            "space name",
        ] {
            fs::write(self.0.join(name), b"abc").unwrap();
        }
        symlink("dir", self.0.join("link")).unwrap();
        symlink("absent", self.0.join("broken")).unwrap();
    }
    pub fn args(&self, expression: &[&str]) -> Vec<OsString> {
        std::iter::once(OsString::from("-I"))
            .chain(std::iter::once(self.0.clone().into_os_string()))
            .chain(expression.iter().map(OsString::from))
            .collect()
    }
    pub fn run(&self, expression: &[&str]) -> (Outcome, Output) {
        run(&self.args(expression))
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
pub(in crate::find) struct Output {
    pub bytes: Vec<u8>,
    pub diagnostics: Vec<String>,
    pub batches: Vec<Vec<OsString>>,
    pub flushes: usize,
    pub answer: bool,
}
impl Effects for Output {
    fn print(&mut self, path: &Path, nul: bool) -> io::Result<()> {
        self.bytes.extend_from_slice(path.as_os_str().as_bytes());
        self.bytes.push(if nul { 0 } else { b'\n' });
        Ok(())
    }
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    fn error(&mut self, error: &WalkError) {
        self.diagnostics
            .push(format!("{}: {}", error.path.display(), error.error));
    }
    fn warning(&mut self, message: &str) {
        self.diagnostics.push(message.to_owned());
    }
    fn flush(&mut self) -> io::Result<()> {
        self.flushes += 1;
        Ok(())
    }
    fn command(&mut self, command: &mut Command) -> io::Result<bool> {
        self.batches
            .push(command.get_args().map(OsStr::to_owned).collect());
        let output = command.output()?;
        self.bytes.extend_from_slice(&output.stdout);
        if !output.stderr.is_empty() {
            self.diagnostics
                .push(String::from_utf8_lossy(&output.stderr).into_owned());
        }
        Ok(output.status.success())
    }
    fn confirm(&mut self, _: &OsStr, _: &Path) -> io::Result<bool> {
        self.diagnostics.push("prompt".into());
        Ok(self.answer)
    }
}

pub(in crate::find) fn run(args: &[OsString]) -> (Outcome, Output) {
    let plan = Plan::parse(args).unwrap();
    let mut output = Output::default();
    let outcome = plan.run(&mut plan.live_source(), &mut output).unwrap();
    (outcome, output)
}

#[test]
fn exec_substitution_truth_and_spawn_errors_use_the_real_process_path() {
    let tree = Tree::new();
    let (outcome, output) = tree.run(&[
        "-maxdepth",
        "0",
        "-exec",
        "printf",
        "x{}y:{}",
        "{}",
        ";",
        "-print",
    ]);
    assert_eq!(outcome.errors, 0);
    let path = tree.0.to_string_lossy();
    assert_eq!(output.bytes, format!("x{path}y:{path}{path}\n").as_bytes());
    assert_eq!(output.flushes, 1);
    let (outcome, output) = tree.run(&["-maxdepth", "0", "-exec", "false", ";", "-o", "-print"]);
    assert_eq!(outcome.errors, 0);
    assert!(!output.bytes.is_empty());
    let (outcome, output) = tree.run(&[
        "-maxdepth",
        "0",
        "-exec",
        "ferret-absent-command",
        ";",
        "-o",
        "-print",
    ]);
    assert_eq!(outcome.errors, 0);
    assert_eq!(output.diagnostics.len(), 1);
    assert!(!output.bytes.is_empty());
    let (outcome, _) = tree.run(&["-maxdepth", "0", "-exec", "false", "{}", "+", "-print"]);
    assert_eq!(outcome.errors, 1);
}

#[test]
fn batches_flush_at_quit_and_directory_boundaries_and_match_128k_limit() {
    let tree = Tree::new();
    let (_, output) = tree.run(&["-exec", "echo", "{}", "+", "-quit"]);
    assert_eq!(output.batches.len(), 1);
    assert_eq!(output.batches[0].len(), 1);
    let (_, output) = tree.run(&["-execdir", "echo", "{}", "+"]);
    assert!(output.batches.iter().any(|args| args
        == &[OsString::from(format!(
            "./{}",
            tree.0.file_name().unwrap().to_string_lossy()
        ))]));
    assert!(
        output
            .batches
            .iter()
            .any(|args| args.contains(&OsString::from("./file")))
    );
    // String bytes rather than argv-pointer bytes determine GNU's batches.
    for i in 0..1100 {
        fs::write(tree.0.join(format!("{i:05}{}", "x".repeat(220))), b"").unwrap();
    }
    let (_, output) = tree.run(&["-type", "f", "-exec", "true", "{}", "+"]);
    assert!(output.batches.len() >= 2);
    for args in &output.batches {
        assert!(
            5 + args
                .iter()
                .map(|arg| arg.as_bytes().len() + 1)
                .sum::<usize>()
                <= 131072
        );
    }
}

#[test]
fn delete_failure_is_false_and_removing_a_directory_before_descent_faults() {
    let tree = Tree::new();
    let (outcome, output) = tree.run(&["-name", "dir", "-delete", "-o", "-print"]);
    assert_eq!(outcome.errors, 1);
    assert!(output.bytes.windows(4).any(|bytes| bytes == b"/dir"));
    let (outcome, _) = tree.run(&["-name", "dir", "-exec", "rm", "-rf", "{}", ";"]);
    assert_eq!(outcome.errors, 1);
    assert!(!tree.0.join("dir").exists());
    let (outcome, _) = tree.run(&["-delete"]);
    assert_eq!(outcome.errors, 0);
    assert!(!tree.0.exists());
    assert!(Plan::parse(&["-prune", "-delete"].map(OsString::from)).is_err());
    assert!(Plan::parse(&["-prune", "-delete", "-depth"].map(OsString::from)).is_ok());
}

#[test]
fn symlink_following_and_cycles_go_through_entry_metadata() {
    let tree = Tree::new();
    let (outcome, output) = tree.run(&["-follow", "-type", "l"]);
    assert_eq!(outcome.errors, 0);
    assert_eq!(
        output.bytes,
        format!("{}/broken\n", tree.0.display()).as_bytes()
    );
    let (_, output) = tree.run(&["-follow", "-xtype", "l"]);
    assert_eq!(
        output
            .bytes
            .split(|&b| b == b'\n')
            .filter(|line| !line.is_empty())
            .count(),
        2
    );
    let (_, output) = tree.run(&["-follow", "-lname", "*"]);
    assert_eq!(
        output.bytes,
        format!("{}/broken\n", tree.0.display()).as_bytes()
    );
    let args = [
        OsString::from("-I"),
        OsString::from("-H"),
        tree.0.join("link").into_os_string(),
        OsString::from("-type"),
        OsString::from("f"),
    ];
    let (_, output) = run(&args);
    assert!(!output.bytes.is_empty());
    symlink("..", tree.0.join("dir/cycle")).unwrap();
    let (outcome, output) = tree.run(&["-follow"]);
    assert!(outcome.errors > 0);
    assert!(output.diagnostics.iter().any(|text| text.contains("loop")));
    symlink("self", tree.0.join("self")).unwrap();
    let args = [
        "-I".into(),
        "-L".into(),
        tree.0.join("self").into_os_string(),
    ];
    let (outcome, output) = run(&args);
    assert_eq!(outcome.errors, 1);
    assert!(output.bytes.is_empty());
}

#[test]
fn output_files_open_at_parse_time_and_same_name_shares_one_stream() {
    let tree = Tree::new();
    let path = tree.0.join("output");
    fs::write(&path, b"old").unwrap();
    let args = [
        "-I".into(),
        tree.0.clone().into_os_string(),
        "-false".into(),
        "-fprint".into(),
        path.clone().into_os_string(),
    ];
    let plan = Plan::parse(&args).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"");
    drop(plan);
    let args = [
        "-I".into(),
        tree.0.clone().into_os_string(),
        "-maxdepth".into(),
        "0".into(),
        "-fprintf".into(),
        path.clone().into_os_string(),
        "a".into(),
        ",".into(),
        "-fprintf".into(),
        path.clone().into_os_string(),
        "b".into(),
    ];
    run(&args);
    assert_eq!(fs::read(&path).unwrap(), b"ab");
}

#[test]
fn nonexecutable_command_and_confirmation_do_not_change_find_status() {
    let tree = Tree::new();
    let command = tree.0.join("a");
    fs::set_permissions(&command, fs::Permissions::from_mode(0o644)).unwrap();
    let args = [
        "-I".into(),
        tree.0.clone().into_os_string(),
        "-maxdepth".into(),
        "0".into(),
        "-exec".into(),
        command.into_os_string(),
        ";".into(),
    ];
    let (outcome, output) = run(&args);
    assert_eq!(outcome.errors, 0);
    assert_eq!(output.diagnostics.len(), 1);
    let plan = Plan::parse(&tree.args(&[
        "-maxdepth",
        "0",
        "-okdir",
        "echo",
        "{}",
        ";",
        "-o",
        "-print",
    ]))
    .unwrap();
    for answer in [false, true] {
        let mut output = Output {
            answer,
            ..Output::default()
        };
        let outcome = plan.run(&mut plan.live_source(), &mut output).unwrap();
        assert_eq!(outcome.errors, 0);
        assert_eq!(output.batches.len(), usize::from(answer));
        assert_eq!(output.diagnostics, ["prompt"]);
    }
}

fn records(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut rows = bytes
        .split(|&b| b == b'\n')
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    rows.sort();
    rows
}

pub(in crate::find) fn differential() {
    if !Path::new(GNU).exists() {
        return;
    }
    let tree = Tree::new();
    let expressions: &[&[&str]] = &[
        &["-exec", "echo", "x{}y", ";"],
        &["-exec", "echo", "{}", "+"],
        &["-exec", "false", ";", "-o", "-print"],
        &["-exec", "false", "{}", "+", "-print"],
        &["-exec", "ferret-absent-command", ";", "-o", "-print"],
        &["-exec", "echo", "{}", "+", "-quit"],
        &["-print", "-exec", "echo", "{}", ";"],
        &["-execdir", "echo", "{}", ";"],
        &["-execdir", "echo", "{}", "+"],
        &["-regex", r".*\.pyc?"],
        &["-iregex", r".*\.PYC?"],
        &["-regex", r".*\.\(py\|pyc\)"],
        &["-regextype", "posix-extended", "-regex", r".*\.(py|pyc)"],
        &["-regextype", "posix-basic", "-regex", r".*/a+b"],
        &["-follow", "-type", "l"],
        &["-follow", "-xtype", "l"],
        &["-follow", "-lname", "*"],
        &["-ilname", "DIR"],
        &[
            "-printf",
            "%p|%f|%P|%h|%H|%y|%Y|%l|%s|%m|%M|%b|%k|%S|%i|%D|%u|%g|%U|%G|%n|%d|%F\n",
        ],
        &["-printf", r"%10s|%-10s|%010s|%#m|%06m|%.2s|%L|\q|\012|\0\n"],
        &[
            "-printf",
            "%TY|%Tm|%Td|%TH|%TM|%Tb|%Tz|%Tc|%TF|%Te|%AY|%Am\n",
        ],
        &["-ls"],
        &["-delete", "-print"],
        &["-name", "dir", "-delete", "-o", "-print"],
        &["-name", "dir", "-exec", "rm", "-rf", "{}", ";"],
    ];
    for expression in expressions {
        // Nonmutating expressions share the actual inodes and times.
        let mut gnu = Command::new(GNU);
        gnu.arg(&tree.0)
            .args(*expression)
            .env("LC_ALL", "C")
            .env("TZ", "UTC");
        let destructive = expression.contains(&"-delete") || expression.contains(&"rm");
        let output = gnu.output().unwrap();
        let expected_tree = if destructive {
            Some(snapshot(&tree.0))
        } else {
            None
        };
        if destructive {
            tree.reset();
        }
        let (outcome, actual) = tree.run(expression);
        assert_eq!(
            i32::from(outcome.errors != 0),
            output.status.code().unwrap(),
            "status {expression:?}"
        );
        assert_eq!(
            records(&actual.bytes),
            records(&output.stdout),
            "stdout {expression:?}"
        );
        assert_eq!(
            actual.diagnostics.is_empty(),
            output.stderr.is_empty(),
            "stderr {expression:?}"
        );
        if let Some(expected) = expected_tree {
            assert_eq!(snapshot(&tree.0), expected, "tree {expression:?}");
            tree.reset();
        }
    }
    eprintln!("lane A differential: {} expressions", expressions.len());
}

fn snapshot(root: &Path) -> Vec<PathBuf> {
    fn visit(path: &Path, root: &Path, paths: &mut Vec<PathBuf>) {
        if let Ok(stat) = fs::symlink_metadata(path) {
            paths.push(path.strip_prefix(root).unwrap().to_owned());
            if stat.is_dir() {
                for child in fs::read_dir(path).unwrap() {
                    visit(&child.unwrap().path(), root, paths);
                }
            }
        }
    }
    let mut paths = Vec::new();
    visit(root, root, &mut paths);
    paths.sort();
    paths
}
