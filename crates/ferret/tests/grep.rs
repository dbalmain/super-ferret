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
        self.check(args, input, false)
    }
    fn compare_raw(&self, args: &[&str], input: Option<&[u8]>) -> Output {
        self.check(args, input, true)
    }
    fn check(&self, args: &[&str], input: Option<&[u8]>, raw: bool) -> Output {
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
        if raw {
            assert_eq!(
                actual.stdout, oracle.stdout,
                "{args:?}: {actual:?} vs {oracle:?}"
            );
        } else {
            assert_eq!(
                groups(&actual.stdout, &self.tree()),
                groups(&oracle.stdout, &self.tree()),
                "{args:?}: {actual:?} vs {oracle:?}"
            );
        }
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
        "--heading-off",
        "-U",
        "-r",
        "--json",
        "-j2",
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
    for (flags, bytes, status, expected) in [
        (vec![], &b"\0"[..], 1, &b""[..]),
        (
            vec![],
            &b"red\n\0"[..],
            0,
            &b"(standard input): binary file matches\n"[..],
        ),
        (
            vec![],
            &b"red\0"[..],
            0,
            &b"(standard input): binary file matches\n"[..],
        ),
        // M4 R1: the NUL is either in the fragment reaching the limit or
        // beyond a complete selected line in the same received write.
        (
            vec!["-m1"],
            b"red\0x",
            0,
            b"(standard input): binary file matches\n",
        ),
        (
            vec!["-m1"],
            b"red\nred\0x",
            0,
            b"(standard input): binary file matches\n",
        ),
        (vec!["-c"], b"red\0x", 0, b"1\n"),
        (vec!["-c"], b"red\nred\0x", 0, b"2\n"),
        (vec!["-m1", "-c"], b"red\0x", 0, b"1\n"),
        (vec!["-m1", "-c"], b"red\nred\0x", 0, b"1\n"),
        (vec!["--count-matches"], b"red\0x", 0, b"1\n"),
        (vec!["--count-matches"], b"red\nred\0x", 0, b"2\n"),
        (vec!["-m1", "--count-matches"], b"red\0x", 0, b"1\n"),
        (vec!["-m1", "--count-matches"], b"red\nred\0x", 0, b"1\n"),
        (vec!["-m1", "--count-matches"], b"red red\0x", 0, b"2\n"),
        (vec!["-co"], b"red red\0x", 0, b"2\n"),
        (vec!["-m1", "-co"], b"red red\0x", 0, b"2\n"),
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
            .arg("grep")
            .args(&flags)
            .args(["red", "-"])
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
        assert_eq!(output.stdout, expected, "{flags:?}: {bytes:?}");
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
    ($($name:ident: $status:expr => $(@ $mode:ident)? [$($arg:expr),*] $(, stdin = $input:expr)?;)+) => { $(
        #[test]
        fn $name() {
            let fixture = Fixture::new(stringify!($name));
            let input: Option<&[u8]> = differential!(@input $($input)?);
            let output = profile_cases!(@run fixture, &[$($arg),*], input $(, $mode)?);
            assert_eq!(output.status.code(), Some($status));
        }
    )+ };
    (@run $fixture:expr, $args:expr, $input:expr) => { $fixture.compare($args, $input) };
    (@run $fixture:expr, $args:expr, $input:expr, raw) => { $fixture.compare_raw($args, $input) };
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
    assert!(fixture.ferret(&["index", "."], None).status.success());
    for mode in [None, Some("--live"), Some("-u")] {
        for quiet in [true, false] {
            let (reader, writer) = UnixStream::pair().unwrap();
            let (release, held) = mpsc::channel();
            let producer = thread::spawn(move || {
                held.recv().unwrap();
                drop(writer);
            });
            let fd: OwnedFd = reader.into();
            let mut command = fixture.command(OsStr::new(env!("CARGO_BIN_EXE_ferret")));
            command
                .arg("--index")
                .arg(fixture.base.join("index"))
                .arg("grep");
            if let Some(mode) = mode {
                command.arg(mode);
            }
            if quiet {
                command.arg("-q");
            }
            command.arg("red");
            if mode.is_some() {
                command.arg(".");
            } else {
                command.args((0..8).map(|file| format!("bulk-{file}")));
            }
            command
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
        vec!["--files", "-f", "-", "one.rs"],
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

profile_cases! {
    m4_pattern_empty: 1 => ["-f", "empty", "one.rs"];
    m4_pattern_blank: 0 => ["-f", "blank", "one.rs"];
    m4_pattern_missing: 2 => ["-f", "missing", "one.rs"];
    m4_pattern_missing_quiet: 2 => ["-q", "-f", "missing", "one.rs"];
    m4_pattern_empty_with_argument: 0 => ["-fempty", "-e", "red", "one.rs"];
    m4_pattern_stdin_explicit: 0 => ["-f", "-", "one.rs"], stdin = b"red\n";
    m4_pattern_stdin_default_directory: 0 => ["-f-"], stdin = b"red\n";
    m4_pattern_stdin_conflict: 2 => ["-f", "-", "-"], stdin = b"red\n";
    m4_pattern_stdin_empty: 1 => ["-f", "-", "one.rs"], stdin = b"";
    m4_pattern_stdin_empty_line: 0 => ["-f", "-", "one.rs"], stdin = b"\n";
    m4_pattern_names_missing_ignored: 0 => ["--files", "-f", "missing", "."];
    m4_pattern_names_stdin_ignored: 0 => ["--files", "-f", "-", "."], stdin = b"[\n";
    m4_pattern_missing_value: 2 => ["-f"];
}

#[test]
fn m4_pattern_file_line_endings_modes_and_decoded_literals() {
    let fixture = Fixture::new("m4-pattern-file");
    for (bytes, args, status) in [
        (
            &b"red\r\nBLUE\r\n"[..],
            vec!["-f", ".patterns", "one.rs"],
            0,
        ),
        (b"red\r", vec!["-f", ".patterns", "crlf"], 0),
        (b"red\r", vec!["-f", ".patterns", "one.rs"], 1),
        (b"\r\n", vec!["-f", ".patterns", "two.txt"], 0),
        (b"red\n\n", vec!["--file=.patterns", "two.txt"], 0),
        (
            b"\xef\xbb\xbfred\n",
            vec!["--file", ".patterns", "one.rs"],
            1,
        ),
        (b"\xff\n", vec!["-f", ".patterns", "one.rs"], 2),
        (b"red.*\nBLUE\n", vec!["-F", "-f.patterns", "one.rs"], 0),
        (b"red\nBLUE\n", vec!["-S", "-f", ".patterns", "one.rs"], 0),
        (b"blue\n", vec!["-S", "-f", ".patterns", "one.rs"], 0),
        (b"red\n", vec!["-x", "-f", ".patterns", "one.rs"], 1),
        (b"BLUE\n", vec!["-x", "-f", ".patterns", "one.rs"], 0),
        (b"red\n", vec!["-w", "-f", ".patterns", "one.rs"], 0),
        (b"blue\n", vec!["-f", ".patterns", "-e", "red", "one.rs"], 0),
        (b"red\n", vec!["-e", "BLUE", "-f", ".patterns", "one.rs"], 0),
        (
            b"red\n",
            vec!["-f", ".patterns", "-f", ".patterns", "one.rs"],
            0,
        ),
        (b"", vec!["-v", "-f", ".patterns", "one.rs"], 0),
    ] {
        fixture.write(".patterns", bytes);
        let output = fixture.compare(&args, None);
        assert_eq!(
            output.status.code(),
            Some(status),
            "{bytes:?}: {args:?}: {output:?}"
        );
    }
}

#[test]
fn m4_pattern_file_paths_are_bytes_in_long_and_attached_options() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let fixture = Fixture::new("m4-pattern-byte-path");
    let name = OsString::from_vec(b".patterns-\xff".to_vec());
    fs::write(fixture.tree().join(&name), b"red\n").unwrap();
    let rg = std::env::var_os("FERRET_RG").unwrap();
    for prefix in [b"-f".as_slice(), b"--file="] {
        let mut option = prefix.to_vec();
        option.extend_from_slice(name.as_bytes());
        let option = OsString::from_vec(option);
        let mut oracle = fixture.command(&rg);
        oracle.arg(&option).arg("one.rs");
        let oracle = run(oracle, None);
        let mut command = fixture.command(OsStr::new(env!("CARGO_BIN_EXE_ferret")));
        command.args(["grep"]).arg(&option).arg("one.rs");
        let actual = run(command, None);
        assert_eq!(actual.status.code(), Some(0));
        assert_eq!(actual.stdout, oracle.stdout);
        assert!(actual.stderr.is_empty() && oracle.stderr.is_empty());
    }
}

profile_cases! {
    m4_count_matches: 0 => ["--count-matches", "red", "one.rs"];
    m4_count_matches_absent: 1 => ["--count-matches", "violet", "one.rs"];
    m4_count_zero_explicit: 1 => ["-c", "--include-zero", "violet", "one.rs"];
    m4_count_zero_empty: 1 => ["-c", "--include-zero", "red", "empty"];
    m4_count_zero_indexed: 1 => ["-c", "--include-zero", "violet", "."];
    m4_count_positive_and_zero: 0 => ["-c", "--include-zero", "red", "."];
    m4_count_matches_zero: 1 => ["--count-matches", "--include-zero", "violet", "."];
    m4_count_zero_negation: 1 => ["-c", "--include-zero", "--no-include-zero", "violet", "."];
    m4_count_lines_override: 0 => ["--count-matches", "-c", "red", "one.rs"];
    m4_count_matches_override: 0 => ["-c", "--count-matches", "red", "one.rs"];
    m4_count_only_modifier: 0 => ["--count-matches", "-oc", "red", "one.rs"];
    m4_count_matches_inverted: 0 => ["-v", "--count-matches", "red", "one.rs"];
    m4_count_inverted_zero: 1 => ["-v", "-c", "--include-zero", "red", "sub/three.rs"];
    m4_count_smart_quiet_zero: 1 => ["-qSc", "--include-zero", "violet", "."];
    m4_count_smart_quiet_hit: 0 => ["-qSc", "--include-zero", "blue", "."];
    m4_count_empty_ascii_spans: 0 => ["--count-matches", "", "-"], stdin = b"abc\n";
    m4_count_empty_unicode_spans: 0 => ["--count-matches", "", "-"], stdin = "éK\n".as_bytes();
    m4_count_word_spans: 0 => ["-w", "--count-matches", "red", "one.rs"];
    m4_count_line_spans: 0 => ["-x", "--count-matches", "BLUE", "one.rs"];
    m4_without_match_explicit: 0 => ["--files-without-match", "violet", "one.rs"];
    m4_without_match_rejected: 1 => ["--files-without-match", "red", "one.rs"];
    m4_without_match_empty: 0 => ["--files-without-match", "red", "empty"];
    m4_without_match_indexed: 0 => ["--files-without-match", "red", "."];
    m4_without_match_inverted: 0 => ["-v", "--files-without-match", "red", "sub/three.rs"];
    m4_without_match_quiet_hit: 0 => ["-q", "--files-without-match", "violet", "one.rs"];
    m4_without_match_quiet_rejected: 1 => ["-q", "--files-without-match", "red", "one.rs"];
    m4_without_match_stdin: 0 => ["--files-without-match", "red", "-"], stdin = b"blue\n";
    m4_without_match_overrides_count: 1 => ["-c", "--files-without-match", "red", "one.rs"];
    m4_count_overrides_without_match: 0 => ["--files-without-match", "-c", "red", "one.rs"];
    m4_names_only_overrides_without_match: 0 => ["--files-without-match", "--files", "one.rs"];
    m4_without_match_overrides_names_only: 0 => ["--files", "--files-without-match", "violet", "one.rs"];
    m4_max_one_line: 0 => ["-nm1", "red", "one.rs"];
    m4_max_two_lines: 0 => ["--max-count=2", "red", "one.rs"];
    m4_max_spans_finish_line: 0 => ["-om1", "red", "-"], stdin = b"red red\nred\n";
    m4_max_count_spans: 0 => ["--count-matches", "-m1", "red", "-"], stdin = b"red red\nred\n";
    m4_max_inverted_lines: 0 => ["-vnm1", "red", "one.rs"];
    m4_max_repeated_override: 0 => ["-m1", "-m2", "-c", "red", "one.rs"];
    m4_max_zero: 1 => ["-m0", "red", "one.rs"];
    m4_max_zero_skips_regex: 1 => ["-m0", "[", "one.rs"];
    m4_max_zero_skips_inputs: 1 => ["-m0", "red", "missing"];
    m4_max_zero_no_zero_record: 1 => ["-m0", "-c", "--include-zero", "red", "one.rs"];
    m4_max_zero_no_without_record: 1 => ["-m0", "--files-without-match", "red", "one.rs"];
    m4_max_zero_pattern_file_error: 2 => ["-m0", "-f", "missing", "one.rs"];
    m4_max_zero_glob_error: 2 => ["-m0", "-g", "[", "red", "one.rs"];
    m4_max_negative: 2 => ["-m-1", "red", "one.rs"];
    m4_max_overflow: 2 => ["-m18446744073709551616", "red", "one.rs"];
    m4_max_missing_value: 2 => ["-m"];
    m4_max_zero_names_only_ignored: 0 => ["--files", "-m0", "one.rs"];
    m4_count_negation_refused: 2 => ["--no-count-matches", "red", "one.rs"];
    m4_without_negation_refused: 2 => ["--no-files-without-match", "red", "one.rs"];
}

#[test]
fn m4_snapshot_zeros_and_no_match_names_do_not_use_current_negative_bytes() {
    let fixture = Fixture::new("m4-snapshot-negatives");
    fs::create_dir_all(fixture.tree().join("records")).unwrap();
    fixture.write("records/positive", b"red\n");
    fixture.write("records/negative", b"blue\n");
    assert!(fixture.ferret(&["index", "."], None).status.success());
    fixture.write("records/positive", b"blue\n");
    fixture.write("records/negative", b"red\n");
    for (args, expected, status) in [
        (
            vec!["-c", "--include-zero", "red", "records"],
            &b"records/negative:0\nrecords/positive:0\n"[..],
            1,
        ),
        (
            vec!["--files-without-match", "red", "records"],
            b"records/negative\nrecords/positive\n",
            0,
        ),
        (
            vec!["-vc", "--include-zero", "red", "records"],
            b"records/negative:0\nrecords/positive:1\n",
            0,
        ),
        (
            vec!["-c", "--include-zero", "red", "records/negative"],
            b"1\n",
            0,
        ),
        (
            vec!["--files-without-match", "red", "records/negative"],
            b"",
            1,
        ),
    ] {
        let mut argv = vec!["grep"];
        argv.extend(args);
        let actual = fixture.ferret(&argv, None);
        assert_eq!(actual.status.code(), Some(status), "{argv:?}: {actual:?}");
        assert_eq!(
            groups(&actual.stdout, &fixture.tree()),
            groups(expected, &fixture.tree()),
            "{argv:?}: {actual:?}"
        );
        assert!(actual.stderr.is_empty());
    }
    // A posting-negative snapshot name can remain after deletion. Its absence
    // proof is from the snapshot; an explicit vanished operand is an error.
    fs::remove_file(fixture.tree().join("records/negative")).unwrap();
    let output = fixture.ferret(&["grep", "-c", "--include-zero", "red", "records"], None);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        output
            .stdout
            .windows(b"records/negative:0\n".len())
            .any(|s| s == b"records/negative:0\n")
    );
    let output = fixture.ferret(
        &["grep", "--files-without-match", "red", "records/negative"],
        None,
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty() && !output.stderr.is_empty());
}

#[test]
fn m4_opaque_fallback_reads_current_bytes_for_zero_and_no_match_modes() {
    let fixture = Fixture::new("m4-fallback-negatives");
    fs::create_dir_all(fixture.tree().join("opaque")).unwrap();
    fixture.write(".ferretignore", b"/opaque/\n");
    fixture.write("opaque/leaf", b"blue\n");
    assert!(fixture.ferret(&["index", "."], None).status.success());
    fixture.write("opaque/leaf", b"red red\n");
    for (args, status) in [
        (vec!["-c", "--include-zero", "red", "opaque"], 0),
        (vec!["--count-matches", "red", "opaque"], 0),
        (vec!["--files-without-match", "red", "opaque"], 1),
        (vec!["-v", "--files-without-match", "red", "opaque"], 0),
    ] {
        let output = fixture.compare(&args, None);
        assert_eq!(output.status.code(), Some(status));
    }
}

#[test]
fn m4_input_faults_never_supply_zero_or_no_match_records() {
    let fixture = Fixture::new("m4-fault-not-negative");
    fs::create_dir_all(fixture.tree().join("denied")).unwrap();
    fixture.write("denied/leaf", b"red\n");
    assert!(fixture.ferret(&["index", "."], None).status.success());
    let path = fixture.tree().join("denied/leaf");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    let mut outcomes = Vec::new();
    for output in ["--files-without-match", "-c", "--count-matches"] {
        for operand in ["denied", "denied/leaf"] {
            let actual = fixture.ferret(&["grep", output, "--include-zero", "red", operand], None);
            outcomes.push((
                actual.status.code(),
                actual.stdout.is_empty(),
                actual.stderr.is_empty(),
            ));
        }
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(outcomes, [(Some(2), true, false); 6]);
}

profile_cases! {
    m4_without_quiet_hit_overrides_operand_error: 0 => ["-q", "--files-without-match", "violet", "one.rs", "missing"];
    m4_without_quiet_failure_keeps_operand_error: 2 => ["-q", "--files-without-match", "red", "one.rs", "missing"];
    m4_count_missing_is_not_zero: 2 => ["-c", "--include-zero", "red", "missing"];
    m4_zero_flag_does_not_change_lines: 0 => ["--include-zero", "red", "one.rs"];
    m4_matching_names_override_without_match: 0 => ["--files-without-match", "-l", "red", "one.rs"];
    m4_without_match_ignores_only_matching: 0 => ["--files-without-match", "-o", "violet", "one.rs"];
}

#[test]
fn m4_zero_limit_needs_no_index_even_for_directories_or_invalid_patterns() {
    let fixture = Fixture::new("m4-zero-no-index");
    fs::remove_dir_all(fixture.base.join("index")).unwrap();
    for args in [
        vec!["-m0", "red", "."],
        vec!["-m0", "[", "."],
        vec!["-m0", "red", "missing"],
    ] {
        let output = fixture.compare(&args, None);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty() && output.stderr.is_empty());
    }
}

// M4 review R1: binary count output is numeric, independently of -m.
profile_cases! {
    m4_binary_count_limited_fragment: 0 => ["-m1", "-c", "red", "-"], stdin = b"red\0x";
    m4_binary_count_limited_after_line: 0 => ["-m1", "-c", "red", "-"], stdin = b"red\nred\0x";
    m4_binary_count_fragment: 0 => ["-c", "red", "-"], stdin = b"red\0x";
    m4_binary_count_after_line: 0 => ["-c", "red", "-"], stdin = b"red\nred\0x";
    m4_binary_match_count_fragment: 0 => ["--count-matches", "red", "-"], stdin = b"red\0x";
    m4_binary_match_count_after_line: 0 => ["--count-matches", "red", "-"], stdin = b"red\nred\0x";
    m4_binary_count_only_matching_spans: 0 => ["-co", "red", "-"], stdin = b"red red\0x";
    m4_binary_count_only_matching_limited_spans: 0 => ["-m1", "-co", "red", "-"], stdin = b"red red\0x";
    m4_binary_match_count_limited_spans: 0 => ["-m1", "--count-matches", "red", "-"], stdin = b"red red\0x";
}

// M5: filenames, headings and NUL terminators compose with every output mode.
profile_cases! {
    m5_filename_force: 0 => @raw ["-H", "red", "one.rs"];
    m5_filename_suppress: 0 => @raw ["-I", "red", "one.rs", "one.rs"];
    m5_filename_last_force: 0 => @raw ["-IHn", "red", "one.rs"];
    m5_filename_last_suppress: 0 => @raw ["-HIn", "red", "one.rs"];
    m5_line_number_disable: 0 => @raw ["-nN", "red", "one.rs"];
    m5_line_number_reenable: 0 => @raw ["-Nn", "red", "one.rs"];
    m5_line_number_long: 0 => @raw ["-n", "--no-line-number", "red", "one.rs"];
    m5_null_prefix: 0 => @raw ["-H0n", "red", "one.rs"];
    m5_null_count: 0 => @raw ["-H0c", "red", "one.rs"];
    m5_null_matching_name: 0 => @raw ["-0l", "red", "one.rs"];
    m5_null_without_name: 0 => @raw ["-0", "--files-without-match", "violet", "one.rs"];
    m5_null_files: 0 => @raw ["--files", "-0", "one.rs"];
    m5_null_files_multiple: 0 => @raw ["--files", "-0", "one.rs", "one.rs"];
    m5_heading_single_suppressed: 0 => @raw ["--heading", "red", "one.rs"];
    m5_heading_forced: 0 => @raw ["--heading", "-Hn", "red", "one.rs"];
    m5_heading_null: 0 => @raw ["--heading", "-H0n", "red", "one.rs"];
    m5_heading_count: 0 => @raw ["--heading", "-Hc", "red", "one.rs"];
    m5_heading_absent: 1 => @raw ["--heading", "-H", "violet", "one.rs"];
    m5_heading_disabled: 0 => @raw ["--heading", "--no-heading", "-Hn", "red", "one.rs"];
    m5_heading_reenabled: 0 => @raw ["--no-heading", "--heading", "-Hn", "red", "one.rs"];
}

// M5 context groups are compared byte for byte, without line sorting.
profile_cases! {
    m5_after: 0 => @raw ["-nA1", "red", "one.rs"];
    m5_before: 0 => @raw ["-nB1", "red", "one.rs"];
    m5_around: 0 => @raw ["-nC1", "red", "one.rs"];
    m5_zero_context: 0 => @raw ["-nC0", "red", "one.rs"];
    m5_before_explicit_wins: 0 => @raw ["-B0", "-C2", "red", "one.rs"];
    m5_after_explicit_wins: 0 => @raw ["-C2", "-A0", "red", "one.rs"];
    m5_repeated_context: 0 => @raw ["-C2", "-C0", "red", "one.rs"];
    m5_context_filename: 0 => @raw ["-HnC1", "red", "one.rs"];
    m5_context_null_filename: 0 => @raw ["-H0nC1", "red", "one.rs"];
    m5_context_heading: 0 => @raw ["--heading", "-HnC1", "red", "one.rs"];
    m5_context_inverted: 0 => @raw ["-vnC1", "red", "one.rs"];
    m5_context_inverted_only: 0 => @raw ["-vonC1", "red", "one.rs"];
    m5_context_only: 0 => @raw ["-onC1", "red", "one.rs"];
    m5_context_limit: 0 => @raw ["-m1", "-nA1", "red", "one.rs"];
    m5_context_limit_selected_tail: 0 => @raw ["-m1", "-nA2", "red", "one.rs"];
    m5_context_limit_only: 0 => @raw ["-m1", "-onA2", "red", "one.rs"];
    m5_context_count_ignored: 0 => @raw ["-cC4", "red", "one.rs"];
    m5_context_matches_ignored: 0 => @raw ["--count-matches", "-C4", "red", "one.rs"];
    m5_context_names_ignored: 0 => @raw ["-lC4", "red", "one.rs"];
    m5_context_quiet_ignored: 0 => @raw ["-qC4", "red", "one.rs"];
    m5_context_absent: 1 => @raw ["-C2", "violet", "one.rs"];
    m5_context_empty: 1 => @raw ["-C2", "red", "empty"];
    m5_context_crlf: 0 => @raw ["-nC2", "red", "crlf"];
    m5_context_unterminated: 0 => @raw ["-nC2", "red", "last"];
    m5_context_multi: 0 => @raw ["-nC1", "red", "one.rs", "one.rs"];
    m5_context_multi_heading: 0 => @raw ["--heading", "-nC1", "red", "one.rs", "one.rs"];
    m5_context_multi_hidden_names: 0 => @raw ["--heading", "-InC1", "red", "one.rs", "one.rs"];
    m5_context_bad_count: 2 => @raw ["-Awrong", "red", "one.rs"];
}

#[test]
fn m5_context_gaps_overlap_adjacency_and_long_spills() {
    let fixture = Fixture::new("context-geometry");
    for (name, bytes) in [
        ("groups", b"red\na\nb\nc\nd\ne\nf\nred\ntail\n".as_slice()),
        ("adjacent", b"red\na\nb\nred\nc\nd\nred\n".as_slice()),
    ] {
        fixture.write(name, bytes);
        for context in ["-C1", "-C2", "-A1", "-B1", "-C0"] {
            fixture.compare_raw(&["-Hn", context, "red", name], None);
        }
    }
    let mut bytes = b"before\n".repeat(20_000);
    bytes.extend_from_slice(b"red\n");
    bytes.extend_from_slice(&b"after\n".repeat(20_000));
    fixture.write("spilled", &bytes);
    fixture.compare_raw(&["-nC25000", "red", "spilled"], None);
    fixture.compare_raw(&["--color=always", "-HnC1", "red", "groups"], None);
    fixture.compare_raw(&["--color=always", "-nC25000", "red", "spilled"], None);
}

#[test]
fn m5_before_context_spill_failure_stops_the_command() {
    let fixture = Fixture::new("before-context-fault");
    let mut bytes = b"blue\n".repeat(20_000);
    bytes.extend_from_slice(b"red\n");
    fixture.write("big.txt", &bytes);
    fixture.write("small.txt", b"red small\n");
    let mut command = fixture.command(OsStr::new(env!("CARGO_BIN_EXE_ferret")));
    command
        .arg("--index")
        .arg(fixture.base.join("index"))
        .args(["grep", "-B25000", "red", "big.txt", "small.txt"])
        .env("TMPDIR", fixture.base.join("missing-spill-dir"));
    let output = run(command, None);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "a later group was published");
    assert!(String::from_utf8_lossy(&output.stderr).contains("output spill"));
}

// M5: these comparisons attach each child to a real pty. The helper captures
// its terminal bytes (including the tty's LF-to-CRLF processing) unchanged.
#[derive(Debug)]
struct TerminalOutput {
    code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Fixture {
    fn terminal(
        &self,
        args: &[&str],
        environment: &[(&str, &str)],
        candidate: bool,
    ) -> TerminalOutput {
        let python = std::env::var_os("FERRET_PYTHON")
            .unwrap_or_else(|| panic!("nix develop supplies Python for real-pty comparisons"));
        let mut command = self.command(&python);
        command
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/support/grep_pty.py"
            ))
            .env("TERM", "xterm-256color");
        for (name, value) in environment {
            command.env(name, value);
        }
        if candidate {
            command
                .arg(env!("CARGO_BIN_EXE_ferret"))
                .arg("--index")
                .arg(self.base.join("index"))
                .arg("grep");
        } else {
            command.arg(std::env::var_os("FERRET_RG").unwrap_or_else(|| panic!("pinned oracle")));
        }
        command.args(args);
        let output = command.output().unwrap();
        assert!(output.status.success(), "pty driver failed: {output:?}");
        let bytes = &output.stdout;
        let code = i32::from_be_bytes(bytes[..4].try_into().unwrap());
        let size = u64::from_be_bytes(bytes[4..12].try_into().unwrap()) as usize;
        TerminalOutput {
            code,
            stdout: bytes[12..12 + size].to_vec(),
            stderr: bytes[12 + size..].to_vec(),
        }
    }
    fn compare_terminal(&self, args: &[&str], environment: &[(&str, &str)]) -> TerminalOutput {
        let expected = self.terminal(args, environment, false);
        let actual = self.terminal(args, environment, true);
        assert_eq!(
            actual.code, expected.code,
            "{args:?} {environment:?}: {actual:?} vs {expected:?}"
        );
        assert_eq!(actual.stdout, expected.stdout, "{args:?} {environment:?}");
        assert_eq!(
            actual.stderr.is_empty(),
            expected.stderr.is_empty(),
            "{args:?} {environment:?}"
        );
        actual
    }
}

#[test]
fn m5_real_terminal_matrix() {
    let fixture = Fixture::new("tty-matrix");
    let profiles: &[&[&str]] = &[
        &[],
        &["-N"],
        &["-n"],
        &["--no-heading"],
        &["--heading"],
        &["-I"],
        &["-H"],
        &["--color=never"],
        &["--color=auto"],
        &["--color=always"],
        &["--color=ansi"],
        &["-p"],
        &["--pretty"],
        &["-p", "-N", "--no-heading", "--color=never"],
        &["--color=never", "-p"],
        &["-nN"],
        &["-Nn"],
        &["-H0"],
        &["-l"],
        &["-c"],
        &["--count-matches"],
        &["--files"],
        &["-HnC1"],
        &["-vonC1"],
        &["-m1", "-A2"],
        &["-q"],
    ];
    for &profile in profiles {
        for multiple in [false, true] {
            let mut args = profile.to_vec();
            if !args.contains(&"--files") {
                args.push("red");
            }
            args.push("one.rs");
            if multiple {
                args.push("one.rs");
            }
            assert_eq!(fixture.compare_terminal(&args, &[]).code, 0);
        }
    }
    for environment in [
        vec![("TERM", "dumb")],
        vec![("TERM", "")],
        vec![("NO_COLOR", "1")],
        vec![("NO_COLOR", "")],
        vec![("TERM", "dumb"), ("NO_COLOR", "1")],
    ] {
        for color in [
            "--color=auto",
            "--color=always",
            "--color=ansi",
            "--color=never",
        ] {
            fixture.compare_terminal(&[color, "-H", "red", "one.rs"], &environment);
        }
    }
    // Recursive defaults, including the filename heading, use the real catalog.
    fixture.compare_terminal(&["red", "sub"], &[]);
    fixture.compare_terminal(&["violet", "one.rs"], &[]);
}

profile_cases! {
    m5_color_match: 0 => @raw ["--color=always", "-Hn", "red", "one.rs"];
    m5_color_ansi: 0 => @raw ["--color=ansi", "-Hn", "red", "one.rs"];
    m5_color_auto_pipe: 0 => @raw ["--color=auto", "-Hn", "red", "one.rs"];
    m5_color_never: 0 => @raw ["--color=never", "-Hn", "red", "one.rs"];
    m5_color_empty: 0 => @raw ["--color=always", "-no", "", "one.rs"];
    m5_color_word_fragment: 0 => @raw ["--color=always", "-won", "red", "one.rs"];
    m5_color_line_fragment: 0 => @raw ["--color=always", "-xon", "red red", "one.rs"];
    m5_color_context: 0 => @raw ["--color=always", "-HnC1", "red", "one.rs"];
    m5_color_inverted_context: 0 => @raw ["--color=always", "-vonC1", "red", "one.rs"];
    m5_color_count: 0 => @raw ["--color=always", "-Hc", "red", "one.rs"];
    m5_color_names: 0 => @raw ["--color=always", "-l0", "red", "one.rs"];
    m5_color_files: 0 => @raw ["--color=always", "--files", "-0", "one.rs"];
    m5_color_null_heading: 0 => @raw ["--color=always", "--heading", "-H0", "red", "one.rs"];
    m5_pretty_pipe: 0 => @raw ["-p", "red", "one.rs"];
    m5_pretty_long_pipe: 0 => @raw ["--pretty", "red", "one.rs"];
    m5_pretty_color_override: 0 => @raw ["-p", "--color=never", "red", "one.rs"];
    m5_pretty_overrides_color: 0 => @raw ["--color=never", "-p", "red", "one.rs"];
    m5_pretty_absent: 1 => @raw ["-p", "violet", "one.rs"];
    m5_color_absent: 1 => @raw ["--color=always", "violet", "one.rs"];
    m5_color_bad: 2 => @raw ["--color=rainbow", "red", "one.rs"];
}

#[test]
fn m5_null_name_records_preserve_newlines_and_distinct_paths() {
    let fixture = Fixture::new("nul-records");
    fixture.write("with\nnewline", b"red\n");
    for args in [
        vec!["--files", "-0", "one.rs", "with\nnewline", "two.txt"],
        vec!["-l0", "red", "one.rs", "with\nnewline", "two.txt"],
        vec![
            "--files-without-match",
            "-0",
            "red",
            "one.rs",
            "with\nnewline",
            "two.txt",
        ],
    ] {
        let expected = fixture.oracle(&args, None);
        let mut candidate = vec!["grep"];
        candidate.extend_from_slice(&args);
        let actual = fixture.ferret(&candidate, None);
        assert_eq!(actual.status.code(), expected.status.code());
        assert_eq!(actual.stderr.is_empty(), expected.stderr.is_empty());
        let split = |bytes: &[u8]| {
            let mut records: Vec<_> = bytes.split(|&byte| byte == 0).map(<[u8]>::to_vec).collect();
            records.sort();
            records
        };
        assert_eq!(split(&actual.stdout), split(&expected.stdout));
        assert_eq!(actual.stdout.last(), Some(&0));
    }
}

#[test]
fn m5_discarded_history_does_not_force_a_small_context_window_to_spill() {
    let fixture = Fixture::new("small-context-window");
    let mut bytes = b"blue\n".repeat(20_000);
    bytes.extend_from_slice(b"red\n");
    fixture.write("long-history", &bytes);
    let mut command = fixture.command(OsStr::new(env!("CARGO_BIN_EXE_ferret")));
    command
        .arg("--index")
        .arg(fixture.base.join("index"))
        .args(["grep", "-B1", "red", "long-history"])
        .env("TMPDIR", fixture.base.join("unavailable-spill-dir"));
    let output = run(command, None);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stdout, b"blue\nred\n");
    assert!(output.stderr.is_empty());
}

// M6: unrestricted freshness is independent of hidden filtering and depth.
profile_cases! {
    m6_u: 0 => ["-u", "red", "."];
    m6_uu: 0 => ["-uu", "red", "."];
    m6_repeated_u: 0 => ["-u", "-u", "red", "."];
    m6_unrestricted_long: 0 => ["--unrestricted", "red", "."];
    m6_no_ignore: 0 => ["--no-ignore", "red", "."];
    m6_no_ignore_hidden: 0 => ["--no-ignore", "--hidden", "red", "."];
    m6_uu_hide: 0 => ["-uu", "--no-hidden", "red", "."];
    m6_u_glob: 0 => ["-u", "-g", "*.rs", "red", "."];
    m6_u_negative_glob: 0 => ["-u", "-g", "!*.rs", "green", "."];
    m6_u_explicit_bypasses_glob: 0 => @raw ["-u", "-g", "!*.rs", "red", "one.rs"];
    m6_u_names: 0 => ["--files", "-u", "."];
    m6_uu_names: 0 => ["--files", "-uu", "."];
    m6_u_stdin: 0 => @raw ["-u", "red", "-"], stdin = b"red\n";
    m6_u_absent: 1 => ["-u", "violet", "."];
    m6_depth_zero: 1 => ["--max-depth=0", "red", "."];
    m6_depth_one: 0 => ["--max-depth=1", "red", "."];
    m6_depth_two: 0 => ["--max-depth=2", "red", "."];
    m6_depth_last: 1 => ["--max-depth=3", "--max-depth", "0", "red", "."];
    m6_depth_unrestricted: 0 => ["-uu", "--max-depth", "1", "red", "."];
    m6_depth_names: 0 => ["--files", "--max-depth", "1", "."];
    m6_depth_live_names: 0 => ["--files", "-uu", "--max-depth", "1", "."];
    m6_depth_explicit_file: 0 => @raw ["--max-depth", "0", "red", "one.rs"];
    m6_depth_explicit_symlink: 0 => @raw ["--max-depth", "0", "red", "link"];
    m6_depth_explicit_directory: 0 => ["--max-depth", "1", "red", "sub"];
    m6_explicit_directory_symlink: 0 => ["red", "dirlink"];
    m6_u_directory_symlink: 0 => ["-u", "red", "dirlink"];
    m6_depth_bad: 2 => ["--max-depth", "wrong", "red", "."];
    m6_depth_negative: 2 => ["--max-depth=-1", "red", "."];
    m6_text: 0 => @raw ["-a", "red", "one.rs"];
    m6_text_long: 0 => @raw ["--text", "red", "one.rs"];
    m6_text_reversed: 0 => @raw ["-a", "--no-text", "red", "one.rs"];
}

#[test]
fn m6_unrestricted_needs_no_index_and_preserves_override_order() {
    let fixture = Fixture::new("unrestricted-no-index");
    fs::create_dir(fixture.tree().join(".git")).unwrap();
    fixture.write(".gitignore", b"blocked\n");
    fixture.write("blocked", b"red ignored\n");
    fixture.write(".held", b"red hidden\n");
    assert!(fixture.ferret(&["index", "."], None).status.success());
    for args in [
        vec!["-u", "--ignore", "-u", "red", "."],
        vec!["-uu", "--ignore", "red", "."],
        vec!["--no-ignore", "--ignore", "red", "."],
        vec!["--ignore", "-u", "red", "."],
    ] {
        fixture.compare(&args, None);
    }
    fs::remove_dir_all(fixture.base.join("index")).unwrap();
    for args in [
        vec!["-u", "red", "."],
        vec!["-uu", "red", "."],
        vec!["--no-ignore", "--hidden", "red", "."],
        vec!["-u", "--ignore", "-u", "red", "blocked"],
        vec!["--files", "-uu", "."],
        vec!["-q", "-u", "red", "."],
    ] {
        fixture.compare(&args, None);
    }
    assert!(
        fixture
            .ferret(&["grep", "red", "."], None)
            .stdout
            .is_empty()
    );
}

#[test]
fn m6_text_reads_snapshot_binary_names_and_all_nul_bytes() {
    let fixture = Fixture::new("text-binary");
    fs::create_dir(fixture.tree().join("nul")).unwrap();
    fixture.write("nul/before", b"red\0x\nblue\n");
    fixture.write("nul/after", b"blue\0red\n");
    fixture.write("nul/later", b"blue\nred\0end\n");
    assert!(fixture.ferret(&["index", "."], None).status.success());
    for source in ["nul", "nul/before", "-"] {
        for flags in [
            vec!["-a"],
            vec!["-an"],
            vec!["-av"],
            vec!["-ao"],
            vec!["-ac"],
            vec!["-a", "--count-matches"],
            vec!["-aq"],
            vec!["-al"],
            vec!["-au"],
            vec!["-auu"],
        ] {
            let mut args = flags;
            args.extend(["red", source]);
            fixture.compare(&args, Some(b"red\0x\nblue\n"));
        }
    }
    for args in [
        vec!["-ac", "--include-zero", "violet", "nul"],
        vec!["-a", "--files-without-match", "violet", "nul"],
        vec!["-a", "--files-without-match", "red", "nul"],
        vec!["-au", "--no-text", "red", "nul"],
        vec!["-u", "red", "nul"],
        vec!["red", "nul"],
    ] {
        fixture.compare(&args, None);
    }
}

#[test]
fn m6_third_unrestricted_step_refuses_until_binary_mode_exists() {
    let fixture = Fixture::new("uuu-refusal");
    for flags in [vec!["-uuu"], vec!["-uu", "-u"], vec!["-uuu", "-a"]] {
        let mut oracle = flags.clone();
        oracle.extend(["red", "."]);
        assert!(fixture.oracle(&oracle, None).status.success());
        let mut args = vec!["grep"];
        args.extend(flags);
        args.extend(["red", "."]);
        let output = fixture.ferret(&args, None);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty() && !output.stderr.is_empty());
    }
}

impl Fixture {
    fn agree_live(&self, args: &[&str]) {
        let mut indexed = vec!["grep"];
        indexed.extend_from_slice(args);
        let mut live = vec!["grep", "--live"];
        live.extend_from_slice(args);
        let context = args.iter().any(|arg| arg.starts_with("-C"));
        if context {
            indexed.extend(["--heading", "-Hn"]);
            live.extend(["--heading", "-Hn"]);
        }
        let snapshot = self.ferret(&indexed, None);
        let current = self.ferret(&live, None);
        assert_eq!(
            current.status.code(),
            snapshot.status.code(),
            "{args:?}: {current:?} vs {snapshot:?}"
        );
        assert_eq!(current.stderr, snapshot.stderr, "{args:?}");
        if context {
            let complete_groups = |bytes: &[u8]| {
                let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
                let mut remaining = bytes;
                let mut groups = Vec::new();
                // Numbered content cannot contain an unprefixed blank;
                // double LF separates complete heading file groups.
                while let Some(at) = remaining.windows(2).position(|w| w == b"\n\n") {
                    groups.push(remaining[..at].to_vec());
                    remaining = &remaining[at + 2..];
                }
                groups.push(remaining.to_vec());
                groups.sort();
                groups
            };
            assert_eq!(
                complete_groups(&current.stdout),
                complete_groups(&snapshot.stdout),
                "{args:?}"
            );
        } else {
            assert_eq!(
                groups(&current.stdout, &self.tree()),
                groups(&snapshot.stdout, &self.tree()),
                "{args:?}"
            );
        }
    }
}

#[test]
fn m6_live_and_snapshot_agree_without_policy_or_tree_changes() {
    let fixture = Fixture::new("live-policy-agreement");
    fs::write(
        fixture.base.join("home/config/ferret/ignore"),
        b"global-block\n.git/\n",
    )
    .unwrap();
    fs::create_dir_all(fixture.tree().join(".git/info")).unwrap();
    fixture.write(".git/info/exclude", b"vcs-exclude\n");
    fixture.write(".gitignore", b"*.vcs\n");
    fixture.write(
        ".ferretignore",
        b"ferret-block\n/target/\n!/target/keep/**\n",
    );
    fs::create_dir_all(fixture.tree().join("target/keep")).unwrap();
    // Both directories are Traverse, so neither input may be opened.
    fixture.write("target/.ferretignore", b"!*\n");
    fixture.write("target/.git", b"not a gitdir file\n");
    fixture.write("target/keep/.ferretignore", b"*\n");
    for (name, bytes) in [
        ("global-block", b"red global\n".as_slice()),
        ("vcs-exclude", b"red vcs exclude\n"),
        ("lost.vcs", b"red vcs\n"),
        ("ferret-block", b"red ferret\n"),
        ("target/lost", b"red excluded\n"),
        ("target/keep/found", b"red reinclude\n"),
        ("binary-held", b"red\0text\n"),
    ] {
        fixture.write(name, bytes);
    }
    let large = fs::File::create(fixture.tree().join("large-held")).unwrap();
    large.set_len(9 << 20).unwrap();
    let indexed = fixture.ferret(&["index", "."], None);
    assert!(indexed.status.success(), "{indexed:?}");
    for args in [
        vec!["red", "."],
        vec!["--hidden", "red", "."],
        vec!["-a", "red", "."],
        vec!["-c", "--include-zero", "red", "."],
        vec!["--files-without-match", "violet", "."],
        vec!["-v", "red", "."],
        vec!["-g", "*block", "red", "."],
        vec!["--files", "."],
        vec!["--files", "--hidden", "."],
        vec!["--max-depth", "1", "red", "."],
        vec!["-C1", "red", "."],
        vec!["red", "target/keep"],
    ] {
        fixture.agree_live(&args);
    }
    let output = fixture.ferret(&["grep", "--live", "red", "."], None);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("target/keep/found:red reinclude"),
        "{output:?}"
    );
    for absent in [
        "global-block",
        "vcs-exclude",
        "lost.vcs",
        "ferret-block",
        "target/lost",
        "binary-held",
        "large-held",
    ] {
        assert!(!text.contains(absent), "{absent}: {output:?}");
    }
}

#[test]
fn m6_live_discovers_new_names_and_new_matches_without_widening_visibility() {
    let fixture = Fixture::new("live-freshness");
    fixture.write(".ferretignore", b"blocked\n");
    fixture.write("old-negative", b"blue\n");
    fixture.write("blocked", b"red excluded\n");
    assert!(fixture.ferret(&["index", "."], None).status.success());
    fixture.write("old-negative", b"red new match\n");
    fixture.write("new-file", b"red new name\n");
    fs::create_dir(fixture.tree().join("new-dir")).unwrap();
    fixture.write("new-dir/leaf", b"red new child\n");
    let snapshot = fixture.ferret(&["grep", "red", "."], None);
    assert!(!String::from_utf8_lossy(&snapshot.stdout).contains("old-negative"));
    assert!(!String::from_utf8_lossy(&snapshot.stdout).contains("new-file"));
    let live = fixture.ferret(&["grep", "--live", "red", "."], None);
    assert!(live.status.success(), "{live:?}");
    let text = String::from_utf8_lossy(&live.stdout);
    for name in ["old-negative", "new-file", "new-dir/leaf"] {
        assert!(text.contains(name), "{live:?}");
    }
    assert!(!text.contains("blocked"));
    for args in [
        vec!["--live", "red", "new-dir"],
        vec!["--live", "--no-ignore", "red", "blocked"],
    ] {
        let mut oracle = args.clone();
        oracle.retain(|arg| *arg != "--live");
        let expected = fixture.oracle(&oracle, None);
        let mut actual = vec!["grep"];
        actual.extend(args);
        let actual = fixture.ferret(&actual, None);
        assert_eq!(actual.status.code(), expected.status.code());
        assert_eq!(
            groups(&actual.stdout, &fixture.tree()),
            groups(&expected.stdout, &fixture.tree())
        );
    }
    // A posting-negative snapshot can establish zero, but a live read cannot
    // use that absence after the file acquired a match.
    for flags in [vec!["-c", "--include-zero"], vec!["--files-without-match"]] {
        let mut indexed = vec!["grep"];
        indexed.extend(flags.clone());
        indexed.extend(["red", "."]);
        let mut current = vec!["grep", "--live"];
        current.extend(flags);
        current.extend(["red", "."]);
        let old = fixture.ferret(&indexed, None);
        let new = fixture.ferret(&current, None);
        assert!(old.status.success() && new.status.success());
        assert!(String::from_utf8_lossy(&old.stdout).contains("old-negative"));
        assert!(!String::from_utf8_lossy(&new.stdout).contains("old-negative:0"));
        if current.contains(&"--files-without-match") {
            assert!(!String::from_utf8_lossy(&new.stdout).contains("old-negative"));
        }
    }
}

#[test]
fn m6_live_nested_roots_and_ancestor_reconstruction_match_snapshot() {
    let fixture = Fixture::new("live-nested-root");
    for directory in ["deep/inner", "target/keep/deeper"] {
        fs::create_dir_all(fixture.tree().join(directory)).unwrap();
    }
    fixture.write(".ferretignore", b"*.blocked\n/target/\n!/target/keep/**\n");
    fixture.write("deep/inner/found.blocked", b"red nested root\n");
    fixture.write("target/.ferretignore", b"!*\n");
    fixture.write("target/keep/.ferretignore", b"*.blocked\n");
    fixture.write("target/keep/deeper/found.blocked", b"red traversed\n");
    assert!(
        fixture
            .ferret(&["index", ".", "deep/inner"], None)
            .status
            .success()
    );
    for args in [
        vec!["red", "."],
        vec!["red", "deep/inner"],
        vec!["--files", "."],
        vec!["red", "target/keep/deeper"],
        vec!["--files", "target/keep/deeper"],
    ] {
        fixture.agree_live(&args);
    }
}

#[test]
fn m6_live_policy_changes_and_explicit_operand_exceptions() {
    let fixture = Fixture::new("live-policy-changes");
    fixture.write(".ferretignore", b"blocked\n");
    fs::create_dir(fixture.tree().join("blocked")).unwrap();
    fixture.write("blocked/.ferretignore", b"*\n");
    fixture.write("blocked/leaf", b"red explicit ignored start\n");
    assert!(fixture.ferret(&["index", "."], None).status.success());
    for args in [
        vec!["red", "blocked"],
        vec!["--files", "blocked"],
        vec!["red", "blocked/leaf"],
    ] {
        fixture.agree_live(&args);
    }
    fixture.write(".ferretignore", b"one.rs\n");
    let current = fixture.ferret(&["grep", "--live", "red", "."], None);
    let text = String::from_utf8_lossy(&current.stdout);
    assert!(current.status.success(), "{current:?}");
    assert!(!text.contains("one.rs"));
    assert!(!text.contains("blocked/leaf")); // entered now, its current rule applies
    let direct = fixture.ferret(&["grep", "--live", "red", "one.rs"], None);
    assert!(direct.status.success());
    let outside = fixture.base.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("leaf"), b"red outside\n").unwrap();
    let outside = outside.to_str().unwrap();
    let denied = fixture.ferret(&["grep", "--live", "red", outside], None);
    assert_eq!(denied.status.code(), Some(2));
    assert!(denied.stdout.is_empty());
    let allowed = fixture.ferret(&["grep", "--live", "-u", "red", outside], None);
    assert!(allowed.status.success(), "{allowed:?}");
}

#[test]
fn m6_live_prunes_excluded_ancestors_of_nested_roots() {
    let fixture = Fixture::new("live-excluded-parent");
    fs::create_dir_all(fixture.tree().join("sealed/inner")).unwrap();
    fixture.write(".ferretignore", b"/sealed/\n");
    fixture.write("sealed/inner/leaf", b"red nested\n");
    fixture.write("sealed/inner/.ferretignore", b"inner-blocked\n");
    fixture.write("sealed/inner/inner-blocked", b"red nested exclusion\n");
    fixture.write("sealed/lost", b"red ignored\n");
    let indexed = fixture.ferret(&["index", ".", "sealed/inner"], None);
    assert!(indexed.status.success(), "{indexed:?}");
    for args in [
        vec!["red", "."],
        vec!["--files", "."],
        vec!["red", "sealed/inner"],
        vec!["red", "sealed"],
    ] {
        fixture.agree_live(&args);
    }
    let output = fixture.ferret(&["grep", "--live", "red", "."], None);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(!text.contains("sealed/inner/leaf"), "{output:?}");
    let explicit = fixture.ferret(&["grep", "--live", "red", "sealed/inner"], None);
    assert!(explicit.status.success());
    assert!(String::from_utf8_lossy(&explicit.stdout).contains("leaf:red nested"));
    assert!(!text.contains("sealed/lost"), "{output:?}");
}

#[test]
fn m6_live_gitdir_commondir_and_symlinked_ignore_inputs_match_snapshot() {
    for git_link in [false, true] {
        let fixture = Fixture::new(if git_link {
            "live-symlink-git"
        } else {
            "live-linked-git"
        });
        let common = fixture.base.join("git-area");
        fs::create_dir_all(common.join("info")).unwrap();
        fs::create_dir_all(common.join("worktrees/checkout")).unwrap();
        fs::write(common.join("worktrees/checkout/commondir"), b"../..\n").unwrap();
        fixture.write("exclude-rules", b"vcs-excluded\n");
        symlink(
            fixture.tree().join("exclude-rules"),
            common.join("info/exclude"),
        )
        .unwrap();
        if git_link {
            symlink(&common, fixture.tree().join(".git")).unwrap();
        } else {
            fixture.write(".git", b"gitdir: ../git-area/worktrees/checkout\n");
        }
        fixture.write(".gitignore", b"*.hide\n");
        fixture.write("ferret-rules", b"!kept.hide\n");
        symlink("ferret-rules", fixture.tree().join(".ferretignore")).unwrap();
        for name in ["vcs-excluded", "lost.hide", "kept.hide", "plain"] {
            fixture.write(name, b"red\n");
        }
        assert!(fixture.ferret(&["index", "."], None).status.success());
        for args in [
            vec!["red", "."],
            vec!["--files", "--hidden", "."],
            vec!["red", "sub"],
        ] {
            fixture.agree_live(&args);
        }
        fs::remove_file(fixture.tree().join(".gitignore")).unwrap();
        fixture.write("vcs-rules", b"plain\n");
        symlink("vcs-rules", fixture.tree().join(".gitignore")).unwrap();
        assert!(fixture.ferret(&["index", "."], None).status.success());
        fixture.agree_live(&["red", "."]);
    }
}

impl Fixture {
    fn live_oracle(&self, args: &[&str], input: Option<&[u8]>) {
        let mut oracle = args.to_vec();
        oracle.retain(|arg| *arg != "--live");
        let expected = self.oracle(&oracle, input);
        let mut candidate = vec!["grep"];
        candidate.extend_from_slice(args);
        let actual = self.ferret(&candidate, input);
        assert_eq!(
            actual.status.code(),
            expected.status.code(),
            "{args:?}: {actual:?} vs {expected:?}"
        );
        assert_eq!(
            actual.stderr.is_empty(),
            expected.stderr.is_empty(),
            "{args:?}"
        );
        assert_eq!(
            groups(&actual.stdout, &self.tree()),
            groups(&expected.stdout, &self.tree()),
            "{args:?}"
        );
    }
}

#[test]
fn m6_live_binary_depth_and_operand_matrix() {
    let fixture = Fixture::new("live-operands");
    fs::create_dir(fixture.tree().join("nul")).unwrap();
    fixture.write("nul/before", b"red\0x\nblue\n");
    assert!(fixture.ferret(&["index", "."], None).status.success());
    for args in [
        vec!["--live", "red", "one.rs"],
        vec!["--live", "red", "link"],
        vec!["--live", "red", "dirlink"],
        vec!["--live", "red", "sub", "-"],
        vec!["--live", "red", "-"],
        vec!["--live", "--files", "sub"],
        vec!["--live", "--max-depth=0", "red", "."],
        vec!["--live", "--max-depth=1", "red", "sub"],
        vec!["--live", "--hidden", "red", "."],
        vec!["--live", "-g", "*.rs", "red", "."],
        vec!["--live", "-a", "red", "nul"],
        vec!["--live", "-av", "red", "nul"],
        vec!["--live", "-ac", "red", "nul"],
        vec!["--live", "-aq", "red", "nul"],
        vec!["--live", "-al", "red", "nul"],
        vec!["--live", "-ao", "red", "nul"],
        vec!["--live", "-a", "--files-without-match", "red", "nul"],
        vec!["--live", "-ac", "--include-zero", "violet", "nul"],
    ] {
        fixture.live_oracle(&args, Some(b"red stdin\n"));
    }
    fs::remove_dir_all(fixture.base.join("index")).unwrap();
    for args in [
        vec!["--live", "red", "one.rs"],
        vec!["--live", "red", "-"],
        vec!["--live", "-u", "red", "."],
        vec!["--live", "--files", "-uu", "."],
    ] {
        fixture.live_oracle(&args, Some(b"red stdin\n"));
    }
}

#[test]
fn m6_recursive_cap_is_independent_of_text_and_freshness() {
    let fixture = Fixture::new("live-cap");
    fs::create_dir(fixture.tree().join("large")).unwrap();
    fixture.write("large/big", b"red\n");
    let file = fs::OpenOptions::new()
        .write(true)
        .open(fixture.tree().join("large/big"))
        .unwrap();
    file.set_len(9 << 20).unwrap();
    assert!(fixture.ferret(&["index", "."], None).status.success());
    for flags in [vec!["-a"], vec!["--live", "-a"]] {
        let mut args = vec!["grep"];
        args.extend(flags);
        args.extend(["-q", "red", "large"]);
        let result = fixture.ferret(&args, None);
        assert_eq!(result.status.code(), Some(1), "{result:?}");
    }
    for args in [
        vec!["--live", "-auq", "red", "large"],
        vec!["-auq", "red", "large"],
        vec!["--live", "-aq", "red", "large/big"],
    ] {
        fixture.live_oracle(&args, None);
    }
    fixture.agree_live(&["--files", "large"]);
}

#[test]
fn m6_live_policy_faults_never_establish_zero_or_no_match() {
    let fixture = Fixture::new("live-policy-faults");
    // A socket cannot be opened as a policy file. A dangling scoped ignore
    // symlink is absent under crawl's rules, rather than an input fault.
    let _socket =
        std::os::unix::net::UnixListener::bind(fixture.tree().join(".ferretignore")).unwrap();
    for flags in [
        vec!["-c", "--include-zero"],
        vec!["--files-without-match"],
        vec!["--files"],
    ] {
        let mut args = vec!["grep", "--live"];
        args.extend(flags);
        if !args.contains(&"--files") {
            args.push("violet");
        }
        args.push(".");
        let output = fixture.ferret(&args, None);
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty() && !output.stderr.is_empty());
    }
    fixture.live_oracle(&["--live", "-u", "red", "."], None);
}

// M6 audit: a NUL beyond the 8 KiB indexing probe is still inside rg's first
// recursive read. Early -l/-q must not publish a match before detecting it.
#[test]
fn m6_unrestricted_binary_probe_covers_the_initial_scan_window() {
    let fixture = Fixture::new("unrestricted-late-nul");
    fs::create_dir(fixture.tree().join("late-nul")).unwrap();
    let mut bytes = b"red\n".to_vec();
    bytes.extend_from_slice(&b"blue\n".repeat(6400));
    bytes.extend_from_slice(b"\0red\n");
    fixture.write("late-nul/sample", &bytes);
    for flags in [
        vec![],
        vec!["-l"],
        vec!["-q"],
        vec!["-c"],
        vec!["--count-matches"],
        vec!["-o"],
        vec!["-c", "--include-zero"],
    ] {
        let mut args = vec!["-u"];
        args.extend(flags);
        args.extend(["red", "late-nul"]);
        assert_eq!(fixture.compare(&args, None).status.code(), Some(1));
        args.insert(0, "-a");
        assert_eq!(fixture.compare(&args, None).status.code(), Some(0));
    }
    fixture.compare(&["-ul", "red", "late-nul/sample"], None);
    // Recursive binary absence is the written policy, including exit status.
    // rg's --files-without-match returns 0 with no record here; ferret's
    // absent input produces no record and no selected name, hence exit 1.
    let rg = fixture.oracle(&["-u", "--files-without-match", "red", "late-nul"], None);
    let own = fixture.ferret(
        &["grep", "-u", "--files-without-match", "red", "late-nul"],
        None,
    );
    assert_eq!(rg.status.code(), Some(0));
    assert!(rg.stdout.is_empty());
    assert_eq!(own.status.code(), Some(1));
    assert!(own.stdout.is_empty() && own.stderr.is_empty());
}

// Live policy used to abort on the bad line instead of retaining usable rules.
#[test]
fn m6_live_bad_patterns_warn_and_preserve_valid_rules() {
    let fixture = Fixture::new("live-bad-patterns");
    fs::create_dir(fixture.tree().join("rules")).unwrap();
    fixture.write("rules/.ferretignore", b"*.log\nbad\\\n");
    fixture.write("rules/keep.txt", b"red\n");
    fixture.write("rules/drop.log", b"red\n");
    for (flags, expected) in [
        (vec!["red"], &b"rules/keep.txt:red\n"[..]),
        (vec!["--files"], &b"rules/keep.txt\n"[..]),
    ] {
        let mut args = vec!["grep", "--live"];
        args.extend(flags);
        args.push("rules");
        let result = fixture.ferret(&args, None);
        assert_eq!(result.status.code(), Some(0), "{result:?}");
        assert_eq!(result.stdout, expected, "{result:?}");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("warning: ignore pattern skipped:"),
            "{result:?}"
        );
    }
}

// Scoped policy must use crawl's bounded loader, including names-only and
// negative-output modes: rejected policy cannot establish absence.
#[test]
fn m6_live_oversized_policy_is_a_fault() {
    let fixture = Fixture::new("live-oversized-policy");
    fs::write(fixture.base.join("home/config/ferret/ignore"), b"blocked\n").unwrap();
    fixture.write("blocked", b"red\n");
    let mut rules = b"!blocked\n#".to_vec();
    rules.resize((1 << 20) + 1, b'x');
    fixture.write(".ferretignore", &rules);
    for flags in [
        vec!["red"],
        vec!["--files"],
        vec!["-c", "--include-zero", "violet"],
        vec!["--files-without-match", "violet"],
    ] {
        let mut args = vec!["grep", "--live"];
        args.extend(flags);
        args.push(".");
        let result = fixture.ferret(&args, None);
        assert_eq!(result.status.code(), Some(2), "{result:?}");
        assert!(result.stdout.is_empty(), "{result:?}");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("ignore file is larger than 1 MiB"),
            "{result:?}"
        );
    }
}

// Resolving an inaccessible root used to abort queries outside its scope.
// An ancestor query includes that failed anchor and must still fail.
#[test]
fn m6_live_anchor_faults_are_scoped_to_requested_roots() {
    let fixture = Fixture::new("live-root-faults");
    fs::create_dir(fixture.tree().join("healthy")).unwrap();
    fs::create_dir_all(fixture.tree().join("private/other")).unwrap();
    fixture.write("healthy/hit", b"red\n");
    fixture.write("private/other/hit", b"red\n");
    let indexed = fixture.ferret(&["index", "healthy", "private/other"], None);
    assert!(indexed.status.success(), "{indexed:?}");
    let private = fixture.tree().join("private");
    fs::set_permissions(&private, fs::Permissions::from_mode(0o000)).unwrap();
    // Restore permissions before assertions, including when a CLI regresses.
    let results = [
        vec!["grep", "--live", "red", "healthy"],
        vec!["grep", "--live", "--files", "healthy"],
        vec!["grep", "--live", "red", "private/other"],
        vec!["grep", "--live", "--files", "private/other"],
        vec!["grep", "--live", "red", "."],
        vec!["grep", "--live", "--files", "."],
    ]
    .map(|args| fixture.ferret(&args, None));
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    for (result, expected) in results[..2]
        .iter()
        .zip([&b"healthy/hit:red\n"[..], &b"healthy/hit\n"[..]])
    {
        assert_eq!(result.status.code(), Some(0), "{result:?}");
        assert_eq!(result.stdout, expected, "{result:?}");
        assert!(result.stderr.is_empty(), "{result:?}");
    }
    for result in &results[2..] {
        assert_eq!(result.status.code(), Some(2), "{result:?}");
        assert!(result.stdout.is_empty(), "{result:?}");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("Permission denied"),
            "{result:?}"
        );
    }
}

// Failed roots must share the operand's canonical namespace even when the
// catalog retains a symlink spelling. The operand is both a root and the
// ancestor of a now-missing root; aliases and multiple missing components
// must not hide that failure from either search or names-only mode.
#[test]
fn m6_live_failed_roots_resolve_symlink_ancestors() {
    for (name, nested) in [("direct", "gone"), ("deep", "gone/child")] {
        let fixture = Fixture::new(&format!("live-symlink-root-{name}"));
        fs::create_dir_all(fixture.tree().join("policy-real").join(nested)).unwrap();
        fixture.write("policy-real/hit", b"red\n");
        symlink("policy-real", fixture.tree().join("policy-link")).unwrap();
        let nested_root = format!("policy-link/{nested}");
        let indexed = fixture.ferret(&["index", "policy-link", &nested_root], None);
        assert!(indexed.status.success(), "{indexed:?}");
        fs::remove_dir_all(fixture.tree().join("policy-real/gone")).unwrap();
        for operand in ["policy-link", "policy-real"] {
            for flags in [vec!["red"], vec!["--files"]] {
                let mut args = vec!["grep", "--live"];
                args.extend(flags);
                args.push(operand);
                let output = fixture.ferret(&args, None);
                assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
                assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
                let diagnostic = String::from_utf8_lossy(&output.stderr);
                assert!(
                    diagnostic.contains("policy root") && diagnostic.contains("gone"),
                    "{output:?}"
                );
            }
        }
        for flags in [vec!["red"], vec!["--files"]] {
            let mut args = vec!["grep", "--live"];
            args.extend(flags);
            args.push("sub");
            let output = fixture.ferret(&args, None);
            assert_eq!(output.status.code(), Some(0), "{output:?}");
            assert!(
                !output.stdout.is_empty() && output.stderr.is_empty(),
                "{output:?}"
            );
        }
    }
}
