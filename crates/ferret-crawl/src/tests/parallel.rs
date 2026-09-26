//! Exercises the real work queue against the sequential event stream.

use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;

use ferret_policy::{Config, Decision};

use super::{Scratch, write};
use crate::{Event, EventVisitor, walk, walk_parallel};

#[derive(Default)]
struct Collected {
    events: Vec<String>,
    max_fds: usize,
}

impl EventVisitor for Collected {
    fn visit(&mut self, event: Event<'_>) {
        self.events.push(line(event));
        let count = fs::read_dir("/proc/self/fd").unwrap().count();
        self.max_fds = self.max_fds.max(count);
    }
}

fn line(event: Event<'_>) -> String {
    match event {
        Event::Decided(decided) => format!(
            "{} {:?} {:?}",
            decided.path.display(),
            decided.decision,
            decided.stat
        ),
        Event::Io { path, error } => format!("{} io {:?}", path.display(), error.kind()),
        Event::Pattern(error) => format!("pattern {error:?}"),
    }
}

fn compare(root: &Path, workers: usize) -> (Vec<String>, usize) {
    let mut sequential = Vec::new();
    walk(root, None, Config::default(), |event| {
        sequential.push(line(event))
    });
    sequential.sort_unstable();
    let visitors = walk_parallel(root, None, Config::default(), workers, Collected::default);
    let max_fds = visitors
        .iter()
        .map(|visitor| visitor.max_fds)
        .max()
        .unwrap_or(0);
    let mut parallel: Vec<_> = visitors
        .into_iter()
        .flat_map(|visitor| visitor.events)
        .collect();
    parallel.sort_unstable();
    assert_eq!(parallel, sequential);
    (parallel, max_fds)
}

/// A broad frontier must not drop jobs when workers take and return parent
/// cursors.
#[test]
fn many_siblings_match_sequential() {
    let tree = Scratch::new("parallel-siblings");
    for index in 0..192 {
        let dir = tree.path.join(format!("dir-{index:03}"));
        fs::create_dir(&dir).unwrap();
        write(&dir.join("file.txt"), "text");
    }
    let (events, _) = compare(&tree.path, 8);
    assert_eq!(events.len(), 384);
}

/// A parent, child, git directory and exclude file must fit in four worker
/// descriptor slots while the exclude is read. Run in a child so the low
/// soft limit cannot affect other tests in this process.
#[test]
fn exclude_read_fits_four_worker_descriptors() {
    const ROOT: &str = "FERRET_FD_BOUND_ROOT";
    if let Some(root) = std::env::var_os(ROOT) {
        let baseline = fs::read_dir("/proc/self/fd").unwrap().count() - 1;
        let limit = baseline + 4;
        let status = Command::new("prlimit")
            .args([
                "--pid".to_owned(),
                std::process::id().to_string(),
                format!("--nofile={limit}:{limit}"),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "prlimit failed: {status}");
        let result = super::walked(Path::new(&root), None, Config::default());
        assert!(result.io.is_empty(), "{:?}", result.io);
        assert_eq!(super::decision(&result, "work/secret.txt"), Decision::Skip);
        return;
    }

    let tree = Scratch::new("exclude-fd-bound");
    write(&tree.join("work/.git/info/exclude"), "secret.txt\n");
    write(&tree.join("work/secret.txt"), "x");
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::parallel::exclude_read_fits_four_worker_descriptors",
        ])
        .env(ROOT, &tree.path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The packed listing must preserve arbitrary filename bytes.
#[test]
fn a_non_utf8_name_survives_the_packed_listing() {
    let tree = Scratch::new("parallel-raw-name");
    let name = OsString::from_vec(b"raw-\xff".to_vec());
    write(&tree.path.join(&name), "text");
    let mut seen = false;
    walk(&tree.path, None, Config::default(), |event| {
        if let Event::Decided(decided) = event
            && decided.path.as_os_str().as_bytes() == name.as_bytes()
        {
            seen = true;
        }
    });
    assert!(seen);
}

/// A deep frontier must keep descriptors bounded instead of retaining a stack.
#[test]
fn deep_chain_matches_sequential_with_bounded_fds() {
    let tree = Scratch::new("parallel-deep");
    for index in 0..160 {
        let dir = tree.path.join(format!("sibling-{index:03}"));
        fs::create_dir(&dir).unwrap();
    }
    let mut current = tree.path.clone();
    for _ in 0..180 {
        let parent = current.clone();
        current.push("d");
        fs::create_dir(&current).unwrap();
        write(&parent.join("side"), "text");
        write(&current.join("file"), "text");
    }
    let (single, single_fds) = compare(&tree.path, 1);
    let (events, max_fds) = compare(&tree.path, 8);
    assert_eq!(single, events);
    assert_eq!(events.len(), 700);
    assert!(
        single_fds < 160,
        "observed {single_fds} open fds with one worker"
    );
    assert!(max_fds < 256, "observed {max_fds} open fds");
}

/// A spilled parent reopened after an ancestor swap must fault, not follow it.
#[test]
fn a_spilled_parent_does_not_follow_a_swapped_ancestor() {
    let tree = Scratch::new("parallel-spill-swap");
    let root = tree.join("root");
    let outside = tree.join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    write(&outside.join("leaked"), "outside");
    let mut current = root.clone();
    for _ in 0..140 {
        let parent = current.clone();
        current.push("d");
        fs::create_dir(&current).unwrap();
        write(&parent.join("side"), "inside");
    }

    let mut swapped = false;
    let mut events = Vec::new();
    walk(&root, None, Config::default(), |event| {
        if let Event::Decided(decided) = &event
            && decided.decision == Decision::Descend
            && decided.path.components().count() == 140
        {
            fs::rename(root.join("d"), root.join("d.was")).unwrap();
            symlink(&outside, root.join("d")).unwrap();
            swapped = true;
        }
        events.push(line(event));
    });
    assert!(swapped);
    assert!(events.iter().any(|event| event.contains(" io ")));
    assert!(!events.iter().any(|event| event.contains("leaked")));
}

/// A failed directory must not terminate workers that have sibling jobs.
#[test]
fn one_fault_does_not_stop_other_subtrees() {
    let tree = Scratch::new("parallel-fault");
    let bad = tree.join("bad");
    fs::create_dir(&bad).unwrap();
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o000)).unwrap();
    for index in 0..64 {
        let dir = tree.path.join(format!("good-{index:02}"));
        fs::create_dir(&dir).unwrap();
        write(&dir.join("file"), "text");
    }
    let (events, _) = compare(&tree.path, 8);
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(events.iter().any(|line| line.starts_with("bad io ")));
    assert!(events.iter().any(|line| line.starts_with("good-63/file ")));
}

/// A visitor panic must wake idle workers so scoped-thread joining can unwind.
#[test]
fn a_panicking_visitor_does_not_strand_idle_workers() {
    struct Panic;

    impl EventVisitor for Panic {
        fn visit(&mut self, _: Event<'_>) {
            panic!("visitor panic");
        }
    }

    let tree = Scratch::new("parallel-panic");
    write(&tree.join("file"), "text");
    let result = std::panic::catch_unwind(|| {
        walk_parallel(&tree.path, None, Config::default(), 8, || Panic);
    });
    assert!(result.is_err());
}
