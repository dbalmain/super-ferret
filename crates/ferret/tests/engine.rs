//! Resident engine tests use the production crawl, writer, query and reader.
#![allow(clippy::unwrap_used)] // A fixture failure should stop the test.

use std::fs;
use std::io;
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use ferret::engine::{Engine, QuerySession};
use ferret_catalog::Catalog;
use ferret_crawl::{IndexOptions, Refresh, index};
use ferret_query::Query;
use ferret_query::find::{Effects, Plan, WalkError};

#[path = "support/fixture.rs"]
mod fixture;
#[path = "../../ferret-catalog/tests/support/listing.rs"]
mod oracle;

struct Tree(PathBuf);
impl Tree {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "ferret-engine-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(base.join("tree/sub")).unwrap();
        fs::write(base.join("tree/a.txt"), b"alpha").unwrap();
        fs::write(base.join("tree/sub/b.txt"), b"beta").unwrap();
        index(
            &base.join("index"),
            &[base.join("tree")],
            Refresh::All,
            &options(),
        )
        .unwrap();
        Self(base)
    }
    fn index(&self) -> PathBuf {
        self.0.join("index")
    }
    fn root(&self) -> PathBuf {
        self.0.join("tree")
    }
    fn oracle(&self, pin: &QuerySession) {
        let destination = self.0.join("oracle");
        if destination.exists() {
            fs::remove_dir_all(&destination).unwrap();
        }
        index(&destination, &[self.root()], Refresh::All, &options()).unwrap();
        let fresh = Catalog::open(&destination).unwrap().unwrap();
        assert_eq!(oracle::listings(pin.catalog()), oracle::listings(&fresh));
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn options() -> IndexOptions {
    IndexOptions {
        workers: 4,
        ..IndexOptions::default()
    }
}
fn query() -> Query {
    Query::from_args([b"*.txt".as_slice()], SystemTime::now()).unwrap()
}
fn search(pin: &QuerySession) -> Vec<Vec<u8>> {
    let mut rows = Vec::new();
    pin.search(&query(), |row| {
        rows.push(row.path.to_vec());
        ControlFlow::Continue(())
    })
    .unwrap();
    rows.sort();
    rows
}
#[derive(Clone, Default)]
struct Output(Arc<Mutex<Vec<u8>>>);
impl Effects for Output {
    fn print(&mut self, path: &Path, nul: bool) -> io::Result<()> {
        let mut bytes = self.0.lock().unwrap();
        bytes.extend_from_slice(path.as_os_str().as_bytes());
        bytes.push(if nul { 0 } else { b'\n' });
        Ok(())
    }
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(())
    }
    fn error(&mut self, error: &WalkError) {
        panic!("unexpected find error: {error:?}")
    }
}
fn lines(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut rows: Vec<_> = bytes
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    rows.sort();
    rows
}

#[test]
fn search_and_find_match_current_hosts_and_two_queries_share_one_load() {
    let tree = Tree::new();
    let engine = Engine::open(&tree.index()).unwrap().unwrap();
    let pin = engine.pin();
    let loaded = pin.catalog().bytes_read();
    let expected = search(&pin);
    assert_eq!(search(&engine.pin()), expected);
    assert_eq!(engine.pin().catalog().bytes_read(), loaded);
    tree.oracle(&pin);
    let cli = fixture::bounded_command(env!("CARGO_BIN_EXE_ferret"), &tree.0)
        .args(["search", "*.txt"])
        .output()
        .unwrap();
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    assert_eq!(lines(&cli.stdout), expected);
    let args = ["tree".into(), "-type".into(), "f".into(), "-print".into()];
    let plan = Plan::parse_at(&args, &tree.0, SystemTime::now()).unwrap();
    let output = Output::default();
    assert_eq!(pin.find(&plan, output.clone(), 4).unwrap().errors, 0);
    let cli = fixture::bounded_command(env!("CARGO_BIN_EXE_ferret"), &tree.0)
        .args(["find", "tree", "-type", "f", "-print"])
        .output()
        .unwrap();
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    assert_eq!(lines(&cli.stdout), lines(&output.0.lock().unwrap()));
    assert_eq!(engine.pin().catalog().bytes_read(), loaded);
}
