//! Real walkers with multiple workers; fixture failures should identify setup.
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::super::{Effects, Outcome, Plan, WalkError};

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Tree(PathBuf);
impl Tree {
    fn new() -> Self {
        let tree = Self(std::env::temp_dir().join(format!(
            "ferret-parallel-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        for branch in 0..32 {
            let mut path = tree.0.join(format!("b{branch}"));
            for _ in 0..6 {
                fs::create_dir_all(&path).unwrap();
                for file in ["keep", "drop", "skip"] {
                    fs::write(path.join(file), b"x").unwrap();
                }
                path.push("child");
            }
        }
        tree
    }
    fn plan(&self, expression: &[&str]) -> Plan {
        let args: Vec<_> = [OsString::from("-I"), self.0.clone().into_os_string()]
            .into_iter()
            .chain(expression.iter().map(OsString::from))
            .collect();
        Plan::parse(&args).unwrap()
    }
    fn run(&self, expression: &[&str], workers: usize) -> (Outcome, Output) {
        let plan = self.plan(expression);
        let output = Output::default();
        let outcome = plan
            .run_parallel(plan.live_source(), output.clone(), workers)
            .unwrap();
        (outcome, output)
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Default)]
struct Output {
    records: Arc<Mutex<Vec<Vec<u8>>>>,
    errors: Arc<Mutex<Vec<PathBuf>>>,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}
impl Effects for Output {
    fn print(&mut self, path: &Path, _: bool) -> io::Result<()> {
        self.records
            .lock()
            .unwrap()
            .push(path.as_os_str().as_bytes().to_vec());
        Ok(())
    }
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.records.lock().unwrap().extend(
            bytes
                .split(|&b| b == b'\n')
                .filter(|b| !b.is_empty())
                .map(<[u8]>::to_vec),
        );
        Ok(())
    }
    fn error(&mut self, error: &WalkError) {
        self.errors.lock().unwrap().push(error.path.clone());
    }
    fn command(&mut self, command: &mut Command) -> io::Result<bool> {
        let mut bytes = Vec::new();
        let result = self.capture(command, &mut bytes)?;
        self.write(&bytes)?;
        Ok(result)
    }
    fn capture(&mut self, command: &mut Command, sink: &mut dyn io::Write) -> io::Result<bool> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        let output = command.output();
        self.active.fetch_sub(1, Ordering::SeqCst);
        let output = output?;
        sink.write_all(&output.stdout)?;
        Ok(output.status.success())
    }
}

#[test]
fn repeated_wide_deep_walks_preserve_ancestry_and_every_entry() {
    // A donated range must neither evaluate its ancestor nor finish the donor
    // before its descendants. Verify both halves of that boundary repeatedly.
    let tree = Tree::new();
    let (outcome, expected) = tree.run(&["-print"], 1);
    assert_eq!(outcome.errors, 0);
    let mut expected = expected.records.lock().unwrap().clone();
    expected.sort();
    for depth in [false, true] {
        for workers in [2, 16, 32] {
            for _ in 0..8 {
                let expression = if depth {
                    vec!["-depth", "-print"]
                } else {
                    vec!["-print"]
                };
                let (outcome, output) = tree.run(&expression, workers);
                assert_eq!(outcome.errors, 0, "{:?}", output.errors);
                let records = output.records.lock().unwrap().clone();
                let positions: HashMap<_, _> = records
                    .iter()
                    .enumerate()
                    .map(|(i, p)| (p.as_slice(), i))
                    .collect();
                for (i, bytes) in records.iter().enumerate() {
                    let parent = Path::new(std::ffi::OsStr::from_bytes(bytes))
                        .parent()
                        .unwrap()
                        .as_os_str()
                        .as_bytes();
                    if let Some(&at) = positions.get(parent) {
                        assert_eq!(
                            at > i,
                            depth,
                            "depth={depth}, workers={workers}, path={}",
                            String::from_utf8_lossy(bytes)
                        );
                    }
                }
                let mut sorted = records;
                sorted.sort();
                assert!(
                    sorted == expected,
                    "depth={depth}, workers={workers}, got={}, expected={}, missing={:?}, extra={:?}",
                    sorted.len(),
                    expected.len(),
                    expected
                        .iter()
                        .filter(|p| !sorted.contains(p))
                        .map(|p| String::from_utf8_lossy(p))
                        .collect::<Vec<_>>(),
                    sorted
                        .iter()
                        .filter(|p| !expected.contains(p))
                        .map(|p| String::from_utf8_lossy(p))
                        .collect::<Vec<_>>()
                );
            }
        }
    }
}

#[test]
fn prune_and_quit_control_all_real_workers() {
    let tree = Tree::new();
    let (outcome, output) = tree.run(&["-name", "child", "-prune", "-o", "-print"], 16);
    assert_eq!(outcome.errors, 0);
    assert!(
        output
            .records
            .lock()
            .unwrap()
            .iter()
            .all(|path| !path.windows(5).any(|part| part == b"child"))
    );
    let (outcome, output) = tree.run(&["-print", "-quit"], 16);
    assert_eq!(outcome.errors, 0);
    assert_eq!(output.records.lock().unwrap().len(), 1);
    // Reach quit after publication, while other branches have outstanding work.
    let (outcome, output) = tree.run(&["-name", "keep", "-print", "-quit"], 16);
    assert_eq!(outcome.errors, 0);
    let records = output.records.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert!(records.iter().all(|path| path.ends_with(b"/keep")));
}

#[test]
fn concurrent_exec_status_is_a_test_and_batches_flush_on_completion_and_quit() {
    let tree = Tree::new();
    let (outcome, output) = tree.run(
        &[
            "-maxdepth",
            "2",
            "-type",
            "f",
            "-exec",
            "sh",
            "-c",
            "sleep 0.01; test \"${1##*/}\" = keep",
            "sh",
            "{}",
            ";",
            "-print",
        ],
        16,
    );
    assert_eq!(outcome.errors, 0);
    assert_eq!(output.records.lock().unwrap().len(), 32);
    assert!(
        output
            .records
            .lock()
            .unwrap()
            .iter()
            .all(|path| path.ends_with(b"/keep"))
    );
    assert!(output.peak.load(Ordering::SeqCst) > 1);
    for quit in [false, true] {
        let mut expression = vec!["-type", "f", "-exec", "printf", "%s\n", "{}", "+"];
        if quit {
            expression.push("-quit");
        }
        let (outcome, output) = tree.run(&expression, 16);
        assert_eq!(outcome.errors, 0);
        let records = output.records.lock().unwrap();
        if quit {
            // Several workers may collect an argument before the first quit;
            // every collected argument must still flush exactly once.
            assert!(!records.is_empty());
            assert!(records.len() <= 16);
            let unique: std::collections::HashSet<_> = records.iter().collect();
            assert_eq!(unique.len(), records.len());
            assert!(
                records
                    .iter()
                    .all(|path| Path::new(std::ffi::OsStr::from_bytes(path)).is_file())
            );
        } else {
            assert_eq!(records.len(), 32 * 6 * 3);
        }
    }
}

#[test]
fn parallel_delete_waits_for_every_descendant() {
    let tree = Tree::new();
    let (outcome, output) = tree.run(&["-delete"], 32);
    assert_eq!(outcome.errors, 0, "{:?}", output.errors);
    assert!(!tree.0.exists());
}
