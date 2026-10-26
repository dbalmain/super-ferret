//! A FIFO, or a symlink to one, at a name the catalog reads, locks or
//! publishes through. Opening one blocks until a peer appears, so every case
//! runs in a child process under `timeout --signal=KILL`: a regression fails
//! the parent instead of hanging the suite. The children are separate from
//! the crate's unit tests because a forked child holds copies of every open
//! descriptor, catalog locks included, until it execs.
#![allow(clippy::unwrap_used)] // A fixture failure should stop the test.

use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ferret_catalog::log::{ChangeSet, Error as WriterError, Published, Record, Writer};
use ferret_catalog::{
    Catalog, Content, ContentState, DecodeError, Kind, OpenError, Stat, Transaction,
};

const CHILD: &str = "FERRET_CATALOG_SPECIAL_CHILD";

/// Runs `body` in a child process: this test binary filtered to `name`,
/// killed after 20 s. In the child, runs it directly.
fn bounded(name: &str, body: impl FnOnce()) {
    if std::env::var_os(CHILD).is_some() {
        body();
        return;
    }
    let output = Command::new("timeout")
        .args(["--signal=KILL", "20"])
        .arg(std::env::current_exe().unwrap())
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{name}: {} (killed means an open blocked)\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "ferret-catalog-special-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stat(ino: u64, mode: u32) -> Stat {
    Stat {
        dev: 7,
        ino,
        size: 100,
        mtime_sec: 1_700_000_000,
        mtime_nsec: 5,
        ctime_sec: 1_700_000_000,
        ctime_nsec: 6,
        mode,
        uid: 1000,
        gid: 100,
        nlink: 1,
    }
}

/// A published catalog with one file, and one change-log commit on top, so
/// `current`, a snapshot, a non-empty change log and `lock` all exist.
fn published(name: &str) -> Scratch {
    let scratch = Scratch::new(name);
    let mut txn = Transaction::begin(&scratch.0, 1).unwrap();
    let mut batch = txn.batch();
    let root = batch.root(b"/root", stat(1, 0o040_755));
    batch.file(root, b"file", stat(2, 0o100_644), Content::Hashed([1; 16]));
    txn.add(batch);
    txn.commit().unwrap();
    append(&scratch.0);
    scratch
}

/// Commits one change set through the change-log writer.
fn append(dir: &Path) {
    let published = Published::open(dir).unwrap().unwrap();
    let mut writer = Writer::open(dir).unwrap();
    writer
        .commit(published.generation(), &changes(&published))
        .unwrap();
}

/// Replaces the fixture's file row with an equivalent one: valid on any
/// generation of [`published`].
fn changes(published: &Published) -> ChangeSet {
    ChangeSet {
        counters: published.counters(),
        counts: published.counts(),
        records: vec![
            Record::LifePut {
                id: 1,
                kind: Kind::File,
                flags: 0,
                names: 1,
            },
            Record::NamePut {
                revealed: false,
                id: 0,
                parent: 0,
                child: 1,
                name: b"new".to_vec(),
            },
            Record::InodePut {
                id: 1,
                kind: Kind::File,
                state: ContentState::Hashed,
                doc: Some(0),
                stat: stat(2, 0o100_644),
            },
            Record::DocPut {
                id: 0,
                references: 1,
                hash: [1; 16],
            },
        ],
    }
}

/// The name in `dir` starting `prefix` and a checkpoint number.
fn numbered(dir: &Path, prefix: &str) -> PathBuf {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix(prefix))
                .is_some_and(|n| n.parse::<u64>().is_ok())
        })
        .unwrap_or_else(|| panic!("no {prefix}N in {}", dir.display()))
}

fn mkfifo(path: &Path) {
    let status = Command::new("mkfifo").arg(path).status().unwrap();
    assert!(status.success(), "mkfifo {}", path.display());
}

/// How a special file replaces a name.
#[derive(Clone, Copy, Debug)]
enum Special {
    Fifo,
    SymlinkToFifo,
}

const SPECIALS: [Special; 2] = [Special::Fifo, Special::SymlinkToFifo];

/// Puts `special` at `path`, removing what was there.
fn replace(path: &Path, special: Special) {
    let _ = fs::remove_file(path);
    match special {
        Special::Fifo => mkfifo(path),
        Special::SymlinkToFifo => {
            let target = path.with_extension("fifo-target");
            let _ = fs::remove_file(&target);
            mkfifo(&target);
            symlink(&target, path).unwrap();
        }
    }
}

fn corrupt(result: Result<Option<Catalog>, OpenError>, what: &str) {
    match result {
        Err(OpenError::Decode(DecodeError::Corrupt(found))) if found == what => {}
        other => panic!("expected Corrupt({what}), got {:?}", other.map(|_| ())),
    }
}

#[test]
fn a_special_current_is_a_corrupt_manifest() {
    bounded("a_special_current_is_a_corrupt_manifest", || {
        for special in SPECIALS {
            let s = published(&format!("current-{special:?}"));
            replace(&s.0.join("current"), special);
            assert!(
                matches!(
                    Catalog::published_generation(&s.0),
                    Err(OpenError::Decode(DecodeError::Corrupt("manifest")))
                ),
                "{special:?}"
            );
            corrupt(Catalog::open(&s.0), "manifest");
        }
    });
}

#[test]
fn a_special_legacy_catalog_is_not_a_catalog() {
    bounded("a_special_legacy_catalog_is_not_a_catalog", || {
        for special in SPECIALS {
            let s = Scratch::new(&format!("legacy-{special:?}"));
            replace(&s.0.join("catalog"), special);
            assert!(
                matches!(
                    Catalog::open(&s.0),
                    Err(OpenError::Decode(DecodeError::NotACatalog))
                ),
                "{special:?}"
            );
        }
    });
}

#[test]
fn a_special_snapshot_or_change_log_is_corrupt() {
    bounded("a_special_snapshot_or_change_log_is_corrupt", || {
        for special in SPECIALS {
            for (prefix, what) in [("snapshot.", "snapshot"), ("changes.", "change log")] {
                let s = published(&format!("pair-{prefix}{special:?}"));
                replace(&numbered(&s.0, prefix), special);
                corrupt(Catalog::open(&s.0), what);
            }
        }
    });
}

#[test]
fn a_special_lock_refuses_the_writer_without_waiting() {
    bounded("a_special_lock_refuses_the_writer_without_waiting", || {
        for special in SPECIALS {
            let s = published(&format!("lock-{special:?}"));
            replace(&s.0.join("lock"), special);
            match Writer::open(&s.0) {
                Err(WriterError::Io(e)) => {
                    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{special:?}: {e}");
                }
                Err(other) => panic!("{special:?}: {other}"),
                Ok(_) => panic!("{special:?}: a writer opened"),
            }
            assert!(Transaction::begin(&s.0, 1).is_err(), "{special:?}");
        }
    });
}

/// A temporary name taken by a FIFO after the writer opened (and recovery
/// cleared the names) is replaced, so publication goes ahead.
#[test]
fn special_publication_temporaries_are_replaced() {
    bounded("special_publication_temporaries_are_replaced", || {
        for special in SPECIALS {
            let s = published(&format!("temp-{special:?}"));
            let published = Published::open(&s.0).unwrap().unwrap();
            let mut writer = Writer::open(&s.0).unwrap();
            replace(&s.0.join("current.tmp"), special);
            let next = writer
                .commit(published.generation(), &changes(&published))
                .unwrap();
            drop(writer);
            assert_eq!(Catalog::published_generation(&s.0).unwrap(), Some(next));

            let mut txn = Transaction::begin(&s.0, 1).unwrap();
            for name in ["catalog.tmp", "changes.tmp", "current.tmp"] {
                replace(&s.0.join(name), special);
            }
            let mut batch = txn.batch();
            let root = batch.root(b"/root", stat(1, 0o040_755));
            batch.file(root, b"other", stat(3, 0o100_644), Content::Hashed([2; 16]));
            txn.add(batch);
            let committed = txn.commit().unwrap();
            assert_eq!(
                Catalog::published_generation(&s.0).unwrap(),
                Some(committed.generation())
            );
        }
    });
}

/// The positive control: state files reached through symlinks to regular
/// files open and load as before.
#[test]
fn symlinked_regular_state_files_still_open() {
    bounded("symlinked_regular_state_files_still_open", || {
        let s = published("symlinked");
        let real = s.0.join("real");
        fs::create_dir(&real).unwrap();
        for path in [
            s.0.join("current"),
            numbered(&s.0, "snapshot."),
            numbered(&s.0, "changes."),
            s.0.join("lock"),
        ] {
            let moved = real.join(path.file_name().unwrap());
            fs::rename(&path, &moved).unwrap();
            symlink(&moved, &path).unwrap();
        }
        let catalog = Catalog::open(&s.0).unwrap().unwrap();
        catalog.load_all().unwrap();
        drop(Writer::open(&s.0).unwrap());
    });
}
