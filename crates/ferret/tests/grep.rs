//! Owned differential fixtures, derived from the compatibility report's
//! lessons. No private corpus commands or data are copied here. Every case runs
//! the real CLI and pinned rg, comparing file groups, status and diagnostic
//! presence.

// Fixture helpers run outside test functions; setup and oracle failures must
// panic instead of turning a broken fixture into a compatibility agreement.
#![allow(clippy::unwrap_used)]

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Fixture {
    base: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!("ferret-grep-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("tree/sub")).unwrap();
        fs::create_dir_all(base.join("tree/.hidden")).unwrap();
        fs::create_dir_all(base.join("home/config/ferret")).unwrap();
        fs::write(base.join("home/config/ferret/ignore"), b"# owned fixture\n").unwrap();
        let fixture = Self { base };
        for (path, bytes) in [
            ("one.rs", &b"red lantern\nBLUE\nred red\n"[..]),
            ("two.txt", b"green lantern\n42\n"),
            ("sub/three.rs", b"red delta\n"),
            ("sub/extra.md", b"other\n"),
            (".dot.rs", b"red hidden\n"),
            (".hidden/deep.rs", b"red hidden directory\n"),
            ("crlf", b"red\r\nblue\r\n"),
            ("last", b"red final"),
            ("blank", b"\n"),
            ("empty", b""),
            ("bom", b"\xef\xbb\xbfred\n"),
            ("invalid", b"\xff red\n"),
            ("unicode", "Kilo\nÉclair\nſea\n".as_bytes()),
            ("-dash", b"-red\n"),
        ] {
            fixture.write(path, bytes);
        }
        symlink("one.rs", fixture.tree().join("link")).unwrap();
        symlink("absent", fixture.tree().join("dangling")).unwrap();
        symlink("sub", fixture.tree().join("dirlink")).unwrap();
        let indexed = fixture.ferret(&["index", "."], None);
        assert!(indexed.status.success(), "{indexed:?}");
        fixture
    }
    fn tree(&self) -> PathBuf {
        self.base.join("tree")
    }
    fn write(&self, path: &str, bytes: &[u8]) {
        fs::write(self.tree().join(path), bytes).unwrap();
    }
    fn command(&self, binary: &OsStr) -> Command {
        let mut command = Command::new(binary);
        command
            .current_dir(self.tree())
            .env_clear()
            .env("HOME", self.base.join("home"))
            .env("XDG_CONFIG_HOME", self.base.join("home/config"))
            .env("XDG_DATA_HOME", self.base.join("home/data"))
            .env("XDG_STATE_HOME", self.base.join("home/state"))
            .env("XDG_RUNTIME_DIR", self.base.join("home/runtime"))
            .env("FERRET_NO_DAEMON", "1")
            .env("LC_ALL", "C.UTF-8")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
    fn ferret(&self, args: &[&str], input: Option<&[u8]>) -> Output {
        let mut command = self.command(OsStr::new(env!("CARGO_BIN_EXE_ferret")));
        command
            .arg("--index")
            .arg(self.base.join("index"))
            .args(args);
        run(command, input)
    }
    fn oracle(&self, args: &[&str], input: Option<&[u8]>) -> Output {
        let binary = std::env::var_os("FERRET_RG")
            .unwrap_or_else(|| panic!("nix develop supplies the pinned FERRET_RG oracle"));
        let version = Command::new(&binary).arg("--version").output().unwrap();
        assert!(
            version.stdout.starts_with(b"ripgrep 15.2.0\n"),
            "wrong oracle: {version:?}"
        );
        let mut command = self.command(&binary);
        command.args(args);
        run(command, input)
    }
    fn compare(&self, args: &[&str], input: Option<&[u8]>) -> Output {
        let oracle = self.oracle(args, input);
        let mut ferret_args = vec!["grep"];
        ferret_args.extend_from_slice(args);
        let actual = self.ferret(&ferret_args, input);
        assert_eq!(
            actual.status.code(),
            oracle.status.code(),
            "{args:?}: {actual:?} vs {oracle:?}"
        );
        assert_eq!(
            actual.stderr.is_empty(),
            oracle.stderr.is_empty(),
            "{args:?}: {actual:?} vs {oracle:?}"
        );
        assert_eq!(
            groups(&actual.stdout, &self.tree()),
            groups(&oracle.stdout, &self.tree()),
            "{args:?}: {actual:?} vs {oracle:?}"
        );
        actual
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn run(mut command: Command, input: Option<&[u8]>) -> Output {
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().unwrap();
    if let Some(input) = input {
        child.stdin.take().unwrap().write_all(input).unwrap();
    }
    child.wait_with_output().unwrap()
}

// Preserve line order within a file. Only groups may move relative to each
// other. Fixtures have colon-free names and contents; direct output stays one
// group. A sorted-line comparison would hide line-order and duplicate defects.
fn groups(bytes: &[u8], root: &Path) -> Vec<Vec<u8>> {
    let mut groups: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for line in bytes.split_inclusive(|&b| b == b'\n') {
        let prefix = line
            .strip_suffix(b"\n")
            .unwrap_or(line)
            .split(|&b| b == b':')
            .next()
            .unwrap_or_default();
        let key = if prefix == b"<stdin>" || root.join(OsStr::from_bytes(prefix)).is_file() {
            prefix.to_vec()
        } else {
            Vec::new()
        };
        if let Some((last, group)) = groups.last_mut()
            && *last == key
        {
            group.extend_from_slice(line);
        } else {
            groups.push((key, line.to_vec()));
        }
    }
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    groups.into_iter().map(|(_, v)| v).collect()
}

macro_rules! differential {
    ($($name:ident: [$($arg:expr),*] $(, stdin = $input:expr)?;)+) => { $(
        #[test]
        fn $name() {
            let fixture = Fixture::new(stringify!($name));
            let input: Option<&[u8]> = differential!(@input $($input)?);
            fixture.compare(&[$($arg),*], input);
        }
    )+ };
    (@input) => { None };
    (@input $input:expr) => { Some($input) };
}

differential! {
    literal_positive: ["red", "one.rs"];
    literal_negative: ["violet", "one.rs"];
    sensitive_positive: ["BLUE", "one.rs"];
    sensitive_negative: ["blue", "one.rs"];
    numbered_positive: ["-n", "red", "one.rs"];
    numbered_negative: ["-n", "violet", "one.rs"];
    quiet_positive: ["-q", "red", "one.rs"];
    quiet_negative: ["-q", "violet", "one.rs"];
    clustered_positive: ["-nq", "red", "one.rs"];
    clustered_negative: ["-qn", "violet", "one.rs"];
    default_directory_with_closed_stdin: ["red"];
    recursive_positive: ["red", "."];
    recursive_negative: ["violet", "."];
    short_positive: ["r", "."];
    short_negative: ["ZZ", "."];
    broad_positive: ["[a-z]+", "."];
    broad_negative: ["[0-9]{8}", "."];
    alternate_positive: ["lantern|delta", "."];
    alternate_negative: ["violet|purple", "."];
    anchors_positive: ["\\Ared delta\\z", "sub/three.rs"];
    anchors_negative: ["\\Ared\\z", "one.rs"];
    anchor_each_line: ["\\ABLUE\\z", "one.rs"];
    empty_pattern: ["", "one.rs"];
    empty_input: ["", "empty"];
    blank_line: ["^$", "blank"];
    class_intersection: ["[a-z&&[^aeiou]]{3}", "one.rs"];
    class_difference_negative: ["[a-z&&[^a-z]]", "one.rs"];
    unicode_inline: ["(?i)kilo", "unicode"];
    unicode_sensitive_negative: ["kilo", "unicode"];
    scoped_flags: ["(?i:blue)|red", "one.rs"];
    escaped_digit: ["\\d+", "two.txt"];
    crlf_preserved: ["-n", "red", "crlf"];
    unterminated_line: ["-n", "final$", "last"];
    bom_anchor: ["^red$", "bom"];
    invalid_bytes: ["(?-u:\\xFF)", "invalid"];
    explicit_symlink: ["red", "link"];
    explicit_hidden: ["red", ".dot.rs"];
    explicit_glob_bypass: ["-g!*.rs", "red", "one.rs"];
    glob_positive: ["-g*.rs", "red", "."];
    glob_negative: ["-g!*.rs", "red", "."];
    glob_explicit_start: ["-gsub/*.rs", "red", "sub"];
    glob_explicit_hidden_start: ["-g*.rs", "red", ".hidden"];
    glob_explicit_excluded_start: ["-g!sub", "red", "sub"];
    glob_slash: ["-gsub/*.rs", "red", "."];
    glob_globstar: ["-g**/*.rs", "red", "."];
    glob_braces: ["-g*.{rs,md}", "red", "."];
    glob_classes: ["-g[o-z]*.rs", "red", "."];
    glob_last_positive: ["-g!*.rs", "-gone.rs", "red", "."];
    glob_last_negative: ["-g*.rs", "-g!one.rs", "red", "."];
    pruned_parent: ["-g!sub", "-gsub/three.rs", "red", "."];
    hidden_glob: ["-g*", "red", ".dot.rs"];
    stdin_implicit: ["-n", "red"], stdin = b"blue\nred\n";
    stdin_no_match: ["violet"], stdin = b"red\n";
    stdin_bom: ["^red$"], stdin = b"\xef\xbb\xbfred\n";
    stdin_plus_file: ["-n", "red", "-", "one.rs"], stdin = b"red pipe\n";
    explicit_file_with_pipe: ["red", "one.rs"], stdin = b"red pipe\n";
    explicit_directory_with_pipe: ["red", "sub"], stdin = b"red pipe\n";
    long_aliases: ["--line-number", "--glob=*.rs", "red", "."];
    option_separator: ["--", "-red", "-dash"];
    option_after_operand: ["red", "one.rs", "-n"];
    repeated_number: ["-nn", "red", "one.rs"];
    color_never: ["--color=never", "red", "one.rs"];
    missing_file: ["red", "absent"];
    dangling_file: ["red", "dangling"];
    quiet_after_error: ["-q", "red", "absent", "one.rs"];
    nonquiet_after_error: ["red", "absent", "one.rs"];
    malformed_regex: ["[", "one.rs"];
    literal_lf_refused: ["red\nBLUE", "one.rs"];
    escaped_lf_refused: ["\\n", "one.rs"];
    malformed_glob: ["-g[", "red", "."];
}

#[test]
fn snapshot_absence_and_current_candidates_are_separate_from_direct_input() {
    let fixture = Fixture::new("snapshot");
    fixture.write("new.rs", b"red new\n");
    fixture.write("two.txt", b"red newly changed\n");
    fixture.write("one.rs", b"red updated\n");
    fs::remove_file(fixture.tree().join("sub/three.rs")).unwrap();
    let output = fixture.ferret(&["grep", "red", "."], None);
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("one.rs:red updated"));
    assert!(
        !text.contains("new.rs") && !text.contains("two.txt") && !text.contains("three.rs"),
        "{text}"
    );
    fixture.compare(&["red", "new.rs", "two.txt"], None);
}

#[test]
fn no_content_index_automatically_scans_all_catalogued_documents() {
    let fixture = Fixture::new("no-trigrams");
    fs::remove_dir_all(fixture.base.join("index/index")).unwrap();
    fixture.compare(&["red", "."], None);
    fixture.compare(&["z", "."], None);
}

#[test]
fn utf16_is_the_approved_encoding_difference() {
    let fixture = Fixture::new("utf16-difference");
    fixture.write("utf16", b"\xff\xfer\0e\0d\0\n\0");
    let rg = fixture.oracle(&["^red$", "utf16"], None);
    let ferret = fixture.ferret(&["grep", "^red$", "utf16"], None);
    assert_eq!(rg.status.code(), Some(0));
    assert_eq!(ferret.status.code(), Some(1));
    assert!(ferret.stdout.is_empty() && ferret.stderr.is_empty());
}

#[test]
fn unsupported_tiers_refuse_before_output() {
    let fixture = Fixture::new("refusals");
    for flag in [
        "--no-files",
        "-P",
        "-m1",
        "-f",
        "-H",
        "-N",
        "--no-line-number",
        "--heading-off",
        "--live",
        "-u",
        "-a",
        "-U",
        "-r",
        "--json",
        "-j2",
        "--color=always",
        "--no-quiet",
    ] {
        let output = fixture.ferret(&["grep", flag, "red", "."], None);
        assert_eq!(output.status.code(), Some(2), "{flag}: {output:?}");
        assert!(
            output.stdout.is_empty() && !output.stderr.is_empty(),
            "{flag}: {output:?}"
        );
    }
}

#[test]
fn direct_inputs_help_and_version_do_not_need_an_index_or_config() {
    let fixture = Fixture::new("no-index");
    fs::remove_dir_all(fixture.base.join("index")).unwrap();
    fixture.compare(&["red", "one.rs"], None);
    fixture.compare(&["red"], Some(b"red pipe\n"));
    for flag in ["--help", "--version", "-h", "-V"] {
        let mut command = fixture.command(OsStr::new(env!("CARGO_BIN_EXE_ferret")));
        command
            .args(["grep", flag])
            .env("RIPGREP_CONFIG_PATH", "/missing");
        let output = run(command, None);
        assert!(
            output.status.success() && !output.stdout.is_empty() && output.stderr.is_empty(),
            "{output:?}"
        );
    }
    let output = fixture.ferret(&["grep", "red", "."], None);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty() && !output.stderr.is_empty());
}

#[test]
fn changed_candidates_over_the_cap_report_errors() {
    let fixture = Fixture::new("changed-size");
    fixture.write("one.rs", &vec![b'r'; 8 * 1024 * 1024 + 1]);
    let output = fixture.ferret(&["grep", "lantern", "."], None);
    assert_eq!(output.status.code(), Some(2));
    assert!(!output.stderr.is_empty());
}

#[test]
fn no_search_log_or_find_configuration_is_used() {
    let fixture = Fixture::new("config");
    fs::write(
        fixture.base.join("home/config/ferret/config"),
        b"find_no_ignore = nonsense\n",
    )
    .unwrap();
    let log = fixture.base.join("home/state/ferret/log.jsonl");
    let before = fs::read(&log).unwrap();
    fixture.compare(&["red", "."], None);
    assert_eq!(fs::read(&log).unwrap(), before);
    let mut command = fixture.command(OsStr::new(env!("CARGO_BIN_EXE_ferret")));
    command
        .args(["grep", "red", "one.rs"])
        .env("RIPGREP_CONFIG_PATH", "/missing");
    let output = run(command, None);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty() && !output.stderr.is_empty());
}

#[test]
fn binary_message_is_the_documented_difference_and_quiet_agrees() {
    let fixture = Fixture::new("binary");
    fixture.write("binary", b"red before\ntrailer\0after\n");
    let oracle = fixture.oracle(&["red", "binary"], None);
    let actual = fixture.ferret(&["grep", "red", "binary"], None);
    assert_eq!(oracle.status.code(), Some(0));
    assert_eq!(actual.status.code(), Some(0));
    assert_eq!(actual.stdout, b"binary: binary file matches\n");
    assert!(oracle.stdout.windows(7).any(|w| w == b"binary "));
    fixture.compare(&["-q", "red", "binary"], None);
}

#[test]
fn unindexed_directory_is_a_fatal_presearch_error_even_in_quiet_mode() {
    let fixture = Fixture::new("unindexed");
    let outside = fixture.base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    for args in [
        vec!["grep", "red", "one.rs", outside.to_str().unwrap()],
        vec!["grep", "-q", "red", "one.rs", outside.to_str().unwrap()],
    ] {
        let output = fixture.ferret(&args, None);
        assert_eq!(output.status.code(), Some(2));
        assert!(
            output.stdout.is_empty() && !output.stderr.is_empty(),
            "{output:?}"
        );
    }
}

#[test]
fn glob_prunes_an_opaque_live_fallback_before_reading_its_directory() {
    let fixture = Fixture::new("prune-fallback");
    fixture.write(".ferretignore", b"/private/\n");
    fs::create_dir_all(fixture.tree().join("private/denied")).unwrap();
    fixture.write("private/denied/leaf", b"red\n");
    assert!(fixture.ferret(&["index", "."], None).status.success());
    let denied = fixture.tree().join("private/denied");
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o000)).unwrap();
    let output = fixture.compare(&["-g!denied", "red", "private"], None);
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn ignore_visibility_is_the_documented_d74_difference() {
    let fixture = Fixture::new("visibility");
    fixture.write(".ignore", b"*.cache\n");
    fixture.write("extra.cache", b"lantern\n");
    assert!(fixture.ferret(&["index", "."], None).status.success());
    let rg = fixture.oracle(&["lantern", "."], None);
    let ferret = fixture.ferret(&["grep", "lantern", "."], None);
    assert_eq!(rg.status.code(), Some(0));
    assert_eq!(ferret.status.code(), Some(0));
    let extra = b"./extra.cache:lantern\n";
    assert!(ferret.stdout.windows(extra.len()).any(|w| w == extra));
    assert!(!rg.stdout.windows(extra.len()).any(|w| w == extra));
}

#[test]
fn spill_output_preserves_a_large_group_and_unterminated_last_line() {
    let fixture = Fixture::new("spill");
    fixture.write(
        "large",
        &[vec![b'x'; 200_000], b"red\nred".to_vec()].concat(),
    );
    fixture.compare(&["-n", "red", "large", "one.rs"], None);
}

#[test]
fn glob_literals_and_class_braces_keep_their_argv_meaning() {
    let fixture = Fixture::new("glob-literals");
    for path in [
        "space ",
        "{literal}",
        "nested.rs",
        "literal#",
        "#literal",
        "a]suffix",
    ] {
        fixture.write(path, b"red\n");
    }
    assert!(fixture.ferret(&["index", "."], None).status.success());
    for glob in [
        "space ",
        r"space\ ",
        "#literal",
        "[[:alpha:]]*",
        "[z-a]",
        "[{]literal[}]",
        "{one,{two,nested}}.rs",
        "literal#",
    ] {
        fixture.compare(&["-g", glob, "red", "."], None);
    }
}

#[test]
fn read_denial_is_an_error_and_special_operands_are_refused() {
    let fixture = Fixture::new("read-denial");
    let path = fixture.tree().join("one.rs");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    let result = fixture.compare(&["red", "one.rs"], None);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(result.status.code(), Some(2));
    let result = fixture.ferret(&["grep", "red", "/dev/null"], None);
    assert_eq!(result.status.code(), Some(2));
    assert!(result.stdout.is_empty() && !result.stderr.is_empty());
}

#[test]
fn non_utf8_filename_bytes_are_preserved() {
    let fixture = Fixture::new("path-bytes");
    let path = OsStr::from_bytes(b"name-\xff");
    fs::write(fixture.tree().join(path), b"red\n").unwrap();
    let mut command = fixture.command(OsStr::new(env!("CARGO_BIN_EXE_ferret")));
    command.args([
        OsStr::new("grep"),
        OsStr::new("-n"),
        OsStr::new("red"),
        path,
        OsStr::new("one.rs"),
    ]);
    let actual = run(command, None);
    let mut command = fixture.command(&std::env::var_os("FERRET_RG").unwrap());
    command.args([
        OsStr::new("-n"),
        OsStr::new("red"),
        path,
        OsStr::new("one.rs"),
    ]);
    let oracle = run(command, None);
    assert_eq!(actual.status.code(), oracle.status.code());
    assert_eq!(
        groups(&actual.stdout, &fixture.tree()),
        groups(&oracle.stdout, &fixture.tree())
    );
    assert!(actual.stdout.windows(7).any(|w| w == b"name-\xff:"));
}

// R1/P2: the writer must remain open until grep exits. A newline or EOF
// must not be needed to notice a NUL already received by the real CLI.
#[test]
fn nul_stops_stdin_while_its_producer_holds_the_stream_open() {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    let fixture = Fixture::new("nul-open-producer");
    for (bytes, status, expected) in [
        (&b"\0"[..], 1, &b""[..]),
        (
            &b"red\n\0"[..],
            0,
            &b"(standard input): binary file matches\n"[..],
        ),
        (
            &b"red\0"[..],
            0,
            &b"(standard input): binary file matches\n"[..],
        ),
    ] {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let (release, held) = mpsc::channel();
        let producer = thread::spawn(move || {
            writer.write_all(bytes).unwrap();
            held.recv().unwrap();
        });
        let fd: OwnedFd = reader.into();
        let mut command = fixture.command(OsStr::new(env!("CARGO_BIN_EXE_ferret")));
        let child = command
            .args(["grep", "red", "-"])
            .stdin(Stdio::from(fd))
            .spawn()
            .unwrap();
        let (finished, completion) = mpsc::channel();
        let waiter =
            thread::spawn(move || finished.send(child.wait_with_output().unwrap()).unwrap());
        let observed = completion.recv_timeout(Duration::from_secs(5));
        release.send(()).unwrap();
        producer.join().unwrap();
        let output = match observed {
            Ok(output) => output,
            Err(error) => {
                waiter.join().unwrap();
                panic!("grep needed producer EOF after NUL: {error}");
            }
        };
        waiter.join().unwrap();
        assert_eq!(output.status.code(), Some(status), "{output:?}");
        assert_eq!(output.stdout, expected);
        assert!(output.stderr.is_empty());
    }
}

// R1/P3: creating the first group's spill fails. Explicit files, catalogued
// files and stdin must stop before the subsequent small matching file.
#[test]
fn spill_failure_stops_before_the_next_matching_file() {
    let fixture = Fixture::new("spill-failure");
    fs::create_dir_all(fixture.tree().join("ordered")).unwrap();
    fixture.write("ordered/big.txt", &b"red\n".repeat(20_000));
    fixture.write("ordered/small.txt", b"red small\n");
    assert!(fixture.ferret(&["index", "."], None).status.success());
    let mut outcomes = Vec::new();
    for args in [
        vec!["grep", "red", "ordered/big.txt", "ordered/small.txt"],
        vec!["grep", "red", "ordered"],
        vec!["grep", "red", "-", "ordered/small.txt"],
    ] {
        let mut command = fixture.command(OsStr::new(env!("CARGO_BIN_EXE_ferret")));
        if args.contains(&"-") {
            command.stdin(Stdio::from(
                fs::File::open(fixture.tree().join("ordered/big.txt")).unwrap(),
            ));
        }
        command
            .arg("--index")
            .arg(fixture.base.join("index"))
            .args(args)
            .env("TMPDIR", fixture.base.join("unavailable-spill-directory"));
        let output = run(command, None);
        outcomes.push((
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).contains("output spill"),
            output.stdout.is_empty(),
        ));
    }
    assert_eq!(
        outcomes,
        [(Some(2), true, true); 3],
        "a later file printed after output failed"
    );
}

// R2: regular virtual files can report size zero while yielding content.
// Exercise the real CLI for both an explicit operand and an ignored start's
// live fallback; no symlink fixture can stand in for that recursive route.
#[test]
fn zero_size_virtual_files_stream_explicitly_and_in_opaque_fallback() {
    let version = Path::new("/proc/version");
    let ostype = Path::new("/proc/sys/kernel/ostype");
    for path in [version, ostype] {
        let Ok(metadata) = fs::metadata(path) else {
            eprintln!(
                "skipping virtual-file regression: {} is absent",
                path.display()
            );
            return;
        };
        if !metadata.is_file() || metadata.len() != 0 {
            eprintln!(
                "skipping virtual-file regression: {} is not a zero-size regular file",
                path.display()
            );
            return;
        }
    }
    let fixture = Fixture::new("virtual-file");
    let explicit = fixture.ferret(&["grep", "^Linux", "/proc/version"], None);

    // Index only opaque child directories: avoid crawling unrelated procfs
    // contents and ensure kernel's files are supplied by the live fallback.
    fs::write(fixture.base.join("home/config/ferret/ignore"), b"*\n").unwrap();
    let indexed = fixture.ferret(&["index", "/proc/sys"], None);
    assert!(indexed.status.success(), "{indexed:?}");
    let fallback = fixture.ferret(&["grep", "-gostype", "^Linux", "/proc/sys/kernel"], None);
    assert_eq!(
        [explicit.status.code(), fallback.status.code()],
        [Some(0), Some(0)],
        "explicit={explicit:?}, fallback={fallback:?}"
    );
    assert_eq!(explicit.stdout, fs::read(version).unwrap());
    assert!(explicit.stderr.is_empty());
    let oracle = fixture.oracle(&["^Linux", "/proc/version"], None);
    assert_eq!(explicit.stdout, oracle.stdout);
    assert_eq!(explicit.status.code(), oracle.status.code());
    let mut expected = b"/proc/sys/kernel/ostype:".to_vec();
    expected.extend(fs::read(ostype).unwrap());
    assert_eq!(fallback.stdout, expected);
    assert!(fallback.stderr.is_empty(), "{fallback:?}");
}

// R2 control: sysfs may overstate its extent; actual EOF must end the scan.
#[test]
fn overreported_virtual_file_sizes_end_at_real_eof() {
    let path = Path::new("/sys/kernel/uevent_seqnum");
    let Ok(metadata) = fs::metadata(path) else {
        eprintln!(
            "skipping virtual-file EOF control: {} is absent",
            path.display()
        );
        return;
    };
    let bytes = fs::read(path).unwrap();
    assert!(metadata.len() > bytes.len() as u64);
    let fixture = Fixture::new("virtual-eof");
    let output = fixture.ferret(&["grep", "^[0-9]+$", "/sys/kernel/uevent_seqnum"], None);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse::<u64>()
            .is_ok()
    );
    assert!(output.stderr.is_empty());
}

// M2's owned cases assert actual hit/no-hit/error status as well as agreement,
// so negative self-agreements cannot stand in for positive compatibility.
macro_rules! profile_cases {
    ($($name:ident: $status:expr => [$($arg:expr),*] $(, stdin = $input:expr)?;)+) => { $(
        #[test]
        fn $name() {
            let fixture = Fixture::new(stringify!($name));
            let input: Option<&[u8]> = differential!(@input $($input)?);
            let output = fixture.compare(&[$($arg),*], input);
            assert_eq!(output.status.code(), Some($status));
        }
    )+ };
}

profile_cases! {
    folded_lines: 0 => ["-i", "blue", "one.rs"];
    folded_absent: 1 => ["--ignore-case", "aubergine", "."];
    folded_kelvin: 0 => ["-i", "KILO", "unicode"];
    folded_fixed_long_s: 0 => ["-iF", "SEA", "unicode"];
    fixed_words: 0 => ["-F", "red red", "one.rs"];
    fixed_metachar_absent: 1 => ["--fixed-strings", "red.*", "one.rs"];
    fixed_off: 0 => ["-F", "--no-fixed-strings", "red.*", "one.rs"];
    repeated_patterns: 0 => ["-e", "red", "-e", "42", "."];
    repeated_absent: 1 => ["-e", "aubergine", "-e", "cerulean", "."];
    attached_pattern: 0 => ["-inered", "one.rs"];
    long_pattern_equals: 0 => ["--regexp=42", "two.txt"];
    positional_before_pattern: 0 => ["one.rs", "-ered"];
    repeated_empty: 0 => ["-o", "-e", "", "-e", "red", "one.rs"];
    repeated_priority: 0 => ["-o", "-e", "re", "-e", "red", "one.rs"];
    duplicate_pattern_count: 0 => ["-co", "-e", "red", "-e", "red", "one.rs"];
    only_spans: 0 => ["-o", "r.d", "one.rs"];
    only_numbered: 0 => ["-no", "red", "one.rs"];
    only_absent: 1 => ["--only-matching", "aubergine", "one.rs"];
    only_empty_ascii: 0 => ["-o", "", "-"], stdin = b"abc\n";
    only_empty_unicode: 0 => ["-o", "", "-"], stdin = "é界x\n".as_bytes();
    only_scoped_unicode_empty: 0 => ["-o", "(?u:)", "-"], stdin = "é界x\n".as_bytes();
    only_byte_empty: 0 => ["-o", "(?-u:)", "-"], stdin = b"\xffx\n";
    only_after_consuming: 0 => ["-o", "a*", "-"], stdin = b"abc\n";
    only_word_boundaries: 0 => ["-o", r"\b", "-"], stdin = b"cedar stone\n";
    only_line_anchors: 0 => ["-o", "^|$", "one.rs"];
    only_empty_file: 1 => ["-o", "", "empty"];
    inverted_lines: 0 => ["-v", "red", "one.rs"];
    inverted_universe: 0 => ["--invert-match", "red", "."];
    inverted_absent_lines: 1 => ["-v", ".*", "."];
    inverted_only: 0 => ["-ov", "red", "one.rs"];
    inverted_count: 0 => ["-cv", "red", "one.rs"];
    inverted_count_only: 0 => ["-cvo", "red", "one.rs"];
    invert_off: 0 => ["-v", "--no-invert-match", "red", "one.rs"];
    names_match: 0 => ["-l", "red", "."];
    names_absent: 1 => ["--files-with-matches", "aubergine", "."];
    names_stdin: 0 => ["-l", "red", "-"], stdin = b"red\n";
    names_inverted: 0 => ["-lv", "red", "."];
    count_lines: 0 => ["-c", "red", "one.rs"];
    count_absent: 1 => ["--count", "red", "two.txt"];
    count_matches: 0 => ["-co", "red", "one.rs"];
    count_empty_matches: 0 => ["-co", "", "-"], stdin = "é界x\n".as_bytes();
    count_then_names: 0 => ["-cl", "red", "one.rs"];
    names_then_count: 0 => ["-lc", "red", "one.rs"];
    quiet_count_names: 0 => ["-qcl", "red", "one.rs"];
    quiet_names_count: 0 => ["-lcq", "red", "one.rs"];
    missing_pattern_value: 2 => ["-e"];
    repeated_invalid_pattern: 2 => ["-e", "red", "-e", "[", "one.rs"];
    fixed_linefeed_error: 2 => ["-F", "red\nBLUE", "one.rs"];
}

// M2: compare whole file groups at different D77 settings, with enough output
// to force spill. Sorting individual lines would conceal interleaving.
#[test]
fn parallel_groups_preserve_whole_spilled_files_at_one_and_four_threads() {
    let fixture = Fixture::new("parallel-groups");
    fs::create_dir_all(fixture.tree().join("groups")).unwrap();
    for file in 0..12 {
        let mut bytes = Vec::new();
        for line in 0..4000 {
            writeln!(bytes, "red file-{file:02} line-{line:04} payload").unwrap();
        }
        fixture.write(&format!("groups/part-{file:02}"), &bytes);
    }
    assert!(fixture.ferret(&["index", "."], None).status.success());
    let oracle = fixture.oracle(&["-n", "red", "groups"], None);
    assert_eq!(oracle.status.code(), Some(0));
    fs::write(
        fixture.base.join("index/settings"),
        b"version = 1\nsearch_threads = 4\n",
    )
    .unwrap();
    for threads in ["1", "4"] {
        let mut command = fixture.command(OsStr::new(env!("CARGO_BIN_EXE_ferret")));
        command
            .args(["--index"])
            .arg(fixture.base.join("index"))
            .args(["grep", "-n", "red", "groups"])
            .env("FERRET_SEARCH_THREADS", threads);
        let output = run(command, None);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert!(output.stderr.is_empty());
        let actual = groups(&output.stdout, &fixture.tree());
        assert_eq!(actual.len(), 12);
        assert_eq!(actual, groups(&oracle.stdout, &fixture.tree()));
    }
}

#[test]
fn duplicate_aliases_and_current_bytes_agree_at_each_thread_count() {
    let fixture = Fixture::new("duplicate-current");
    fs::create_dir_all(fixture.tree().join("copies")).unwrap();
    fixture.write("copies/a", b"red same\n");
    fixture.write("copies/b", b"red same\n");
    fs::hard_link(
        fixture.tree().join("copies/a"),
        fixture.tree().join("copies/link"),
    )
    .unwrap();
    assert!(fixture.ferret(&["index", "."], None).status.success());
    for stage in 0..3 {
        if stage == 1 {
            fixture.write("copies/b", b"blue now\n");
        }
        if stage == 2 {
            fixture.write("copies/a", b"blue now\n");
            fixture.write("copies/b", b"red anew\n");
        }
        for args in [
            vec!["red", "copies"],
            vec!["-cv", "red", "copies"],
            vec!["-l", "red", "copies"],
        ] {
            let oracle = fixture.oracle(&args, None);
            for threads in ["1", "4"] {
                let mut command = fixture.command(OsStr::new(env!("CARGO_BIN_EXE_ferret")));
                command
                    .arg("--index")
                    .arg(fixture.base.join("index"))
                    .arg("grep")
                    .args(&args)
                    .env("FERRET_SEARCH_THREADS", threads);
                let output = run(command, None);
                assert_eq!(output.status.code(), oracle.status.code());
                assert_eq!(output.stderr.is_empty(), oracle.stderr.is_empty());
                assert_eq!(
                    groups(&output.stdout, &fixture.tree()),
                    groups(&oracle.stdout, &fixture.tree())
                );
            }
        }
    }
}

// Keep stdin open until the test observes process exit. There are no sleeps:
// the closed-output case first observes a real output byte, then closes stdout.
#[test]
fn quiet_and_closed_stdout_stop_parallel_work_before_open_stdin() {
    use std::io::Read;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;
    let fixture = Fixture::new("parallel-cancel");
    for file in 0..8 {
        fixture.write(&format!("bulk-{file}"), &b"red payload\n".repeat(20_000));
    }
    for quiet in [true, false] {
        let (reader, writer) = UnixStream::pair().unwrap();
        let (release, held) = mpsc::channel();
        let producer = thread::spawn(move || {
            held.recv().unwrap();
            drop(writer);
        });
        let fd: OwnedFd = reader.into();
        let mut command = fixture.command(OsStr::new(env!("CARGO_BIN_EXE_ferret")));
        command.arg("grep");
        if quiet {
            command.arg("-q");
        }
        command
            .arg("red")
            .args((0..8).map(|file| format!("bulk-{file}")))
            .arg("-")
            .env("FERRET_SEARCH_THREADS", "4")
            .stdin(Stdio::from(fd));
        let mut child = command.spawn().unwrap();
        let reader = if quiet {
            None
        } else {
            let mut stdout = child.stdout.take().unwrap();
            Some(thread::spawn(move || {
                let mut byte = [0];
                stdout.read_exact(&mut byte).unwrap();
                drop(stdout);
            }))
        };
        let (finished, completion) = mpsc::channel();
        let waiter =
            thread::spawn(move || finished.send(child.wait_with_output().unwrap()).unwrap());
        let observed = completion.recv_timeout(Duration::from_secs(5));
        release.send(()).unwrap();
        producer.join().unwrap();
        let output = match observed {
            Ok(output) => output,
            Err(error) => {
                waiter.join().unwrap();
                panic!("grep waited for stdin after cancellation: {error}");
            }
        };
        waiter.join().unwrap();
        if let Some(reader) = reader {
            reader.join().unwrap();
        }
        assert_eq!(
            output.status.code(),
            Some(if quiet { 0 } else { 2 }),
            "{output:?}"
        );
        assert_eq!(output.stderr.is_empty(), quiet);
        assert!(output.stdout.is_empty());
    }
}

profile_cases! {
    quiet_before_error: 0 => ["-q", "red", "one.rs", "absent"];
}

// Review R1: trailing extended-mode comments must survive composition. rg
// 15.2 rejects these too, so assert the restored M1 syntax directly.
fn check_extended_comment(args: &[&str], expected: &[u8]) {
    let fixture = Fixture::new(if args.len() == 3 {
        "extended-comment-single"
    } else {
        "extended-comment-multiple"
    });
    let output = fixture.ferret(args, Some(b"red\nblue\n"));
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stdout, expected);
    assert!(output.stderr.is_empty());
}

#[test]
fn extended_comments_survive_single_patterns() {
    check_extended_comment(&["grep", "(?x)red # comment", "-"], b"red\n");
}

#[test]
fn extended_comments_survive_repeated_patterns() {
    check_extended_comment(
        &["grep", "-e", "(?x)red # comment", "-e", "blue", "-"],
        b"red\nblue\n",
    );
}

profile_cases! {
    repeated_flags_are_local: 0 => ["-e", "(?i)a", "-e", "b", "-"], stdin = b"A\nB\nb\n";
    repeated_flags_do_not_leak: 1 => ["-e", "(?i)a", "-e", "b", "-"], stdin = b"B\n";
}

// Review R1: lowering resolves folding once; global -i must not refold an
// argument whose inline flags explicitly disable it.
profile_cases! {
    repeated_global_fold_keeps_inline_override: 0 => ["-i", "-e", "(?-i)red", "-e", "blue", "-"], stdin = b"RED\nBLUE\nred\n";
    repeated_global_fold_does_not_refold: 1 => ["-i", "-e", "(?-i)red", "-e", "blue", "-"], stdin = b"RED\n";
}

// M3 hidden selection uses the same policy/glob layer in content and names
// mode.
profile_cases! {
    hidden_lines: 0 => ["--hidden", "red", "."];
    hidden_absent: 1 => ["--hidden", "aubergine", "."];
    hidden_off: 0 => ["--hidden", "--no-hidden", "red", "."];
    hidden_alias_cluster: 0 => ["-.n", "red", "."];
    hidden_folded: 0 => ["--hidden", "-i", "blue", "."];
    hidden_inverted: 0 => ["--hidden", "-v", "red", "."];
    hidden_matching_names: 0 => ["--hidden", "-l", "red", "."];
    hidden_counts: 0 => ["--hidden", "-c", "red", "."];
    hidden_glob_composition: 0 => ["--hidden", "-g", "*.rs", "red", "."];
    hidden_glob_excluded: 1 => ["--hidden", "-g", "!.hidden/**", "red", ".hidden"];
    hidden_repeated_enable: 0 => ["--hidden", "--no-hidden", "--hidden", "red", "."];
    hidden_value_refused: 2 => ["--hidden=yes", "red", "."];
}

// M3 names-only argv is its own profile; every positional is a path.
profile_cases! {
    files_default: 0 => ["--files"];
    files_explicit_root: 0 => ["--files", "."];
    files_hidden: 0 => ["--files", "--hidden", "."];
    files_hidden_alias: 0 => ["--files", "-.", "."];
    files_hidden_off: 0 => ["--files", "--hidden", "--no-hidden", "."];
    files_positive_glob: 0 => ["--files", "-g", "*.rs", "."];
    files_negative_glob: 0 => ["--files", "-g", "!*.rs", "."];
    files_hidden_glob: 0 => ["--files", "--hidden", "-g", "*.rs", "."];
    files_glob_reinclude: 0 => ["--files", "-g", "*.rs", "-g", "!one.rs", "-g", "one.rs", "."];
    files_glob_no_names: 1 => ["--files", "-g", "*.absent", "."];
    files_hidden_excluded: 1 => ["--files", "--hidden", "-g", "!.hidden/**", ".hidden"];
    files_explicit_hidden: 0 => ["--files", ".dot.rs"];
    files_explicit_glob_bypass: 0 => ["--files", "-g", "!*.rs", "one.rs"];
    files_explicit_hidden_directory: 0 => ["--files", ".hidden"];
    files_explicit_symlink: 0 => ["--files", "link"];
    files_explicit_directory_symlink: 0 => ["--files", "dirlink"];
    files_explicit_directory_slash: 0 => ["--files", "dirlink/"];
    files_empty_file: 0 => ["--files", "empty"];
    files_duplicate_operands: 0 => ["--files", "one.rs", "one.rs"];
    files_dash_operand: 0 => ["--files", "--", "-dash"];
    files_stdin_label: 0 => ["--files", "-"];
    files_stdin_and_file: 0 => ["--files", "-", "one.rs"];
    files_ignores_piped_stdin: 0 => ["--files"], stdin = b"no path here\n";
    files_quiet: 0 => ["--files", "-q", "."];
    files_quiet_no_names: 1 => ["--files", "-q", "-g", "*.absent", "."];
    files_quiet_missing: 2 => ["--files", "-q", "missing"];
    files_quiet_hit_with_error: 0 => ["--files", "-q", "one.rs", "missing"];
    files_missing_after_name: 2 => ["--files", "one.rs", "missing"];
    files_dangling_symlink: 2 => ["--files", "dangling"];
    files_bad_glob: 2 => ["--files", "-g", "[", "."];
    files_bad_regex_ignored: 0 => ["--files", "-e", "[", "."];
    files_other_match_flags: 0 => ["--files", "-ivFon", "."];
    files_overrides_matching_names: 0 => ["-l", "--files", "."];
    files_overrides_count: 0 => ["-c", "--files", "."];
    files_count_override: 0 => ["--files", "-c", "-e", "red", "."];
    files_matching_names_override: 0 => ["--files", "-l", "-e", "red", "."];
    files_pattern_required_after_override: 2 => ["--files", "-l"];
    files_switch_value_refused: 2 => ["--files=yes"];
}

#[test]
fn files_includes_binary_and_oversized_names() {
    let fixture = Fixture::new("files-no-content");
    // Name visibility is independent of the recursive content policy.
    fixture.write("binary", b"red\0tail");
    fs::File::create(fixture.tree().join("oversized"))
        .unwrap()
        .set_len(9 * 1024 * 1024)
        .unwrap();
    let indexed = fixture.ferret(&["index", "."], None);
    assert!(indexed.status.success(), "{indexed:?}");
    let output = fixture.compare(&["--files", "--hidden", "."], None);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output
            .stdout
            .windows(b"binary\n".len())
            .any(|s| s == b"binary\n")
    );
    assert!(
        output
            .stdout
            .windows(b"oversized\n".len())
            .any(|s| s == b"oversized\n")
    );
}

// The table gives expected walked outcomes for each hidden/ignored pair:
// catalog visibility, then opaque fallback visibility, each without/with
// --hidden. Explicit files are always selected, even by negative globs.
const VISIBILITY: [(&str, [i32; 2], [i32; 2]); 4] = [
    ("plain.case", [0, 0], [0, 0]),
    (".hidden.case", [1, 0], [1, 0]),
    ("ignored.case", [1, 1], [0, 0]),
    (".ignored.case", [1, 1], [1, 0]),
];

fn visibility_matrix(files: bool) {
    let fixture = Fixture::new(if files {
        "matrix-files"
    } else {
        "matrix-lines"
    });
    for directory in ["catalog", "opaque"] {
        fs::create_dir_all(fixture.tree().join(directory)).unwrap();
        for (name, _, _) in VISIBILITY {
            fixture.write(&format!("{directory}/{name}"), b"red\n");
        }
    }
    fixture.write(".ferretignore", b"/opaque/\n/catalog/ignored.case\n/catalog/.ignored.case\n/opaque/ignored.case\n/opaque/.ignored.case\n");
    assert!(fixture.ferret(&["index", "."], None).status.success());
    let mut hits = 0;
    let mut absent = 0;
    for (directory, fallback) in [("catalog", false), ("opaque", true)] {
        for (name, catalogued, live) in VISIBILITY {
            for explicit in [false, true] {
                for hidden in [false, true] {
                    let path = format!("{directory}/{name}");
                    let mut args = vec!["grep", if files { "--files" } else { "-l" }];
                    if hidden {
                        args.push("--hidden");
                    }
                    // Negative-only globs do not expose a hidden name. They
                    // isolate the target cell, without a synthetic find plan.
                    let excluded: Vec<_> = VISIBILITY
                        .iter()
                        .filter(|(other, _, _)| *other != name)
                        .map(|(other, _, _)| format!("!{other}"))
                        .collect();
                    for glob in &excluded {
                        args.extend(["-g", glob]);
                    }
                    if !files {
                        args.push("red");
                    }
                    args.push(if explicit { &path } else { directory });
                    let expected = if explicit {
                        0
                    } else if fallback {
                        live[usize::from(hidden)]
                    } else {
                        catalogued[usize::from(hidden)]
                    };
                    let actual = fixture.ferret(&args, None);
                    assert_eq!(actual.status.code(), Some(expected), "{args:?}: {actual:?}");
                    assert!(actual.stderr.is_empty(), "{args:?}: {actual:?}");
                    assert_eq!(
                        actual.stdout,
                        if expected == 0 {
                            format!("{path}\n").into_bytes()
                        } else {
                            Vec::new()
                        },
                        "{args:?}"
                    );
                    if expected == 0 {
                        hits += 1;
                    } else {
                        absent += 1;
                    }
                    // Ferret-only exclusions intentionally differ from rg
                    // only for walked ignored catalog rows. The other cells
                    // are also independent binary differential checks.
                    if explicit || fallback || !name.contains("ignored") {
                        let rg = fixture.oracle(&args[1..], None);
                        assert_eq!(rg.status.code(), actual.status.code(), "{args:?}: {rg:?}");
                        assert_eq!(rg.stdout, actual.stdout, "{args:?}: {rg:?}");
                        assert!(rg.stderr.is_empty());
                    }
                }
            }
        }
    }
    assert_eq!((hits, absent), (25, 7));
}

#[test]
fn files_visibility_matrix_covers_hidden_ignored_explicit_and_fallback() {
    visibility_matrix(true);
}

#[test]
fn content_visibility_matrix_covers_hidden_ignored_explicit_and_fallback() {
    visibility_matrix(false);
}

#[test]
fn files_hidden_ancestors_globs_and_opaque_starts_compose() {
    let fixture = Fixture::new("files-hidden-ancestor");
    fs::create_dir_all(fixture.tree().join(".opaque/.nested")).unwrap();
    fixture.write(".opaque/.nested/leaf", b"red\n");
    fixture.write(".ferretignore", b"/.opaque/\n");
    assert!(fixture.ferret(&["index", "."], None).status.success());
    for (args, status) in [
        (vec!["--files", ".opaque"], 1),
        (vec!["--files", "--hidden", ".opaque"], 0),
        (vec!["--files", "--hidden", "-g", "!.nested", ".opaque"], 1),
        (vec!["--files", "-g", "**", ".opaque"], 0),
        (vec!["--files", "--hidden", "-g", "!**/leaf", ".opaque"], 1),
        (vec!["--files", ".opaque/.nested"], 0),
        (vec!["--files", ".opaque/.nested/leaf"], 0),
    ] {
        let output = fixture.compare(&args, None);
        assert_eq!(output.status.code(), Some(status), "{args:?}: {output:?}");
    }
    // The opaque start is an explicit exception. Walking from its parent
    // cannot reveal it, even with --hidden and a positive glob.
    let output = fixture.ferret(&["grep", "--files", "--hidden", "-g", "**/leaf", "."], None);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
}

#[test]
fn files_explicit_operands_need_no_index_and_never_open_streams() {
    use std::os::unix::net::UnixListener;
    let fixture = Fixture::new("files-direct");
    let _listener = UnixListener::bind(fixture.tree().join("socket")).unwrap();
    fs::remove_dir_all(fixture.base.join("index")).unwrap();
    for args in [
        vec!["--files", "one.rs"],
        vec!["--files", ".dot.rs"],
        vec!["--files", "link"],
        vec!["--files", "empty"],
        vec!["--files", "socket"],
        vec!["--files", "-"],
        vec!["--files", "one.rs", "-", "empty"],
    ] {
        let output = fixture.compare(&args, None);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
    }
}

#[test]
fn files_requires_directory_coverage_before_any_output_including_quiet() {
    let fixture = Fixture::new("files-coverage");
    let outside = fixture.base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    for quiet in [false, true] {
        let mut args = vec!["grep", "--files"];
        if quiet {
            args.push("-q");
        }
        args.extend(["one.rs", outside.to_str().unwrap()]);
        let output = fixture.ferret(&args, None);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty() && !output.stderr.is_empty());
    }
}

#[test]
fn files_never_waits_for_stdin_eof_even_when_implicitly_piped() {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    let fixture = Fixture::new("files-open-stdin");
    for args in [
        vec!["--files", "-"],
        vec!["--files", "-q", "-"],
        vec!["--files"],
    ] {
        let (reader, writer) = UnixStream::pair().unwrap();
        let fd: OwnedFd = reader.into();
        let mut command = fixture.command(OsStr::new(env!("CARGO_BIN_EXE_ferret")));
        let child = command
            .arg("--index")
            .arg(fixture.base.join("index"))
            .arg("grep")
            .args(&args)
            .stdin(Stdio::from(fd))
            .spawn()
            .unwrap();
        let (finished, completion) = mpsc::channel();
        let waiter =
            thread::spawn(move || finished.send(child.wait_with_output().unwrap()).unwrap());
        // The producer stays open until grep exits; the timeout only releases
        // a broken implementation so this regression cannot hang the suite.
        let observed = completion.recv_timeout(Duration::from_secs(5));
        drop(writer);
        let output = match observed {
            Ok(output) => output,
            Err(error) => {
                waiter.join().unwrap();
                panic!("{args:?} waited for EOF: {error}");
            }
        };
        waiter.join().unwrap();
        let oracle = fixture.oracle(&args, None);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
        assert_eq!(
            groups(&output.stdout, &fixture.tree()),
            groups(&oracle.stdout, &fixture.tree())
        );
        assert!(output.stderr.is_empty() && oracle.stderr.is_empty());
    }
}

// M4: qualifiers are part of the executed matcher and necessary query.
profile_cases! {
    m4_word_half_punctuation: 0 => ["-wo", "@red@", "-"], stdin = b"x@red@x\n@red@\n!@red@!\n";
    m4_word_half_rejects_word_neighbors: 1 => ["-w", "@red@", "-"], stdin = b"x@red@x\n";
    m4_word_unicode_neighbors: 0 => ["-wo", "red", "-"], stdin = "éredé\n!red!\n".as_bytes();
    m4_word_unicode_rejected: 1 => ["-w", "red", "-"], stdin = "éredé\n".as_bytes();
    m4_word_empty_half_spans: 0 => ["-wo", "", "-"], stdin = b"a ! b\n";
    m4_word_fixed: 0 => ["-wFo", "red.*", "-"], stdin = b" red.* \nxred.*y\n";
    m4_word_indexed: 0 => ["-w", "red", "."];
    m4_word_inverted: 0 => ["-wv", "red", "one.rs"];
    m4_word_count: 0 => ["-wco", "red", "one.rs"];
    m4_line_exact: 0 => ["-x", "red", "-"], stdin = b"red red\nred\n";
    m4_line_absent: 1 => ["-x", "red", "one.rs"];
    m4_line_empty: 0 => ["-xo", "", "-"], stdin = b"\nred\n";
    m4_line_cr_retained: 1 => ["-x", "red", "crlf"];
    m4_line_overrides_word: 0 => ["-wx", "red", "-"], stdin = b"red red\nred\n";
    m4_word_overrides_line: 0 => ["-xw", "red", "-"], stdin = b"red red\nred\n";
    m4_line_negation: 2 => ["-x", "--no-line-regexp", "red", "one.rs"];
    m4_word_negation: 2 => ["-w", "--no-word-regexp", "red", "-"], stdin = b"bred\n";
    m4_smart_lower: 0 => ["-S", "blue", "one.rs"];
    m4_smart_upper: 1 => ["-S", "Blue", "one.rs"];
    m4_smart_mixed_arguments: 0 => ["-S", "-e", "red", "-e", "BLUE", "-"], stdin = b"RED\nred\nBLUE\nblue\n";
    m4_smart_mixed_reverse: 0 => ["-S", "-e", "BLUE", "-e", "red", "-"], stdin = b"RED\nred\nBLUE\nblue\n";
    m4_smart_mixed_no_fold: 1 => ["-S", "-e", "red", "-e", "BLUE", "-"], stdin = b"RED\nblue\n";
    m4_smart_decoded_lower: 0 => ["-S", r"\x61", "-"], stdin = b"A\na\n";
    m4_smart_decoded_upper: 1 => ["-S", r"\x41", "-"], stdin = b"a\n";
    m4_smart_property_name: 1 => ["-S", r"\p{Lu}+", "-"], stdin = b"red\n";
    m4_smart_class_range: 0 => ["-So", "[a-z]+", "-"], stdin = b"RED\n";
    m4_smart_upper_class: 1 => ["-S", "[A-Z]+", "-"], stdin = b"red\n";
    m4_smart_capture_name: 0 => ["-S", "(?P<COLOR>red)", "-"], stdin = b"RED\n";
    m4_smart_fixed: 0 => ["-SF", "red.*", "-"], stdin = b"RED.*\n";
    m4_sensitive_overrides_fold: 1 => ["-is", "blue", "one.rs"];
    m4_smart_overrides_sensitive: 0 => ["-sS", "blue", "one.rs"];
    m4_fold_overrides_smart: 0 => ["-Si", "Blue", "one.rs"];
    m4_sensitive_long: 1 => ["-i", "--case-sensitive", "blue", "one.rs"];
    m4_profile_names_only_ignored: 0 => ["--files", "-Swx", "-e", "[", "."];
}
