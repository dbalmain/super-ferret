//! The `ferret` binary end to end: each test runs the built executable on a
//! temp tree with its own index, and its own HOME and XDG directories, so
//! nothing reads or writes the user's.

// Test-only helpers outside `#[test]` fns: clippy's allow-unwrap-in-tests
// does not reach them, and a panic is the right failure here.
#![allow(clippy::unwrap_used)]

use std::ffi::OsStr;
use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};

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
            .env("XDG_RUNTIME_DIR", home.join("runtime"))
            .env("FERRET_NO_DAEMON", "1")
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("XDG_STATE_HOME", self.state())
            .env("XDG_CACHE_HOME", home.join("cache"))
            .env_remove("FERRET_INDEX")
            .stdin(Stdio::null());
        command
    }

    fn ignore_file(&self) -> PathBuf {
        self.base.join("home/config/ferret/ignore")
    }

    /// Writes the global ignore file, so a run has no first-run note to
    /// print before it publishes.
    fn seed_ignore_file(&self) {
        fs::create_dir_all(self.ignore_file().parent().unwrap()).unwrap();
        fs::write(self.ignore_file(), "# seeded by the test\n").unwrap();
    }

    /// `file` is findable, and the log holds exactly one line: an index run
    /// that published and exited with `exit`.
    fn assert_published_and_logged(&self, file: &Path, exit: i32) {
        let found = self.run(&[os("search"), file.file_name().unwrap()]);
        assert_eq!(paths(&found), [file]);
        let lines = self.log_lines();
        let line = &lines[0];
        assert!(line.contains(r#""cmd":"index","#), "{line}");
        assert!(line.contains(r#""outcome":"published""#), "{line}");
        assert!(line.contains(&format!(r#""exit":{exit},"#)), "{line}");
        assert_eq!(lines.len(), 2, "the index line, then the find line");
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
    let found = env.run(&[os("search"), os(".txt")]);
    assert_eq!(code(&found), 0);
    let mut rows = paths(&found);
    rows.sort();
    assert_eq!(rows, [top.clone(), inner_file.clone()]);
    let roots = env.run(&[os("roots"), os("list")]);
    assert_eq!(paths(&roots), [outer.clone(), inner.clone()]);

    let removed = env.run(&[os("roots"), os("remove"), outer.as_os_str()]);
    assert_eq!(code(&removed), 0, "{}", stderr(&removed));
    let found = env.run(&[os("search"), os(".txt")]);
    assert_eq!(paths(&found), [inner_file]);
    let roots = env.run(&[os("roots"), os("list")]);
    assert_eq!(paths(&roots), std::slice::from_ref(&inner));

    let again = env.run(&[os("roots"), os("remove"), outer.as_os_str()]);
    assert_eq!(code(&again), 3, "removing a root that is not configured");
    assert!(stderr(&again).contains("not a configured root"));

    // Removing the last root publishes an empty catalog.
    let removed = env.run(&[os("roots"), os("remove"), inner.as_os_str()]);
    assert_eq!(code(&removed), 0, "{}", stderr(&removed));
    assert_eq!(code(&env.run(&[os("search"), os(".txt")])), 1);
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
    assert_eq!(paths(&env.run(&[os("search"), os("two")])), [two]);
}

#[test]
// `index ../x` stored the canonical path while `roots remove ../x` kept the
// `..` and was refused. Both now spell a root the same way, and a root whose
// directory is gone is removed by the same spelling, resolved by name.
fn a_root_is_removed_by_the_spelling_that_added_it() {
    let env = Env::new("dotdot");
    env.write("a/one.txt", b"1\n");
    env.write("b/two.txt", b"2\n");
    let run_in = |args: &[&OsStr]| {
        let output = env.command(args).current_dir(env.at("a")).output().unwrap();
        assert_eq!(code(&output), 0, "{args:?}: {}", stderr(&output));
    };
    run_in(&[os("index"), os("../b"), os("../a")]);
    assert_eq!(
        paths(&env.run(&[os("roots"), os("list")])),
        [env.at("a"), env.at("b")]
    );
    run_in(&[os("roots"), os("remove"), os("../b")]);
    assert_eq!(paths(&env.run(&[os("roots"), os("list")])), [env.at("a")]);

    // Once the directory is gone, `..` cannot be resolved: the spelling that
    // added it is refused, and the stored spelling removes it.
    run_in(&[os("index"), os("../b")]);
    fs::remove_dir_all(env.at("b")).unwrap();
    let refused = env
        .command(&[os("roots"), os("remove"), os("../b")])
        .current_dir(env.at("a"))
        .output()
        .unwrap();
    assert_eq!(code(&refused), 3);
    assert!(
        stderr(&refused).contains("roots list"),
        "{}",
        stderr(&refused)
    );
    let stored = env.at("b").into_os_string();
    run_in(&[os("roots"), os("remove"), &stored]);
    assert_eq!(paths(&env.run(&[os("roots"), os("list")])), [env.at("a")]);
}

#[test]
// Resolving a gone root's `..` by name once removed a different root: with
// `w/link -> data/sub`, `w/link/../project` added `data/project`, but after
// that directory was deleted the same spelling removed `w/project`.
fn a_gone_root_named_through_a_symlink_removes_nothing_else() {
    let env = Env::new("dotdot-link");
    fs::create_dir_all(env.at("data/sub")).unwrap();
    env.write("data/project/d.txt", b"d\n");
    env.write("w/project/w.txt", b"w\n");
    std::os::unix::fs::symlink(env.at("data/sub"), env.at("w/link")).unwrap();
    let through = env.at("w/link/../project").into_os_string();
    let wp = env.at("w/project").into_os_string();
    let indexed = env.run(&[os("index"), &through, &wp]);
    assert_eq!(code(&indexed), 0, "{}", stderr(&indexed));
    let both = [env.at("data/project"), env.at("w/project")];
    assert_eq!(paths(&env.run(&[os("roots"), os("list")])), both);

    fs::remove_dir_all(env.at("data/project")).unwrap();
    let refused = env.run(&[os("roots"), os("remove"), &through]);
    assert_eq!(code(&refused), 3, "{}", stderr(&refused));
    assert_eq!(paths(&env.run(&[os("roots"), os("list")])), both);

    let stored = env.at("data/project").into_os_string();
    let removed = env.run(&[os("roots"), os("remove"), &stored]);
    assert_eq!(code(&removed), 0, "{}", stderr(&removed));
    assert_eq!(
        paths(&env.run(&[os("roots"), os("list")])),
        [env.at("w/project")]
    );
}

#[test]
fn bare_index_with_no_roots_and_no_terminal_fails() {
    let env = Env::new("no-roots");
    let output = env.run(&[os("index")]);
    assert_eq!(code(&output), 2);
    assert!(stderr(&output).contains("no roots"), "{}", stderr(&output));
    assert!(
        !env.index().join("current").exists(),
        "nothing is published"
    );
}

#[test]
fn exit_codes_are_stable() {
    let env = Env::new("exit");
    env.write("hit.txt", b"x\n");
    let no_index = env.run(&[os("search"), os("hit")]);
    assert_eq!(code(&no_index), 3, "find before any index");
    assert!(stderr(&no_index).contains("no index"));
    assert_eq!(code(&env.run(&[os("stats")])), 3, "stats before any index");

    let not_a_dir = env.run(&[os("index"), env.at("hit.txt").as_os_str()]);
    assert_eq!(code(&not_a_dir), 3);
    assert!(stderr(&not_a_dir).contains("not a directory"));

    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    let cases: &[(&[&str], i32)] = &[
        (&["search", "hit"], 0),
        (&["search", "--limit", "1", "hit"], 0),
        (&["search", "miss"], 1),
        (&["search", "--nope", "hit"], 2),
        (&["search", "--limit"], 2),
        (&["search", "size:huge"], 2),
        (&["search", "re:("], 2),
        // An empty atom once reached the heap scan and panicked (101).
        (&["search", ""], 2),
        (&["search", "case:"], 2),
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
fn a_catalog_from_another_version_says_how_to_rebuild_and_index_replaces_it() {
    // Written by ferret at format version 1 (main at `ccd1dab`): the same
    // file the catalog crate's version test reads.
    let env = Env::new("old-version");
    env.seed_ignore_file();
    let file = env.write("hit.txt", b"x\n");
    fs::create_dir_all(env.index()).unwrap();
    fs::write(
        env.index().join("catalog"),
        include_bytes!("../../ferret-catalog/src/tests/v1.catalog"),
    )
    .unwrap();

    for args in [
        &["search", "hit"][..],
        &["stats"],
        &["roots", "list"],
        &["index"],
    ] {
        let args: Vec<&OsStr> = args.iter().map(os).collect();
        let output = env.run(&args);
        assert_eq!(code(&output), 3, "{args:?}");
        let text = stderr(&output);
        assert!(
            text.contains("catalog format version 1, expected"),
            "{args:?}: {text}"
        );
        assert!(
            text.contains("run `ferret index DIR...`"),
            "{args:?}: {text}"
        );
    }

    let rebuilt = env.run(&[os("index"), env.tree().as_os_str()]);
    assert_eq!(code(&rebuilt), 0, "{}", stderr(&rebuilt));
    assert!(
        stderr(&rebuilt).contains("rebuilding it from scratch with only the roots named here"),
        "{}",
        stderr(&rebuilt)
    );
    assert_eq!(paths(&env.run(&[os("search"), os("hit")])), [file]);
    assert_eq!(
        code(&env.run(&[os("index")])),
        0,
        "the new roots are configured"
    );
}

/// Overwrites the little-endian u32 at `at` in the published catalog, as a
/// flipped bit or a long history would leave it.
fn patch_catalog(env: &Env, at: usize, value: u32) {
    let path = ferret_catalog::Catalog::snapshot_path(&env.index())
        .unwrap()
        .unwrap();
    let mut bytes = fs::read(&path).unwrap();
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    fs::write(&path, bytes).unwrap();
}

#[test]
// D36 B: stats once sized a counter array by `next_doc`, so a catalog with a
// long history needed memory for every document ever assigned, and indexed
// it by an inode's unchecked DocId, which panicked on a corrupt one.
fn stats_counts_documents_sparsely_and_survives_a_corrupt_reference() {
    let env = Env::new("stats-docs");
    env.write("a.txt", b"same\n");
    env.write("b.txt", b"same\n");
    env.write("c.txt", b"other\n");
    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    let duplicates = |output: &Output| {
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        let line = text.lines().find(|l| l.contains("duplicates:"));
        line.map(str::to_owned).unwrap_or_else(|| panic!("{text}"))
    };
    let before = duplicates(&env.run(&[os("stats")]));
    assert!(
        before.contains("1 documents are held by 1 more"),
        "{before}"
    );

    // Two billion documents assigned in the past, three live: a dense
    // counter would be 8 GB, past this 1 GB address-space limit.
    let snapshot = ferret_catalog::Catalog::snapshot_path(&env.index())
        .unwrap()
        .unwrap();
    fs::remove_file(snapshot).unwrap();
    fs::remove_file(env.index().join("current")).unwrap();
    let mut legacy = include_bytes!("../../ferret-catalog/src/tests/v3.catalog").to_vec();
    legacy[16..20].copy_from_slice(&(1u32 << 31).to_le_bytes());
    fs::write(env.index().join("catalog"), legacy).unwrap();
    assert_eq!(code(&env.run(&[os("import-v3")])), 0);
    let limited = |env: &Env| {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(r#"ulimit -v 1048576 && exec "$@""#)
            .arg("sh")
            .arg(env!("CARGO_BIN_EXE_ferret"))
            .arg("--index")
            .arg(env.index())
            .arg("stats")
            .env("XDG_STATE_HOME", env.state())
            .env("HOME", env.base.join("home"));
        command.output().unwrap()
    };
    let sparse = limited(&env);
    assert_eq!(sparse.status.code(), Some(0), "{}", stderr(&sparse));
    assert_eq!(
        duplicates(&sparse).split(',').next(),
        before.split(',').next()
    );

    // Every file's DocId moved past `next_doc`, by the base of the doc
    // column's one block: the first word of the doc section (the 18th
    // section, whose offset is in the table after the 40 B header).
    // Decoding does not check an inode's DocId.
    let bytes = fs::read(
        ferret_catalog::Catalog::snapshot_path(&env.index())
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let doc_section = u64::from_le_bytes(bytes[96 + 17 * 32..][..8].try_into().unwrap());
    patch_catalog(&env, doc_section as usize, u32::MAX - 2);
    let corrupt = limited(&env);
    assert_eq!(corrupt.status.code(), Some(3), "{}", stderr(&corrupt));
    assert!(stderr(&corrupt).contains("corrupt: doc"));
}

#[test]
fn limit_stops_after_n_rows() {
    let env = Env::new("limit");
    for i in 0..5 {
        env.write(&format!("f{i}.txt"), b"x\n");
    }
    env.run(&[os("index"), env.tree().as_os_str()]);
    let all = env.run(&[os("search"), os("txt")]);
    assert_eq!(paths(&all).len(), 5);
    let two = env.run(&[os("search"), os("--limit=2"), os("txt")]);
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

    let output = env.run(&[os("search"), os("--json"), os("caf")]);
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
        // Metadata comes from columns the plain listing never loads.
        let meta = fs::metadata(&path).unwrap();
        assert!(
            line.contains(&format!("\"size\":{},", meta.len())),
            "{line}"
        );
        assert!(
            line.contains(&format!("\"mtime\":{},", meta.mtime())),
            "{line}"
        );
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
// D26: a denied directory is opaque and retires old children; trustworthy
// siblings still publish.
fn an_unreadable_directory_publishes_its_row_and_other_changes() {
    let env = Env::new("coverage");
    env.write("ok.txt", b"ok\n");
    let locked = env
        .write("locked/secret.txt", b"s\n")
        .parent()
        .unwrap()
        .to_owned();
    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    env.write("new.txt", b"new\n");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let output = env.run(&[os("index")]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(paths(&env.run(&[os("search"), os("locked")])), [locked]);
    assert_eq!(code(&env.run(&[os("search"), os("secret")])), 1);
    assert_eq!(code(&env.run(&[os("search"), os("new")])), 0);
}

#[test]
// D26 A′: an ignore file that exists but cannot be read leaves the rules
// unknown. It once fell back to the defaults and published, so a user's
// exclusion (here `private/`) silently lapsed.
fn an_unreadable_ignore_file_publishes_nothing_and_fails() {
    let env = Env::new("ignore-unreadable");
    env.write("ok.txt", b"ok\n");
    env.write("private/secret.txt", b"s\n");
    fs::create_dir_all(env.ignore_file().parent().unwrap()).unwrap();
    fs::write(env.ignore_file(), "private/\n").unwrap();
    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    let before = fs::read(
        ferret_catalog::Catalog::snapshot_path(&env.index())
            .unwrap()
            .unwrap(),
    )
    .unwrap();

    env.write("new.txt", b"new\n");
    fs::set_permissions(env.ignore_file(), fs::Permissions::from_mode(0o000)).unwrap();
    let output = env.run(&[os("index")]);
    fs::set_permissions(env.ignore_file(), fs::Permissions::from_mode(0o600)).unwrap();

    assert_eq!(code(&output), 3);
    let message = stderr(&output);
    assert!(message.contains("nothing published"), "{message}");
    assert_eq!(
        fs::read(
            ferret_catalog::Catalog::snapshot_path(&env.index())
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        before
    );
    assert_eq!(code(&env.run(&[os("search"), os("new")])), 1);
    assert_eq!(code(&env.run(&[os("search"), os("secret")])), 1);
}

#[test]
// A dangling ignore symlink read as NotFound and so as "no file": the run
// published on the defaults and the user's `private/` exclusion lapsed.
fn a_dangling_ignore_symlink_publishes_nothing_and_fails() {
    let env = Env::new("ignore-dangling");
    env.write("ok.txt", b"ok\n");
    env.write("private/secret.txt", b"s\n");
    let rules = env.base.join("rules");
    fs::write(&rules, "private/\n").unwrap();
    fs::create_dir_all(env.ignore_file().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&rules, env.ignore_file()).unwrap();
    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    let before = fs::read(
        ferret_catalog::Catalog::snapshot_path(&env.index())
            .unwrap()
            .unwrap(),
    )
    .unwrap();

    fs::remove_file(&rules).unwrap();
    let output = env.run(&[os("index")]);
    assert_eq!(code(&output), 3, "{}", stderr(&output));
    assert!(stderr(&output).contains("nothing published"));
    assert_eq!(
        fs::read(
            ferret_catalog::Catalog::snapshot_path(&env.index())
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        before
    );
    assert_eq!(code(&env.run(&[os("search"), os("secret")])), 1);
}

#[test]
// A missing ignore file that setup cannot create (its directory is read
// only) is the defaults setup would have written: the run publishes.
fn a_missing_ignore_file_is_the_defaults() {
    let env = Env::new("ignore-missing");
    env.write("ok.txt", b"ok\n");
    let target = env.write("target/built.o", b"o\n");
    let config = env.ignore_file().parent().unwrap().to_owned();
    fs::create_dir_all(&config).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o500)).unwrap();
    let output = env.run(&[os("index"), env.tree().as_os_str()]);
    fs::set_permissions(&config, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!env.ignore_file().exists());
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(stderr(&output).contains("default ignore rules"));
    assert_eq!(code(&env.run(&[os("search"), os("ok")])), 0);
    assert_eq!(
        code(&env.run(&[os("search"), target.file_name().unwrap()])),
        1
    );
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
    let json = env.run(&[os("search"), os("--json"), os("private")]);
    assert!(String::from_utf8_lossy(&json.stdout).contains("\"doc\":null"));
}

#[test]
fn the_log_gains_one_line_per_find_and_index_run() {
    let env = Env::new("log");
    env.write("a/f.txt", b"f\n");
    let steps: &[(&[&str], usize)] = &[
        (&["index", "a"], 1),
        (&["search", "f.txt"], 1),
        (&["search", "missing"], 1),
        (&["search", "size:bad"], 0),
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
        "\"cmd\":\"search\"",
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
    let found = env.run(&[os("search"), os("f.txt")]);
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
            .env("XDG_RUNTIME_DIR", home.join("runtime"))
            .env("FERRET_NO_DAEMON", "1")
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
    assert!(home.join("data/ferret/current").exists(), "XDG default");

    let from_env = env.base.join("from-env");
    assert_eq!(
        code(&bare(&[("FERRET_INDEX", &from_env)]).output().unwrap()),
        0
    );
    assert!(from_env.join("current").exists(), "FERRET_INDEX");

    let from_flag = env.base.join("from-flag");
    let mut command = bare(&[("FERRET_INDEX", &from_env)]);
    command.arg("--index").arg(&from_flag);
    assert_eq!(code(&command.output().unwrap()), 0);
    assert!(
        from_flag.join("current").exists(),
        "--index beats FERRET_INDEX"
    );
}

#[test]
// The report of a published run goes to a reader that has gone: the run
// still exits 0 and logs. The ignore file is seeded first, so nothing is
// written to stdout or stderr before the publish, and the pipe error can
// only come from the report after it.
fn index_into_a_closed_stdout_still_publishes_and_logs() {
    let env = Env::new("index-epipe");
    env.write("a/f.txt", b"f\n");
    env.seed_ignore_file();
    let status = env
        .command(&[os("index"), env.at("a").as_os_str()])
        .stdout(closed_pipe())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(0));
    env.assert_published_and_logged(&env.at("a/f.txt"), 0);
}

#[test]
// A closed stderr on a first run, which writes "wrote the default ignore
// rules" before the walk: no panic, the run publishes and logs.
fn index_with_a_closed_stderr_still_publishes_and_logs() {
    let env = Env::new("index-stderr");
    env.write("a/f.txt", b"f\n");
    let status = env
        .command(&[os("index"), env.at("a").as_os_str()])
        .stdout(Stdio::null())
        .stderr(closed_pipe())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(0));
    assert!(
        env.ignore_file().exists(),
        "the first run seeds the ignore file"
    );
    env.assert_published_and_logged(&env.at("a/f.txt"), 0);
}

#[test]
// A report that cannot be written for a reason other than a closed pipe
// is a failure (exit 3), as it is for `search` and `stats`, but the generation
// it reports on is published and logged as such.
fn index_into_a_full_device_publishes_and_exits_3() {
    let env = Env::new("index-full");
    env.write("a/f.txt", b"f\n");
    env.seed_ignore_file();
    let output = env
        .command(&[os("index"), env.at("a").as_os_str()])
        .stdout(File::create("/dev/full").unwrap())
        .output()
        .unwrap();
    assert_eq!(code(&output), 3);
    assert!(
        stderr(&output).contains("writing the report"),
        "{}",
        stderr(&output)
    );
    env.assert_published_and_logged(&env.at("a/f.txt"), 3);
}

#[test]
// Every command that writes to stdout treats a reader that went away as
// done, not failed. `search` has written rows (more than its 64 KiB buffer,
// so the error comes mid-stream) and exits 0 as it would have.
fn every_command_exits_normally_into_a_closed_pipe() {
    let env = Env::new("epipe");
    for i in 0..2000 {
        env.write(&format!("d/a-long-enough-file-name-{i:04}.txt"), b"x\n");
    }
    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    let cases: &[&[&str]] = &[
        &["search", "txt"],
        &["search", "--json", "txt"],
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
// The log is best-effort, so a stopped holder of its lock (SIGSTOP, a
// debugger) must not hang a command that has finished its work. With the
// lock held for the whole run, `search` prints its row, gives up on the lock
// within its bound, warns, writes no line and exits 0.
fn a_held_log_lock_drops_the_line_and_the_command_finishes() {
    let env = Env::new("log-lock");
    env.write("a/f.txt", b"f\n");
    assert_eq!(code(&env.run(&[os("index"), env.at("a").as_os_str()])), 0);
    let held = File::options().append(true).open(env.log()).unwrap();
    held.lock().unwrap();

    let started = Instant::now();
    let mut child = env
        .command(&[os("search"), os("f.txt")])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Poll rather than wait, so a regression to a blocking lock fails here
    // instead of hanging the suite.
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(5) {
            child.kill().unwrap();
            panic!("find hung on the held log lock");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(code(&output), 0);
    assert_eq!(paths(&output), [env.at("a/f.txt")]);
    assert!(
        stderr(&output).contains("not written"),
        "{}",
        stderr(&output)
    );
    assert_eq!(env.log_lines().len(), 1, "only the index line");
    held.unlock().unwrap();
}

#[test]
// Lines appended by processes racing for the lock are whole: eight
// concurrent runs add eight lines, each one complete JSON object.
fn concurrent_log_lines_are_whole() {
    let env = Env::new("log-race");
    env.write("a/f.txt", b"f\n");
    assert_eq!(code(&env.run(&[os("index"), env.at("a").as_os_str()])), 0);
    let children: Vec<_> = (0..8)
        .map(|_| {
            env.command(&[os("search"), os("f.txt")])
                .stdout(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut child in children {
        assert_eq!(child.wait().unwrap().code(), Some(0));
    }
    let lines = env.log_lines();
    assert_eq!(lines.len(), 9);
    for line in &lines[1..] {
        assert!(
            line.starts_with(r#"{"v":1,"cmd":"search","#) && line.ends_with('}'),
            "{line}"
        );
        assert_eq!(line.matches(r#""v":1"#).count(), 1, "{line}");
    }
}

#[test]
// A second index run while one holds the writer lock fails at once with a
// message that says why, and publishes nothing.
fn an_index_run_while_another_holds_the_lock_fails_clearly() {
    let env = Env::new("locked");
    env.write("a/f.txt", b"f\n");
    assert_eq!(code(&env.run(&[os("index"), env.at("a").as_os_str()])), 0);
    let before = fs::read(
        ferret_catalog::Catalog::snapshot_path(&env.index())
            .unwrap()
            .unwrap(),
    )
    .unwrap();
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
    assert_eq!(
        fs::read(
            ferret_catalog::Catalog::snapshot_path(&env.index())
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        before
    );
    let lines = env.log_lines();
    assert!(lines[1].contains(r#""outcome":"error""#), "{}", lines[1]);

    lock.unlock().unwrap();
    assert_eq!(code(&env.run(&[os("index")])), 0);
}

#[test]
fn find_probe_needs_no_index_config_home_or_log() {
    let env = Env::new("find-probe");
    let tree = env.tree();
    let args = [
        os("find"),
        os("-I"),
        tree.as_os_str(),
        os("-maxdepth"),
        os("0"),
        os("-print"),
    ];
    let output = env
        .command(&args)
        .env_remove("HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME")
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(output.stdout, [tree.as_os_str().as_bytes(), b"\n"].concat());
    assert!(output.stderr.is_empty());
    assert!(!env.index().exists());
    assert!(!env.log().exists());

    env.seed_ignore_file();
    let other = env.base.join("other");
    fs::create_dir(&other).unwrap();
    assert_eq!(code(&env.run(&[os("index"), other.as_os_str()])), 0);
    let before = fs::read(
        ferret_catalog::Catalog::snapshot_path(&env.index())
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    fs::set_permissions(
        ferret_catalog::Catalog::snapshot_path(&env.index())
            .unwrap()
            .unwrap(),
        fs::Permissions::from_mode(0o400),
    )
    .unwrap();
    fs::set_permissions(env.index(), fs::Permissions::from_mode(0o500)).unwrap();
    let output = env.run(&args);
    fs::set_permissions(env.index(), fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(output.stdout, [tree.as_os_str().as_bytes(), b"\n"].concat());
    assert!(output.stderr.is_empty());
    assert_eq!(
        fs::read(
            ferret_catalog::Catalog::snapshot_path(&env.index())
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        before
    );
    assert_eq!(env.log_lines().len(), 1, "only index logged");
}

#[test]
fn find_perm_zero_any_mode_warns_but_still_matches() {
    let env = Env::new("find-perm-warning");
    let tree = env.tree();
    for mode in ["/000", "-000"] {
        let output = env.run(&[
            os("find"),
            os("-I"),
            tree.as_os_str(),
            os("-maxdepth"),
            os("0"),
            os("-perm"),
            os(mode),
        ]);
        assert_eq!(code(&output), 0, "{}", stderr(&output));
        assert_eq!(output.stdout, [tree.as_os_str().as_bytes(), b"\n"].concat());
        assert_eq!(output.stderr.is_empty(), mode == "-000");
    }
}

#[test]
fn find_reports_usage_and_unsupported_features_with_status_one() {
    let env = Env::new("find-errors");
    let tree = env.tree();
    let cases: &[(&[&str], bool)] = &[
        (&["-name"], false),
        (&["-unknown"], false),
        (&["-type", "q"], false),
        (&["-type", "f,f"], false),
        (&["-type", "D"], false),
        (&["-perm", "+066"], false),
        (&["(", ")"], false),
        (&["-regex", r".*\(a\)\2"], false),
        (&["-printf", "%"], false),
        (&["-prune", "-delete"], false),
    ];
    for (expression, unsupported) in cases {
        let mut args = vec![os("find"), os("-I"), tree.as_os_str()];
        args.extend(expression.iter().map(os));
        let output = env.run(&args);
        assert_eq!(code(&output), 1, "{expression:?}: {}", stderr(&output));
        assert!(output.stdout.is_empty());
        assert_eq!(
            stderr(&output).contains("not implemented yet"),
            *unsupported,
            "{expression:?}"
        );
    }
    let output = env.run(&[os("find"), tree.as_os_str(), os("-maxdepth"), os("0")]);
    assert_eq!(code(&output), 1);
    assert!(stderr(&output).contains("no index"));
    let output = env.run(&[
        os("find"),
        os("--no-ignore"),
        os("-P"),
        os("-O3"),
        tree.as_os_str(),
        os("-false"),
    ]);
    assert_eq!(code(&output), 0);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

/// 4a: the real binary's search must hide name-only rows, while stats must
/// count their types without indexing reserved child tags as stat-row ids.
#[test]
fn ignored_names_and_special_files_survive_index_search_and_stats() {
    let env = Env::new("ignored-special-stats");
    env.seed_ignore_file();
    env.write(".ferretignore", b"*.ignored\nbuild/\n");
    env.write("kept.txt", b"visible");
    env.write("hidden.ignored", b"ignored");
    env.write("build/hidden.txt", b"ignored");
    let pipe = env.at("pipe");
    assert!(
        Command::new("mkfifo")
            .arg(&pipe)
            .status()
            .unwrap()
            .success()
    );
    let _socket = std::os::unix::net::UnixListener::bind(env.at("socket")).unwrap();
    let indexed = env.run(&[os("index"), env.tree().as_os_str()]);
    assert_eq!(code(&indexed), 0, "{}", stderr(&indexed));
    let output = env.run(&[os("search"), os("*")]);
    let found = paths(&output);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(found.contains(&env.at("kept.txt")));
    for missing in [
        "pipe",
        "socket",
        "hidden.ignored",
        "build",
        "build/hidden.txt",
    ] {
        assert!(!found.contains(&env.at(missing)), "{found:?}");
    }
    let stats = env.run(&[os("stats")]);
    assert_eq!(code(&stats), 0, "{}", stderr(&stats));
    let text = String::from_utf8(stats.stdout).unwrap();
    assert!(
        text.contains("ignored   2 names without inode rows"),
        "{text}"
    );
    assert!(text.contains("1 FIFO names, 1 socket names"), "{text}");
    assert!(text.contains("2 visible inodes"), "{text}");
}

mod find_expressions {
    include!("../../ferret-query/src/find/tests/expressions.rs");
}

fn sorted_records(bytes: &[u8], separator: u8) -> Vec<&[u8]> {
    let mut records: Vec<_> = bytes
        .split(|&byte| byte == separator || (separator == b' ' && byte == b'\n'))
        .filter(|record| !record.is_empty())
        .collect();
    records.sort_unstable();
    records
}

#[test]
fn catalog_find_matches_live_across_the_differential_expressions_as_sorted_records() {
    // Sibling order is catalog order; GNU sibling order is not promised.
    let env = Env::new("catalog-equivalence");
    env.seed_ignore_file();
    for name in [
        "z.c",
        "b.txt",
        ".hidden",
        "ABC",
        "a1",
        "[",
        "dir/file",
        "dir/sub/deep.c",
        "back\\slash",
        "reference",
    ] {
        env.write(name, b"contents");
    }
    fs::create_dir(env.at("empty")).unwrap();
    env.write("only-empty", b"");
    env.write("target/not-ignored", b"visible");
    env.write(".gitignore", b"# no rules\n");
    env.write(".ferretignore", b"# no rules\n");
    env.write("nonutf8-placeholder", b"");
    fs::rename(
        env.at("nonutf8-placeholder"),
        env.tree().join(OsStr::from_bytes(b"nonutf8-\xff")),
    )
    .unwrap();
    std::os::unix::fs::symlink("dir", env.at("link")).unwrap();
    std::os::unix::fs::symlink("missing", env.at("broken")).unwrap();
    let indexed = env.run(&[os("index"), env.tree().as_os_str()]);
    assert_eq!(code(&indexed), 0, "{}", stderr(&indexed));
    for start in [".", "./", "dir/", "dir/..", "link", "broken", "b.txt"] {
        for template in find_expressions::EXPRESSIONS {
            let reference = env.at("reference");
            let expression: Vec<_> = template
                .iter()
                .map(|arg| {
                    if *arg == "@REFERENCE@" {
                        reference.as_os_str()
                    } else {
                        os(arg)
                    }
                })
                .collect();
            let mut args = vec![os("find"), os(start)];
            args.extend(&expression);
            let catalog = env.command(&args).current_dir(env.tree()).output().unwrap();
            args.insert(1, os("-I"));
            let live = env.command(&args).current_dir(env.tree()).output().unwrap();
            assert_eq!(
                code(&catalog),
                code(&live),
                "status {start} {template:?}: {}",
                stderr(&catalog)
            );
            if template.contains(&"-quit") {
                let all: Vec<_> = args
                    .iter()
                    .copied()
                    .filter(|arg| *arg != os("-quit"))
                    .collect();
                let all = env.command(&all).current_dir(env.tree()).output().unwrap();
                let records = sorted_records(&catalog.stdout, b'\n');
                assert!(records.len() <= 16);
                for record in records {
                    assert!(sorted_records(&all.stdout, b'\n').contains(&record));
                }
            } else {
                let separator = if template.contains(&"-print0") {
                    0
                } else {
                    b'\n'
                };
                assert_eq!(
                    sorted_records(&catalog.stdout, separator),
                    sorted_records(&live.stdout, separator),
                    "set {start} {template:?}"
                );
            }
            assert_eq!(
                catalog.stderr.is_empty(),
                live.stderr.is_empty(),
                "stderr {start} {template:?}: {}",
                stderr(&catalog)
            );
        }
    }
    for expression in [
        vec!["-type", "f", "-exec", "echo", "{}", "+"],
        vec!["-type", "f", "-execdir", "echo", "{}", "+"],
        vec!["-printf", "%H|%P|%p|%y|%s|%n\\n"],
    ] {
        let mut args = vec![os("find"), os(".")];
        args.extend(expression.iter().map(os));
        let catalog = env.command(&args).current_dir(env.tree()).output().unwrap();
        args.insert(1, os("-I"));
        let live = env.command(&args).current_dir(env.tree()).output().unwrap();
        assert_eq!(
            code(&catalog),
            code(&live),
            "{expression:?}: {}",
            stderr(&catalog)
        );
        let separator = if expression.contains(&"-printf") {
            b'\n'
        } else {
            b' '
        };
        assert_eq!(
            sorted_records(&catalog.stdout, separator),
            sorted_records(&live.stdout, separator),
            "{expression:?}"
        );
        assert_eq!(catalog.stderr.is_empty(), live.stderr.is_empty());
    }
    assert_eq!(env.log_lines().len(), 1, "find does not append query logs");
}

#[test]
fn catalog_find_skips_ignored_recursion_but_walks_explicit_starts_and_references() {
    let env = Env::new("catalog-ignored");
    env.seed_ignore_file();
    env.write(
        ".ferretignore",
        b"*.tmp\nbuild/\nonly/hidden\ntarget/\n!/target/doc/**\n",
    );
    env.write("visible", b"v");
    env.write("hidden.tmp", b"h");
    env.write("build/nested/.ferretignore", b"*\n");
    env.write("build/nested/result.tmp", b"h");
    env.write("only/hidden", b"h");
    env.write("target/doc/api.html", b"visible");
    env.write("target/hidden.tmp", b"h");
    let indexed = env.run(&[os("index"), env.tree().as_os_str()]);
    assert_eq!(code(&indexed), 0, "{}", stderr(&indexed));
    let run = |args: &[&str]| {
        env.command(&args.iter().map(os).collect::<Vec<_>>())
            .current_dir(env.tree())
            .output()
            .unwrap()
    };
    let output = run(&["find", "."]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(!text.contains("hidden"));
    assert!(!text.contains("build"));
    assert!(text.contains("./target\n"));
    assert!(text.contains("./target/doc/api.html\n"));
    for start in [
        "build",
        "build/nested",
        "build/nested/result.tmp",
        "hidden.tmp",
        "only/hidden",
    ] {
        let catalog = run(&["find", start]);
        let live = run(&["find", "-I", start]);
        assert_eq!(code(&catalog), 0, "{start}: {}", stderr(&catalog));
        assert_eq!(catalog.stdout, live.stdout, "{start}");
    }
    let output = run(&["find", "only", "-empty"]);
    assert_eq!(code(&output), 0);
    assert!(output.stdout.is_empty());
    let output = run(&["find", ".", "-newer", "build/nested/result.tmp"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let output = run(&["find", ".", "-path", "./target", "-prune", "-o", "-print"]);
    assert_eq!(code(&output), 0);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("target"));
    let output = run(&["find", ".", "-name", "visible", "-delete"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(!env.at("visible").exists());
    assert!(env.at("hidden.tmp").exists());
    assert_eq!(code(&env.run(&[os("index")])), 0);
    let output = run(&["find", ".", "-type", "f", "-delete"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(env.at("hidden.tmp").exists());
    assert!(env.at("build/nested/result.tmp").exists());
    assert!(env.at("target/hidden.tmp").exists());
}

#[test]
fn catalog_find_uses_stored_stat_and_names_and_walks_opaque_directories_live() {
    let env = Env::new("catalog-stat");
    env.seed_ignore_file();
    env.write("changed", b"old");
    env.write("cheap", b"old");
    env.write("deleted", b"old");
    env.write("denied/file", b"contents");
    fs::set_permissions(env.at("denied"), fs::Permissions::from_mode(0o000)).unwrap();
    let indexed = env.run(&[os("index"), env.tree().as_os_str()]);
    fs::set_permissions(env.at("denied"), fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(code(&indexed), 0, "{}", stderr(&indexed));
    env.write("changed", b"new size");
    fs::remove_file(env.at("deleted")).unwrap();
    let run = |args: &[&str]| {
        env.command(&args.iter().map(os).collect::<Vec<_>>())
            .current_dir(env.tree())
            .output()
            .unwrap()
    };
    // Stored name, kind and stat fields remain usable after an action removes
    // this entry. New entries still need existence checks before effects.
    let output = run(&[
        "find", ".", "-name", "cheap", "-exec", "rm", "{}", ";", "-type", "f", "-print",
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(output.stdout, b"./cheap\n");
    fs::hard_link(env.at("changed"), env.base.join("alias")).unwrap();
    let output = run(&["find", ".", "-name", "changed", "-links", "1"]);
    assert_eq!(code(&output), 0);
    assert_eq!(output.stdout, b"./changed\n");
    let output = run(&["find", ".", "-name", "changed", "-size", "3c"]);
    assert_eq!(code(&output), 0);
    assert_eq!(output.stdout, b"./changed\n");
    let output = run(&["find", ".", "-name", "deleted"]);
    assert_eq!(code(&output), 0);
    assert_eq!(output.stdout, b"./deleted\n");
    let output = run(&[
        "find", ".", "-name", "changed", "-exec", "rm", "{}", ";", "-size", "3c", "-print",
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(output.stdout, b"./changed\n");
    fs::set_permissions(env.at("denied"), fs::Permissions::from_mode(0o000)).unwrap();
    let readable = fs::read_dir(env.at("denied")).is_ok();
    let output = run(&["find", "."]);
    fs::set_permissions(env.at("denied"), fs::Permissions::from_mode(0o700)).unwrap();
    // Privileged test runners can read mode 000 directories.
    if !readable {
        assert_eq!(code(&output), 1);
        assert!(stderr(&output).contains("Permission denied"));
    }
}

#[test]
fn catalog_find_refuses_unresolved_starts_and_config_can_select_live_mode() {
    let env = Env::new("catalog-config");
    env.seed_ignore_file();
    let run = |args: &[&str]| {
        env.command(&args.iter().map(os).collect::<Vec<_>>())
            .current_dir(env.tree())
            .output()
            .unwrap()
    };
    let output = run(&["find", "."]);
    assert_eq!(code(&output), 1);
    assert!(stderr(&output).contains("no index"));
    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    fs::create_dir(env.base.join("outside")).unwrap();
    env.write("new-after-index", b"contents");
    for start in ["../outside", "new-after-index", "missing"] {
        let output = run(&["find", start]);
        assert_eq!(code(&output), 1, "{start}");
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
    let output = run(&["find", "."]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(output.stderr.is_empty());
    assert!(!paths(&output).contains(&PathBuf::from("./new-after-index")));
    fs::write(
        env.base.join("home/config/ferret/config"),
        b"find_no_ignore = true\n",
    )
    .unwrap();
    let configured = run(&["find", "../outside"]);
    assert_eq!(code(&configured), 0, "{}", stderr(&configured));
    assert_eq!(configured.stdout, run(&["find", "-I", "../outside"]).stdout);
    // Config lookup must not depend on the unused state/cache directories.
    let configured = env
        .command(&[os("find"), os("../outside")])
        .current_dir(env.tree())
        .env_remove("HOME")
        .env_remove("XDG_STATE_HOME")
        .output()
        .unwrap();
    assert_eq!(code(&configured), 0, "{}", stderr(&configured));
    fs::write(env.base.join("home/config/ferret/config"), b"invalid\n").unwrap();
    assert_eq!(code(&run(&["find", "."])), 1);
    assert_eq!(code(&run(&["find", "-I", "."])), 0);
}

#[test]
fn catalog_find_crosses_nested_index_roots_and_honours_the_inner_policy() {
    // D34 boundaries have a root record, but no child name in the outer crawl.
    let env = Env::new("catalog-inner-root");
    env.seed_ignore_file();
    env.write("outer", b"visible");
    env.write("nested/inner/.ferretignore", b"hidden\n");
    env.write("nested/inner/visible", b"visible");
    env.write("nested/inner/hidden", b"ignored");
    let indexed = env.run(&[
        os("index"),
        env.tree().as_os_str(),
        env.at("nested/inner").as_os_str(),
    ]);
    assert_eq!(code(&indexed), 0, "{}", stderr(&indexed));
    let args = [os("find"), os(".")];
    let output = env.command(&args).current_dir(env.tree()).output().unwrap();
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("./nested/inner\n"));
    assert!(text.contains("./nested/inner/visible\n"));
    assert!(!text.contains("hidden"));
}

fn catalog_delete_tree(name: &str) -> Env {
    let env = Env::new(name);
    env.seed_ignore_file();
    env.write(
        ".ferretignore",
        b"target/\n!/target/doc/**\n!/target/visible\nnode_modules/\n!/node_modules/visible\n",
    );
    for path in [
        "sub/visible",
        "node_modules/visible",
        "target/doc/visible",
        "target/visible",
        "target/hidden",
    ] {
        env.write(path, b"contents");
    }
    let indexed = env.run(&[os("index"), env.tree().as_os_str()]);
    assert_eq!(code(&indexed), 0, "{}", stderr(&indexed));
    // Change live mtimes after indexing to guard the decided snapshot contract
    // even when a deletion expression asks for those times.
    let old = SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60);
    for path in [
        ".ferretignore",
        "sub/visible",
        "node_modules/visible",
        "target/doc/visible",
        "target/visible",
        "target/hidden",
        "sub",
        "node_modules",
        "target/doc",
        "target",
        ".",
    ] {
        File::open(env.at(path))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
    }
    env
}

#[test]
fn catalog_find_keeps_indexed_mtime_after_live_timestamps_change() {
    let env = catalog_delete_tree("catalog-delete-mtime");
    let output = env
        .command(&[os("find"), os("."), os("-mmin"), os("+720"), os("-delete")])
        .current_dir(env.tree())
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(output.stderr.is_empty());
    for dir in ["sub", "node_modules", "target/doc"] {
        assert!(env.at(dir).exists(), "{dir} used live mtime");
    }
    assert!(env.at("target/hidden").exists());
    assert!(env.tree().exists());
}

#[test]
fn catalog_find_counts_its_own_deletions_for_indexed_emptiness() {
    // Successful child deletes reduce the indexed count before -empty runs.
    let env = catalog_delete_tree("catalog-delete-empty");
    let output = env
        .command(&[
            os("find"),
            os("."),
            os("-depth"),
            os("("),
            os("-type"),
            os("f"),
            os("-name"),
            os("visible"),
            os("-or"),
            os("-type"),
            os("d"),
            os("-empty"),
            os(")"),
            os("-delete"),
        ])
        .current_dir(env.tree())
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(output.stderr.is_empty());
    for dir in ["sub", "node_modules", "target/doc"] {
        assert!(
            !env.at(dir).exists(),
            "{dir} remained after its last child was deleted"
        );
    }
    assert!(
        env.at("target").exists(),
        "ignored child must count toward -empty"
    );
    assert!(env.at("target/hidden").exists());
}

#[test]
fn catalog_find_failed_delete_does_not_reduce_indexed_emptiness() {
    let env = Env::new("catalog-delete-empty-failure");
    env.seed_ignore_file();
    env.write("kept/file", b"contents");
    let indexed = env.run(&[os("index"), env.tree().as_os_str()]);
    assert_eq!(code(&indexed), 0, "{}", stderr(&indexed));
    let output = env
        .command(&[
            os("find"),
            os("."),
            os("-depth"),
            os("-type"),
            os("d"),
            os("-delete"),
            os("-o"),
            os("-empty"),
            os("-print"),
        ])
        .current_dir(env.tree())
        .output()
        .unwrap();
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    assert!(env.at("kept").exists());
    assert!(
        !output
            .stdout
            .windows(b"./kept\n".len())
            .any(|w| w == b"./kept\n")
    );
}

#[test]
fn catalog_find_exec_rmdir_removals_do_not_change_indexed_emptiness() {
    // The child command removes `inner`; its parent still sees the raw count.
    let env = Env::new("catalog-exec-rmdir-empty");
    env.seed_ignore_file();
    fs::create_dir_all(env.at("outer/inner")).unwrap();
    let indexed = env.run(&[os("index"), env.tree().as_os_str()]);
    assert_eq!(code(&indexed), 0, "{}", stderr(&indexed));
    let output = env
        .command(&[
            os("find"),
            os("outer"),
            os("-depth"),
            os("-type"),
            os("d"),
            os("-empty"),
            os("-exec"),
            os("rmdir"),
            os("{}"),
            os(";"),
        ])
        .current_dir(env.tree())
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(env.at("outer").exists());
    assert!(!env.at("outer/inner").exists());
    assert!(output.stderr.is_empty());
}

#[test]
fn catalog_find_does_not_evaluate_names_created_by_exec() {
    // A directory's pre-order -exec creates a name before its listing.
    let env = Env::new("catalog-exec-touch-new");
    env.seed_ignore_file();
    env.write(".ferretignore", b"*.tmp\n");
    env.write("parent/child/original", b"contents");
    env.write("old.tmp", b"ignored");
    let indexed = env.run(&[os("index"), env.tree().as_os_str()]);
    assert_eq!(code(&indexed), 0, "{}", stderr(&indexed));
    let output = env
        .command(&[
            os("find"),
            os("."),
            os("-type"),
            os("d"),
            os("-exec"),
            os("touch"),
            os("{}/new.tmp"),
            os(";"),
            os("-o"),
            os("-type"),
            os("f"),
            os("-name"),
            os("*.tmp"),
            os("-print"),
        ])
        .current_dir(env.tree())
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(output.stderr.is_empty());
    let found = paths(&output);
    for (printed, path) in [
        ("./new.tmp", "new.tmp"),
        ("./parent/new.tmp", "parent/new.tmp"),
        ("./parent/child/new.tmp", "parent/child/new.tmp"),
    ] {
        assert!(!found.contains(&PathBuf::from(printed)), "{found:?}");
        assert!(env.at(path).exists());
    }
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn catalog_find_excludes_directories_created_after_indexing() {
    let env = Env::new("catalog-new-directory");
    env.seed_ignore_file();
    env.write(".ferretignore", b"*.tmp\n");
    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    env.write("new-dir/nested/new.tmp", b"contents");
    let output = env
        .command(&[
            os("find"),
            os("."),
            os("-type"),
            os("f"),
            os("-name"),
            os("*.tmp"),
            os("-size"),
            os("8c"),
        ])
        .current_dir(env.tree())
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(output.stderr.is_empty());
    assert!(output.stdout.is_empty());
}

#[test]
fn catalog_find_keeps_metadata_and_reference_values_after_live_entries_vanish() {
    // A pure index query must work even when the entire indexed tree is gone.
    let env = Env::new("catalog-vanished-tree");
    env.seed_ignore_file();
    env.write("reference", b"reference");
    env.write("child", b"old");
    std::os::unix::fs::symlink("child", env.at("link")).unwrap();
    let old = SystemTime::now() - Duration::from_secs(24 * 60 * 60);
    File::open(env.at("reference"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(old))
        .unwrap();
    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    // Move the live reference forward; default still compares against the old
    // catalog value, while -I sees the newly modified reference.
    File::open(env.at("reference"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(SystemTime::now()))
        .unwrap();
    let reference = env.at("reference");
    let tree = env.tree();
    let args = [
        os("find"),
        tree.as_os_str(),
        os("-name"),
        os("child"),
        os("-newer"),
        reference.as_os_str(),
    ];
    let output = env.run(&args);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(paths(&output), [env.at("child")]);
    fs::remove_dir_all(env.tree()).unwrap();
    let output = env.run(&args);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(paths(&output), [env.at("child")]);
    let output = env.run(&[
        os("find"),
        env.tree().as_os_str(),
        os("-name"),
        os("child"),
        os("-size"),
        os("3c"),
        os("-links"),
        os("1"),
        os("-printf"),
        os("%s|%n|%y\\n"),
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(output.stdout, b"3|1|f\n");
    let output = env.run(&[
        os("find"),
        env.tree().as_os_str(),
        os("-lname"),
        os("child"),
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(paths(&output), [env.at("link")]);
}

#[test]
fn catalog_find_guarantees_parent_order_depth_order_and_prune() {
    let env = Env::new("catalog-order");
    env.seed_ignore_file();
    for path in ["z/child", "a/child", "middle"] {
        env.write(path, b"contents");
    }
    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    let run = |expression: &[&OsStr]| {
        let tree = env.tree();
        let mut args = vec![os("find"), tree.as_os_str()];
        args.extend(expression);
        env.run(&args)
    };
    for expression in [vec![], vec![os("-depth")]] {
        let output = run(&expression);
        assert_eq!(code(&output), 0, "{}", stderr(&output));
        let found = paths(&output);
        for dir in [".", "a", "z"] {
            let parent = if dir == "." { env.tree() } else { env.at(dir) };
            let parent_at = found.iter().position(|path| *path == parent).unwrap();
            for (at, child) in found.iter().enumerate() {
                if child != &parent && child.starts_with(&parent) {
                    assert_eq!(parent_at < at, expression.is_empty());
                }
            }
        }
    }
    let output = run(&[os("-name"), os("a"), os("-prune"), os("-o"), os("-print")]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(!paths(&output).contains(&env.at("a/child")));
    let output = run(&[os("-type"), os("f"), os("-print"), os("-quit")]);
    let found = paths(&output);
    assert!(!found.is_empty());
    assert!(found.len() <= 3);
    assert!(
        found
            .iter()
            .all(|path| [env.at("a/child"), env.at("z/child"), env.at("middle")].contains(path))
    );
}

#[test]
fn catalog_find_resolves_symlink_starts_and_references_from_the_snapshot() {
    // Resolving aliases through canonicalize would fail after the live tree
    // vanished, and lexical .. would choose the link's parent incorrectly.
    let env = Env::new("catalog-snapshot-links");
    env.seed_ignore_file();
    env.write("place/inside/file", b"old");
    let reference = env.write("reference", b"reference");
    File::open(&reference)
        .unwrap()
        .set_times(
            fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(86_400)),
        )
        .unwrap();
    std::os::unix::fs::symlink("place/inside", env.at("alias")).unwrap();
    std::os::unix::fs::symlink("reference", env.at("ref-link")).unwrap();
    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    fs::remove_dir_all(env.tree()).unwrap();
    let start = env.at("alias/..");
    let output = env.run(&[
        os("find"),
        start.as_os_str(),
        os("-name"),
        os("file"),
        os("-size"),
        os("3c"),
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(paths(&output), [start.join("inside/file")]);
    let start = env.at("alias");
    let link = env.at("ref-link");
    let output = env.run(&[
        os("find"),
        os("-H"),
        start.as_os_str(),
        os("-name"),
        os("file"),
        os("-newer"),
        link.as_os_str(),
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(paths(&output), [start.join("file")]);
    let output = env.run(&[
        os("find"),
        os("-H"),
        start.as_os_str(),
        os("-maxdepth"),
        os("0"),
        os("-xtype"),
        os("l"),
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(paths(&output), [start]);
    let output = env.run(&[
        os("find"),
        env.at("ref-link").as_os_str(),
        os("-printf"),
        os("%Y|%l\\n"),
    ]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(output.stdout, b"f|reference\n");
}

#[test]
fn catalog_find_reports_removed_directory_descent_and_skips_deleted_files_on_repeated_starts() {
    // Catalog children outlive rm -rf and repeated -delete operands. GNU
    // faults on the removed directory, but relisting a kept directory sees
    // no previously deleted files and therefore performs no second deletion.
    let env = Env::new("catalog-effect-descent");
    env.seed_ignore_file();
    env.write("sub/child", b"contents");
    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    let output = env
        .command(&[
            os("find"),
            os("."),
            os("-type"),
            os("d"),
            os("-name"),
            os("sub"),
            os("-exec"),
            os("rm"),
            os("-rf"),
            os("{}"),
            os(";"),
        ])
        .current_dir(env.tree())
        .output()
        .unwrap();
    assert_eq!(code(&output), 1, "{}", stderr(&output));
    assert!(stderr(&output).contains("No such file or directory"));
    assert!(!env.at("sub").exists());

    let env = Env::new("catalog-effect-repeated-starts");
    env.seed_ignore_file();
    env.write("sub/child", b"contents");
    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    let output = env
        .command(&[
            os("find"),
            os("."),
            os("sub"),
            os("sub"),
            os("-type"),
            os("f"),
            os("-delete"),
        ])
        .current_dir(env.tree())
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert!(output.stderr.is_empty());
    assert!(env.at("sub").is_dir());
    assert!(!env.at("sub/child").exists());
    let env = Env::new("catalog-effect-listed-sibling");
    env.seed_ignore_file();
    env.write("a", b"a");
    env.write("b", b"b");
    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    let output = env
        .command(&[
            os("find"),
            os("."),
            os("-name"),
            os("a"),
            os("-exec"),
            os("rm"),
            os("./b"),
            os(";"),
            os("-o"),
            os("-name"),
            os("b"),
            os("-print"),
        ])
        .current_dir(env.tree())
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(output.stdout, b"./b\n");
}

#[test]
fn catalog_find_candidate_guards_preserve_earlier_effects_and_followed_directory_links() {
    let env = Env::new("catalog-candidate-guards");
    env.seed_ignore_file();
    env.write("dir/file", b"contents");
    std::os::unix::fs::symlink("dir", env.at("link")).unwrap();
    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    for expression in [
        vec!["-print", "-type", "d"],
        vec!["-type", "d", "-o", "-print"],
        vec!["-type", "f", "-o", "-type", "d"],
        vec!["-follow", "-type", "d"],
        vec!["-print", "-name", "file"],
        vec!["-name", "file", "-o", "-print"],
        vec!["-name", "file", "-o", "-name", "link"],
        vec!["-name", "file", "-exec", "echo", "{}", ";"],
        vec!["-not", "-name", "file"],
    ] {
        let mut args = vec![os("find"), os(".")];
        args.extend(expression.iter().map(os));
        let catalog = env.command(&args).current_dir(env.tree()).output().unwrap();
        args.insert(1, os("-I"));
        let live = env.command(&args).current_dir(env.tree()).output().unwrap();
        assert_eq!(
            code(&catalog),
            code(&live),
            "{expression:?}: {}",
            stderr(&catalog)
        );
        assert_eq!(
            sorted_records(&catalog.stdout, b'\n'),
            sorted_records(&live.stdout, b'\n'),
            "{expression:?}"
        );
        assert_eq!(catalog.stderr.is_empty(), live.stderr.is_empty());
    }
}

#[test]
fn resident_search_checks_every_section_before_emitting_rows() {
    // D46's common engine warms and checks every section, including fields
    // a name-only query will not use. Corruption fails before any output.
    let env = Env::new("lazy-checksum-query");
    env.write("checked.txt", b"content\n");
    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    let before = env.run(&[os("search"), os("checked")]);
    assert_eq!(code(&before), 0);
    let catalog = ferret_catalog::Catalog::open(&env.index())
        .unwrap()
        .unwrap();
    let mut offset = catalog.head_len() as usize;
    for (section, len) in catalog.section_sizes() {
        if section == ferret_catalog::Section::Size {
            break;
        }
        offset += len as usize;
    }
    let path = ferret_catalog::Catalog::snapshot_path(&env.index())
        .unwrap()
        .unwrap();
    let mut bytes = fs::read(&path).unwrap();
    bytes[offset] ^= 1;
    fs::write(path, bytes).unwrap();
    let names = env.run(&[os("search"), os("checked")]);
    assert_eq!(code(&names), 3, "{}", stderr(&names));
    assert!(names.stdout.is_empty());
    assert!(stderr(&names).contains("corrupt: size"));
    let metadata = env.run(&[os("search"), os("checked"), os("size:>0")]);
    assert_eq!(code(&metadata), 3, "{}", stderr(&metadata));
    assert!(stderr(&metadata).contains("corrupt: size"));
}

#[test]
fn stats_effective_depth_census_matches_a_fresh_checkpoint() {
    use ferret_catalog::log::{ChangeSet, Record, Writer};
    use ferret_catalog::{Catalog, Content, Kind, Stat, Transaction};
    let env = Env::new("stats-overlay-depth");
    let oracle = Env::new("stats-oracle-depth");
    let st = |ino, mode| Stat {
        dev: 1,
        ino,
        mode,
        nlink: 1,
        size: 17,
        ..Stat::default()
    };
    for (index, moved) in [(env.index(), false), (oracle.index(), true)] {
        let mut tx = Transaction::begin(&index, 1).unwrap();
        let mut b = tx.batch();
        let r = b.root(b"/r", st(1, 0o040755));
        b.entry_count(r, 1);
        let parent = if moved {
            let d = b.dir(r, b"later", st(4, 0o040755));
            b.entry_count(d, 1);
            d
        } else {
            r
        };
        let a = b.dir(parent, b"a", st(2, 0o040755));
        b.entry_count(a, 1);
        b.file(a, b"file.rs", st(3, 0o100644), Content::Hashed([1; 16]));
        tx.add(b);
        tx.commit().unwrap();
    }
    let c = Catalog::open(&env.index()).unwrap().unwrap();
    c.load_all().unwrap();
    let r = c.roots().next().unwrap().0;
    let name = c.lookup(r, b"a").unwrap();
    let a = c.name(name).child;
    let id = c.next_inode().0;
    let edge = c.next_name().0;
    assert!(id > a.0);
    let mut w = Writer::open(&env.index()).unwrap();
    let p = ferret_catalog::log::Published::open(&env.index())
        .unwrap()
        .unwrap();
    let mut counters = p.counters();
    let mut counts = p.counts();
    counters[0] += 1;
    counters[1] += 1;
    counts[0] += 1;
    counts[1] += 1;
    counts[2] += 1;
    w.commit(
        w.generation(),
        &ChangeSet {
            counters,
            counts,
            records: vec![
                Record::LifePut {
                    id,
                    kind: Kind::Dir,
                    flags: 0,
                    names: 1,
                },
                Record::InodePut {
                    id,
                    kind: Kind::Dir,
                    state: ferret_catalog::ContentState::Unindexed,
                    doc: None,
                    stat: st(4, 0o040755),
                },
                Record::NamePut {
                    id: edge,
                    parent: r.0,
                    child: id,
                    name: b"later".to_vec(),
                },
                Record::DirPut {
                    id,
                    name: Some(edge),
                    entries: Some(1),
                    flags: 4,
                    retained_at: None,
                },
                Record::NamePut {
                    id: name.0,
                    parent: id,
                    child: a.0,
                    name: b"a".to_vec(),
                },
            ],
        },
    )
    .unwrap();
    drop(w);
    let outputs = [env.run(&[os("stats")]), oracle.run(&[os("stats")])];
    for output in &outputs {
        assert_eq!(
            code(output),
            0,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let text: Vec<_> = outputs
        .iter()
        .map(|o| String::from_utf8(o.stdout.clone()).unwrap())
        .collect();
    assert_eq!(
        text[0].split_once("\ncontent (file inodes)").unwrap().1,
        text[1].split_once("\ncontent (file inodes)").unwrap().1
    );
}

#[test]
fn transient_root_fault_retains_searchable_subtree_and_find_uses_the_live_boundary() {
    let env = Env::new("retained-find");
    env.seed_ignore_file();
    let old = env.write("locked/old.txt", b"old\n");
    let locked = old.parent().unwrap().to_owned();
    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    let displaced = env.tree().with_extension("displaced");
    fs::rename(env.tree(), &displaced).unwrap();
    let recrawl = env.run(&[os("index")]);
    fs::rename(&displaced, env.tree()).unwrap();
    assert_eq!(code(&recrawl), 0, "{}", stderr(&recrawl));
    assert!(stderr(&recrawl).contains("protected scope"));
    assert_eq!(
        paths(&env.run(&[os("search"), os("old.txt")])),
        std::slice::from_ref(&old)
    );
    // The retained indexed listing cannot see this addition. Find must cross
    // its unknown-count boundary live, including metadata through the handle.
    let added = env.write("locked/new.txt", b"new content\n");
    let found = env.run(&[
        os("find"),
        env.tree().as_os_str(),
        os("-name"),
        os("*.txt"),
        os("-print"),
    ]);
    let mut actual = paths(&found);
    actual.sort();
    let mut expected = vec![old.clone(), added];
    expected.sort();
    assert_eq!(code(&found), 0, "{}", stderr(&found));
    assert_eq!(actual, expected);
    fs::write(&old, b"updated live content with a different length\n").unwrap();
    let metadata = env.run(&[
        os("find"),
        locked.as_os_str(),
        os("-name"),
        os("old.txt"),
        os("-printf"),
        os("%s"),
    ]);
    assert_eq!(code(&metadata), 0, "{}", stderr(&metadata));
    assert_eq!(
        String::from_utf8_lossy(&metadata.stdout),
        fs::metadata(&old).unwrap().len().to_string()
    );
}

/// With no content index, `text:` reads every document (each is
/// uncovered), and the log records the content plan as it ran.
#[test]
fn text_search_reads_uncovered_documents_and_logs_the_content_plan() {
    let env = Env::new("text");
    let alpha = env.write("a.txt", b"alpha beta\n");
    let gamma = env.write("sub/b.txt", b"gamma requestHandler\n");
    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    fs::remove_dir_all(env.base.join("index/index")).unwrap();
    let found = env.run(&[os("search"), os("text:alpha")]);
    assert_eq!(
        paths(&found),
        std::slice::from_ref(&alpha),
        "{}",
        stderr(&found)
    );
    let phrase = env.run(&[os("search"), os("text:request handler")]);
    assert_eq!(paths(&phrase), std::slice::from_ref(&gamma));
    let either = env.run(&[
        os("search"),
        os("("),
        os("text:alpha"),
        os("OR"),
        os("text:gamma"),
        os(")"),
        os("ext:txt"),
    ]);
    assert_eq!(paths(&either), [alpha.clone(), gamma.clone()]);
    // NOT holds on rows with no document: the directory. (The root is not
    // a row.)
    let not = env.run(&[os("search"), os("NOT"), os("text:alpha")]);
    let mut expected = vec![env.at("sub"), gamma];
    expected.sort();
    let mut got = paths(&not);
    got.sort();
    assert_eq!(got, expected);

    let lines = env.log_lines();
    let line = lines.iter().find(|l| l.contains("text:alpha")).unwrap();
    for part in [
        r#""content":{"driver":"#,
        r#""atoms":[{"estimate":"#,
        r#""uncovered":2,"live":2,"#,
        r#""verified":"#,
        "content: ",
    ] {
        assert!(line.contains(part), "{part} in {line}");
    }
}

/// A file named `OR` is reached through `name:` (folded) or `case:`
/// (exact); the bare operator is a usage error.
#[test]
fn a_file_named_like_an_operator_is_reached_through_a_prefix() {
    let env = Env::new("or-file");
    let or = env.write("OR", b"x\n");
    let color = env.write("color.txt", b"x\n");
    env.write("other.txt", b"x\n");
    env.run(&[os("index"), env.tree().as_os_str()]);
    let folded = env.run(&[os("search"), os("name:OR")]);
    let mut got = paths(&folded);
    got.sort();
    assert_eq!(got, [or.clone(), color]);
    assert_eq!(paths(&env.run(&[os("search"), os("case:OR")])), [or]);
    let bare = env.run(&[os("search"), os("OR")]);
    assert_eq!(code(&bare), 2);
    assert!(stderr(&bare).contains("`OR` needs a query on each side"));
    let help = env.run(&[os("help")]);
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("name:OR"), "{help}");
}

/// More uncovered documents than the bound refuse a `text:` query, saying
/// how to proceed; `--scan-uncovered` reads them.
#[test]
fn too_many_uncovered_documents_need_scan_uncovered() {
    let env = Env::new("incomplete");
    let count = ferret_query::UNCOVERED_BOUND as usize + 1;
    for i in 0..count {
        // Distinct content: equal bytes would be one document.
        env.write(&format!("d{}/f{i}", i % 64), format!("x{i}\n").as_bytes());
    }
    let hit = env.write("hit.txt", b"needle\n");
    assert_eq!(code(&env.run(&[os("index"), env.tree().as_os_str()])), 0);
    let covered = env.run(&[os("search"), os("text:needle")]);
    assert_eq!(paths(&covered), [hit.clone()], "{}", stderr(&covered));
    fs::remove_dir_all(env.base.join("index/index")).unwrap();
    let refused = env.run(&[os("search"), os("text:needle")]);
    assert_eq!(code(&refused), 3, "{}", stderr(&refused));
    let message = stderr(&refused);
    let total = count + 1;
    assert!(
        message.contains(&format!("{total} of {total}")) && message.contains("--scan-uncovered"),
        "{message}"
    );
    assert!(
        env.log_lines()
            .last()
            .unwrap()
            .contains(r#""error":"index incomplete""#)
    );
    // A name-only query is unaffected.
    assert_eq!(code(&env.run(&[os("search"), os("hit")])), 0);
    let scanned = env.run(&[os("search"), os("--scan-uncovered"), os("text:needle")]);
    assert_eq!(paths(&scanned), [hit], "{}", stderr(&scanned));
}
