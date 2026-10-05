//! End-to-end checks for the resident batch host.
#![allow(clippy::unwrap_used)]

#[path = "support/fixture.rs"]
#[allow(dead_code)]
mod fixture;

use std::fs;
use std::io::Write;
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

const FERRET: &str = env!("CARGO_BIN_EXE_ferret");

struct Tree(std::path::PathBuf);
impl Tree {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ferret-batch-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(path.join("src")).unwrap();
        fs::write(path.join("src/main.rs"), b"fn main() {}\n").unwrap();
        let output = fixture::command(FERRET, &path)
            .arg("index")
            .arg(path.join("src"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self(path)
    }
    fn run(&self, input: &[u8]) -> std::process::Output {
        self.run_with_index(input, self.0.join("index"))
    }
    fn run_with_index(&self, input: &[u8], index: std::path::PathBuf) -> std::process::Output {
        let mut command = fixture::command(FERRET, &self.0);
        command.env("FERRET_INDEX", index);
        command
            .arg("batch")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn resident_search_and_find_emit_tagged_blocks_and_recover_after_bad_lines() {
    let tree = Tree::new();
    let output = tree.run(b"{\"id\":\"s\",\"op\":\"search\",\"args\":[\"case:main\"]}\nnot-json\n{\"id\":\"f\",\"op\":\"find\",\"args\":[\"src\",\"-name\",\"*.rs\",\"-print0\"]}\n{\"id\":\"z\",\"op\":\"status\",\"args\":[]}\n");
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("\"id\":\"s\",\"event\":\"begin\""));
    assert!(text.contains("\"id\":\"s\",\"event\":\"row\""));
    assert!(text.contains("\"id\":\"s\",\"event\":\"end\",\"exit\":0"));
    assert!(text.contains("\"id\":null,\"event\":\"error\""));
    assert!(text.contains("\"id\":\"f\",\"event\":\"stdout\""));
    assert!(text.contains("\"id\":\"z\",\"event\":\"status\""));
}

#[test]
fn find_requires_effects_capability_before_running_actions() {
    let tree = Tree::new();
    let marker = tree.0.join("marker");
    let line = format!(
        "{{\"id\":\"x\",\"op\":\"find\",\"args\":[\"src\",\"-exec\",\"touch\",\"{}\",\";\"]}}\n",
        marker.display()
    );
    let output = tree.run(line.as_bytes());
    assert_eq!(output.status.code(), Some(0));
    assert!(!marker.exists());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("LocalEffectsRequired"));
}

fn request(id: &str, args: &[&str], cwd: &str) -> Vec<u8> {
    let argv = args
        .iter()
        .map(|arg| format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"id\":\"{id}\",\"op\":\"find\",\"cwd\":\"{cwd}\",\"args\":[{argv}]}}\n")
        .into_bytes()
}

fn event_lines(output: &[u8], id: &str) -> Vec<String> {
    String::from_utf8_lossy(output)
        .lines()
        .filter(|line| line.contains(&format!("\"id\":\"{id}\"")))
        .map(str::to_owned)
        .collect()
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let marker = format!("\"{key}\":\"");
    let tail = line.split_once(&marker)?.1;
    Some(tail.split_once('"')?.0)
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(a >> 2) as usize] as char);
        out.push(TABLE[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[test]
fn find_frames_match_cli_bytes_for_a_find_compat_style_corpus() {
    let tree = Tree::new();
    let cases: &[&[&str]] = &[
        &["src", "-print"],
        &["src", "-print0"],
        &["src", "-type", "f", "-print"],
        &["src", "-name", "*.rs", "-print"],
        &["src", "-iname", "MAIN.RS", "-print"],
        &["src", "-maxdepth", "1", "-print"],
        &["src", "-mindepth", "1", "-print"],
        &["src", "-printf", "%p:%s\\n"],
        &["src", "-printf", "%f\\0"],
        &["src", "-type", "d", "-print"],
        &["src", "-false", "-o", "-print0"],
        &["-I", "src", "-name", "main.rs", "-print"],
        &["-I", ".", "-maxdepth", "0", "-print"],
        &["-I", "src/..", "-maxdepth", "0", "-print"],
        &["-I", "./src/", "-maxdepth", "0", "-print"],
    ];
    let cwd = tree.0.to_str().unwrap();
    for (index, args) in cases.iter().enumerate() {
        let mut cli = fixture::command(FERRET, &tree.0);
        cli.arg("find").args(*args);
        let expected = cli.output().unwrap();
        let output = tree.run(&request(&format!("f{index}"), args, cwd));
        assert_eq!(output.status.code(), Some(0));
        let lines = event_lines(&output.stdout, &format!("f{index}"));
        let records = lines
            .iter()
            .filter(|line| line.contains("\"event\":\"stdout\""))
            .filter_map(|line| {
                line.split_once("\"record\":")?
                    .1
                    .split_once(',')?
                    .0
                    .parse::<u64>()
                    .ok()
            })
            .collect::<std::collections::BTreeSet<_>>();
        let frame_count = lines
            .iter()
            .filter(|line| line.contains("\"event\":\"stdout\""))
            .count();
        assert_eq!(
            records.len(),
            frame_count,
            "duplicate record id in {args:?}"
        );
        let encoded = lines
            .iter()
            .filter_map(|line| field(line, "bytes_base64"))
            .collect::<String>();
        assert_eq!(encoded, base64(&expected.stdout), "argv={args:?}");
        let end = lines
            .iter()
            .find(|line| line.contains("\"event\":\"end\""))
            .unwrap();
        assert!(end.contains("\"exit\":0"), "argv={args:?}: {end}");
    }
}

#[test]
fn batch_statuses_limits_protocol_recovery_and_actual_open_count() {
    let tree = Tree::new();
    let input = b"{\"id\":\"hit\",\"op\":\"search\",\"args\":[\"case:main\"],\"limit\":1}\n{\"id\":\"miss\",\"op\":\"search\",\"args\":[\"case:absent\"]}\n{\"id\":\"usage\",\"op\":\"search\",\"args\":[\"re:[\"]}\n{\"id\":\"stats\",\"op\":\"status\",\"args\":[]}\n";
    let output = tree.run(input);
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0));
    for (id, status) in [("hit", 0), ("miss", 1), ("usage", 2)] {
        assert!(
            text.contains(&format!(
                "\"id\":\"{id}\",\"event\":\"end\",\"exit\":{status}"
            )),
            "{text}"
        );
    }
    assert!(text.contains("\"engine_opens\":1"), "{text}");
    assert_eq!(text.matches("\"event\":\"row\"").count(), 1);
    let mut malformed = vec![b'x'; 1_048_577];
    malformed.extend_from_slice(b"\n{\"id\":\"after\",\"op\":\"status\",\"args\":[]}");
    let output = tree.run(&malformed);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("LineTooLong"));
    assert!(text.contains("\"id\":\"after\",\"event\":\"status\""));
    let mut oversized =
        b"{\"id\":\"large-id\",\"op\":\"status\",\"args\":[],\"padding\":\"".to_vec();
    oversized.extend(std::iter::repeat_n(b'x', 1_048_600));
    oversized.extend_from_slice(b"\"}\n");
    let output = tree.run(&oversized);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("\"id\":\"large-id\",\"event\":\"error\"")
    );
}

#[test]
fn large_find_record_splits_at_64_kib_and_eof_partial_request_is_reported() {
    let tree = Tree::new();
    let format = "x".repeat(70_000);
    let args = ["-I", "src", "-maxdepth", "0", "-printf", &format];
    let input = request("large", &args, tree.0.to_str().unwrap());
    let output = tree.run(&input);
    let lines = event_lines(&output.stdout, "large");
    let frames: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("\"event\":\"stdout\""))
        .collect();
    assert_eq!(frames.len(), 2);
    assert!(frames[0].contains("\"part\":0,\"last\":false"));
    assert!(frames[1].contains("\"part\":1,\"last\":true"));
    assert_eq!(
        field(frames[0], "bytes_base64").unwrap().len(),
        4 * (64 * 1024 / 3 + 1)
    );
    let partial = tree.run(b"{\"id\":\"eof\",\"op\":\"search\"");
    assert!(
        String::from_utf8_lossy(&partial.stdout).contains("\"id\":\"eof\",\"event\":\"error\"")
    );
}

#[test]
fn non_utf8_search_and_find_operands_are_decoded_from_base64() {
    use std::os::unix::ffi::OsStringExt;
    let tree = Tree::new();
    let bad = std::ffi::OsString::from_vec(b"bad\xff".to_vec());
    fs::create_dir(tree.0.join(&bad)).unwrap();
    let request = format!(
        "{{\"id\":\"bytes\",\"op\":\"find\",\"args\":[\"-I\",{{\"base64\":\"{}\"}},\"-maxdepth\",\"0\",\"-print0\"]}}\n",
        "YmFk/w=="
    );
    let output = tree.run(request.as_bytes());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("\"bytes_base64\":\"YmFk/wA=\""), "{text}");
    let output =
        tree.run(b"{\"id\":\"atom\",\"op\":\"search\",\"args\":[{\"base64\":\"/w==\"}]}\n");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("\"id\":\"atom\",\"event\":\"end\",\"exit\":1"),
        "{text}"
    );
}

#[test]
fn missing_index_is_a_search_runtime_status() {
    let tree = Tree::new();
    let output = tree.run_with_index(
        b"{\"id\":\"runtime\",\"op\":\"search\",\"args\":[\"x\"]}\n",
        tree.0.join("missing-index"),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("\"id\":\"runtime\",\"event\":\"end\",\"exit\":3")
    );
}

#[test]
fn reload_adopts_a_writer_refresh_while_the_batch_engine_is_resident() {
    use ferret_crawl::{IndexOptions, Refresh, index};
    use std::io::{BufRead, BufReader, Read};

    let tree = Tree::new();
    let mut command = fixture::command(FERRET, &tree.0);
    command
        .arg("batch")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    writeln!(input, "{{\"id\":\"before\",\"op\":\"status\",\"args\":[]}}").unwrap();
    input.flush().unwrap();
    let mut before = String::new();
    output.read_line(&mut before).unwrap();
    assert!(before.contains("\"sequence\":0"), "{before}");
    // An unchanged generation must not rebuild the resident engine, and a
    // control request needs no `args`.
    writeln!(input, "{{\"id\":\"same\",\"op\":\"reload\"}}").unwrap();
    input.flush().unwrap();
    let mut same = String::new();
    output.read_line(&mut same).unwrap();
    assert!(same.contains("\"event\":\"reload\""), "{same}");
    assert!(same.contains("\"engine_opens\":1"), "{same}");

    fs::write(tree.0.join("src/newleaf.rs"), b"pub fn newleaf() {}\n").unwrap();
    index(
        &tree.0.join("index"),
        &[tree.0.join("src")],
        Refresh::All,
        &IndexOptions::default(),
    )
    .unwrap();
    writeln!(input, "{{\"id\":\"reload\",\"op\":\"reload\",\"args\":[]}}").unwrap();
    writeln!(
        input,
        "{{\"id\":\"new\",\"op\":\"search\",\"args\":[\"case:newleaf\"]}}"
    )
    .unwrap();
    drop(input);
    let mut rest = String::new();
    output.read_to_string(&mut rest).unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
    assert!(
        rest.contains("\"id\":\"reload\",\"event\":\"reload\""),
        "{rest}"
    );
    assert!(rest.contains("\"id\":\"new\",\"event\":\"row\""), "{rest}");
    assert!(
        rest.contains("\"id\":\"new\",\"event\":\"end\",\"exit\":0"),
        "{rest}"
    );
    assert!(rest.contains("\"engine_opens\":2"), "{rest}");
}

fn effects_request(args: &[&str], tree: &Tree) -> Vec<u8> {
    let mut line = request("effect", args, tree.0.to_str().unwrap());
    line.truncate(line.len() - 2);
    line.extend_from_slice(b",\"capabilities\":[\"local-effects\"],\"child_stdin\":\"null\"}\n");
    line
}

fn decoded_frames(output: &[u8], event: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    for line in String::from_utf8(output.to_vec()).unwrap().lines() {
        if field(line, "event") != Some(event) {
            continue;
        }
        let encoded = field(line, "bytes_base64").unwrap();
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

fn tree_state(tree: &Tree) -> Vec<(std::path::PathBuf, &'static str, Vec<u8>)> {
    fn visit(
        root: &std::path::Path,
        path: &std::path::Path,
        state: &mut Vec<(std::path::PathBuf, &'static str, Vec<u8>)>,
    ) {
        let metadata = fs::symlink_metadata(path).unwrap();
        let (kind, bytes) = if metadata.is_dir() {
            ("dir", Vec::new())
        } else if metadata.is_symlink() {
            use std::os::unix::ffi::OsStrExt;
            (
                "symlink",
                fs::read_link(path).unwrap().as_os_str().as_bytes().to_vec(),
            )
        } else {
            ("file", fs::read(path).unwrap())
        };
        state.push((path.strip_prefix(root).unwrap().to_owned(), kind, bytes));
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), state);
            }
        }
    }
    let mut state = Vec::new();
    for entry in fs::read_dir(&tree.0).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() != "index" && entry.file_name() != "home" {
            visit(&tree.0, &entry.path(), &mut state);
        }
    }
    state.sort();
    state
}

#[test]
fn effectful_find_matches_cli_bytes_status_and_tree() {
    // These shapes also appear in find/action/tests.rs's differential corpus.
    let cases: &[&[&str]] = &[
        &["src", "-exec", "echo", "{}", ";"],
        &["src", "-exec", "echo", "{}", "+"],
        &["src", "-exec", "echo", "x{}y", ";"],
        &["src", "-exec", "false", ";", "-o", "-print"],
        &["src", "-exec", "false", "{}", "+", "-print"],
        &["src", "-exec", "echo", "{}", "+", "-quit"],
        &["src", "-print", "-exec", "echo", "{}", ";"],
        &["src", "-execdir", "echo", "{}", ";"],
        &["src", "-execdir", "echo", "{}", "+"],
        &["src", "-delete", "-print"],
        &[
            "src", "-depth", "(", "-type", "f", "-o", "-empty", ")", "-delete", "-print",
        ],
        &["src", "-fprint", "result"],
        &["src", "-fprint0", "result"],
        &["src", "-fprintf", "result", "%p\\0"],
        &["src", "missing", "-exec", "echo", "{}", ";", "-quit"],
    ];
    for args in cases {
        let expected_tree = Tree::new();
        let actual_tree = Tree::new();
        let expected = fixture::command(FERRET, &expected_tree.0)
            .arg("find")
            .args(*args)
            .output()
            .unwrap();
        let actual = actual_tree.run(&effects_request(args, &actual_tree));
        assert!(actual.status.success(), "{args:?}: {:?}", actual.stderr);
        assert_eq!(
            decoded_frames(&actual.stdout, "stdout"),
            expected.stdout,
            "{args:?}"
        );
        let lines = event_lines(&actual.stdout, "effect");
        assert!(
            lines
                .last()
                .unwrap()
                .contains(&format!("\"exit\":{}", expected.status.code().unwrap())),
            "{args:?}: {lines:?}"
        );
        assert_eq!(
            tree_state(&actual_tree),
            tree_state(&expected_tree),
            "{args:?}"
        );
    }
}

fn assert_json_lines(tree: &Tree, bytes: &[u8]) {
    let mut child = fixture::command("python3", &tree.0)
        .args([
            "-c",
            "import json,sys; [json.loads(line) for line in sys.stdin.buffer]",
        ])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(bytes).unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
fn child_stderr_streams_exact_bytes_and_cannot_block_stdout() {
    let tree = Tree::new();
    // A sequential stdout-then-stderr drain hangs: the child fills stderr
    // before ever writing stdout. The timeout bounds the entire process tree.
    for script in [
        "printf 'error\\000bytes\\377' >&2; printf output",
        "head -c 131073 /dev/zero >&2; printf output",
    ] {
        let args = ["src", "-maxdepth", "0", "-exec", "sh", "-c", script, ";"];
        let expected = fixture::bounded_command(FERRET, &tree.0)
            .arg("find")
            .args(args)
            .output()
            .unwrap();
        assert_eq!(expected.status.code(), Some(0));
        let mut input = effects_request(&args, &tree);
        input.extend_from_slice(b"{\"id\":\"after\",\"op\":\"status\"}\n");
        let mut child = fixture::bounded_command(FERRET, &tree.0)
            .arg("batch")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&input).unwrap();
        let actual = child.wait_with_output().unwrap();
        assert_eq!(
            actual.status.code(),
            Some(0),
            "timeout or failure: {:?}",
            actual.stderr
        );
        assert!(actual.stderr.is_empty());
        assert_json_lines(&tree, &actual.stdout);
        assert_eq!(decoded_frames(&actual.stdout, "stdout"), expected.stdout);
        assert_eq!(decoded_frames(&actual.stdout, "stderr"), expected.stderr);
        assert!(
            String::from_utf8_lossy(&actual.stdout)
                .contains("\"id\":\"after\",\"event\":\"status\"")
        );
        let lines = event_lines(&actual.stdout, "effect");
        assert!(lines.last().unwrap().contains("\"exit\":0"));
        let first_stderr = lines
            .iter()
            .position(|line| field(line, "event") == Some("stderr"))
            .unwrap();
        let first_stdout = lines
            .iter()
            .position(|line| field(line, "event") == Some("stdout"))
            .unwrap();
        assert!(
            first_stderr < first_stdout,
            "stderr should stream before the entry commits"
        );
    }
}

#[test]
fn every_host_refusal_precedes_commands_and_output_file_preparation() {
    for (options, expected_error, prompt) in [
        (r#""child_stdin":"null""#, "LocalEffectsRequired", "-exec"),
        (
            r#""capabilities":["local-effects"],"child_stdin":"null""#,
            "InteractiveRequired",
            "-ok",
        ),
        (
            r#""capabilities":["local-effects"],"child_stdin":"null""#,
            "InteractiveRequired",
            "-okdir",
        ),
        (
            r#""capabilities":["local-effects","interactive"],"child_stdin":"null""#,
            "Noninteractive",
            "-ok",
        ),
        (
            r#""capabilities":["local-effects","interactive"],"child_stdin":"null""#,
            "Noninteractive",
            "-okdir",
        ),
        (
            r#""capabilities":["local-effects"],"child_stdin":"inherit""#,
            "ChildStdinOnProtocol",
            "-exec",
        ),
        (
            r#""capabilities":["local-effects"]"#,
            "ChildStdinRequired",
            "-exec",
        ),
    ] {
        let tree = Tree::new();
        fs::write(tree.0.join("result"), b"do not truncate").unwrap();
        // The unconditional exec and fprint precede the interactive action;
        // validating only when confirm is reached would already mutate.
        let mut input = request(
            "refuse",
            &[
                "src",
                "-exec",
                "touch",
                "MARKER",
                ";",
                "-fprint",
                "result",
                prompt,
                "touch",
                "PROMPT_MARKER",
                ";",
            ],
            tree.0.to_str().unwrap(),
        );
        input.truncate(input.len() - 2);
        input.extend_from_slice(format!(",{options}}}\n").as_bytes());
        let output = tree.run(&input);
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
        assert_json_lines(&tree, &output.stdout);
        assert!(!tree.0.join("MARKER").exists());
        assert!(!tree.0.join("src/PROMPT_MARKER").exists());
        assert!(!tree.0.join("PROMPT_MARKER").exists());
        assert_eq!(fs::read(tree.0.join("result")).unwrap(), b"do not truncate");
        let lines = event_lines(&output.stdout, "refuse");
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[1].contains(&format!("\"error\":\"{expected_error}\"")),
            "{lines:?}"
        );
        assert!(lines[1].contains("\"exit\":1"));
        if expected_error == "Noninteractive" {
            // File input frees stdin from the protocol, but a pipe/null fd is
            // still not a terminal and must not reach either earlier action.
            fs::write(tree.0.join("requests"), &input).unwrap();
            let file_input = fixture::command(FERRET, &tree.0)
                .args(["batch", "--input", "requests"])
                .output()
                .unwrap();
            assert_eq!(file_input.status.code(), Some(0));
            assert!(file_input.stderr.is_empty());
            let lines = event_lines(&file_input.stdout, "refuse");
            assert_eq!(lines.len(), 2);
            assert!(lines[1].contains("\"error\":\"Noninteractive\""));
            assert!(!tree.0.join("MARKER").exists());
            assert!(!tree.0.join("PROMPT_MARKER").exists());
            assert!(!tree.0.join("src/PROMPT_MARKER").exists());
            assert_eq!(fs::read(tree.0.join("result")).unwrap(), b"do not truncate");
        }
    }
}

#[test]
fn file_input_can_inherit_stdin_and_protocol_stdin_is_null_for_children() {
    let tree = Tree::new();
    let args = ["src", "-maxdepth", "0", "-exec", "cat", ";"];
    let mut input = effects_request(&args, &tree);
    let text = String::from_utf8(input.clone())
        .unwrap()
        .replace("\"child_stdin\":\"null\"", "\"child_stdin\":\"inherit\"");
    fs::write(tree.0.join("requests"), text).unwrap();
    let caller_stdin = b"caller input\0\xff\n";
    fs::write(tree.0.join("caller"), caller_stdin).unwrap();
    let actual = fixture::command(FERRET, &tree.0)
        .args(["batch", "--input", "requests"])
        .stdin(fs::File::open(tree.0.join("caller")).unwrap())
        .output()
        .unwrap();
    let expected = fixture::command(FERRET, &tree.0)
        .arg("find")
        .args(args)
        .stdin(fs::File::open(tree.0.join("caller")).unwrap())
        .output()
        .unwrap();
    assert_eq!(actual.status.code(), Some(0));
    assert_eq!(decoded_frames(&actual.stdout, "stdout"), expected.stdout);
    assert_eq!(expected.stdout, caller_stdin);
    input.extend_from_slice(b"{\"id\":\"after\",\"op\":\"status\"}\n");
    let actual = tree.run(&input);
    assert_eq!(actual.status.code(), Some(0));
    assert!(decoded_frames(&actual.stdout, "stdout").is_empty());
    assert!(
        String::from_utf8_lossy(&actual.stdout).contains("\"id\":\"after\",\"event\":\"status\"")
    );
}

#[test]
fn execdir_and_relative_file_outputs_use_request_cwd_without_chdir() {
    let expected_tree = Tree::new();
    let actual_tree = Tree::new();
    let args = [
        ".",
        "-type",
        "f",
        "-execdir",
        "sh",
        "-c",
        "test -f main.rs && touch MARKER && printf here",
        ";",
        "-fprint",
        "result",
    ];
    let expected = fixture::command(FERRET, &expected_tree.0)
        .current_dir(expected_tree.0.join("src"))
        .arg("find")
        .args(args)
        .output()
        .unwrap();
    let input = String::from_utf8(effects_request(&args, &actual_tree))
        .unwrap()
        .replace(
            &format!("\"cwd\":\"{}\"", actual_tree.0.display()),
            &format!("\"cwd\":\"{}/src\"", actual_tree.0.display()),
        );
    let actual = actual_tree.run(input.as_bytes());
    assert!(actual.status.success());
    assert_eq!(decoded_frames(&actual.stdout, "stdout"), expected.stdout);
    assert_eq!(expected.status.code(), Some(0));
    assert!(
        event_lines(&actual.stdout, "effect")
            .last()
            .unwrap()
            .contains("\"exit\":0")
    );
    assert!(actual_tree.0.join("src/MARKER").exists());
    assert!(expected_tree.0.join("src/MARKER").exists());
    assert!(!actual_tree.0.join("MARKER").exists());
    assert!(!actual_tree.0.join("result").exists());
    assert_eq!(
        fs::read(actual_tree.0.join("src/result")).unwrap(),
        fs::read(expected_tree.0.join("src/result")).unwrap()
    );
    // The following request still resolves relative paths from launch cwd.
    let after = actual_tree.run(&request(
        "after",
        &["src", "-maxdepth", "0", "-print"],
        actual_tree.0.to_str().unwrap(),
    ));
    assert_eq!(decoded_frames(&after.stdout, "stdout"), b"src\n");
}

fn terminal_command(tree: &Tree, args: &[&str], answer: &str) -> std::process::Output {
    fixture::bounded_command("python3", &tree.0).args([
        "-c",
        "import os,pty,subprocess,sys; master,slave=pty.openpty(); child=subprocess.Popen(sys.argv[2:],stdin=slave,stdout=subprocess.PIPE,stderr=subprocess.PIPE); os.close(slave); os.write(master,sys.argv[1].encode()+b'\\n'); out,err=child.communicate(); os.close(master); sys.stdout.buffer.write(out); sys.stderr.buffer.write(err); sys.exit(child.returncode)",
        answer,
        FERRET,
    ]).args(args).output().unwrap()
}

#[test]
fn file_input_interactive_actions_require_real_approval_and_close_child_stdin() {
    for action in ["-ok", "-okdir"] {
        for answer in ["y", "n"] {
            let actual_tree = Tree::new();
            let expected_tree = Tree::new();
            let args = [
                "src/main.rs",
                action,
                "sh",
                "-c",
                "touch MARKER; if read -r line; then printf unexpected; else printf closed; fi",
                ";",
            ];
            let mut input = String::from_utf8(effects_request(&args, &actual_tree)).unwrap();
            input = input.replace("[\"local-effects\"]", "[\"local-effects\",\"interactive\"]");
            fs::write(actual_tree.0.join("requests"), input).unwrap();
            let actual = terminal_command(&actual_tree, &["batch", "--input", "requests"], answer);
            let mut cli_args = vec!["find"];
            cli_args.extend_from_slice(&args);
            let expected = terminal_command(&expected_tree, &cli_args, answer);
            assert_eq!(actual.status.code(), Some(0), "{:?}", actual.stderr);
            assert_eq!(actual.status, expected.status);
            assert!(actual.stderr.is_empty());
            assert_json_lines(&actual_tree, &actual.stdout);
            assert_eq!(decoded_frames(&actual.stdout, "stdout"), expected.stdout);
            assert_eq!(decoded_frames(&actual.stdout, "stderr"), expected.stderr);
            assert_eq!(
                actual_tree.0.join("MARKER").exists(),
                expected_tree.0.join("MARKER").exists()
            );
            assert_eq!(
                actual_tree.0.join("src/MARKER").exists(),
                expected_tree.0.join("src/MARKER").exists()
            );
            if answer == "n" {
                assert!(!actual_tree.0.join("MARKER").exists());
                assert!(!actual_tree.0.join("src/MARKER").exists());
            } else {
                assert_eq!(expected.stdout, b"closed");
            }
        }
    }
}
