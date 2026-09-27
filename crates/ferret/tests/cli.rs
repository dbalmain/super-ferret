//! The `ferret` binary end to end: each test runs the built executable on a
//! temp tree with its own index, and its own HOME and XDG directories, so
//! nothing reads or writes the user's.

// Test-only helpers outside `#[test]` fns: clippy's allow-unwrap-in-tests
// does not reach them, and a panic is the right failure here.
#![allow(clippy::unwrap_used)]

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A temp directory holding `tree/` (what is indexed), `index/`, and the
/// XDG homes under `home/`.
struct Env {
    base: PathBuf,
}

impl Env {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!("ferret-cli-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("tree")).unwrap();
        fs::create_dir_all(base.join("home")).unwrap();
        Self { base }
    }

    fn tree(&self) -> PathBuf {
        self.base.join("tree")
    }

    fn at(&self, rel: &str) -> PathBuf {
        self.tree().join(rel)
    }

    fn index(&self) -> PathBuf {
        self.base.join("index")
    }

    fn state(&self) -> PathBuf {
        self.base.join("home/state")
    }

    fn log(&self) -> PathBuf {
        self.state().join("ferret/log.jsonl")
    }

    fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = self.at(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }

    /// `ferret --index INDEX ARGS...`, with stdin closed.
    fn run(&self, args: &[&OsStr]) -> Output {
        self.command(args).output().unwrap()
    }

    /// `ferret --index INDEX ARGS...` in this environment, to adjust.
    fn command(&self, args: &[&OsStr]) -> Command {
        let home = self.base.join("home");
        let mut command = Command::new(env!("CARGO_BIN_EXE_ferret"));
        command
            .arg("--index")
            .arg(self.index())
            .args(args)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("XDG_STATE_HOME", self.state())
            .env("XDG_CACHE_HOME", home.join("cache"))
            .env_remove("FERRET_INDEX")
            .stdin(Stdio::null());
        command
    }

    fn log_lines(&self) -> Vec<String> {
        fs::read_to_string(self.log())
            .map(|text| text.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn os<S: AsRef<OsStr> + ?Sized>(s: &S) -> &OsStr {
    s.as_ref()
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap()
}

/// stdout's lines, as paths.
fn paths(output: &Output) -> Vec<PathBuf> {
    output
        .stdout
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| PathBuf::from(OsStr::from_bytes(line)))
        .collect()
}

/// A pipe whose reader is already closed: the first write to it fails with
/// `EPIPE`, as when `ferret … | head` has exited. Rust ignores SIGPIPE, so
/// the process sees the error rather than being killed.
fn closed_pipe() -> Stdio {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    Stdio::from(writer)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
// D34: roots are independent. Removing the outer root leaves the inner one
// indexed, and the outer root's own files go.
fn index_find_remove_find() {
    let env = Env::new("lifecycle");
    let top = env.write("top.txt", b"top\n");
    let inner_file = env.write("w/x/inner.txt", b"inner\n");
    let (outer, inner) = (env.tree(), env.at("w/x"));

    let indexed = env.run(&[os("index"), outer.as_os_str(), inner.as_os_str()]);
    assert_eq!(code(&indexed), 0, "{}", stderr(&indexed));
    let found = env.run(&[os("find"), os(".txt")]);
    assert_eq!(code(&found), 0);
    let mut rows = paths(&found);
    rows.sort();
    assert_eq!(rows, [top.clone(), inner_file.clone()]);
    let roots = env.run(&[os("roots"), os("list")]);
    assert_eq!(paths(&roots), [outer.clone(), inner.clone()]);

    let removed = env.run(&[os("roots"), os("remove"), outer.as_os_str()]);
    assert_eq!(code(&removed), 0, "{}", stderr(&removed));
    let found = env.run(&[os("find"), os(".txt")]);
    assert_eq!(paths(&found), [inner_file]);
    let roots = env.run(&[os("roots"), os("list")]);
    assert_eq!(paths(&roots), std::slice::from_ref(&inner));

    let again = env.run(&[os("roots"), os("remove"), outer.as_os_str()]);
    assert_eq!(code(&again), 3, "removing a root that is not configured");
    assert!(stderr(&again).contains("not a configured root"));

    // Removing the last root publishes an empty catalog.
    let removed = env.run(&[os("roots"), os("remove"), inner.as_os_str()]);
    assert_eq!(code(&removed), 0, "{}", stderr(&removed));
    assert_eq!(code(&env.run(&[os("find"), os(".txt")])), 1);
    assert!(paths(&env.run(&[os("roots"), os("list")])).is_empty());
}

#[test]
fn bare_index_refreshes_every_root_and_a_relative_root_is_made_absolute() {
    let env = Env::new("bare");
    env.write("a/one.txt", b"1\n");
    let mut command = env.command(&[os("index"), os("a")]);
    let indexed = command.current_dir(env.tree()).output().unwrap();
    assert_eq!(code(&indexed), 0, "{}", stderr(&indexed));
    assert_eq!(paths(&env.run(&[os("roots"), os("list")])), [env.at("a")]);

    let two = env.write("a/two.txt", b"2\n");
    let refreshed = env.run(&[os("index")]);
    assert_eq!(code(&refreshed), 0, "{}", stderr(&refreshed));
    assert_eq!(paths(&env.run(&[os("find"), os("two")])), [two]);
}

#[test]
fn bare_index_with_no_roots_and_no_terminal_fails() {
    let env = Env::new("no-roots");
    let output = env.run(&[os("index")]);
    assert_eq!(code(&output), 2);
    assert!(stderr(&output).contains("no roots"), "{}", stderr(&output));
    assert!(
        !env.index().join("catalog").exists(),
        "nothing is published"
    );
}

#[test]
fn exit_codes_are_stable() {
    let env = Env::new("exit");
    env.write("hit.txt", b"x\n");
    let no_index = env.run(&[os("find"), os("hit")]);
    assert_eq!(code(&no_index), 3, "find before any index");
    assert!(stderr(&no_index).contains("no index"));
    assert_eq!(code(&env.run(&[os("stats")])), 3, "stats before any index");

    let not_a_dir = env.run(&[os("index"), env.at("hit.txt").as_os_str()]);
    assert_eq!(code(&not_a_dir), 3);
    assert!(stderr(&not_a_dir).contains("not a directory"));

    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    let cases: &[(&[&str], i32)] = &[
        (&["find", "hit"], 0),
        (&["find", "--limit", "1", "hit"], 0),
        (&["find", "miss"], 1),
        (&["find", "--nope", "hit"], 2),
        (&["find", "--limit"], 2),
        (&["find", "size:huge"], 2),
        (&["find", "re:("], 2),
        (&["frobnicate"], 2),
        (&["stats"], 0),
        (&["roots", "list"], 0),
        (&["help"], 0),
        (&["help", "typo"], 2),
    ];
    for (args, expected) in cases {
        let args: Vec<&OsStr> = args.iter().map(os).collect();
        let output = env.run(&args);
        assert_eq!(code(&output), *expected, "{args:?}: {}", stderr(&output));
    }
}

#[test]
fn limit_stops_after_n_rows() {
    let env = Env::new("limit");
    for i in 0..5 {
        env.write(&format!("f{i}.txt"), b"x\n");
    }
    env.run(&[os("index"), env.tree().as_os_str()]);
    let all = env.run(&[os("find"), os("txt")]);
    assert_eq!(paths(&all).len(), 5);
    let two = env.run(&[os("find"), os("--limit=2"), os("txt")]);
    assert_eq!(paths(&two), paths(&all)[..2]);
}

/// The value of `"key":"…"` in one JSON line; values here have no escapes.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let start = line.find(&format!("\"{key}\":\""))? + key.len() + 4;
    Some(&line[start..start + line[start..].find('"')?])
}

/// Standard padded base64, decoded: the inverse of what `--json` writes.
fn unbase64(text: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => panic!("not base64: {c}"),
    };
    let mut out = Vec::new();
    for quad in text.as_bytes().chunks(4) {
        let digits: Vec<u8> = quad
            .iter()
            .filter(|&&c| c != b'=')
            .map(|&c| value(c))
            .collect();
        let n = digits
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &d)| n | u32::from(d) << (18 - 6 * i));
        out.extend(n.to_be_bytes()[1..].iter().take(digits.len() - 1));
    }
    out
}

#[test]
// The agent skill reads paths from JSON, so a name that is not UTF-8 must
// come back byte for byte: `path_base64` carries it, and only when needed.
fn json_round_trips_a_path_that_is_not_utf8() {
    let env = Env::new("json");
    let name = OsStr::from_bytes(b"caf\xe9-\xff\xfe.txt");
    let odd = env.tree().join(name);
    fs::write(&odd, b"odd\n").unwrap();
    let plain = env.write("caf\u{e9}-plain.txt", b"plain\n");
    env.run(&[os("index"), env.tree().as_os_str()]);

    let output = env.run(&[os("find"), os("--json"), os("caf")]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = String::from_utf8(output.stdout).expect("JSON output is UTF-8");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    let mut seen = Vec::new();
    for line in lines {
        assert!(line.starts_with('{') && line.ends_with('}'), "{line}");
        assert!(line.contains("\"type\":\"file\""), "{line}");
        let path = match field(line, "path_base64") {
            Some(encoded) => unbase64(encoded),
            None => field(line, "path").unwrap().as_bytes().to_vec(),
        };
        let path = PathBuf::from(OsStr::from_bytes(&path));
        assert!(path.exists(), "{} does not name the file", path.display());
        seen.push(path);
    }
    seen.sort();
    let mut expected = vec![odd, plain];
    expected.sort();
    assert_eq!(seen, expected);
    assert_eq!(
        text.matches("path_base64").count(),
        1,
        "only the non-UTF-8 path carries base64"
    );
}

#[test]
// D26 A′: a walk fault may hide entries, so nothing is published and the
// previous generation stays byte for byte.
fn a_coverage_fault_publishes_nothing_and_fails() {
    let env = Env::new("coverage");
    env.write("ok.txt", b"ok\n");
    let locked = env
        .write("locked/secret.txt", b"s\n")
        .parent()
        .unwrap()
        .to_owned();
    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    let before = fs::read(env.index().join("catalog")).unwrap();

    env.write("new.txt", b"new\n");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let output = env.run(&[os("index")]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();

    assert_eq!(code(&output), 3);
    let message = stderr(&output);
    assert!(message.contains("nothing published"), "{message}");
    assert!(
        message.contains("locked"),
        "the fault names the path: {message}"
    );
    assert_eq!(fs::read(env.index().join("catalog")).unwrap(), before);
    assert_eq!(code(&env.run(&[os("find"), os("new")])), 1);
}

#[test]
// A file that cannot be read is a content fault: published without its
// content, reported as a warning, and the run succeeds.
fn a_content_fault_is_a_warning() {
    let env = Env::new("content");
    let unreadable = env.write("private.txt", b"p\n");
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
    let output = env.run(&[os("index"), env.tree().as_os_str()]);
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let message = stderr(&output);
    assert!(message.contains("warning"), "{message}");
    assert!(message.contains("private.txt"), "{message}");
    let json = env.run(&[os("find"), os("--json"), os("private")]);
    assert!(String::from_utf8_lossy(&json.stdout).contains("\"doc\":null"));
}

#[test]
fn the_log_gains_one_line_per_find_and_index_run() {
    let env = Env::new("log");
    env.write("a/f.txt", b"f\n");
    let steps: &[(&[&str], usize)] = &[
        (&["index", "a"], 1),
        (&["find", "f.txt"], 1),
        (&["find", "missing"], 1),
        (&["find", "size:bad"], 0),
        (&["stats"], 0),
        (&["roots", "list"], 0),
        (&["index"], 1),
        (&["roots", "remove", "a"], 1),
    ];
    for (args, added) in steps {
        let before = env.log_lines().len();
        let args: Vec<&OsStr> = args.iter().map(os).collect();
        let output = env.command(&args).current_dir(env.tree()).output().unwrap();
        assert_ne!(code(&output), 3, "{args:?}: {}", stderr(&output));
        assert_eq!(env.log_lines().len(), before + added, "{args:?}");
    }
    let lines = env.log_lines();
    let find = &lines[1];
    for key in [
        "\"cmd\":\"find\"",
        "\"query\":[\"f.txt\"]",
        "\"plan\":",
        "\"rows\":1",
        "\"first_row_us\":",
        "\"total_us\":",
        "\"candidates\":",
    ] {
        assert!(find.contains(key), "{key} in {find}");
    }
    assert!(
        !find.contains(&*env.tree().to_string_lossy()),
        "no paths: {find}"
    );
    let index = &lines[0];
    for key in [
        "\"cmd\":\"index\"",
        "\"walk_us\":",
        "\"hash_us\":",
        "\"commit_us\":",
        "\"faults_us\":",
        "\"peak_rss_kb\":",
        "\"files_read\":1",
    ] {
        assert!(index.contains(key), "{key} in {index}");
    }
    assert!(
        !index.contains(&*env.tree().to_string_lossy()),
        "no paths: {index}"
    );
    assert!(lines[4].contains("\"cmd\":\"roots-remove\""));
}

#[test]
fn a_log_that_cannot_be_written_does_not_fail_the_command() {
    let env = Env::new("log-fail");
    env.write("f.txt", b"f\n");
    // The state home is a file, so the log's directory cannot be made.
    fs::write(env.state(), b"not a directory").unwrap();
    let indexed = env.run(&[os("index"), env.tree().as_os_str()]);
    assert_eq!(code(&indexed), 0, "{}", stderr(&indexed));
    let found = env.run(&[os("find"), os("f.txt")]);
    assert_eq!(code(&found), 0);
    assert_eq!(paths(&found), [env.at("f.txt")]);
    assert!(stderr(&found).contains("query log"), "{}", stderr(&found));
}

#[test]
fn the_index_comes_from_the_flag_then_the_environment_then_xdg() {
    let env = Env::new("location");
    env.write("f.txt", b"f\n");
    let home = env.base.join("home");
    let bare = |extra: &[(&str, &Path)]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ferret"));
        command
            .args(["index", &*env.tree().to_string_lossy()])
            .env("HOME", &home)
            .env("XDG_DATA_HOME", home.join("data"))
            .env("XDG_STATE_HOME", env.state())
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env_remove("FERRET_INDEX")
            .stdin(Stdio::null());
        for (key, value) in extra {
            command.env(key, value);
        }
        command
    };
    assert_eq!(code(&bare(&[]).output().unwrap()), 0);
    assert!(home.join("data/ferret/catalog").exists(), "XDG default");

    let from_env = env.base.join("from-env");
    assert_eq!(
        code(&bare(&[("FERRET_INDEX", &from_env)]).output().unwrap()),
        0
    );
    assert!(from_env.join("catalog").exists(), "FERRET_INDEX");

    let from_flag = env.base.join("from-flag");
    let mut command = bare(&[("FERRET_INDEX", &from_env)]);
    command.arg("--index").arg(&from_flag);
    assert_eq!(code(&command.output().unwrap()), 0);
    assert!(
        from_flag.join("catalog").exists(),
        "--index beats FERRET_INDEX"
    );
}

#[test]
// A report the reader no longer wants must not undo a publish: the run
// exits 0 and logs it. Both streams are closed, and this is the first run,
// so stderr gets the "wrote the default ignore rules" note as well.
fn index_into_a_closed_pipe_still_publishes_and_logs() {
    let env = Env::new("index-epipe");
    env.write("a/f.txt", b"f\n");
    let status = env
        .command(&[os("index"), env.at("a").as_os_str()])
        .stdout(closed_pipe())
        .stderr(closed_pipe())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(0));
    let found = env.run(&[os("find"), os("f.txt")]);
    assert_eq!(paths(&found), [env.at("a/f.txt")]);
    let lines = env.log_lines();
    assert_eq!(lines.len(), 2, "the index line, then the find line");
    assert!(lines[0].contains(r#""cmd":"index","#), "{}", lines[0]);
    assert!(
        lines[0].contains(r#""outcome":"published""#),
        "{}",
        lines[0]
    );
}

#[test]
// Every command that writes to stdout treats a reader that went away as
// done, not failed. `find` has written rows (more than its 64 KiB buffer,
// so the error comes mid-stream) and exits 0 as it would have.
fn every_command_exits_normally_into_a_closed_pipe() {
    let env = Env::new("epipe");
    for i in 0..2000 {
        env.write(&format!("d/a-long-enough-file-name-{i:04}.txt"), b"x\n");
    }
    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    let cases: &[&[&str]] = &[
        &["find", "txt"],
        &["find", "--json", "txt"],
        &["roots", "list"],
        &["stats"],
        &["help"],
        &["--version"],
    ];
    for args in cases {
        let args: Vec<&OsStr> = args.iter().map(os).collect();
        let output = env.command(&args).stdout(closed_pipe()).output().unwrap();
        assert_eq!(code(&output), 0, "{args:?}: {}", stderr(&output));
        assert_eq!(stderr(&output), "", "{args:?}");
    }
}

#[test]
// A log another version or the user left world-readable is narrowed to
// 0600 before the next line goes in.
fn a_readable_log_is_made_private_before_it_is_written() {
    let env = Env::new("log-mode");
    env.write("a/f.txt", b"f\n");
    fs::create_dir_all(env.log().parent().unwrap()).unwrap();
    fs::write(env.log(), "{\"v\":1}\n").unwrap();
    fs::set_permissions(env.log(), fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(code(&env.run(&[os("index"), env.at("a").as_os_str()])), 0);
    let mode = fs::metadata(env.log()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(env.log_lines().len(), 2);
}

#[test]
// A line is appended only under the log's exclusive lock. With the lock
// held here, `find` prints its rows and then waits; it appends once the
// lock is released.
fn a_log_line_waits_for_the_log_lock() {
    let env = Env::new("log-lock");
    env.write("a/f.txt", b"f\n");
    assert_eq!(code(&env.run(&[os("index"), env.at("a").as_os_str()])), 0);
    let held = File::options().append(true).open(env.log()).unwrap();
    held.lock().unwrap();

    let mut child = env
        .command(&[os("find"), os("f.txt")])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut row = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut row)
        .unwrap();
    // The row is flushed just before the log append; without the lock the
    // process would be gone well within this.
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(child.try_wait().unwrap().is_none(), "find did not wait");
    assert_eq!(env.log_lines().len(), 1);

    held.unlock().unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
    assert_eq!(env.log_lines().len(), 2);
}

#[test]
// A second index run while one holds the writer lock fails at once with a
// message that says why, and publishes nothing.
fn an_index_run_while_another_holds_the_lock_fails_clearly() {
    let env = Env::new("locked");
    env.write("a/f.txt", b"f\n");
    assert_eq!(code(&env.run(&[os("index"), env.at("a").as_os_str()])), 0);
    let before = fs::read(env.index().join("catalog")).unwrap();
    let lock = File::open(env.index().join("lock")).unwrap();
    lock.lock().unwrap();

    env.write("a/g.txt", b"g\n");
    let output = env.run(&[os("index")]);
    assert_eq!(code(&output), 3);
    assert!(
        stderr(&output).contains("another `ferret index` holds the catalog lock"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fs::read(env.index().join("catalog")).unwrap(), before);
    let lines = env.log_lines();
    assert!(lines[1].contains(r#""outcome":"error""#), "{}", lines[1]);

    lock.unlock().unwrap();
    assert_eq!(code(&env.run(&[os("index")])), 0);
}
