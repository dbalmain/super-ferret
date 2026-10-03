//! Real walkers with multiple workers; fixture failures should identify setup.

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

#[test]
fn start_donation_uses_existing_effectful_plan_for_every_action_variant() {
    let tree = Tree::new();
    let file = tree.0.join("records");
    let file = file.to_str().unwrap();
    let expressions = [
        (vec!["-print"], false),
        (vec!["-printf", "%p"], false),
        (vec!["-ls"], false),
        (vec!["-print", "-quit"], false),
        (vec!["-delete"], true),
        (vec!["-exec", "true", "{}", ";"], true),
        (vec!["-execdir", "true", "{}", ";"], true),
        (vec!["-ok", "true", "{}", ";"], true),
        (vec!["-okdir", "true", "{}", ";"], true),
        (vec!["-fprint", file], true),
        (vec!["-fprint0", file], true),
        (vec!["-fprintf", file, "%p"], true),
        (vec!["-fls", file], true),
    ];
    for (expression, effectful) in expressions {
        let mut args = vec![
            OsString::from("-I"),
            tree.0.clone().into_os_string(),
            tree.0.join("b0").into_os_string(),
        ];
        args.extend(expression.iter().map(OsString::from));
        let plan = Plan::parse(&args).unwrap();
        assert_eq!(
            super::super::has_actions(&plan.expression),
            effectful,
            "{expression:?}"
        );
        let quit = Arc::new(super::AtomicBool::new(false));
        let mut task = super::Task::new(plan.live_source(), None, &quit);
        assert!(super::super::EntrySource::next(&mut task.walk, true).is_some());
        // Starts can be donated before descending into any sibling range.
        assert_eq!(
            task.donate(&quit, effectful)
                .is_some_and(|donated| donated.completion.is_none()),
            !effectful,
            "{expression:?}"
        );
    }
}

#[test]
fn a_batch_boundary_output_failure_cancels_queued_workers() {
    // Inject a host flush error at a batch directory boundary.
    // Task::step once returned false with quit set, bypassing Pool's latch and
    // allowing another queued task to evaluate after sequential execution
    // stopped.
    #[derive(Clone)]
    struct FaultOutput {
        output: Output,
        flushes: Arc<AtomicUsize>,
    }
    impl Effects for FaultOutput {
        fn print(&mut self, path: &Path, nul: bool) -> io::Result<()> {
            self.output.print(path, nul)
        }
        fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.output.write(bytes)
        }
        fn error(&mut self, error: &WalkError) {
            self.output.error(error);
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.flushes.fetch_add(1, Ordering::SeqCst) == 1 {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            } else {
                Ok(())
            }
        }
    }
    let tree = Tree::new();
    let args: Vec<_> = [
        OsString::from("-I"),
        tree.0.join("b0/keep").into_os_string(),
        tree.0.join("b1/keep").into_os_string(),
    ]
    .into_iter()
    .chain(["-execdir", "true", "{}", "+", "-print"].map(OsString::from))
    .collect();
    let plan = Plan::parse(&args).unwrap();
    let mut sequential = FaultOutput {
        output: Output::default(),
        flushes: Arc::default(),
    };
    let expected = plan.run(&mut plan.live_source(), &mut sequential).unwrap();
    assert_eq!(expected.errors, 1);
    assert_eq!(sequential.output.records.lock().unwrap().len(), 1);

    let mut output = FaultOutput {
        output: Output::default(),
        flushes: Arc::default(),
    };
    let quit = Arc::new(super::AtomicBool::new(false));
    let mut failing = super::Task::new(plan.live_source(), None, &quit);
    assert!(failing.step(&plan, &plan.expression, &mut output, true));
    let later = tree.plan(&["-maxdepth", "0", "-print"]);
    let queued = super::Task::new(later.live_source(), None, &quit);
    let pool = super::Pool {
        queue: Mutex::new(super::Queue {
            tasks: vec![queued, failing],
            active: 0,
        }),
        changed: super::Condvar::new(),
        quit: quit.clone(),
        workers: 2,
    };
    let errors = pool.worker(&plan, &plan.expression, output.clone());
    assert_eq!(errors, expected.errors);
    assert!(
        quit.load(Ordering::Acquire),
        "output failure must cancel the pool"
    );
    assert_eq!(
        *output.output.records.lock().unwrap(),
        *sequential.output.records.lock().unwrap()
    );
}

#[test]
fn execdir_cwd_failure_stops_before_the_next_start_like_gnu() {
    // A child chdir failure was treated like an executable launch failure and
    // allowed a later start to run; GNU stops when its batch cwd is
    // inaccessible.
    use std::os::unix::fs::PermissionsExt;
    let tree = Tree::new();
    let first = tree.0.join("b0/keep");
    let second = tree.0.join("b1/keep");
    let directory = tree.0.join("b0");
    let args: Vec<_> = [
        OsString::from("-I"),
        first.clone().into_os_string(),
        second.into_os_string(),
    ]
    .into_iter()
    .chain([
        "-execdir".into(),
        "echo".into(),
        "{}".into(),
        "+".into(),
        "-exec".into(),
        "sh".into(),
        "-c".into(),
        "test \"$1\" != \"$2\" || chmod 000 \"$3\"".into(),
        "sh".into(),
        "{}".into(),
        first.into_os_string(),
        directory.clone().into_os_string(),
        ";".into(),
        "-print".into(),
    ])
    .collect();
    let expected = super::super::gnu::output(super::super::gnu::command().args(&args[1..]));
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(expected.status.code(), Some(1));
    assert!(!expected.stderr.is_empty());
    let expected: Vec<_> = expected
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    assert_eq!(expected.len(), 1);
    let plan = Plan::parse(&args).unwrap();
    for workers in [1, 8] {
        let output = Output::default();
        let outcome = plan
            .run_parallel(plan.live_source(), output.clone(), workers)
            .unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(outcome.errors > 0, "workers={workers}");
        assert!(!output.errors.lock().unwrap().is_empty());
        assert_eq!(
            *output.records.lock().unwrap(),
            expected,
            "workers={workers}"
        );
    }
}
