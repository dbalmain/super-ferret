//! Directory tokens, typed faults, work trees and boundaries, driven through
//! the real walker on temporary trees.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use ferret_policy::{Config, DEFAULT_IGNORE, Decision};
use rustix::fs::{AtFlags, statat};

use super::gitfile::{git_ok, init};
use super::{Scratch, write};
use crate::{
    Boundary, Event, EventVisitor, FaultContext, IoOp, Stat, WalkOptions, WorkTreeKind,
    walk_parallel,
};

/// `(worker, index)`: minted by one worker, meaningful only through that
/// worker's table of directory paths.
type Token = (usize, usize);

type DecidedHook<'h> = &'h (dyn Fn(&Path, Decision) + Sync);

/// Test-side hooks into the walk. Each runs inside the visitor callback named.
#[derive(Default)]
struct Hooks<'h> {
    on_root: Option<&'h (dyn Fn() + Sync)>,
    /// Called with each `Decided` path before the token is returned.
    on_decided: Option<DecidedHook<'h>>,
    on_pattern: Option<&'h (dyn Fn() + Sync)>,
    /// The visitor returns `None` for this directory.
    prune: Option<&'h Path>,
}

struct Recorder<'h> {
    worker: usize,
    hooks: &'h Hooks<'h>,
    root_called: bool,
    /// Index → root-relative path of each directory this worker minted.
    dirs: Vec<PathBuf>,
    decided: Vec<(Token, PathBuf, Decision)>,
    entered: Vec<(Token, Option<OwnedWorkTree>)>,
    boundaries: Vec<(Token, PathBuf, OsString)>,
    io: Vec<(PathBuf, IoOp, Context<Token>, io::ErrorKind)>,
    /// `Decided` events whose `parent_fd` + `name` reached the statted inode.
    fd_checked: usize,
    fd_mismatches: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OwnedWorkTree {
    kind: WorkTreeKind,
    common_dir: PathBuf,
    common_id: (u64, u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Context<T> {
    Root,
    Dir(T),
    Child(T, OsString),
}

impl<'h> Recorder<'h> {
    fn mint(&mut self, path: &Path) -> Token {
        self.dirs.push(path.to_path_buf());
        (self.worker, self.dirs.len() - 1)
    }
}

impl EventVisitor for Recorder<'_> {
    type Dir = Token;

    fn root(&mut self, _stat: Stat<'_>) -> Token {
        self.root_called = true;
        if let Some(hook) = self.hooks.on_root {
            hook();
        }
        self.mint(Path::new(""))
    }

    fn visit(&mut self, event: Event<'_, Token>) -> Option<Token> {
        match event {
            Event::Decided(decided) => {
                if let Some(hook) = self.hooks.on_decided {
                    hook(decided.path, decided.decision);
                }
                if let Some(stat) = decided.stat {
                    match statat(decided.parent_fd, decided.name, AtFlags::SYMLINK_NOFOLLOW) {
                        Ok(seen) if (seen.st_dev, seen.st_ino) == (stat.dev, stat.ino) => {
                            self.fd_checked += 1;
                        }
                        _ => self.fd_mismatches.push(decided.path.to_path_buf()),
                    }
                }
                self.decided
                    .push((decided.parent, decided.path.to_path_buf(), decided.decision));
                let enters = matches!(decided.decision, Decision::Descend | Decision::Traverse);
                if !enters || self.hooks.prune == Some(decided.path) {
                    return None;
                }
                return Some(self.mint(decided.path));
            }
            Event::Entered { dir, work_tree } => {
                let owned = work_tree.map(|tree| OwnedWorkTree {
                    kind: tree.kind,
                    common_dir: tree.common_dir.to_path_buf(),
                    common_id: tree.common_id,
                });
                self.entered.push((dir, owned));
            }
            Event::Boundary { parent, name, path } => {
                self.boundaries
                    .push((parent, path.to_path_buf(), name.to_os_string()));
            }
            Event::Io {
                path,
                op,
                context,
                error,
            } => {
                let context = match context {
                    FaultContext::Root => Context::Root,
                    FaultContext::Dir(dir) => Context::Dir(dir),
                    FaultContext::Child { parent, name } => {
                        Context::Child(parent, name.to_os_string())
                    }
                };
                self.io
                    .push((path.to_path_buf(), op, context, error.kind()));
            }
            Event::Pattern(_) => {
                if let Some(hook) = self.hooks.on_pattern {
                    hook();
                }
            }
        }
        None
    }
}

/// Every visitor's events with tokens resolved to the paths they were minted
/// for. A token resolved through the wrong worker's table, or a parent's token
/// swapped for another, shows up as a wrong path.
#[derive(Default)]
struct Merged {
    root_called: bool,
    /// (parent path, path, decision).
    decided: Vec<(PathBuf, PathBuf, Decision)>,
    entered: Vec<(PathBuf, Option<OwnedWorkTree>)>,
    /// (parent path, path, name).
    boundaries: Vec<(PathBuf, PathBuf, OsString)>,
    io: Vec<(PathBuf, IoOp, Context<PathBuf>, io::ErrorKind)>,
    /// Child events reported on a worker other than the one that minted the
    /// parent's token.
    cross_worker: usize,
    fd_checked: usize,
    fd_mismatches: Vec<PathBuf>,
}

impl Merged {
    fn decision(&self, rel: &str) -> Option<Decision> {
        self.decided
            .iter()
            .find(|(_, path, _)| path == Path::new(rel))
            .map(|(_, _, decision)| *decision)
    }

    fn entered(&self, rel: &str) -> Option<&Option<OwnedWorkTree>> {
        self.entered
            .iter()
            .find(|(path, _)| path == Path::new(rel))
            .map(|(_, tree)| tree)
    }

    fn io_at(&self, rel: &str) -> Vec<(IoOp, Context<PathBuf>, io::ErrorKind)> {
        self.io
            .iter()
            .filter(|(path, ..)| path == Path::new(rel))
            .map(|(_, op, context, kind)| (*op, context.clone(), *kind))
            .collect()
    }
}

fn run(root: &Path, boundaries: Vec<Boundary>, workers: usize, hooks: &Hooks<'_>) -> Merged {
    let next = AtomicUsize::new(0);
    let options = WalkOptions {
        workers,
        boundaries,
    };
    let visitors = walk_parallel(
        root,
        Some(DEFAULT_IGNORE),
        Config::default(),
        &options,
        || Recorder {
            worker: next.fetch_add(1, Ordering::Relaxed),
            hooks,
            root_called: false,
            dirs: Vec::new(),
            decided: Vec::new(),
            entered: Vec::new(),
            boundaries: Vec::new(),
            io: Vec::new(),
            fd_checked: 0,
            fd_mismatches: Vec::new(),
        },
    );
    let mut paths = HashMap::new();
    for visitor in &visitors {
        for (index, path) in visitor.dirs.iter().enumerate() {
            paths.insert((visitor.worker, index), path.clone());
        }
    }
    let resolve = |token: &Token| {
        paths
            .get(token)
            .unwrap_or_else(|| panic!("token {token:?} was never minted"))
            .clone()
    };
    let mut merged = Merged::default();
    for visitor in visitors {
        merged.root_called |= visitor.root_called;
        merged.fd_checked += visitor.fd_checked;
        merged.fd_mismatches.extend(visitor.fd_mismatches);
        for (parent, path, decision) in visitor.decided {
            if parent.0 != visitor.worker {
                merged.cross_worker += 1;
            }
            merged.decided.push((resolve(&parent), path, decision));
        }
        for (dir, tree) in visitor.entered {
            merged.entered.push((resolve(&dir), tree));
        }
        for (parent, path, name) in visitor.boundaries {
            merged.boundaries.push((resolve(&parent), path, name));
        }
        for (path, op, context, kind) in visitor.io {
            let context = match context {
                Context::Root => Context::Root,
                Context::Dir(dir) => Context::Dir(resolve(&dir)),
                Context::Child(parent, name) => Context::Child(resolve(&parent), name),
            };
            merged.io.push((path, op, context, kind));
        }
    }
    merged
}

fn parent_of(path: &Path) -> PathBuf {
    path.parent().map(Path::to_path_buf).unwrap_or_default()
}

fn id(path: &Path) -> (u64, u64) {
    let meta = fs::symlink_metadata(path).unwrap();
    (meta.dev(), meta.ino())
}

fn child(parent: &str, name: &str) -> Context<PathBuf> {
    Context::Child(PathBuf::from(parent), OsString::from(name))
}

// ── tokens ──

/// Each child is reported with the token minted for its own parent, including
/// when the parent's job was taken over by another worker. The hook slows
/// every directory decision so idle workers take shared jobs; the assertion is
/// on tokens, and `cross_worker` only proves the case was exercised.
#[test]
fn every_child_carries_its_parents_token_across_workers() {
    let tree = Scratch::new("tokens-workers");
    for outer in 0..24 {
        for inner in 0..6 {
            let dir = tree.join(&format!("o{outer:02}/i{inner}"));
            for file in 0..4 {
                write(&dir.join(format!("f{file}")), "x");
            }
        }
        write(&tree.join(&format!("o{outer:02}/top")), "x");
    }
    let slow = |_: &Path, decision: Decision| {
        if decision == Decision::Descend {
            std::thread::sleep(Duration::from_micros(300));
        }
    };
    let hooks = Hooks {
        on_decided: Some(&slow),
        ..Hooks::default()
    };
    let merged = run(&tree.path, Vec::new(), 8, &hooks);

    assert!(merged.io.is_empty(), "{:?}", merged.io);
    assert_eq!(merged.decided.len(), 24 * (1 + 6 * 5 + 1));
    for (parent, path, _) in &merged.decided {
        assert_eq!(
            parent,
            &parent_of(path),
            "parent token of {}",
            path.display()
        );
    }
    assert!(
        merged.cross_worker > 0,
        "no child was reported on a worker other than its parent's"
    );

    // Entered once per descended directory, plus the root, each under the
    // token that directory's `Decided` returned.
    let mut entered: Vec<_> = merged
        .entered
        .iter()
        .map(|(path, _)| path.clone())
        .collect();
    entered.sort();
    let mut descended: Vec<_> = merged
        .decided
        .iter()
        .filter(|(_, _, decision)| *decision == Decision::Descend)
        .map(|(_, path, _)| path.clone())
        .collect();
    descended.push(PathBuf::new());
    descended.sort();
    assert_eq!(entered, descended);

    assert!(
        merged.fd_mismatches.is_empty(),
        "{:?}",
        merged.fd_mismatches
    );
    assert_eq!(merged.fd_checked, merged.decided.len());
}

/// A visitor that returns no token for a directory prunes it: not entered,
/// nothing below reported. Its sibling is walked.
#[test]
fn a_directory_without_a_token_is_not_entered() {
    let tree = Scratch::new("tokens-prune");
    write(&tree.join("a/deep/f"), "x");
    write(&tree.join("b/f"), "x");
    let hooks = Hooks {
        prune: Some(Path::new("a")),
        ..Hooks::default()
    };
    let merged = run(&tree.path, Vec::new(), 1, &hooks);

    assert_eq!(merged.decision("a"), Some(Decision::Descend));
    assert!(merged.entered("a").is_none());
    assert_eq!(merged.decision("a/deep"), None);
    assert_eq!(merged.decision("b/f"), Some(Decision::Index));
}

// ── work trees ──

/// Kinds from real git layouts. A linked work tree reports the main
/// repository's common directory, not its own gitdir; a gitdir without
/// `commondir` is a submodule only inside another work tree.
#[test]
fn work_trees_report_kind_and_common_directory() {
    let tree = Scratch::new("worktree-kinds");
    let root = tree.path.clone();
    let repo = root.join("repo");
    init(&repo);
    write(&repo.join("file"), "x");
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-q", "-m", "init"]);
    git_ok(&repo, &["worktree", "add", "-q", "../linked"]);
    // A separate gitdir inside the main work tree: the submodule layout.
    fs::create_dir_all(repo.join(".git/modules")).unwrap();
    git_ok(
        &root,
        &[
            "init",
            "-q",
            "--separate-git-dir",
            "repo/.git/modules/sub",
            "repo/sub",
        ],
    );
    // The same layout outside any work tree is a main work tree.
    fs::create_dir_all(root.join("gitdirs")).unwrap();
    git_ok(
        &root,
        &["init", "-q", "--separate-git-dir", "gitdirs/solo", "solo"],
    );
    write(&root.join("plain/file"), "x");

    let merged = run(&root, Vec::new(), 1, &Hooks::default());
    let absolute = std::path::absolute(&root).unwrap();
    let tree_at = |rel: &str| {
        merged
            .entered(rel)
            .unwrap_or_else(|| panic!("{rel} was not entered"))
            .clone()
    };

    let main = tree_at("repo").expect("repo is a work tree");
    assert_eq!(main.kind, WorkTreeKind::Main);
    assert_eq!(main.common_dir, absolute.join("repo/.git"));
    assert_eq!(main.common_id, id(&repo.join(".git")));

    let linked = tree_at("linked").expect("linked is a work tree");
    assert_eq!(linked.kind, WorkTreeKind::Linked);
    assert_eq!(linked.common_id, main.common_id);
    // Git writes the linked gitdir as a real path; the main tree's is the
    // root as given. They agree when the root has no symlink in it.
    assert_eq!(
        linked.common_dir,
        fs::canonicalize(repo.join(".git")).unwrap()
    );

    let sub = tree_at("repo/sub").expect("repo/sub is a work tree");
    assert_eq!(sub.kind, WorkTreeKind::Submodule);
    assert_eq!(sub.common_id, id(&repo.join(".git/modules/sub")));
    assert_eq!(
        sub.common_dir,
        fs::canonicalize(repo.join(".git/modules/sub")).unwrap()
    );

    let solo = tree_at("solo").expect("solo is a work tree");
    assert_eq!(solo.kind, WorkTreeKind::Main);
    assert_eq!(solo.common_id, id(&root.join("gitdirs/solo")));

    assert_eq!(tree_at(""), None);
    assert_eq!(tree_at("plain"), None);
}

/// A traversed directory probes no `.git`, so it reports no work tree even
/// when it holds one, and it is still entered.
#[test]
fn a_traversed_directory_is_entered_without_a_work_tree() {
    let tree = Scratch::new("worktree-traverse");
    write(&tree.join(".ferretignore"), "target/\n!/target/doc/**\n");
    init(&tree.join("target"));
    write(&tree.join("target/doc/x.txt"), "x");

    let merged = run(&tree.path, Vec::new(), 1, &Hooks::default());
    assert_eq!(merged.decision("target"), Some(Decision::Traverse));
    assert_eq!(merged.entered("target"), Some(&None));
    assert_eq!(merged.decision("target/doc/x.txt"), Some(Decision::Index));
}

// ── faults ──

#[test]
fn a_missing_root_is_an_open_fault_on_the_root_before_any_token() {
    let tree = Scratch::new("fault-root-missing");
    let merged = run(&tree.join("absent"), Vec::new(), 1, &Hooks::default());
    assert!(!merged.root_called);
    assert_eq!(
        merged.io_at(""),
        [(IoOp::OpenDir, Context::Root, io::ErrorKind::NotFound)]
    );
}

/// `/proc/<pid>/fd` opens while the process lives and fails `getdents` with
/// `ENOENT` once it is reaped. Reaping it inside `root()` lands the failure
/// between the root's open and its listing.
#[test]
fn a_root_whose_listing_fails_is_a_list_fault_on_the_root() {
    let sleeper = Command::new("sleep").arg("60").spawn().unwrap();
    let root = PathBuf::from(format!("/proc/{}/fd", sleeper.id()));
    let sleeper = std::sync::Mutex::new(sleeper);
    let reaped = AtomicBool::new(false);
    let on_root = || {
        let mut sleeper = sleeper.lock().unwrap();
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        reaped.store(true, Ordering::SeqCst);
    };
    let hooks = Hooks {
        on_root: Some(&on_root),
        ..Hooks::default()
    };
    let merged = run(&root, Vec::new(), 1, &hooks);
    assert!(reaped.load(Ordering::SeqCst));
    assert!(merged.root_called);
    assert_eq!(
        merged.io_at(""),
        [(IoOp::List, Context::Root, io::ErrorKind::NotFound)]
    );
    assert!(
        merged.entered("").is_none(),
        "an unlisted root is not entered"
    );
}

/// A directory that has a token but cannot be opened: the fault names it as a
/// child of its parent, and it is never entered.
#[test]
fn a_directory_that_cannot_be_opened_is_a_child_fault() {
    let tree = Scratch::new("fault-open");
    write(&tree.join("locked/f"), "x");
    write(&tree.join("gone/f"), "x");
    fs::set_permissions(tree.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
    let root = tree.path.clone();
    let remove = |path: &Path, _: Decision| {
        if path == Path::new("gone") {
            fs::remove_dir_all(root.join("gone")).unwrap();
        }
    };
    let hooks = Hooks {
        on_decided: Some(&remove),
        ..Hooks::default()
    };
    let merged = run(&root, Vec::new(), 1, &hooks);

    assert_eq!(merged.decision("locked"), Some(Decision::Descend));
    assert_eq!(
        merged.io_at("locked"),
        [(
            IoOp::OpenDir,
            child("", "locked"),
            io::ErrorKind::PermissionDenied
        )]
    );
    assert!(merged.entered("locked").is_none());
    assert_eq!(merged.decision("gone"), Some(Decision::Descend));
    assert_eq!(
        merged.io_at("gone"),
        [(IoOp::OpenDir, child("", "gone"), io::ErrorKind::NotFound)]
    );
    assert!(merged.entered("gone").is_none());
}

/// A listed name removed before its `lstat`. The pattern error fires after
/// the root is listed and before any child is considered.
#[test]
fn an_entry_gone_before_lstat_is_a_child_lstat_fault() {
    let tree = Scratch::new("fault-lstat");
    write(&tree.join(".ferretignore"), "bad\\\n");
    write(&tree.join("victim"), "x");
    let root = tree.path.clone();
    let remove = || fs::remove_file(root.join("victim")).unwrap();
    let hooks = Hooks {
        on_pattern: Some(&remove),
        ..Hooks::default()
    };
    let merged = run(&root, Vec::new(), 1, &hooks);
    assert_eq!(
        merged.io_at("victim"),
        [(IoOp::Lstat, child("", "victim"), io::ErrorKind::NotFound)]
    );
    assert_eq!(merged.decision("victim"), None);
}

/// Ignore files and the `.git` probe fault as children of the directory
/// being entered, named by the entry the walk opened in it.
#[test]
fn ignore_and_git_faults_name_the_entry_in_the_entered_directory() {
    let tree = Scratch::new("fault-ignore");
    write(&tree.join("sub/.ferretignore"), "x\n");
    fs::set_permissions(
        tree.join("sub/.ferretignore"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    // A symlinked `.ferretignore` is followed, so its target's mode applies.
    write(&tree.join("linked/rules"), "x\n");
    symlink("rules", tree.join("linked/.ferretignore")).unwrap();
    fs::set_permissions(tree.join("linked/rules"), fs::Permissions::from_mode(0o000)).unwrap();
    init(&tree.join("repo"));
    write(&tree.join("repo/.git/info/exclude"), "x\n");
    fs::set_permissions(
        tree.join("repo/.git/info/exclude"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    fs::create_dir_all(tree.join("shut/.git")).unwrap();
    fs::set_permissions(tree.join("shut/.git"), fs::Permissions::from_mode(0o000)).unwrap();

    let merged = run(&tree.path, Vec::new(), 1, &Hooks::default());
    let denied = io::ErrorKind::PermissionDenied;
    assert_eq!(
        merged.io_at("sub/.ferretignore"),
        [(IoOp::ReadIgnore, child("sub", ".ferretignore"), denied)]
    );
    assert_eq!(
        merged.io_at("linked/.ferretignore"),
        [(IoOp::ReadIgnore, child("linked", ".ferretignore"), denied)]
    );
    assert_eq!(
        merged.io_at("repo/.git/info/exclude"),
        [(IoOp::ReadIgnore, child("repo", ".git"), denied)]
    );
    assert_eq!(
        merged.io_at("shut/.git"),
        [(IoOp::ProbeGit, child("shut", ".git"), denied)]
    );
    // Each of these directories was still entered.
    for rel in ["sub", "linked", "repo", "shut"] {
        assert!(merged.entered(rel).is_some(), "{rel} not entered");
    }
}

/// A continuation spilled past the descriptor budget is reopened from the
/// root. When an ancestor was swapped for a symlink meanwhile, the reopen
/// faults on the spilled directory itself, named by its own token.
#[test]
fn a_failed_reopen_is_a_fault_on_the_spilled_directory() {
    let tree = Scratch::new("fault-reopen");
    let root = tree.join("root");
    let outside = tree.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let mut current = root.clone();
    for _ in 0..140 {
        let parent = current.clone();
        current.push("d");
        fs::create_dir_all(&current).unwrap();
        write(&parent.join("side"), "inside");
    }
    let swapped = AtomicBool::new(false);
    let swap = |path: &Path, decision: Decision| {
        if decision == Decision::Descend && path.components().count() == 140 {
            fs::rename(root.join("d"), root.join("d.was")).unwrap();
            symlink(&outside, root.join("d")).unwrap();
            swapped.store(true, Ordering::SeqCst);
        }
    };
    let hooks = Hooks {
        on_decided: Some(&swap),
        ..Hooks::default()
    };
    let merged = run(&root, Vec::new(), 1, &hooks);
    assert!(swapped.load(Ordering::SeqCst));
    let reopens: Vec<_> = merged
        .io
        .iter()
        .filter(|(_, op, ..)| *op == IoOp::Reopen)
        .collect();
    assert!(!reopens.is_empty(), "{:?}", merged.io);
    for (path, _, context, _) in reopens {
        assert_eq!(context, &Context::Dir(path.clone()));
    }
}

// ── boundaries ──

fn boundary(path: &str, id: Option<(u64, u64)>) -> Boundary {
    Boundary {
        path: PathBuf::from(path),
        id,
    }
}

fn boundary_tree(name: &str) -> Scratch {
    let tree = Scratch::new(name);
    write(&tree.join("a/inner/f"), "x");
    write(&tree.join("a/other/f"), "x");
    tree
}

/// A boundary is reported under its parent's token and nothing at or below
/// it is decided; its sibling is walked.
#[test]
fn a_boundary_by_path_stops_the_walk() {
    let tree = boundary_tree("boundary-path");
    let merged = run(
        &tree.path,
        vec![boundary("a/inner", None)],
        1,
        &Hooks::default(),
    );
    assert_eq!(
        merged.boundaries,
        [(
            PathBuf::from("a"),
            PathBuf::from("a/inner"),
            OsString::from("inner")
        )]
    );
    assert_eq!(merged.decision("a/inner"), None);
    assert_eq!(merged.decision("a/inner/f"), None);
    assert_eq!(merged.decision("a/other/f"), Some(Decision::Index));
}

/// The inner root owns its directory whatever the outer root's rules say
/// about it (D34), so a boundary the outer policy skips is still reported.
#[test]
fn a_boundary_the_outer_policy_skips_is_still_a_boundary() {
    let tree = boundary_tree("boundary-skipped");
    write(&tree.join("a/.ferretignore"), "inner/\n");
    let merged = run(
        &tree.path,
        vec![boundary("a/inner", None)],
        1,
        &Hooks::default(),
    );
    assert_eq!(merged.boundaries.len(), 1);
    assert_eq!(merged.decision("a/inner"), None);
}

/// With an identity, a directory at the path that is some other inode is not
/// the boundary and is walked; the right inode is.
#[test]
fn a_boundary_with_an_identity_matches_only_that_inode() {
    let tree = boundary_tree("boundary-id");
    let wrong = id(&tree.join("a/other"));
    let merged = run(
        &tree.path,
        vec![boundary("a/inner", Some(wrong))],
        1,
        &Hooks::default(),
    );
    assert!(merged.boundaries.is_empty(), "{:?}", merged.boundaries);
    assert_eq!(merged.decision("a/inner/f"), Some(Decision::Index));

    let right = id(&tree.join("a/inner"));
    let merged = run(
        &tree.path,
        vec![boundary("a/inner", Some(right))],
        1,
        &Hooks::default(),
    );
    assert_eq!(merged.boundaries.len(), 1);
    assert_eq!(merged.decision("a/inner/f"), None);
}

const IN_NAMESPACE: &str = "FERRET_CRAWL_TEST_IN_NAMESPACE";

/// A bind mount of the boundary directory elsewhere under the root has the
/// same `(dev, ino)` at a different path, and is walked: boundaries match by
/// path, and the identity only confirms it. Bind mounts need a mount
/// namespace, so the test re-runs itself under `unshare -rm` (an unprivileged
/// user and mount namespace) and does the work there.
#[test]
fn a_same_identity_alias_of_a_boundary_is_walked() {
    let name = "tests::lifecycle::a_same_identity_alias_of_a_boundary_is_walked";
    if std::env::var_os(IN_NAMESPACE).is_none() {
        let output = Command::new("unshare")
            .args(["-rm"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(IN_NAMESPACE, "1")
            .output()
            .expect("unshare (util-linux) runs this test in a mount namespace");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "in-namespace run failed: {}\n{stdout}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );
        return;
    }

    let tree = boundary_tree("boundary-alias");
    fs::create_dir(tree.join("alias")).unwrap();
    let mount = |args: &[&OsStr]| {
        let status = Command::new("mount").args(args).status().unwrap();
        assert!(status.success(), "mount {args:?}: {status}");
    };
    mount(&[
        OsStr::new("--bind"),
        tree.join("a/inner").as_os_str(),
        tree.join("alias").as_os_str(),
    ]);
    let inner = id(&tree.join("a/inner"));
    assert_eq!(id(&tree.join("alias")), inner, "the bind mount is an alias");

    let merged = run(
        &tree.path,
        vec![boundary("a/inner", Some(inner))],
        1,
        &Hooks::default(),
    );
    let status = Command::new("umount")
        .arg(tree.join("alias"))
        .status()
        .unwrap();
    assert!(status.success());

    assert_eq!(merged.boundaries.len(), 1, "{:?}", merged.boundaries);
    assert_eq!(merged.boundaries[0].1, PathBuf::from("a/inner"));
    assert_eq!(merged.decision("alias"), Some(Decision::Descend));
    assert_eq!(merged.decision("alias/f"), Some(Decision::Index));
}
