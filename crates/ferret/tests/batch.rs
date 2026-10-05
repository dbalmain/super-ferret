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
fn read_only_find_refuses_actions_before_running_them() {
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
    assert!(text.contains("actions are not available until M2c"));
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
    assert!(String::from_utf8_lossy(&partial.stdout).contains("\"id\":null,\"event\":\"error\""));
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
