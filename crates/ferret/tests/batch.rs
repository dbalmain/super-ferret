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
        let mut command = fixture::command(FERRET, &self.0);
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
