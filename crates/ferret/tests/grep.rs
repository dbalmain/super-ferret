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
        let prefix = line.split(|&b| b == b':').next().unwrap_or_default();
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
        "-i",
        "-o",
        "-l",
        "-v",
        "-F",
        "-e",
        "-c",
        "--files",
        "--hidden",
        "-P",
        "-m1",
        "-x",
        "-w",
        "-S",
        "-f",
        "-s",
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
