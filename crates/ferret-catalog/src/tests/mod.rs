//! Tests of the catalog through its public API: batches in, a committed file
//! out, read back through [`Catalog`].

mod carry;
mod commit;
mod decode;
mod epoch;
mod roots;
mod round_trip;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::{Catalog, Hash, InoId, Stat, Transaction};

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("ferret-catalog-{}-{name}", std::process::id()));
        restore(&path);
        let _ = fs::remove_dir_all(&path);
        Self { path }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        restore(&self.path);
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Makes a tree a test may have left read-only removable again. In Rust, not
/// by spawning `chmod`: a child forked while another test thread holds the
/// catalog lock inherits the lock's descriptor until it execs, and a `begin`
/// in that window is refused.
fn restore(path: &Path) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.is_dir() {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
        for entry in fs::read_dir(path).into_iter().flatten().flatten() {
            restore(&entry.path());
        }
    }
}

const SNIFFER: u32 = 1;

fn file_stat(ino: u64) -> Stat {
    Stat {
        dev: 7,
        ino,
        size: 100 + ino,
        mtime_sec: 1_700_000_000,
        mtime_nsec: 5,
        ctime_sec: 1_700_000_000,
        ctime_nsec: 6,
        mode: 0o100_644,
        uid: 1000,
        gid: 100,
        nlink: 1,
    }
}

fn dir_stat(ino: u64) -> Stat {
    Stat {
        mode: 0o040_755,
        size: 4096,
        ..file_stat(ino)
    }
}

fn link_stat(ino: u64) -> Stat {
    Stat {
        mode: 0o120_777,
        ..file_stat(ino)
    }
}

fn hash(n: u8) -> Hash {
    [n; 16]
}

/// Runs one transaction on `dir` and commits it.
fn commit(dir: &Path, fill: impl FnOnce(&mut Transaction)) -> Catalog {
    let mut txn = Transaction::begin(dir, SNIFFER).unwrap();
    fill(&mut txn);
    txn.commit().unwrap()
}

/// Opens the committed catalog in `dir` with every section loaded.
fn reopen(dir: &Path) -> Catalog {
    let catalog = Catalog::open(dir).unwrap().unwrap();
    catalog.load_all().unwrap();
    catalog
}

/// Every name's full path, mapped to the inode it names.
fn paths(catalog: &Catalog) -> BTreeMap<String, InoId> {
    catalog
        .names()
        .map(|(id, _)| {
            let mut path = Vec::new();
            catalog.path(id, &mut path);
            (
                String::from_utf8_lossy(&path).into_owned(),
                catalog.name(id).child,
            )
        })
        .collect()
}

/// The inode a path names.
fn at(catalog: &Catalog, path: &str) -> InoId {
    *paths(catalog)
        .get(path)
        .unwrap_or_else(|| panic!("{path} not catalogued"))
}

fn snapshot(dir: &Path) -> PathBuf {
    Catalog::snapshot_path(dir).unwrap().unwrap()
}
