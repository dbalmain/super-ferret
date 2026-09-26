#![allow(clippy::unwrap_used)]

mod ancestor;
mod gitfile;
mod golden;
mod parallel;
mod race;

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ferret_policy::{Config, DEFAULT_IGNORE, Decision, PatternError, Reason};

use crate::{Decided, Event, Stat, walk};

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("ferret-crawl-{}-{name}", std::process::id()));
        restore(&path);
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn join(&self, rel: &str) -> PathBuf {
        self.path.join(rel)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        restore(&self.path);
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn restore(path: &Path) {
    // `lstat` succeeds for a mode-000 directory the owner still has to clean
    // up. `exists` does not: it follows and treats "permission denied" as
    // absent.
    if fs::symlink_metadata(path).is_ok() {
        let _ = Command::new("chmod")
            .args(["-R", "u+rwx"])
            .arg(path)
            .status();
    }
}

struct Row {
    decision: Decision,
    stat: Option<OwnedStat>,
}

struct OwnedStat {
    size: u64,
    mtime_sec: i64,
    mtime_nsec: i64,
    ctime_sec: i64,
    ctime_nsec: i64,
    dev: u64,
    ino: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    target: Option<OsString>,
}

impl OwnedStat {
    fn from(stat: Stat<'_>) -> Self {
        Self {
            size: stat.size,
            mtime_sec: stat.mtime_sec,
            mtime_nsec: stat.mtime_nsec,
            ctime_sec: stat.ctime_sec,
            ctime_nsec: stat.ctime_nsec,
            dev: stat.dev,
            ino: stat.ino,
            mode: stat.mode,
            uid: stat.uid,
            gid: stat.gid,
            target: stat.link_target.map(OsStr::to_os_string),
        }
    }
}

struct Walked {
    rows: BTreeMap<PathBuf, Row>,
    io: Vec<(PathBuf, io::ErrorKind)>,
    patterns: Vec<PatternError>,
}

fn walked(root: &Path, global: Option<&str>, config: Config) -> Walked {
    let mut out = Walked {
        rows: BTreeMap::new(),
        io: Vec::new(),
        patterns: Vec::new(),
    };
    walk(root, global, config, |event| match event {
        Event::Decided(Decided {
            path,
            decision,
            stat,
        }) => {
            out.rows.insert(
                path.to_path_buf(),
                Row {
                    decision,
                    stat: stat.map(OwnedStat::from),
                },
            );
        }
        Event::Io { path, error } => out.io.push((path.to_path_buf(), error.kind())),
        Event::Pattern(error) => out.patterns.push(error),
    });
    out
}

fn decision(walked: &Walked, rel: &str) -> Decision {
    walked
        .rows
        .get(Path::new(rel))
        .unwrap_or_else(|| panic!("missing {rel}"))
        .decision
}

fn assert_lstat(root: &Path, rel: &str, stat: &OwnedStat) {
    let meta = fs::symlink_metadata(root.join(rel)).unwrap();
    assert_eq!(stat.size, meta.len(), "{rel} size");
    assert_eq!(stat.mtime_sec, meta.mtime(), "{rel} mtime");
    assert_eq!(stat.mtime_nsec, meta.mtime_nsec(), "{rel} mtime_nsec");
    assert_eq!(stat.ctime_sec, meta.ctime(), "{rel} ctime");
    assert_eq!(stat.ctime_nsec, meta.ctime_nsec(), "{rel} ctime_nsec");
    assert_eq!(stat.dev, meta.dev(), "{rel} dev");
    assert_eq!(stat.ino, meta.ino(), "{rel} ino");
    assert_eq!(stat.mode, meta.mode(), "{rel} mode");
    assert_eq!(stat.uid, meta.uid(), "{rel} uid");
    assert_eq!(stat.gid, meta.gid(), "{rel} gid");
}

fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, bytes).unwrap();
}

fn mkfifo(path: &Path) {
    let status = Command::new("mkfifo").arg(path).status().unwrap();
    assert!(status.success(), "mkfifo {}: {status}", path.display());
}

#[test]
fn gitignore_applies_inside_a_work_tree_and_not_outside_one() {
    let dir = Scratch::new("gitignore");
    write(&dir.join(".gitignore"), "*.log\n");
    write(&dir.join("a.log"), "x");
    write(&dir.join("a.txt"), "y");
    write(&dir.join("repo/.gitignore"), "*.log\n");
    fs::create_dir(dir.join("repo/.git")).unwrap();
    write(&dir.join("repo/a.log"), "x");
    write(&dir.join("repo/a.txt"), "y");
    // A `.git` file starts a work tree, and is not itself matched by `.git/`.
    write(&dir.join("filegit/.git"), "gitdir: /nowhere\n");
    write(&dir.join("filegit/.gitignore"), "*.o\n");
    write(&dir.join("filegit/a.o"), "x");
    write(&dir.join("filegit/a.c"), "y");
    // A dangling symlink `.git` also starts a work tree, and exclude is not
    // read through it.
    fs::create_dir(dir.join("linkgit")).unwrap();
    std::os::unix::fs::symlink("nowhere", dir.join("linkgit/.git")).unwrap();
    write(&dir.join("linkgit/.gitignore"), "*.a\n");
    write(&dir.join("linkgit/a.a"), "x");
    write(&dir.join("linkgit/a.c"), "y");

    let walked = walked(dir.path.as_path(), Some(DEFAULT_IGNORE), Config::default());
    assert!(walked.io.is_empty(), "{:?}", walked.io);
    assert!(walked.patterns.is_empty(), "{:?}", walked.patterns);

    assert_eq!(decision(&walked, "a.log"), Decision::Index);
    assert_eq!(decision(&walked, "a.txt"), Decision::Index);
    assert_eq!(decision(&walked, "repo/a.log"), Decision::Skip);
    assert_eq!(decision(&walked, "repo/a.txt"), Decision::Index);
    assert_eq!(decision(&walked, "repo/.git"), Decision::Skip);
    assert_eq!(decision(&walked, "filegit/a.o"), Decision::Skip);
    assert_eq!(decision(&walked, "filegit/a.c"), Decision::Index);
    assert_eq!(decision(&walked, "filegit/.git"), Decision::Index);
    assert_eq!(decision(&walked, "linkgit/a.a"), Decision::Skip);
    assert_eq!(decision(&walked, "linkgit/a.c"), Decision::Index);
    assert_eq!(
        decision(&walked, "linkgit/.git"),
        Decision::Catalog(Reason::Symlink)
    );
    assert!(
        walked
            .rows
            .get(Path::new("repo/a.log"))
            .unwrap()
            .stat
            .is_none()
    );
}

#[test]
// D13: an excluded directory is traversed, not entered, so a `.ferretignore`
// inside it does not apply. The `!` that re-includes lives at the level above.
fn a_ferretignore_bang_reincludes_under_an_excluded_directory() {
    let dir = Scratch::new("reinclude");
    write(&dir.join(".ferretignore"), "target/\n!/target/doc/**\n");
    write(&dir.join("target/.ferretignore"), "!*\n");
    write(&dir.join("target/README.md"), "no");
    write(&dir.join("target/debug/a.o"), "no");
    write(&dir.join("target/doc/.ferretignore"), "*.html\n");
    write(&dir.join("target/doc/x.html"), "yes");
    write(&dir.join("target/doc/y.txt"), "yes");

    let walked = walked(dir.path.as_path(), Some(DEFAULT_IGNORE), Config::default());
    assert!(walked.io.is_empty(), "{:?}", walked.io);
    assert!(walked.patterns.is_empty(), "{:?}", walked.patterns);

    assert_eq!(decision(&walked, "target"), Decision::Traverse);
    assert!(walked.rows.get(Path::new("target")).unwrap().stat.is_some());
    assert_eq!(decision(&walked, "target/README.md"), Decision::Skip);
    assert_eq!(decision(&walked, "target/.ferretignore"), Decision::Skip);
    assert_eq!(decision(&walked, "target/debug"), Decision::Skip);
    assert!(
        !walked.rows.contains_key(Path::new("target/debug/a.o")),
        "a skipped directory is not listed"
    );
    assert_eq!(decision(&walked, "target/doc"), Decision::Traverse);
    assert_eq!(decision(&walked, "target/doc/x.html"), Decision::Index);
    assert_eq!(decision(&walked, "target/doc/y.txt"), Decision::Index);
}

#[test]
// Git cannot re-include a file below an excluded directory: it never lists the
// directory, so `!out/keep.txt` is dead. The walker must not traverse `out`
// for it. The same two lines in a `.ferretignore` do re-include (D13), and
// that is the one case in which an excluded directory is traversed.
fn a_gitignore_bang_under_an_excluded_directory_is_not_traversed() {
    let dir = Scratch::new("git-reinclude");
    let repo = dir.join("repo");
    gitfile::init(&repo);
    write(&repo.join(".gitignore"), "out/\n!out/keep.txt\n");
    write(&repo.join("out/keep.txt"), "k");
    write(&repo.join("out/drop.txt"), "d");

    let git_walk = walked(&repo, None, Config::default());
    assert!(git_walk.io.is_empty(), "{:?}", git_walk.io);
    assert!(gitfile::ignored(&repo, "out"));
    assert_eq!(decision(&git_walk, "out"), Decision::Skip);
    // Git's own view of what is visible: the untracked files it does not
    // ignore. `keep.txt` is not among them.
    let visible = gitfile::git(&repo, &["ls-files", "--others", "--exclude-standard"]);
    assert!(visible.status.success());
    assert_eq!(String::from_utf8_lossy(&visible.stdout), ".gitignore\n");
    assert!(
        !git_walk.rows.contains_key(Path::new("out/keep.txt")),
        "a skipped directory is not listed"
    );

    let ferret = dir.join("ferret");
    write(&ferret.join(".ferretignore"), "out/\n!out/keep.txt\n");
    write(&ferret.join("out/keep.txt"), "k");
    write(&ferret.join("out/drop.txt"), "d");
    let ferret_walk = walked(&ferret, None, Config::default());
    assert!(ferret_walk.io.is_empty(), "{:?}", ferret_walk.io);
    assert_eq!(decision(&ferret_walk, "out"), Decision::Traverse);
    assert_eq!(decision(&ferret_walk, "out/keep.txt"), Decision::Index);
    assert_eq!(decision(&ferret_walk, "out/drop.txt"), Decision::Skip);
}

#[test]
fn a_symlink_is_catalogued_and_not_followed() {
    let dir = Scratch::new("symlink");
    fs::create_dir(dir.join("real")).unwrap();
    write(&dir.join("real/inside.txt"), "hi");
    std::os::unix::fs::symlink("real", dir.join("dirlink")).unwrap();
    std::os::unix::fs::symlink("real/inside.txt", dir.join("filelink")).unwrap();
    std::os::unix::fs::symlink("missing", dir.join("dangling")).unwrap();
    let raw = OsStr::from_bytes(b"odd-\xff");
    std::os::unix::fs::symlink(raw, dir.join("rawlink")).unwrap();
    std::os::unix::fs::symlink("real", dir.join("result")).unwrap();

    let walked = walked(dir.path.as_path(), Some(DEFAULT_IGNORE), Config::default());
    assert!(walked.io.is_empty(), "{:?}", walked.io);

    assert_eq!(decision(&walked, "real"), Decision::Descend);
    assert_eq!(decision(&walked, "real/inside.txt"), Decision::Index);
    assert_eq!(
        decision(&walked, "dirlink"),
        Decision::Catalog(Reason::Symlink)
    );
    assert_eq!(
        decision(&walked, "filelink"),
        Decision::Catalog(Reason::Symlink)
    );
    assert_eq!(
        decision(&walked, "dangling"),
        Decision::Catalog(Reason::Symlink)
    );
    assert_eq!(
        decision(&walked, "rawlink"),
        Decision::Catalog(Reason::Symlink)
    );
    assert_eq!(decision(&walked, "result"), Decision::Skip);
    assert!(walked.rows.get(Path::new("result")).unwrap().stat.is_none());
    assert!(!walked.rows.contains_key(Path::new("dirlink/inside.txt")));

    let dirlink = walked
        .rows
        .get(Path::new("dirlink"))
        .unwrap()
        .stat
        .as_ref()
        .unwrap();
    assert_eq!(dirlink.target.as_deref(), Some(OsStr::new("real")));
    assert_lstat(dir.path.as_path(), "dirlink", dirlink);
    let real_ino = fs::symlink_metadata(dir.join("real")).unwrap().ino();
    assert_ne!(dirlink.ino, real_ino, "the link's inode, not the target's");

    let filelink = walked
        .rows
        .get(Path::new("filelink"))
        .unwrap()
        .stat
        .as_ref()
        .unwrap();
    assert_eq!(
        filelink.target.as_deref(),
        Some(OsStr::new("real/inside.txt"))
    );
    assert_lstat(dir.path.as_path(), "filelink", filelink);

    let dangling = walked
        .rows
        .get(Path::new("dangling"))
        .unwrap()
        .stat
        .as_ref()
        .unwrap();
    assert_eq!(dangling.target.as_deref(), Some(OsStr::new("missing")));

    let rawlink = walked
        .rows
        .get(Path::new("rawlink"))
        .unwrap()
        .stat
        .as_ref()
        .unwrap();
    assert_eq!(rawlink.target.as_deref(), Some(raw));

    let file = walked
        .rows
        .get(Path::new("real/inside.txt"))
        .unwrap()
        .stat
        .as_ref()
        .unwrap();
    assert!(file.target.is_none());
    assert_lstat(dir.path.as_path(), "real/inside.txt", file);
}

#[test]
fn a_fifo_is_skipped_and_not_opened() {
    let dir = Scratch::new("fifo");
    mkfifo(&dir.join("pipe"));
    write(&dir.join("note.txt"), "a");

    let walked = walked(dir.path.as_path(), None, Config::default());
    assert!(walked.io.is_empty(), "{:?}", walked.io);
    assert_eq!(decision(&walked, "pipe"), Decision::Skip);
    assert!(walked.rows.get(Path::new("pipe")).unwrap().stat.is_none());
    assert_eq!(decision(&walked, "note.txt"), Decision::Index);
}

#[test]
fn an_unreadable_directory_is_reported_and_the_walk_continues() {
    let dir = Scratch::new("unreadable");
    write(&dir.join("visible.txt"), "ok");
    fs::create_dir(dir.join("blocked")).unwrap();
    write(&dir.join("blocked/hidden.txt"), "no");
    fs::set_permissions(dir.join("blocked"), fs::Permissions::from_mode(0o000)).unwrap();

    let walked = walked(dir.path.as_path(), None, Config::default());
    assert_eq!(decision(&walked, "visible.txt"), Decision::Index);
    assert_eq!(decision(&walked, "blocked"), Decision::Descend);
    assert!(
        walked
            .rows
            .get(Path::new("blocked"))
            .unwrap()
            .stat
            .is_some()
    );
    assert!(!walked.rows.contains_key(Path::new("blocked/hidden.txt")));
    assert_eq!(
        walked.io,
        vec![(PathBuf::from("blocked"), io::ErrorKind::PermissionDenied)]
    );
    assert!(walked.patterns.is_empty());
}

#[test]
fn the_size_cap_catalogues_a_file_one_byte_over_and_indexes_the_cap() {
    let dir = Scratch::new("cap");
    write(&dir.join("exact.txt"), "xxxx");
    write(&dir.join("over.txt"), "xxxx!");
    write(&dir.join(".ferretignore"), "*.big\n");
    write(&dir.join("huge.big"), vec![b'x'; 100]);

    let config = Config { size_cap: 4 };
    let walked = walked(dir.path.as_path(), None, config);
    assert!(walked.io.is_empty(), "{:?}", walked.io);
    assert_eq!(decision(&walked, "exact.txt"), Decision::Index);
    assert_eq!(
        decision(&walked, "over.txt"),
        Decision::Catalog(Reason::TooLarge)
    );
    assert_eq!(decision(&walked, "huge.big"), Decision::Skip);

    let exact = walked
        .rows
        .get(Path::new("exact.txt"))
        .unwrap()
        .stat
        .as_ref()
        .unwrap();
    assert_eq!(exact.size, 4);
    assert_lstat(dir.path.as_path(), "exact.txt", exact);
    let over = walked
        .rows
        .get(Path::new("over.txt"))
        .unwrap()
        .stat
        .as_ref()
        .unwrap();
    assert_eq!(over.size, 5);
    assert_lstat(dir.path.as_path(), "over.txt", over);
    assert!(
        walked
            .rows
            .get(Path::new("huge.big"))
            .unwrap()
            .stat
            .is_none()
    );
}

#[test]
fn an_unreadable_ignore_file_is_a_fault_and_does_not_apply() {
    let dir = Scratch::new("bad-ignore");
    write(&dir.join(".ferretignore"), "*.log\n");
    fs::set_permissions(dir.join(".ferretignore"), fs::Permissions::from_mode(0o000)).unwrap();
    write(&dir.join("a.log"), "x");
    write(&dir.join("b.txt"), "y");

    let walked = walked(dir.path.as_path(), None, Config::default());
    assert_eq!(
        walked.io,
        vec![(
            PathBuf::from(".ferretignore"),
            io::ErrorKind::PermissionDenied
        )]
    );
    assert_eq!(decision(&walked, "a.log"), Decision::Index);
    assert_eq!(decision(&walked, "b.txt"), Decision::Index);
}

#[test]
fn a_bad_pattern_is_reported_and_the_rest_of_the_file_applies() {
    let dir = Scratch::new("bad-pattern");
    write(&dir.join(".ferretignore"), "*.log\nbad\\\n");
    write(&dir.join("a.log"), "x");
    write(&dir.join("b.txt"), "y");

    let walked = walked(dir.path.as_path(), None, Config::default());
    assert!(walked.io.is_empty(), "{:?}", walked.io);
    assert_eq!(walked.patterns.len(), 1, "{:?}", walked.patterns);
    assert!(
        matches!(
            &walked.patterns[0],
            PatternError::Line { line: 2, pattern, .. } if pattern == "bad\\"
        ),
        "{:?}",
        walked.patterns
    );
    assert_eq!(decision(&walked, "a.log"), Decision::Skip);
    assert_eq!(decision(&walked, "b.txt"), Decision::Index);
}

#[test]
fn an_unreadable_root_is_a_fault_on_the_empty_path() {
    let dir = Scratch::new("bad-root");
    write(&dir.join("a.txt"), "x");
    fs::set_permissions(&dir.path, fs::Permissions::from_mode(0o000)).unwrap();

    let walked = walked(dir.path.as_path(), None, Config::default());
    assert!(walked.rows.is_empty(), "{:?}", walked.rows.keys());
    assert_eq!(
        walked.io,
        vec![(PathBuf::new(), io::ErrorKind::PermissionDenied)]
    );
}

#[test]
fn a_non_utf8_ignore_file_is_read_lossily_and_valid_lines_apply() {
    let dir = Scratch::new("lossy");
    let mut bytes = b"*.log\n".to_vec();
    bytes.push(0xff);
    bytes.push(b'\n');
    write(&dir.join(".ferretignore"), bytes);
    write(&dir.join("a.log"), "x");
    write(&dir.join("b.txt"), "y");

    let walked = walked(dir.path.as_path(), None, Config::default());
    assert!(walked.io.is_empty(), "{:?}", walked.io);
    assert!(walked.patterns.is_empty(), "{:?}", walked.patterns);
    assert_eq!(decision(&walked, "a.log"), Decision::Skip);
    assert_eq!(decision(&walked, "b.txt"), Decision::Index);
}

#[test]
fn a_special_ignore_file_does_not_stall_the_walk() {
    // The child is this same test. Without `O_NONBLOCK`, opening a FIFO for
    // reading waits for a writer and the child never exits.
    if std::env::var_os("FERRET_FIFO_CHILD").is_some() {
        let root = PathBuf::from(std::env::var_os("FERRET_FIFO_ROOT").unwrap());
        let walked = walked(&root, None, Config::default());
        assert!(walked.io.is_empty(), "{:?}", walked.io);
        assert_eq!(decision(&walked, "note.txt"), Decision::Index);
        assert_eq!(decision(&walked, "repo/note.txt"), Decision::Index);
        assert_eq!(decision(&walked, "repo/kept.log"), Decision::Index);
        // libtest swallows a passing test's stdout, so the parent checks a
        // file.
        fs::write(root.join(".walked"), b"1").unwrap();
        return;
    }

    let dir = Scratch::new("fifo-ignore");
    mkfifo(&dir.join(".ferretignore"));
    mkfifo(&dir.join(".gitignore"));
    fs::create_dir(dir.join("repo")).unwrap();
    fs::create_dir(dir.join("repo/.git")).unwrap();
    fs::create_dir(dir.join("repo/.git/info")).unwrap();
    mkfifo(&dir.join("repo/.gitignore"));
    mkfifo(&dir.join("repo/.git/info/exclude"));
    write(&dir.join("note.txt"), "a");
    write(&dir.join("repo/note.txt"), "b");
    write(&dir.join("repo/kept.log"), "c");

    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("tests::a_special_ignore_file_does_not_stall_the_walk")
        .arg("--exact")
        .arg("--nocapture")
        .env("FERRET_FIFO_CHILD", "1")
        .env("FERRET_FIFO_ROOT", &dir.path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let status = loop {
        if start.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("walk stalled on a special ignore file");
        }
        match child.try_wait().unwrap() {
            Some(status) => break status,
            None => thread::sleep(Duration::from_millis(20)),
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        status.success(),
        "child status {status}: {stderr}\n{stdout}"
    );
    assert_eq!(
        fs::read_to_string(dir.join(".walked")).ok().as_deref(),
        Some("1"),
        "child did not walk\n{stdout}\n{stderr}"
    );
}

#[test]
fn an_ignore_file_of_one_mibibyte_still_applies() {
    let dir = Scratch::new("ignore-bound");
    let mut bytes = b"*.log\n".to_vec();
    bytes.resize(1 << 20, b'#');
    write(&dir.join(".ferretignore"), bytes);
    write(&dir.join("a.log"), "x");
    write(&dir.join("b.txt"), "y");

    let walked = walked(dir.path.as_path(), None, Config::default());
    assert!(walked.io.is_empty(), "{:?}", walked.io);
    assert_eq!(decision(&walked, "a.log"), Decision::Skip);
    assert_eq!(decision(&walked, "b.txt"), Decision::Index);
}

#[test]
fn an_ignore_file_over_one_mibibyte_is_a_fault_and_does_not_apply() {
    let dir = Scratch::new("ignore-over");
    let mut bytes = b"*.log\n".to_vec();
    bytes.resize((1 << 20) + 1, b'#');
    write(&dir.join(".ferretignore"), bytes);
    write(&dir.join("a.log"), "x");
    write(&dir.join("b.txt"), "y");

    let walked = walked(dir.path.as_path(), None, Config::default());
    assert_eq!(
        walked.io,
        vec![(PathBuf::from(".ferretignore"), io::ErrorKind::FileTooLarge)]
    );
    assert_eq!(decision(&walked, "a.log"), Decision::Index);
    assert_eq!(decision(&walked, "b.txt"), Decision::Index);
}

#[test]
fn gitignore_outside_a_work_tree_is_not_read() {
    let dir = Scratch::new("gitignore-unread");
    write(&dir.join(".gitignore"), "*.log\n");
    fs::set_permissions(dir.join(".gitignore"), fs::Permissions::from_mode(0o000)).unwrap();
    write(&dir.join("a.log"), "x");
    fs::create_dir(dir.join("nested")).unwrap();
    write(&dir.join("nested/.gitignore"), "*.txt\n");
    fs::set_permissions(
        dir.join("nested/.gitignore"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    write(&dir.join("nested/a.txt"), "y");

    fs::create_dir_all(dir.join("repo/.git")).unwrap();
    write(&dir.join("repo/.gitignore"), "*.o\n");
    fs::set_permissions(
        dir.join("repo/.gitignore"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    write(&dir.join("repo/a.o"), "z");
    write(&dir.join("repo/a.c"), "z");

    let walked = walked(dir.path.as_path(), None, Config::default());
    assert_eq!(
        walked.io,
        vec![(
            PathBuf::from("repo/.gitignore"),
            io::ErrorKind::PermissionDenied
        )]
    );
    assert_eq!(decision(&walked, "a.log"), Decision::Index);
    assert_eq!(decision(&walked, "nested/a.txt"), Decision::Index);
    assert_eq!(decision(&walked, "repo/a.o"), Decision::Index);
    assert_eq!(decision(&walked, "repo/a.c"), Decision::Index);
}

/// Fires while ignore patterns are reported, which is after the directory is
/// listed and before any child is classified. `bad\` is the pattern error that
/// opens that window.
fn on_pattern(dir: &Scratch, body: impl FnOnce(&Path)) -> Walked {
    write(&dir.join(".ferretignore"), "bad\\\n");
    let root = dir.path.clone();
    let mut out = Walked {
        rows: BTreeMap::new(),
        io: Vec::new(),
        patterns: Vec::new(),
    };
    let mut body = Some(body);
    walk(&root, None, Config::default(), |event| match event {
        Event::Pattern(error) => {
            out.patterns.push(error);
            let body = body.take().expect("one pattern error");
            body(&root);
        }
        Event::Decided(Decided {
            path,
            decision,
            stat,
        }) => {
            out.rows.insert(
                path.to_path_buf(),
                Row {
                    decision,
                    stat: stat.map(OwnedStat::from),
                },
            );
        }
        Event::Io { path, error } => out.io.push((path.to_path_buf(), error.kind())),
    });
    assert!(body.is_none(), "the pattern hook did not run");
    out
}

#[test]
fn a_listed_file_replaced_by_a_symlink_is_catalogued_as_the_link() {
    let dir = Scratch::new("d-type-link");
    write(&dir.join("victim"), "file");
    let walked = on_pattern(&dir, |root| {
        fs::remove_file(root.join("victim")).unwrap();
        std::os::unix::fs::symlink("target", root.join("victim")).unwrap();
    });
    assert!(walked.io.is_empty(), "{:?}", walked.io);
    assert_eq!(
        decision(&walked, "victim"),
        Decision::Catalog(Reason::Symlink)
    );
    let stat = walked
        .rows
        .get(Path::new("victim"))
        .unwrap()
        .stat
        .as_ref()
        .unwrap();
    assert_eq!(stat.target.as_deref(), Some(OsStr::new("target")));
    assert_lstat(dir.path.as_path(), "victim", stat);
}

#[test]
fn a_listed_symlink_replaced_by_a_file_is_indexed_as_the_file() {
    let dir = Scratch::new("d-type-file");
    std::os::unix::fs::symlink("old", dir.join("victim")).unwrap();
    let walked = on_pattern(&dir, |root| {
        fs::remove_file(root.join("victim")).unwrap();
        fs::write(root.join("victim"), "hello").unwrap();
    });
    assert!(walked.io.is_empty(), "{:?}", walked.io);
    assert_eq!(decision(&walked, "victim"), Decision::Index);
    let stat = walked
        .rows
        .get(Path::new("victim"))
        .unwrap()
        .stat
        .as_ref()
        .unwrap();
    assert!(stat.target.is_none());
    assert_eq!(stat.size, 5);
    assert_lstat(dir.path.as_path(), "victim", stat);
}

#[test]
fn a_symlink_removed_before_lstat_is_a_fault_and_not_decided() {
    let dir = Scratch::new("d-type-gone");
    std::os::unix::fs::symlink("old", dir.join("victim")).unwrap();
    let walked = on_pattern(&dir, |root| {
        fs::remove_file(root.join("victim")).unwrap();
    });
    assert!(!walked.rows.contains_key(Path::new("victim")));
    assert_eq!(
        walked.io,
        vec![(PathBuf::from("victim"), io::ErrorKind::NotFound)]
    );
}
