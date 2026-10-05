//! `ferret --json find`: the CLI's own structured-event host, parity-checked
//! against the raw `ferret find` on the same tree and argv.
#![allow(clippy::unwrap_used)]

#[path = "support/fixture.rs"]
#[allow(dead_code)]
mod fixture;

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Output, Stdio};

const FERRET: &str = env!("CARGO_BIN_EXE_ferret");

struct Tree(PathBuf);
impl Tree {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("ferret-json-cli-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("d")).unwrap();
        fs::write(path.join("d/a.rs"), b"fn a() {}\n").unwrap();
        fs::write(path.join("d/b.txt"), b"text\n").unwrap();
        Self(path)
    }
    fn raw(&self, args: &[&str]) -> Output {
        let mut command = fixture::command(FERRET, &self.0);
        command.arg("find").args(args);
        command.output().unwrap()
    }
    fn json(&self, args: &[&str]) -> Output {
        let mut command = fixture::command(FERRET, &self.0);
        command.args(["--json", "find"]).args(args);
        command.output().unwrap()
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let marker = format!("\"{key}\":\"");
    let tail = line.split_once(&marker)?.1;
    Some(tail.split_once('"')?.0)
}

fn decoded(output: &[u8], event: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    for line in String::from_utf8(output.to_vec()).unwrap().lines() {
        if field(line, "event") != Some(event) {
            continue;
        }
        let Some(encoded) = field(line, "bytes_base64") else {
            continue;
        };
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        for chunk in encoded.as_bytes().chunks(4) {
            let value = |b| alphabet.iter().position(|byte| *byte == b).unwrap_or(0) as u8;
            let [a, b, c, d] = [
                value(chunk[0]),
                value(chunk[1]),
                value(chunk[2]),
                value(chunk[3]),
            ];
            bytes.push(a << 2 | b >> 4);
            if chunk[2] != b'=' {
                bytes.push(b << 4 | c >> 2);
            }
            if chunk[3] != b'=' {
                bytes.push(c << 6 | d);
            }
        }
    }
    bytes
}

#[test]
fn json_find_stdout_matches_raw_cli_bytes_and_status_on_success() {
    let tree = Tree::new("success");
    let raw = tree.raw(&["-I", "d", "-type", "f", "-name", "*.rs", "-print"]);
    let json = tree.json(&["-I", "d", "-type", "f", "-name", "*.rs", "-print"]);
    assert_eq!(json.status.code(), raw.status.code());
    assert_eq!(json.status.code(), Some(0));
    assert_eq!(decoded(&json.stdout, "stdout"), raw.stdout);
    let text = String::from_utf8(json.stdout).unwrap();
    assert!(text.lines().next().unwrap().contains("\"event\":\"begin\""));
    let last = text.lines().last().unwrap();
    assert!(
        last.contains("\"id\":\"find\",\"event\":\"end\",\"exit\":0"),
        "{last}"
    );
}

#[test]
fn json_find_matches_raw_cli_status_on_a_missing_start() {
    let tree = Tree::new("missing");
    let raw = tree.raw(&["-I", "absent", "-print"]);
    let json = tree.json(&["-I", "absent", "-print"]);
    assert_eq!(json.status.code(), raw.status.code());
    assert_eq!(json.status.code(), Some(1));
    let text = String::from_utf8(json.stdout).unwrap();
    assert!(
        text.contains("\"id\":\"find\",\"event\":\"end\",\"exit\":1"),
        "{text}"
    );
}

#[test]
fn a_json_find_operand_named_json_passes_through_to_finds_own_parser() {
    // `--json` precedes the `find` subcommand as a host flag; find's own
    // argv, including a later literal `--json` operand, is untouched.
    let tree = Tree::new("operand");
    fs::write(tree.0.join("d/--json"), b"literal\n").unwrap();
    let args = ["-I", "d", "-name", "--json", "-print"];
    let raw = tree.raw(&args);
    let json = tree.json(&args);
    assert_eq!(json.status.code(), raw.status.code());
    assert_eq!(json.status.code(), Some(0));
    assert_eq!(decoded(&json.stdout, "stdout"), raw.stdout);
    assert!(raw.stdout.ends_with(b"--json\n"), "{:?}", raw.stdout);
}

#[test]
fn an_exec_childs_stderr_reaches_a_stderr_event() {
    let tree = Tree::new("stderr");
    let args = [
        "-I",
        "d/a.rs",
        "-maxdepth",
        "0",
        "-exec",
        "sh",
        "-c",
        "echo oops >&2",
        ";",
    ];
    let raw = tree.raw(&args);
    let json = tree.json(&args);
    assert_eq!(json.status.code(), raw.status.code());
    assert_eq!(decoded(&json.stdout, "stderr"), raw.stderr);
    assert_eq!(decoded(&json.stdout, "stderr"), b"oops\n");
}

#[test]
fn a_failed_json_begin_stops_before_find_actions() {
    let tree = Tree::new("begin-broken-pipe");
    let marker = tree.0.join("marker");
    let mut command = fixture::command(FERRET, &tree.0);
    command
        .args([
            "--json",
            "find",
            "-I",
            "d",
            "-maxdepth",
            "0",
            "-exec",
            "touch",
        ])
        .arg(&marker)
        .arg(";")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    assert_ne!(output.status.code(), Some(0));
    assert!(!marker.exists(), "action ran after begin failed");
    assert!(output.stderr.starts_with(b"ferret: find: writing stdout: "));
}

#[test]
fn a_failed_json_end_flush_is_an_error_after_find_actions() {
    let tree = Tree::new("end-broken-pipe");
    let marker = tree.0.join("marker");
    let mut command = fixture::command(FERRET, &tree.0);
    command
        .args([
            "--json",
            "find",
            "-I",
            "d",
            "-maxdepth",
            "0",
            "-exec",
            "touch",
        ])
        .arg(&marker)
        .arg(";")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut begin = String::new();
    reader.read_line(&mut begin).unwrap();
    assert!(begin.contains("\"event\":\"begin\""), "{begin}");
    drop(reader);
    let output = child.wait_with_output().unwrap();
    assert_ne!(output.status.code(), Some(0));
    assert!(
        marker.exists(),
        "the action should precede the final end flush"
    );
    assert!(output.stderr.starts_with(b"ferret: find: writing stdout: "));
}

#[test]
fn json_find_warnings_keep_their_severity_and_render_on_stderr() {
    let tree = Tree::new("warnings");
    for args in [
        vec!["-I", "d", "-path", "x/", "-print"],
        vec!["-I", "d", "-printf", "\\q"],
    ] {
        let raw = tree.raw(&args);
        let json = tree.json(&args);
        assert_eq!(json.status.code(), raw.status.code());
        assert_eq!(json.stderr, raw.stderr);
        assert!(!json.stderr.windows(8).any(|bytes| bytes == b"find: : "));
        let text = String::from_utf8(json.stdout).unwrap();
        assert!(text.contains("\"code\":\"warning\""), "{text}");
        assert!(text.contains("\"severity\":\"warning\""), "{text}");
        assert!(text.contains("\"message\":"), "{text}");
        assert!(!text.contains("\"path\":\"\""), "{text}");
    }
}
