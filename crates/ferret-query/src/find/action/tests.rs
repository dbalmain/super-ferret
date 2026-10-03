//! Real effects and filesystem fixtures; the ignored differential extension
//! compares the same tree, including tree mutations and output-file contents.

use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::find::{Outcome, Plan, gnu};

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
        let mut bytes = Vec::new();
        let result = self.capture(command, &mut bytes)?;
        self.write(&bytes)?;
        Ok(result)
    }
    fn capture(&mut self, command: &mut Command, sink: &mut dyn io::Write) -> io::Result<bool> {
        self.batches
            .push(command.get_args().map(OsStr::to_owned).collect());
        let output = command.output()?;
        sink.write_all(&output.stdout)?;
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
    assert_eq!(output.flushes, 3); // Spawn, entry commit, and task completion.
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
        let mut gnu = gnu::command();
        gnu.arg(&tree.0)
            .args(*expression)
            .env("LC_ALL", "C")
            .env("TZ", "UTC");
        let destructive = expression.contains(&"-delete") || expression.contains(&"rm");
        let output = gnu::output(&mut gnu);
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
    eprintln!(
        "lane A differential: {} expressions",
        expressions.len() + differential_extra(&tree) + differential_regex()
    );
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

fn compare(tree: &Tree, args: &[OsString]) {
    let expected = gnu::output(
        gnu::command()
            .args(&args[1..])
            .env("LC_ALL", "C")
            .env("TZ", "UTC"),
    );
    let Ok(plan) = Plan::parse(args) else {
        assert_eq!(expected.status.code(), Some(1), "status {args:?}");
        assert!(expected.stdout.is_empty(), "stdout {args:?}");
        assert!(!expected.stderr.is_empty(), "stderr {args:?}");
        return;
    };
    let mut output = Output::default();
    let outcome = plan.run(&mut plan.live_source(), &mut output).unwrap();
    assert_eq!(
        i32::from(outcome.errors != 0),
        expected.status.code().unwrap(),
        "status {args:?}: {:?}",
        output.diagnostics
    );
    assert_eq!(
        records(&output.bytes),
        records(&expected.stdout),
        "stdout {args:?}"
    );
    assert_eq!(
        output.diagnostics.is_empty(),
        expected.stderr.is_empty(),
        "stderr {args:?}"
    );
    assert!(tree.0.exists());
}

fn differential_extra(tree: &Tree) -> usize {
    use std::fs::{File, FileTimes};
    use std::process::Stdio;
    use std::time::{Duration, UNIX_EPOCH};
    let mut count = 0;
    let names = [
        "aa", "a+", "a?", "a{2}", "a^b", "a$b", "a(b)", "ab", "a|b", "an", "at", "a\\b", "\n",
    ];
    for name in names {
        fs::write(tree.0.join(name), b"").unwrap();
    }
    let byte_name = OsStr::from_bytes(b"\xff");
    fs::write(tree.0.join(byte_name), b"").unwrap();
    for dialect in [
        "emacs",
        "posix-basic",
        "grep",
        "sed",
        "posix-minimal-basic",
        "posix-extended",
        "posix-egrep",
        "egrep",
        "awk",
        "posix-awk",
        "gnu-awk",
    ] {
        for pattern in [
            r".*/a+",
            r".*/a\+",
            r".*/a{2}",
            r".*/a\{2\}",
            r".*/a^b",
            r".*/a$b",
            r".*/\(a\|b\)",
            r".*/(a|b)",
            r".*/[[:alpha:]]+",
            r".*/a\(b\)",
            r".*/a\?",
            r".*/a?",
            r".*/[a\b]+",
            r".*/a\w",
            r".*/.",
            r".*/a\n",
            r".*/a\t",
            r".*/[[.a.]-[.c.]]",
        ] {
            compare(
                tree,
                &tree.args(&["-regextype", dialect, "-regex", pattern]),
            );
            count += 1;
        }
    }
    for name in names {
        fs::remove_file(tree.0.join(name)).unwrap();
    }
    fs::remove_file(tree.0.join(byte_name)).unwrap();
    compare(
        tree,
        &tree.args(&[
            "-maxdepth",
            "0",
            "-printf",
            "%#S|%.3S|%+d|% d|%010d|%10.3d|%#6.4m|%T%|%-10%|%T",
        ]),
    );
    count += 1;
    for primary in ["-ls", "-printf"] {
        let mut args = vec!["-I".into(), "/dev/null".into(), primary.into()];
        if primary == "-printf" {
            args.push("%#S|%.3S|%+d|% d|%010d|%10.3d|%#6.4m\n".into());
        }
        compare(tree, &args);
        count += 1;
    }

    for dialect in [
        "posix-egrep",
        "egrep",
        "posix-awk",
        "awk",
        "sed",
        "grep",
        "posix-minimal-basic",
        "gnu-awk",
        "ed",
    ] {
        compare(
            tree,
            &tree.args(&["-regextype", dialect, "-regex", r".*/[[:alpha:]]+\..*"]),
        );
        count += 1;
    }
    for (flag, path) in [("-L", tree.0.clone()), ("-H", tree.0.join("link"))] {
        let args = [
            "-I".into(),
            flag.into(),
            path.into_os_string(),
            "-printf".into(),
            "%p %y %Y %l\n".into(),
        ];
        compare(tree, &args);
        count += 1;
    }
    let time = UNIX_EPOCH + Duration::new(946684800, 123456789);
    let path = tree.0.join("a");
    File::open(&path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(time).set_accessed(time))
        .unwrap();
    let args=["-I".into(),path.into_os_string(),"-printf".into(),"%T@|%A@|%C@|%T+|%TS|%Ts|%CT|%TT|%a|%c|%t|%Tc|%Tb|%TY|%Tm|%Td|%TH|%TM|%TF|%Te|%Tz|%AY|%Am\n".into()];
    compare(tree, &args);
    count += 1;
    for primary in ["-fprint", "-fprint0", "-fprintf", "-fls"] {
        let path = tree.0.join("output");
        fs::write(&path, b"old").unwrap();
        let mut args = tree.args(&[primary]);
        args.push(path.clone().into_os_string());
        if primary == "-fprintf" {
            args.push("%p %s\n".into());
        }
        let expected = gnu::output(
            gnu::command()
                .args(&args[1..])
                .env("LC_ALL", "C")
                .env("TZ", "UTC"),
        );
        let file = fs::read(&path).unwrap();
        fs::write(&path, b"old").unwrap();
        let (outcome, output) = run(&args);
        assert_eq!(
            i32::from(outcome.errors != 0),
            expected.status.code().unwrap(),
            "{primary}"
        );
        assert_eq!(output.bytes, expected.stdout, "{primary}");
        assert_eq!(
            output.diagnostics.is_empty(),
            expected.stderr.is_empty(),
            "{primary}"
        );
        assert_eq!(fs::read(&path).unwrap(), file, "output file {primary}");
        fs::remove_file(&path).unwrap();
        count += 1;
    }
    for primary in ["-ok", "-okdir"] {
        for answer in [false, true] {
            let path = tree.0.join("answer");
            fs::write(
                &path,
                if answer {
                    b"Yup\n".as_slice()
                } else {
                    b"no\n".as_slice()
                },
            )
            .unwrap();
            let args = tree.args(&["-maxdepth", "0", primary, "echo", "{}", ";", "-o", "-print"]);
            let expected = gnu::output(
                gnu::command()
                    .args(&args[1..])
                    .stdin(Stdio::from(File::open(&path).unwrap()))
                    .env("LC_ALL", "C")
                    .env("TZ", "UTC"),
            );
            let plan = Plan::parse(&args).unwrap();
            let mut output = Output {
                answer,
                ..Output::default()
            };
            let outcome = plan.run(&mut plan.live_source(), &mut output).unwrap();
            assert_eq!(
                i32::from(outcome.errors != 0),
                expected.status.code().unwrap()
            );
            assert_eq!(output.bytes, expected.stdout);
            assert!(!expected.stderr.is_empty());
            fs::remove_file(path).unwrap();
            count += 1;
        }
    }
    for flag in ["-help", "--help", "-version", "--version"] {
        let args = tree.args(&[flag, "-unknown"]);
        let expected = gnu::output(gnu::command().args(&args[1..]));
        let (outcome, output) = run(&args);
        assert_eq!(
            i32::from(outcome.errors != 0),
            expected.status.code().unwrap()
        );
        assert!(!output.bytes.is_empty());
        assert!(!expected.stdout.is_empty());
        assert!(output.diagnostics.is_empty());
        assert!(expected.stderr.is_empty());
        count += 1;
    }
    for flag in [
        "exec", "opt", "rates", "search", "stat", "time", "tree", "all", "unknown",
    ] {
        let args = [
            "-I".into(),
            "-D".into(),
            flag.into(),
            tree.0.clone().into_os_string(),
            "-maxdepth".into(),
            "0".into(),
        ];
        compare(tree, &args);
        count += 1;
    }
    let args = ["-I".into(), "-D".into(), "help".into()];
    let (outcome, output) = run(&args);
    assert_eq!(outcome.errors, 0);
    assert!(!output.bytes.is_empty());
    assert!(output.diagnostics.is_empty());
    count += 1;
    let args = tree.args(&[
        "-maxdepth",
        "1",
        "-execdir",
        "sh",
        "-c",
        r#"rm -rf "$@""#,
        "remove",
        "{}",
        "+",
        "-print",
    ]);
    let expected = gnu::output(
        gnu::command()
            .args(&args[1..])
            .env("LC_ALL", "C")
            .env("TZ", "UTC"),
    );
    let expected_tree = snapshot(&tree.0);
    tree.reset();
    let (outcome, output) = run(&args);
    assert_eq!(
        i32::from(outcome.errors != 0),
        expected.status.code().unwrap()
    );
    assert_eq!(records(&output.bytes), records(&expected.stdout));
    assert_eq!(output.diagnostics.is_empty(), expected.stderr.is_empty());
    assert_eq!(snapshot(&tree.0), expected_tree);
    tree.reset();
    count += 1;
    count
}

#[test]
fn failed_unlink_is_false_and_does_not_stop_other_entries() {
    let tree = Tree::new();
    fs::set_permissions(&tree.0, fs::Permissions::from_mode(0o555)).unwrap();
    let (outcome, output) = tree.run(&["-name", "a", "-delete", "-o", "-print"]);
    fs::set_permissions(&tree.0, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(outcome.errors, 1);
    assert!(tree.0.join("a").exists());
    assert!(output.bytes.windows(3).any(|bytes| bytes == b"/a\n"));
    assert!(output.bytes.windows(3).any(|bytes| bytes == b"/b\n"));
}

#[test]
fn following_detects_stat_errors_even_when_the_expression_needs_no_metadata() {
    let tree = Tree::new();
    symlink("self", tree.0.join("self")).unwrap();
    let (outcome, output) = tree.run(&["-follow", "-maxdepth", "1", "-name", "never"]);
    assert_eq!(outcome.errors, 1);
    assert!(output.bytes.is_empty());
    let (outcome, output) = tree.run(&["-name", "self", "-xtype", "l"]);
    assert_eq!(outcome.errors, 0);
    assert_eq!(
        output.bytes,
        format!("{}/self\n", tree.0.display()).as_bytes()
    );
}

#[test]
fn execdir_caches_names_before_a_batch_unlinks_its_directory() {
    let tree = Tree::new();
    let (outcome, output) = tree.run(&[
        "-maxdepth",
        "1",
        "-execdir",
        "sh",
        "-c",
        r#"rm -rf "$@""#,
        "remove",
        "{}",
        "+",
        "-print",
    ]);
    assert!(outcome.errors > 0);
    assert!(!tree.0.exists());
    assert!(output.bytes.windows(3).any(|bytes| bytes == b"/a\n"));
    assert!(output.bytes.windows(3).any(|bytes| bytes == b"/b\n"));
    assert_eq!(output.batches.len(), 2);
}

mod regex_cases {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../ferret/tests/support/regex_cases.rs"
    ));
}

fn differential_regex() -> usize {
    let tree = Tree::new();
    for name in regex_cases::NAMES {
        fs::write(tree.0.join(name), b"").unwrap();
    }
    fs::create_dir_all(tree.0.join("same/same")).unwrap();
    for name in [b"\xff".as_slice(), b"\xff\xff", b"\xc0\xe0"] {
        fs::write(tree.0.join(OsStr::from_bytes(name)), b"").unwrap();
    }
    let cases = regex_cases::cases();
    for (dialect, pattern, fold) in &cases {
        compare(
            &tree,
            &tree.args(&[
                "-regextype",
                dialect,
                if *fold { "-iregex" } else { "-regex" },
                pattern,
            ]),
        );
    }
    eprintln!(
        "milestone 3a regex differential: {} expressions",
        cases.len()
    );
    cases.len()
}

#[test]
fn full_batch_releases_collection_lock_before_running_child() {
    struct CollectWhileRunning {
        state: State,
        exec: Exec,
        next: Option<Entry>,
        output: Output,
    }
    impl Effects for CollectWhileRunning {
        fn print(&mut self, path: &Path, nul: bool) -> io::Result<()> {
            self.output.print(path, nul)
        }
        fn error(&mut self, error: &WalkError) {
            self.output.error(error);
        }
        fn capture(&mut self, command: &mut Command, sink: &mut dyn Write) -> io::Result<bool> {
            // This would fail immediately if child execution retained the lock.
            assert!(self.state.shared.try_lock().is_ok());
            if let Some(next) = self.next.take() {
                execute(&self.exec, &next, &mut self.output, &mut self.state)?;
                self.state.flush(&mut self.output, false)?;
            }
            self.output.capture(command, sink)
        }
    }
    let tree = Tree::new();
    let exec = Exec {
        id: 0,
        args: vec!["true".into()],
        batch: true,
        directory: false,
        prompt: false,
    };
    let paths: Vec<_> = ["a", "b", "c", "d"].map(|name| tree.0.join(name)).into();
    let limit = Budget {
        strings: command_bytes(&exec.args) + 2 * (paths[0].as_os_str().len() + 1),
        kernel: usize::MAX,
    };
    let mut state = State {
        limit: Some(limit),
        ..State::default()
    };
    let mut effects = CollectWhileRunning {
        state: State {
            shared: state.shared.clone(),
            limit: Some(limit),
            ..State::default()
        },
        exec: exec.clone(),
        next: Some(Entry::new(paths[3].clone(), 0, FileKind::File)),
        output: Output::default(),
    };
    for path in &paths[..3] {
        execute(
            &exec,
            &Entry::new(path.clone(), 0, FileKind::File),
            &mut effects,
            &mut state,
        )
        .unwrap();
    }
    state.flush(&mut effects, false).unwrap();
    assert_eq!(
        flush_shared(&state.shared, &mut effects, &state.gate, &state.quit).unwrap(),
        0
    );
    assert_eq!(
        effects.output.batches,
        vec![
            paths[..2]
                .iter()
                .map(|p| p.as_os_str().to_owned())
                .collect::<Vec<_>>(),
            paths[2..]
                .iter()
                .map(|p| p.as_os_str().to_owned())
                .collect::<Vec<_>>(),
        ]
    );
    assert_eq!(state.errors + effects.state.errors, 0);
}

#[test]
fn batch_commit_shares_the_entry_record_gate() {
    // #4: an entry's buffered record and a batch's output used to commit
    // through separate mechanisms, so one worker's batch write could land in
    // the middle of another worker's entry record. Drive both sides by hand
    // through a deterministic handshake (no sleeps) and check that the
    // batch's write cannot land until the entry's commit has released the
    // gate, even though the batch finishes capturing its own output first.
    use crate::find::output::{EntryEffects, Record};
    use std::sync::atomic::AtomicBool as StdAtomicBool;
    use std::sync::mpsc::sync_channel;
    use std::time::Duration;

    #[derive(Clone)]
    struct Handshake {
        bytes: Arc<Mutex<Vec<u8>>>,
        entry_started: std::sync::mpsc::SyncSender<()>,
        await_entry_started: Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
        release_entry: std::sync::mpsc::SyncSender<()>,
        await_release_entry: Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
        triggered: Arc<StdAtomicBool>,
    }
    impl Effects for Handshake {
        fn print(&mut self, _: &Path, _: bool) -> io::Result<()> {
            Ok(())
        }
        fn error(&mut self, error: &WalkError) {
            panic!("{error:?}");
        }
        fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
            if bytes.first() == Some(&b'A') && !self.triggered.swap(true, Ordering::SeqCst) {
                self.entry_started.send(()).map_err(io::Error::other)?;
                self.await_release_entry
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(io::Error::other)?;
            }
            self.bytes.lock().unwrap().extend_from_slice(bytes);
            Ok(())
        }
        fn capture(&mut self, _: &mut Command, sink: &mut dyn Write) -> io::Result<bool> {
            // The batch captures its own output, and only afterwards tries to
            // commit it - proving capture happens before the gate is taken.
            self.await_entry_started
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .map_err(io::Error::other)?;
            sink.write_all(b"B")?;
            self.release_entry.send(()).map_err(io::Error::other)?;
            Ok(true)
        }
    }

    let gate = Arc::new(Mutex::new(()));
    let quit = Arc::new(StdAtomicBool::new(false));
    let (entry_started, await_entry_started) = sync_channel(1);
    let (release_entry, await_release_entry) = sync_channel(1);
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let host = Handshake {
        bytes: bytes.clone(),
        entry_started,
        await_entry_started: Arc::new(Mutex::new(await_entry_started)),
        release_entry,
        await_release_entry: Arc::new(Mutex::new(await_release_entry)),
        triggered: Arc::default(),
    };
    let a_len = 200 * 1024; // exceeds OutputBuffer's 64 KiB memory limit

    std::thread::scope(|scope| {
        let (gate1, quit1, mut host1) = (gate.clone(), quit.clone(), host.clone());
        let entry = scope.spawn(move || {
            let mut record = Record::default();
            let mut output = EntryEffects {
                host: &mut host1,
                record: &mut record,
                gate: &gate1,
                quit: &quit1,
            };
            Effects::write(&mut output, &vec![b'A'; a_len]).unwrap();
            output.commit(false).unwrap();
        });
        let (gate2, quit2, mut host2) = (gate.clone(), quit.clone(), host.clone());
        let batch = scope.spawn(move || {
            let args = [OsString::from("batch-output")];
            spawn_batch(&args, None, None, &mut host2, &gate2, &quit2).unwrap()
        });
        entry.join().unwrap();
        assert!(batch.join().unwrap());
    });

    let bytes = bytes.lock().unwrap();
    let at = bytes.iter().position(|&b| b == b'B').unwrap();
    assert_eq!(at, a_len, "the batch's write split the entry's record");
    assert_eq!(bytes.len(), at + 1);
}
