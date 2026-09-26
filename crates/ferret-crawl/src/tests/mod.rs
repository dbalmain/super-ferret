#![allow(clippy::unwrap_used)]

mod golden;

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

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
    let status = Command::new("mkfifo")
        .arg(dir.join("pipe"))
        .status()
        .unwrap();
    assert!(status.success(), "{status}");
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
