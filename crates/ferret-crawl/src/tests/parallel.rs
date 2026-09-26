//! Exercises the real work queue against the sequential event stream.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use ferret_policy::Config;

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

/// A deep frontier must keep descriptors bounded instead of retaining a stack.
#[test]
fn deep_chain_matches_sequential_with_bounded_fds() {
    let tree = Scratch::new("parallel-deep");
    let mut current = tree.path.clone();
    for _ in 0..180 {
        current.push("d");
        fs::create_dir(&current).unwrap();
        write(&current.join("file"), "text");
    }
    let (events, max_fds) = compare(&tree.path, 8);
    assert_eq!(events.len(), 360);
    assert!(max_fds < 256, "observed {max_fds} open fds");
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
