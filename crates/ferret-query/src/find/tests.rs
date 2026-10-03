//! Real temp trees drive the parser, evaluator and traversal together. The
//! ignored oracle suite is clean-room: only the pinned binary is consulted.

// Helpers build real fixtures; a setup failure should panic with its location.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;

mod expressions;

struct Tree(PathBuf);

impl Tree {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("ferret-find-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("dir/sub")).unwrap();
        fs::create_dir(path.join("empty")).unwrap();
        for name in [
            "a.c",
            "b.txt",
            ".hidden",
            "ABC",
            "a1",
            "[",
            "dir/file",
            "dir/sub/deep.c",
            "back\\slash",
        ] {
            fs::write(path.join(name), b"").unwrap();
        }
        fs::write(path.join(OsStr::from_bytes(b"nonutf8-\xff")), b"").unwrap();
        symlink("dir", path.join("link")).unwrap();
        symlink("missing", path.join("broken")).unwrap();
        Self(path)
    }

    fn args(&self, expression: &[&str]) -> Vec<OsString> {
        [OsString::from("-I"), self.0.as_os_str().to_owned()]
            .into_iter()
            .chain(expression.iter().map(OsString::from))
            .collect()
    }

    fn run(&self, expression: &[&str]) -> (Outcome, Output) {
        run(&self.args(expression))
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::set_permissions(self.0.join("denied"), fs::Permissions::from_mode(0o700));
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    errors: Vec<PathBuf>,
    remove: Option<PathBuf>,
}

impl Effects for Output {
    fn print(&mut self, path: &Path, nul: bool) -> io::Result<()> {
        self.bytes.extend_from_slice(path.as_os_str().as_bytes());
        self.bytes.push(if nul { 0 } else { b'\n' });
        if self.remove.as_deref() == Some(path) {
            fs::remove_dir_all(path)?;
            self.remove = None;
        }
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn command(&mut self, command: &mut Command) -> io::Result<bool> {
        let mut bytes = Vec::new();
        let result = self.capture(command, &mut bytes)?;
        self.write(&bytes)?;
        Ok(result)
    }
    fn capture(&mut self, command: &mut Command, sink: &mut dyn io::Write) -> io::Result<bool> {
        let output = command.output()?;
        sink.write_all(&output.stdout)?;
        if !output.stderr.is_empty() {
            self.errors.push(PathBuf::from("child stderr"));
        }
        Ok(output.status.success())
    }

    fn error(&mut self, error: &WalkError) {
        self.errors.push(error.path.clone());
    }
}

fn run(args: &[OsString]) -> (Outcome, Output) {
    let plan = Plan::parse(args).unwrap();
    assert!(plan.unsupported().is_none());
    let mut output = Output::default();
    let outcome = plan.run(&mut plan.live_source(), &mut output).unwrap();
    (outcome, output)
}

#[derive(Clone, Default)]
struct ParallelOutput(std::sync::Arc<std::sync::Mutex<Output>>);
impl Effects for ParallelOutput {
    fn print(&mut self, path: &Path, nul: bool) -> io::Result<()> {
        self.0.lock().unwrap().print(path, nul)
    }
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0.lock().unwrap().write(bytes)
    }
    fn error(&mut self, error: &WalkError) {
        self.0.lock().unwrap().error(error);
    }
}

fn run_parallel(args: &[OsString]) -> (Outcome, ParallelOutput) {
    let plan = Plan::parse(args).unwrap();
    let output = ParallelOutput::default();
    let outcome = plan
        .run_parallel(plan.live_source(), output.clone(), 16)
        .unwrap();
    (outcome, output)
}

fn records(bytes: &[u8], delimiter: u8) -> Vec<Vec<u8>> {
    let mut records: Vec<_> = bytes
        .split(|&b| b == delimiter)
        .map(<[u8]>::to_vec)
        .collect();
    if records.last().is_some_and(Vec::is_empty) {
        records.pop();
    }
    records.sort();
    records
}

fn paths(tree: &Tree, names: &[&str]) -> Vec<Vec<u8>> {
    let mut result: Vec<_> = names
        .iter()
        .map(|name| {
            if name.is_empty() {
                tree.0.clone()
            } else {
                tree.0.join(name)
            }
            .as_os_str()
            .as_bytes()
            .to_vec()
        })
        .collect();
    result.sort();
    result
}

#[test]
fn implicit_print_wraps_or_but_explicit_print_belongs_to_its_branch() {
    let tree = Tree::new("print");
    let (_, implicit) = tree.run(&["-name", "*.c", "-o", "-name", "*.txt"]);
    assert_eq!(
        records(&implicit.bytes, b'\n'),
        paths(&tree, &["a.c", "b.txt", "dir/sub/deep.c"])
    );
    let (_, explicit) = tree.run(&["-name", "*.c", "-o", "-name", "*.txt", "-print"]);
    assert_eq!(records(&explicit.bytes, b'\n'), paths(&tree, &["b.txt"]));
    let (_, prune) = tree.run(&["-prune"]);
    assert_eq!(records(&prune.bytes, b'\n'), paths(&tree, &[""]));
}

#[test]
fn operators_obey_precedence_short_circuit_and_comma_sequence() {
    let tree = Tree::new("operators");
    let cases: &[(&[&str], &[&str])] = &[
        (
            &["-name", "a.c", "-o", "-name", "b.txt", "-a", "-false"],
            &["a.c"],
        ),
        (
            &[
                "(", "-name", "a.c", "-or", "-name", "b.txt", ")", "-and", "!", "-false",
            ],
            &["a.c", "b.txt"],
        ),
        (&["-true", "-o", "-print"], &[]),
        (&["-false", "-a", "-print"], &[]),
        (&["-maxdepth", "0", "-false", ",", "-print"], &[""]),
        (&["-not", "-true"], &[]),
    ];
    for (expression, expected) in cases {
        let (outcome, output) = tree.run(expression);
        assert_eq!(outcome.errors, 0);
        assert_eq!(
            records(&output.bytes, b'\n'),
            paths(&tree, expected),
            "{expression:?}"
        );
    }
}

#[test]
fn depth_limits_prune_and_quit_drive_the_real_walk() {
    let tree = Tree::new("control");
    let (_, output) = tree.run(&["-maxdepth", "1", "-mindepth", "1", "-type", "d"]);
    assert_eq!(
        records(&output.bytes, b'\n'),
        paths(&tree, &["dir", "empty"])
    );
    let (_, output) = tree.run(&["-name", "dir", "-prune", "-o", "-name", "*.c", "-print"]);
    assert_eq!(records(&output.bytes, b'\n'), paths(&tree, &["a.c"]));
    let (_, output) = tree.run(&["-name", "dir", "-prune"]);
    assert_eq!(records(&output.bytes, b'\n'), paths(&tree, &["dir"]));
    let (_, output) = tree.run(&["-depth", "-print", "-prune"]);
    assert!(
        records(&output.bytes, b'\n').contains(
            &tree
                .0
                .join("dir/sub/deep.c")
                .as_os_str()
                .as_bytes()
                .to_vec()
        )
    );
    assert!(
        output
            .bytes
            .ends_with(&[tree.0.as_os_str().as_bytes(), b"\n"].concat())
    );
    let (_, output) = tree.run(&["-print", "-quit", "-print"]);
    assert_eq!(records(&output.bytes, b'\n'), paths(&tree, &[""]));
    let (_, output) = tree.run(&["-quit"]);
    assert!(output.bytes.is_empty());
}

#[test]
fn name_and_path_globs_use_find_semantics_including_c_classes() {
    let tree = Tree::new("glob");
    let cases: &[(&[&str], &[&str])] = &[
        (&["-name", "*.c"], &["a.c", "dir/sub/deep.c"]),
        (&["-iname", "abc"], &["ABC"]),
        (&["-name", ".*"], &[".hidden"]),
        (&["-name", "[[:upper:]][[:upper:]][[:upper:]]"], &["ABC"]),
        (&["-iname", "[!a]*", "-name", "ABC"], &[]),
        (&["-name", "[a-z][[:digit:]]"], &["a1"]),
        (&["-name", "[^a]1"], &[]),
        (&["-name", "["], &["["]),
        (&["-name", "\\.*"], &[".hidden"]),
        (&["-name", "back\\\\slash"], &["back\\slash"]),
        (&["-path", "*dir*deep.c"], &["dir/sub/deep.c"]),
        (&["-ipath", "*abc"], &["ABC"]),
        (&["-wholename", "*dir/file"], &["dir/file"]),
        (&["-iwholename", "*DIR/FILE"], &["dir/file"]),
    ];
    for (expression, expected) in cases {
        let (_, output) = tree.run(expression);
        assert_eq!(
            records(&output.bytes, b'\n'),
            paths(&tree, expected),
            "{expression:?}"
        );
    }
    let (_, wildcard) = tree.run(&["-name", "*"]);
    assert!(
        records(&wildcard.bytes, b'\n')
            .contains(&tree.0.join(".hidden").as_os_str().as_bytes().to_vec())
    );
    assert!(
        records(&wildcard.bytes, b'\n').contains(
            &tree
                .0
                .join(OsStr::from_bytes(b"nonutf8-\xff"))
                .as_os_str()
                .as_bytes()
                .to_vec()
        )
    );
}

#[test]
fn type_lists_do_not_follow_links_and_print0_preserves_filename_bytes() {
    let tree = Tree::new("types");
    let (_, output) = tree.run(&["-type", "l"]);
    assert_eq!(
        records(&output.bytes, b'\n'),
        paths(&tree, &["link", "broken"])
    );
    let (_, output) = tree.run(&["-type", "d,l"]);
    assert_eq!(
        records(&output.bytes, b'\n'),
        paths(&tree, &["", "dir", "dir/sub", "empty", "link", "broken"])
    );
    fs::write(tree.0.join("line\nbreak"), b"").unwrap();
    let (_, output) = tree.run(&["-name", "line*", "-print0"]);
    assert_eq!(records(&output.bytes, 0), paths(&tree, &["line\nbreak"]));
}

#[test]
fn start_path_spelling_is_retained_for_children_and_root_names() {
    let tree = Tree::new("spelling");
    for suffix in ["/", "///", "/.", "/dir/.."] {
        let start = OsString::from_vec([tree.0.as_os_str().as_bytes(), suffix.as_bytes()].concat());
        let (_, output) = run(&["-I".into(), start.clone(), "-maxdepth".into(), "1".into()]);
        let expected = [
            start.as_bytes(),
            if suffix.ends_with('/') { b"" } else { b"/" },
            b"a.c\n",
        ]
        .concat();
        assert!(
            output
                .bytes
                .windows(expected.len())
                .any(|window| window == expected),
            "{suffix}"
        );
        assert!(
            output
                .bytes
                .starts_with(&[start.as_bytes(), b"\n"].concat())
        );
    }
    let (_, output) = run(&[
        "-I".into(),
        tree.0.join("link/").into_os_string(),
        "-maxdepth".into(),
        "0".into(),
        "-type".into(),
        "d".into(),
    ]);
    assert!(
        !output.bytes.is_empty(),
        "the kernel follows a trailing-slash root link"
    );
}

#[test]
fn errors_continue_across_starts_and_missing_matches_succeed() {
    let tree = Tree::new("errors");
    let (outcome, output) = run(&[
        "-I".into(),
        tree.0.join("missing").into_os_string(),
        tree.0.clone().into_os_string(),
        "-maxdepth".into(),
        "0".into(),
    ]);
    assert_eq!(outcome.errors, 1);
    assert_eq!(output.errors, [tree.0.join("missing")]);
    assert_eq!(records(&output.bytes, b'\n'), paths(&tree, &[""]));
    let (outcome, output) = tree.run(&["-false"]);
    assert_eq!(outcome.errors, 0);
    assert!(output.bytes.is_empty());
    fs::create_dir(tree.0.join("denied")).unwrap();
    fs::set_permissions(tree.0.join("denied"), fs::Permissions::from_mode(0o000)).unwrap();
    let (outcome, output) = tree.run(&[]);
    assert_eq!(outcome.errors, 1);
    assert_eq!(output.errors, [tree.0.join("denied")]);
    assert!(
        records(&output.bytes, b'\n')
            .contains(&tree.0.join("denied").as_os_str().as_bytes().to_vec())
    );
    let (outcome, _) = tree.run(&["-maxdepth", "1"]);
    assert_eq!(outcome.errors, 0, "a depth limit avoids the denied listing");
}

#[test]
fn a_directory_removed_after_its_visit_faults_without_losing_siblings() {
    let tree = Tree::new("vanish");
    let plan = Plan::parse(&tree.args(&["-print"])).unwrap();
    let mut output = Output {
        remove: Some(tree.0.join("dir")),
        ..Output::default()
    };
    let outcome = plan.run(&mut plan.live_source(), &mut output).unwrap();
    assert_eq!(outcome.errors, 1);
    assert_eq!(output.errors, [tree.0.join("dir")]);
    assert!(
        records(&output.bytes, b'\n').contains(&tree.0.join("a.c").as_os_str().as_bytes().to_vec())
    );
}

#[test]
fn directories_removed_by_exec_rm_rf_mid_walk_match_observed_gnu() {
    // Regression guard for the reused path buffer (m3b): a listed child whose
    // directory vanished is reported once, post-order still yields the names
    // already listed, and nothing is printed twice. Expectations are GNU
    // 4.11.0's observed output on the same shapes.
    let tree = Tree::new("rmrf");
    let (outcome, output) = tree.run(&["-name", "dir", "-exec", "rm", "-rf", "{}", ";"]);
    assert_eq!(outcome.errors, 1);
    assert_eq!(output.errors, [tree.0.join("dir")]);
    assert!(output.bytes.is_empty());
    assert!(!tree.0.join("dir").exists());

    let tree = Tree::new("rmrf-depth");
    let (outcome, output) = tree.run(&["-depth", "-name", "dir", "-exec", "rm", "-rf", "{}", ";"]);
    assert_eq!(outcome.errors, 0);
    assert!(output.errors.is_empty());
    assert!(!tree.0.join("dir").exists());

    let tree = Tree::new("rmrf-ancestor");
    let dir = tree.0.join("dir");
    let dir = dir.to_str().unwrap();
    let (outcome, output) = tree.run(&[
        "-depth", "-name", "deep.c", "-exec", "rm", "-rf", dir, ";", "-o", "-path", "*/dir*",
        "-print",
    ]);
    assert_eq!(outcome.errors, 0);
    assert!(output.errors.is_empty());
    assert_eq!(
        records(&output.bytes, b'\n'),
        paths(&tree, &["dir", "dir/file", "dir/sub"])
    );
}

#[test]
fn a_chain_past_path_max_spells_every_level_and_the_walk_continues() {
    // Regression guard for the reused path buffer (m3b): each level is the
    // exact join of its parent and the overlong directory is reported once.
    // GNU walks past PATH_MAX; the live walk opens and lstats by path, so it
    // stops at the first open that fails with ENAMETOOLONG.
    let tree = Tree::new("pathmax");
    let component = "d".repeat(200);
    fs::create_dir(tree.0.join("chain")).unwrap();
    let status = Command::new("sh")
        .current_dir(tree.0.join("chain"))
        .args([
            "-c",
            &format!("for i in $(seq 25); do mkdir {component} && cd {component} || exit 1; done"),
        ])
        .status()
        .unwrap();
    assert!(status.success());
    let mut level = tree.0.join("chain");
    let mut levels = vec![level.as_os_str().as_bytes().to_vec()];
    for _ in 0..25 {
        level.push(&component);
        levels.push(level.as_os_str().as_bytes().to_vec());
    }

    let (outcome, output) = tree.run(&[]);
    let all = records(&output.bytes, b'\n');
    let printed: Vec<_> = all
        .iter()
        .filter(|record| record.starts_with(&levels[0]))
        .cloned()
        .collect();
    assert!(printed.len() > 10 && printed.len() < levels.len());
    assert_eq!(printed, levels[..printed.len()]);
    assert_eq!(outcome.errors, 1);
    assert_eq!(
        output.errors,
        [PathBuf::from(OsString::from_vec(
            printed[printed.len() - 1].clone()
        ))]
    );
    for name in ["a.c", "dir/sub/deep.c", "empty"] {
        assert!(all.contains(&tree.0.join(name).as_os_str().as_bytes().to_vec()));
    }
}

#[test]
fn metadata_observations_and_failures_are_cached_and_name_tests_are_lazy() {
    let tree = Tree::new("stat");
    let plan = Plan::parse(&tree.args(&["-name", "a.c"])).unwrap();
    let mut source = plan.live_source();
    while let Some(item) = source.next(true) {
        let entry = item.unwrap();
        if entry.path() == tree.0.join("a.c") {
            fs::remove_file(entry.path()).unwrap();
            assert!(
                evaluate(
                    &plan.expression,
                    entry,
                    &mut Output::default(),
                    &mut Control::default()
                )
                .unwrap()
            );
            assert!(
                entry.metadata().is_err(),
                "neither source nor name predicate cached stat"
            );
        }
        if entry.path() == tree.0.join("b.txt") {
            let size = entry.metadata().unwrap().len();
            fs::remove_file(entry.path()).unwrap();
            assert_eq!(entry.metadata().unwrap().len(), size);
        }
    }
    let entry = Entry::new(tree.0.join("missing"), 1, FileKind::File);
    assert!(entry.metadata().is_err());
    fs::write(entry.path(), b"later").unwrap();
    assert!(entry.metadata().is_err());
}

#[test]
fn parser_rejects_bad_syntax_and_retains_every_unsupported_operand() {
    let warning = Plan::parse(&["-I", ".", "-perm", "/000"].map(OsString::from)).unwrap();
    assert!(warning.permission_warning());
    let quiet = Plan::parse(&["-I", ".", "-perm", "-000"].map(OsString::from)).unwrap();
    assert!(!quiet.permission_warning());
    let invalid: &[&[&str]] = &[
        &["-unknown"],
        &["-name"],
        &["-type", "q"],
        &["-type", "f,"],
        &["-type", "ff"],
        &["-type", "f,f"],
        &["-type", "D"],
        &["-maxdepth", "2147483648"],
        &["-perm", "u+ug"],
        &["-regextype", "help"],
        &["-type", ""],
        &["-perm", "+066"],
        &["-perm", "888"],
        &["-maxdepth", "+1"],
        &["-maxdepth", "-1"],
        &["-maxdepth", "1.0"],
        &["(", ")"],
        &["("],
        &["-true", ")"],
        &["-true", "-o"],
        &["!"],
        &["-a", "-true"],
        &["-name", "x", "path"],
        &["-size", "1T"],
        &["-mtime", "1h"],
        &["-mtime", ""],
        &["-user", "ferret-user-that-does-not-exist"],
        &["-group", "ferret-group-that-does-not-exist"],
        &["-newermt", "not-a-date"],
        &["-exec", "echo", "{}"],
        &["-exec", "echo", "{}", "suffix", "+"],
        &["-exec", "echo", "{}", "{}", "+"],
        &["-O"],
        &["-D"],
        &["-newerXY", "ref"],
    ];
    for args in invalid {
        assert!(
            Plan::parse(&args.iter().map(OsString::from).collect::<Vec<_>>()).is_err(),
            "{args:?}"
        );
    }
    let cases: &[&[&str]] = &[];
    for args in cases {
        let plan = Plan::parse(&args.iter().map(OsString::from).collect::<Vec<_>>()).unwrap();
        assert!(plan.unsupported().is_some(), "{args:?}");
    }
    let plan = Plan::parse(&["-L", "-I", "-O3", "-P", "a", "b", "-type", "f"].map(OsString::from))
        .unwrap();
    assert!(plan.no_ignore());
    assert!(plan.unsupported().is_none());
    assert_eq!(plan.paths, [PathBuf::from("a"), PathBuf::from("b")]);
}

#[test]
#[ignore = "extended development differential"]
fn differential_against_pinned_gnu() {
    let tree = Tree::new("oracle");
    fs::write(tree.0.join("reference"), b"ref").unwrap();
    fs::create_dir(tree.0.join("denied")).unwrap();
    fs::set_permissions(tree.0.join("denied"), fs::Permissions::from_mode(0o000)).unwrap();
    super::action::tests::differential();
    for template in expressions::EXPRESSIONS {
        let expression = template
            .iter()
            .map(|arg| {
                if *arg == "@REFERENCE@" {
                    tree.0.join("reference").into_os_string()
                } else {
                    OsString::from(arg)
                }
            })
            .collect::<Vec<_>>();
        // Quit can legally defer the denied branch. Keep its differential
        // fixture free of order-dependent errors; error cases still use 000.
        let quitting = template.contains(&"-quit");
        fs::set_permissions(
            tree.0.join("denied"),
            fs::Permissions::from_mode(if quitting { 0o700 } else { 0o000 }),
        )
        .unwrap();
        let gnu = gnu::output(
            gnu::command()
                .arg(&tree.0)
                .args(&expression)
                .env("LC_ALL", "C")
                .env("TZ", "UTC"),
        );
        let args: Vec<_> = [OsString::from("-I"), tree.0.clone().into_os_string()]
            .into_iter()
            .chain(expression.iter().cloned())
            .collect();
        let (outcome, output) = run_parallel(&args);
        let output = output.0.lock().unwrap();
        let delimiter = if expression.iter().any(|arg| arg == "-print0") {
            0
        } else {
            b'\n'
        };
        assert_eq!(
            i32::from(outcome.errors != 0),
            gnu.status.code().unwrap(),
            "status {expression:?}"
        );
        if quitting {
            let all: Vec<_> = expression.iter().filter(|arg| *arg != "-quit").collect();
            let all = gnu::output(gnu::command().arg(&tree.0).args(all));
            let candidates = records(&all.stdout, delimiter);
            for record in records(&output.bytes, delimiter) {
                assert!(candidates.contains(&record));
            }
        } else {
            assert_eq!(
                records(&output.bytes, delimiter),
                records(&gnu.stdout, delimiter),
                "stdout {expression:?}"
            );
        }
        assert_eq!(
            output.errors.is_empty(),
            gnu.stderr.is_empty(),
            "stderr {expression:?}"
        );
    }
}

#[test]
fn c_locale_case_folding_distinguishes_ranges_classes_and_collating_symbols() {
    let tree = Tree::new("classes");
    for name in ["a", "A", "b", "B", "Z", "z", "_", "[[:bogus:]]"] {
        fs::write(tree.0.join(name), b"").unwrap();
    }
    let cases: &[(&[&str], &[&str])] = &[
        (&["-iname", "[[:upper:]]"], &["A", "B", "Z"]),
        (&["-iname", "[[:lower:]]"], &["a", "b", "z"]),
        (&["-iname", "[A-z]"], &["a", "A", "b", "B", "Z", "z"]),
        (&["-iname", "[Z-a]"], &[]),
        (&["-iname", "[[=a=]]"], &["a"]),
        (&["-iname", "[[.a.]]"], &["a"]),
        (&["-iname", "[[.a.]-[.b.]]"], &["a", "A", "b", "B"]),
        (&["-name", "[[.ab.]]"], &[]),
        (&["-name", "[[:bogus:]]"], &[]),
    ];
    for (expression, expected) in cases {
        let (_, output) = tree.run(expression);
        assert_eq!(
            records(&output.bytes, b'\n'),
            paths(&tree, expected),
            "{expression:?}"
        );
    }
}

#[test]
fn unknown_dtype_uses_one_lazy_stat_and_a_failed_stat_keeps_walking() {
    let tree = Tree::new("unknown");
    let plan = Plan::parse(&tree.args(&["-type", "f"])).unwrap();
    let mut live = plan.live_source();
    live.force_unknown = true;
    struct Vanishing {
        live: LiveWalk,
        victim: PathBuf,
    }
    impl EntrySource for Vanishing {
        fn next(&mut self, descend: bool) -> Option<Result<&Entry, WalkError>> {
            let item = self.live.next(descend)?;
            if let Ok(entry) = &item
                && entry.path() == self.victim
            {
                fs::remove_file(&self.victim).unwrap();
            }
            Some(item)
        }
    }
    let mut source = Vanishing {
        live,
        victim: tree.0.join("a.c"),
    };
    let mut output = Output::default();
    let outcome = plan.run(&mut source, &mut output).unwrap();
    assert_eq!(outcome.errors, 1);
    assert_eq!(output.errors, [tree.0.join("a.c")]);
    assert!(
        records(&output.bytes, b'\n')
            .contains(&tree.0.join("b.txt").as_os_str().as_bytes().to_vec())
    );

    // With a depth limit and a name-only test, unknown d_type needs no stat.
    let plan = Plan::parse(&tree.args(&["-maxdepth", "1", "-name", "b.txt"])).unwrap();
    let mut live = plan.live_source();
    live.force_unknown = true;
    let mut source = Vanishing {
        live,
        victim: tree.0.join("b.txt"),
    };
    let mut output = Output::default();
    let outcome = plan.run(&mut source, &mut output).unwrap();
    assert_eq!(outcome.errors, 0);
    assert_eq!(records(&output.bytes, b'\n'), paths(&tree, &["b.txt"]));
}

#[test]
fn parser_precedence_and_implicit_action_are_visible_in_the_ast() {
    let args = ["-false", "-o", "!", "-false", "-a", "-true", ",", "-false"].map(OsString::from);
    let plan = Plan::parse(&args).unwrap();
    let Expression::And(expression, print) = plan.expression else {
        panic!("implicit print must wrap expression");
    };
    assert_eq!(*print, Expression::Print(false));
    let Expression::Comma(left, right) = *expression else {
        panic!("comma binds weakest");
    };
    assert_eq!(*right, Expression::Constant(false));
    let Expression::Or(left, right) = *left else {
        panic!("OR precedes comma");
    };
    assert_eq!(*left, Expression::Constant(false));
    let Expression::And(left, right) = *right else {
        panic!("AND binds inside OR");
    };
    assert_eq!(
        *left,
        Expression::Not(Box::new(Expression::Constant(false)))
    );
    assert_eq!(*right, Expression::Constant(true));
}

#[test]
fn unsupported_plans_fail_before_the_source_or_effects_are_used() {
    let plan =
        Plan::parse(&["-I", "missing", "-context", "x", "-name", "x"].map(OsString::from)).unwrap();
    let mut output = Output::default();
    let error = plan.run(&mut plan.live_source(), &mut output).unwrap_err();
    assert_eq!(error.feature, "-context");
    assert!(output.errors.is_empty());
    assert!(output.bytes.is_empty());
}

#[test]
fn xdev_and_mount_evaluate_a_mount_point_without_entering_it() {
    use std::os::unix::fs::MetadataExt;
    let (Ok(parent), Ok(child)) = (fs::metadata("/proc"), fs::metadata("/proc/sys")) else {
        return;
    };
    if parent.dev() == child.dev() {
        return;
    }
    let expression = [
        "-maxdepth",
        "2",
        "-path",
        "/proc",
        "-o",
        "-path",
        "/proc/sys*",
        "-o",
        "-prune",
        "-a",
        "-false",
    ];
    let base: Vec<OsString> = ["-I", "/proc"]
        .into_iter()
        .chain(expression)
        .map(OsString::from)
        .collect();
    let (outcome, output) = run(&base);
    assert_eq!(outcome.errors, 0);
    assert!(
        records(&output.bytes, b'\n')
            .iter()
            .any(|path| path.starts_with(b"/proc/sys/"))
    );
    for flag in ["-xdev", "-mount"] {
        let mut args = base.clone();
        args.push(flag.into());
        let (outcome, output) = run(&args);
        assert_eq!(outcome.errors, 0);
        assert_eq!(
            records(&output.bytes, b'\n'),
            [b"/proc".to_vec(), b"/proc/sys".to_vec()]
        );
    }
}

#[test]
fn prune_stats_a_removed_non_directory_but_not_a_removed_directory() {
    let tree = Tree::new("prune-removed");
    let (outcome, output) = tree.run(&["-name", "a.c", "-exec", "rm", "{}", ";", "-prune"]);
    assert_eq!(outcome.errors, 1);
    assert_eq!(output.errors, [tree.0.join("a.c")]);
    let (outcome, output) = tree.run(&["-name", "dir", "-exec", "rm", "-rf", "{}", ";", "-prune"]);
    assert_eq!(outcome.errors, 0, "{:?}", output.errors);
}

#[test]
fn d_spells_depth_and_a_path_ending_in_slash_warns() {
    let plan = Plan::parse(&["-I", ".", "-d", "-path", "./"].map(OsString::from)).unwrap();
    assert!(plan.options.depth_first);
    assert_eq!(plan.warnings.len(), 1);
    let plan = Plan::parse(&["-I", ".", "-name", "a/"].map(OsString::from)).unwrap();
    assert!(plan.warnings.is_empty());
}
