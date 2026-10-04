//! `index` end to end, on real temp trees: every test drives the real
//! pipeline and reads the published catalog back.

#[path = "../../../ferret-catalog/tests/support/listing.rs"]
mod checkpoint_oracle;

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferret_catalog::{BeginError, Catalog, ContentState, DocId, InoId, Kind};
use ferret_policy::Config;

use crate::index::{DRAIN_MIN, Deferred, Hasher, PROBES, Probe, content_faults};
use crate::{
    ContentFault, IndexError, IndexOptions, IoOp, Refresh, Report, RootChange, index, index_change,
};

/// A temp directory holding the walked tree (`tree/`) and the catalog
/// (`cat/`), outside the tree. Cleanup is pure Rust: a spawned `chmod` would
/// inherit a catalog lock descriptor between fork and exec, and another test's
/// `begin` would see `Locked`.
pub(super) struct Tmp {
    pub(super) base: PathBuf,
    _fds: std::sync::RwLockReadGuard<'static, ()>,
}

impl Tmp {
    pub(super) fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!("ferret-index-{}-{name}", std::process::id()));
        unlock(&base);
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("tree")).unwrap();
        let fds = super::FDS.read().unwrap_or_else(|e| e.into_inner());
        Self { base, _fds: fds }
    }

    pub(super) fn tree(&self) -> PathBuf {
        self.base.join("tree")
    }

    pub(super) fn at(&self, rel: &str) -> PathBuf {
        self.tree().join(rel)
    }

    pub(super) fn cat(&self) -> PathBuf {
        self.base.join("cat")
    }

    pub(super) fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = self.at(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        unlock(&self.base);
        let _ = fs::remove_dir_all(&self.base);
    }
}

/// Makes every directory and file under `path` owner-accessible again.
fn unlock(path: &Path) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    let mode = if meta.is_dir() { 0o700 } else { 0o600 };
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    if meta.is_dir()
        && let Ok(entries) = fs::read_dir(path)
    {
        for entry in entries.flatten() {
            unlock(&entry.path());
        }
    }
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn options(workers: usize) -> IndexOptions {
    IndexOptions {
        workers,
        ..IndexOptions::default()
    }
}

fn run(tmp: &Tmp, roots: &[PathBuf], refresh: Refresh<'_>, workers: usize) -> Report {
    index(&tmp.cat(), roots, refresh, &options(workers)).unwrap_or_else(|e| panic!("{e}"))
}

/// One published name, by absolute path.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    ino: InoId,
    kind: Kind,
    state: ContentState,
    doc: Option<DocId>,
    hash: Option<[u8; 16]>,
    mtime: (i64, u32),
    target: Option<Vec<u8>>,
}

impl Row {
    /// Without the per-commit `InoId`.
    fn stable(&self) -> Row {
        Row {
            ino: InoId(0),
            ..self.clone()
        }
    }
}

fn listing(catalog: &Catalog) -> BTreeMap<PathBuf, Row> {
    let mut rows = BTreeMap::new();
    let mut path = Vec::new();
    for (id, _) in catalog.names() {
        path.clear();
        catalog.path(id, &mut path);
        let ferret_catalog::Target::Inode(ino) = catalog.name(id).target() else {
            continue;
        };
        let inode = catalog.inode(ino);
        let row = Row {
            ino,
            kind: catalog.kind(ino),
            state: inode.state,
            doc: inode.doc,
            hash: inode.doc.and_then(|d| catalog.doc_hash(d)),
            mtime: (inode.stat.mtime_sec, inode.stat.mtime_nsec),
            target: catalog.link_target(ino).map(<[u8]>::to_vec),
        };
        let key = PathBuf::from(std::ffi::OsStr::from_bytes(&path));
        assert!(rows.insert(key, row).is_none(), "duplicate path");
    }
    rows
}

fn published(tmp: &Tmp) -> (Catalog, BTreeMap<PathBuf, Row>) {
    let catalog = Catalog::open(&tmp.cat()).unwrap().unwrap();
    catalog.load_all().unwrap();
    let rows = listing(&catalog);
    let ignored = catalog
        .names()
        .map(|(id, _)| id)
        .filter(|&id| {
            matches!(
                catalog.name(id).target(),
                ferret_catalog::Target::Ignored(_)
            )
        })
        .count();
    assert_eq!(rows.len() + ignored, catalog.name_count() as usize);
    (catalog, rows)
}

fn blake3_128(bytes: &[u8]) -> [u8; 16] {
    let mut hash = [0; 16];
    hash.copy_from_slice(&blake3::hash(bytes).as_bytes()[..16]);
    hash
}

/// Installs a probe on walks of `root` until dropped.
pub(super) struct Hook {
    root: PathBuf,
}

impl Hook {
    pub(super) fn set(root: &Path, hook: impl Fn(Probe<'_>) + Send + Sync + 'static) -> Self {
        PROBES
            .lock()
            .unwrap()
            .push((root.to_owned(), Arc::new(hook)));
        Hook {
            root: root.to_owned(),
        }
    }
}

impl Drop for Hook {
    fn drop(&mut self) {
        let mut probes = PROBES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        probes.retain(|(root, _)| root != &self.root);
    }
}

fn bump_mtime(path: &Path) {
    let file = File::options().write(true).open(path).unwrap();
    let now = file.metadata().unwrap().modified().unwrap();
    file.set_modified(now + Duration::from_secs(7)).unwrap();
}

#[test]
fn a_crawl_publishes_what_the_tree_holds_and_a_recrawl_reproduces_it() {
    let tmp = Tmp::new("e2e");
    tmp.write("a.txt", b"hello\n");
    tmp.write("same.txt", b"hello\n");
    tmp.write("bin.dat", b"\x7fELF\0\x01");
    tmp.write("big.txt", &[b'x'; 64]);
    tmp.write("sub/deep/c.rs", b"fn main() {}\n");
    fs::create_dir_all(tmp.at("repo/.git/info")).unwrap();
    tmp.write("repo/src.rs", b"// code\n");
    std::os::unix::fs::symlink("a.txt", tmp.at("link")).unwrap();
    let root = tmp.tree();
    let opts = IndexOptions {
        workers: 4,
        config: Config { size_cap: 32 },
        ..IndexOptions::default()
    };

    let report = index(&tmp.cat(), std::slice::from_ref(&root), Refresh::All, &opts).unwrap();
    assert_eq!(report.refreshed, vec![root.clone()]);
    let first = fs::read(Catalog::snapshot_path(&tmp.cat()).unwrap().unwrap()).unwrap();
    let (catalog, rows) = published(&tmp);

    let row = |rel: &str| rows[&tmp.at(rel)].clone();
    assert_eq!(row("a.txt").state, ContentState::Hashed);
    assert_eq!(row("a.txt").hash, Some(blake3_128(b"hello\n")));
    assert_eq!(
        row("same.txt").doc,
        row("a.txt").doc,
        "equal content, one doc"
    );
    assert_ne!(row("same.txt").ino, row("a.txt").ino);
    assert_eq!(row("bin.dat").state, ContentState::Binary);
    assert_eq!(
        row("big.txt").state,
        ContentState::Unindexed,
        "over the cap"
    );
    assert_eq!(
        row("sub/deep/c.rs").hash,
        Some(blake3_128(b"fn main() {}\n"))
    );
    assert_eq!(row("link").kind, Kind::Symlink);
    assert_eq!(row("link").target.as_deref(), Some(&b"a.txt"[..]));
    assert_eq!(row("sub/deep").kind, Kind::Dir);
    let repo = catalog
        .work_tree(row("repo").ino)
        .expect("repo is a work tree");
    assert_eq!(repo.kind, ferret_catalog::WorkTreeKind::Main);
    let git = fs::metadata(tmp.at("repo/.git")).unwrap();
    assert_eq!(repo.common_id, (git.dev(), git.ino()));
    assert_eq!(catalog.roots().count(), 1);
    // Five text files, two with one content: four reads of text, one binary.
    assert_eq!(report.counts.files_read, 5);

    let again = index(&tmp.cat(), &[root], Refresh::All, &opts).unwrap();
    assert_eq!(again.counts.files_read, 0);
    assert_eq!(again.counts.carried, 5);
    let second = fs::read(Catalog::snapshot_path(&tmp.cat()).unwrap().unwrap()).unwrap();
    assert!(
        first[ferret_catalog::Catalog::open(&tmp.cat())
            .unwrap()
            .unwrap()
            .head_len() as usize..]
            == second[ferret_catalog::Catalog::open(&tmp.cat())
                .unwrap()
                .unwrap()
                .head_len() as usize..],
        "an unchanged tree republishes byte for byte"
    );
}

#[test]
fn a_rerun_reads_nothing_unchanged() {
    let tmp = Tmp::new("carry");
    for i in 0..40 {
        tmp.write(
            &format!("d{}/f{i}.txt", i % 5),
            format!("file {i}\n").as_bytes(),
        );
    }
    tmp.write("d0/extra.txt", b"extra\n");
    tmp.write("d1/bin", b"\0\0\0");
    let roots = [tmp.tree()];
    let first = run(&tmp, &roots, Refresh::All, 4);
    assert_eq!(first.counts.files_read, 42);
    let (_, before) = published(&tmp);

    // Every file that is opened is either read or a content fault, so all
    // carried with no read and no fault means none was opened. (Making a
    // file unreadable cannot prove it: chmod moves ctime, which rightly
    // defeats carry.)
    let second = run(&tmp, &roots, Refresh::All, 4);
    assert_eq!(second.counts.indexed, 42);
    assert_eq!(second.counts.carried, 42);
    assert_eq!(second.counts.files_read, 0);
    assert_eq!(second.counts.bytes_read, 0);
    assert_eq!(second.counts.content_faults, 0);
    let (_, after) = published(&tmp);
    assert_eq!(before, after);
}

#[test]
fn a_changed_mtime_alone_defeats_carry() {
    let tmp = Tmp::new("mtime");
    let touched = tmp.write("touched.txt", b"same bytes\n");
    tmp.write("other.txt", b"other\n");
    let roots = [tmp.tree()];
    run(&tmp, &roots, Refresh::All, 2);
    let (_, before) = published(&tmp);

    bump_mtime(&touched);
    let report = run(&tmp, &roots, Refresh::All, 2);
    assert_eq!(report.counts.files_read, 1);
    let (_, after) = published(&tmp);
    assert_ne!(after[&touched].mtime, before[&touched].mtime);
    assert_eq!(
        after[&touched].doc, before[&touched].doc,
        "same content, same doc"
    );
}

#[test]
fn a_file_the_policy_now_indexes_is_read_even_if_carry_says_unindexed() {
    let tmp = Tmp::new("cap");
    let big = tmp.write("big.txt", &[b'y'; 100]);
    let roots = [tmp.tree()];
    let small_cap = IndexOptions {
        workers: 1,
        config: Config { size_cap: 10 },
        ..IndexOptions::default()
    };
    index(&tmp.cat(), &roots, Refresh::All, &small_cap).unwrap();
    assert_eq!(published(&tmp).1[&big].state, ContentState::Unindexed);

    let report = run(&tmp, &roots, Refresh::All, 1);
    assert_eq!(report.counts.files_read, 1);
    assert_eq!(published(&tmp).1[&big].state, ContentState::Hashed);
}

#[test]
fn concurrent_hard_links_are_one_inode_row_and_one_read() {
    let tmp = Tmp::new("links");
    let mut groups = Vec::new();
    for i in 0..60 {
        let first = tmp.write(
            &format!("a{}/n{i}", i % 7),
            format!("linked {i}\n").as_bytes(),
        );
        let mut names = vec![first.clone()];
        for copy in 0..(1 + i % 3) {
            let alias = tmp.at(&format!("b{}/m{i}-{copy}", (i + copy) % 11));
            fs::create_dir_all(alias.parent().unwrap()).unwrap();
            fs::hard_link(&first, &alias).unwrap();
            names.push(alias);
        }
        groups.push(names);
    }
    for i in 0..100 {
        tmp.write(
            &format!("c{}/plain{i}", i % 13),
            format!("plain {i}\n").as_bytes(),
        );
    }
    let report = run(&tmp, &[tmp.tree()], Refresh::All, 8);
    assert_eq!(report.counts.files_read, 60 + 100, "each inode read once");
    assert_eq!(report.counts.cached_inodes, 60, "only linked inodes cached");
    assert_eq!(report.counts.content_faults, 0);
    let (catalog, rows) = published(&tmp);
    for names in &groups {
        let row = &rows[&names[0]];
        assert_eq!(row.state, ContentState::Hashed);
        for alias in &names[1..] {
            assert_eq!(rows[alias].ino, row.ino, "{}", alias.display());
            assert_eq!(rows[alias].doc, row.doc);
        }
    }
    let dirs = catalog.dir_count();
    assert_eq!(catalog.inode_count() - dirs, 160);
}

/// Two names of one inode with an edit between their visits: the inode is a
/// content fault with no `DocId`, whichever name was visited (and so stored)
/// first, and whichever sorts first in the catalog.
#[test]
fn an_edit_between_two_alias_visits_is_a_content_fault_in_either_order() {
    let tmp = Tmp::new("alias-edit");
    let mut pairs = Vec::new();
    for i in 0..24 {
        // Alternate which name is created first, so directory order (hash
        // order on ext4, creation order on tmpfs) visits both ways round.
        let (x, y) = (tmp.at(&format!("p{i}-x")), tmp.at(&format!("p{i}-y")));
        let (made, linked) = if i % 2 == 0 { (&x, &y) } else { (&y, &x) };
        fs::write(made, format!("pair {i}\n")).unwrap();
        fs::hard_link(made, linked).unwrap();
        pairs.push((x, y));
    }
    let tree = tmp.tree();
    let first_read = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
    let _hook = Hook::set(&tree, {
        let first_read = Arc::clone(&first_read);
        let tree = tree.clone();
        move |probe| {
            if let Probe::Read(rel) = probe {
                let path = tree.join(rel);
                first_read.lock().unwrap().push(path.clone());
                let mut file = OpenOptions::new().append(true).open(&path).unwrap();
                file.write_all(b"edited\n").unwrap();
            }
        }
    });
    let report = run(&tmp, std::slice::from_ref(&tree), Refresh::All, 1);
    assert_eq!(
        report.counts.files_read, 24,
        "the second name is never read"
    );
    // Both names of every pair publish unhashed, so both are reported: the
    // alias that saw the edit, and the name read first.
    assert_eq!(report.counts.content_faults, 48);
    let reported: Vec<&PathBuf> = report.content_faults.iter().map(|(p, _)| p).collect();
    let (catalog, rows) = published(&tmp);
    let (mut x_first, mut y_first) = (0, 0);
    for (x, y) in &pairs {
        assert_eq!(rows[x].ino, rows[y].ino);
        assert!(
            reported.contains(&x) && reported.contains(&y),
            "{}",
            x.display()
        );
        assert_eq!(rows[x].state, ContentState::Fault, "{}", x.display());
        assert_eq!(rows[x].doc, None);
        if first_read.lock().unwrap().contains(x) {
            x_first += 1;
        } else {
            y_first += 1;
        }
    }
    assert!(
        x_first > 0 && y_first > 0,
        "both orders: {x_first} x first, {y_first} y first"
    );
    assert_eq!(catalog.doc_count(), 0);
    let faults: Vec<_> = report.content_faults.iter().map(|(_, f)| f).collect();
    assert!(faults.iter().all(|f| matches!(f, ContentFault::Alias)));
}

#[test]
fn a_fault_only_the_build_sees_is_reported_for_every_name() {
    let tmp = Tmp::new("build-fault");
    let a_name = tmp.write("a/f.txt", b"shared\n");
    let b_name = tmp.at("b/g.txt");
    fs::create_dir_all(tmp.at("b")).unwrap();
    fs::hard_link(&a_name, &b_name).unwrap();
    tmp.write("c/other.txt", b"other\n");
    let (a, b, c) = (tmp.at("a"), tmp.at("b"), tmp.at("c"));
    let roots = [a.clone(), b.clone(), c.clone()];
    run(&tmp, &roots, Refresh::All, 1);

    // Roots are walked in path order, so `a`'s name carries the old hash
    // before `b` is entered; the edit there makes `b`'s name read the new
    // content. Each worker saw a clean file: only the build, which keeps one
    // row per inode, sees the two disagree.
    let edited = AtomicBool::new(false);
    let _hook = Hook::set(&b, {
        let b_name = b_name.clone();
        move |probe| {
            if matches!(probe, Probe::Entered) && !edited.swap(true, Ordering::SeqCst) {
                let mut file = OpenOptions::new().append(true).open(&b_name).unwrap();
                file.write_all(b"edited\n").unwrap();
            }
        }
    });
    let report = run(&tmp, &roots, Refresh::All, 1);
    assert_eq!(report.counts.carried, 2, "a's name and c's file");
    assert_eq!(report.counts.files_read, 1, "b's name");
    let (_, rows) = published(&tmp);
    assert_eq!(rows[&a_name].ino, rows[&b_name].ino);
    assert_eq!(rows[&a_name].state, ContentState::Fault);
    assert_eq!(rows[&a_name].doc, None);
    let reported: Vec<&PathBuf> = report.content_faults.iter().map(|(p, _)| p).collect();
    assert_eq!(reported, [&a_name, &b_name]);
    assert!(
        report
            .content_faults
            .iter()
            .all(|(_, f)| matches!(f, ContentFault::Alias))
    );
    assert_eq!(report.counts.content_faults, 2);

    // A kept root's faults are last run's, already reported.
    let report = run(&tmp, &roots, Refresh::Only(std::slice::from_ref(&c)), 1);
    assert_eq!(report.kept, vec![a, b]);
    let (_, rows) = published(&tmp);
    assert_eq!(rows[&a_name].state, ContentState::Fault, "still published");
    assert!(
        report.content_faults.is_empty(),
        "{:?}",
        report.content_faults
    );
    assert_eq!(report.counts.content_faults, 0);
}

#[test]
fn a_file_written_while_it_is_hashed_is_a_content_fault() {
    let tmp = Tmp::new("bracket");
    let moving = tmp.write("moving.txt", b"before\n");
    tmp.write("still.txt", b"still\n");
    let tree = tmp.tree();
    let _hook = Hook::set(&tree, {
        let moving = moving.clone();
        let tree = tree.clone();
        move |probe| {
            if let Probe::Hashed(rel) = probe
                && tree.join(rel) == moving
            {
                let mut file = OpenOptions::new().append(true).open(&moving).unwrap();
                file.write_all(b"during\n").unwrap();
            }
        }
    });
    let report = run(&tmp, std::slice::from_ref(&tree), Refresh::All, 1);
    assert_eq!(report.counts.files_read, 2);
    let faults: Vec<_> = report
        .content_faults
        .iter()
        .map(|(p, f)| (p.clone(), f))
        .collect();
    assert!(
        matches!(faults.as_slice(), [(path, ContentFault::Changed)] if *path == moving),
        "{faults:?}"
    );
    let (_, rows) = published(&tmp);
    assert_eq!(rows[&moving].state, ContentState::Fault);
    assert_eq!(rows[&moving].doc, None);
    assert_eq!(rows[&tmp.at("still.txt")].state, ContentState::Hashed);
}

/// D26: a permanent EACCES publishes the directory with unknown contents;
/// M5 retains old children for both EACCES and other listing faults.
#[test]
fn an_unreadable_directory_and_other_listing_faults_retain_old_children() {
    let tmp = Tmp::new("coverage");
    tmp.write("open/a.txt", b"a\n");
    tmp.write("shut/b.txt", b"b\n");
    let roots = [tmp.tree()];
    run(&tmp, &roots, Refresh::All, 1);
    chmod(&tmp.at("shut"), 0o000);
    tmp.write("open/new.txt", b"new\n");
    let report = index(&tmp.cat(), &roots, Refresh::All, &options(1)).unwrap();
    assert!(report.published.is_some());
    let (_, rows) = published(&tmp);
    assert!(rows.contains_key(&tmp.at("shut")));
    assert!(rows.contains_key(&tmp.at("shut/b.txt")));
    let catalog = Catalog::open(&tmp.cat()).unwrap().unwrap();
    catalog.load_all().unwrap();
    let dir = catalog
        .names()
        .map(|(id, _)| id)
        .find_map(|id| {
            let mut path = Vec::new();
            catalog.path(id, &mut path);
            (path == tmp.at("shut").as_os_str().as_bytes()).then(|| catalog.name(id).child)
        })
        .unwrap();
    assert_eq!(catalog.entry_count(dir), None);
    drop(catalog);
    let before = fs::read(Catalog::snapshot_path(&tmp.cat()).unwrap().unwrap()).unwrap();

    // Same tree: the accessible directory now encounters an actual listing
    // error from the injected getdents seam, rather than a mirrored classifier.
    crate::walk::FAIL_LIST.set(Some(Box::new(|path| {
        (path == Path::new("open")).then(|| std::io::Error::from_raw_os_error(5))
    })));
    let result = index(&tmp.cat(), &roots, Refresh::All, &options(1));
    crate::walk::FAIL_LIST.set(None);
    let report = result.unwrap();
    let faults = &report.coverage_faults;
    assert_eq!(faults.len(), 2);
    let fault = faults.iter().find(|f| f.path == Path::new("open")).unwrap();
    assert_eq!(fault.op, IoOp::List);
    assert_eq!(fault.error.raw_os_error(), Some(5));
    assert!(report.published.is_some());
    assert_eq!(
        fs::read(Catalog::snapshot_path(&tmp.cat()).unwrap().unwrap()).unwrap(),
        before
    );
}

#[test]
fn a_denied_root_is_catalogued_with_unknown_contents() {
    let tmp = Tmp::new("denied-root");
    tmp.write("hidden.txt", b"hidden");
    chmod(&tmp.tree(), 0o000);
    let report = index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options(1)).unwrap();
    assert!(report.published.is_some());
    let catalog = Catalog::open(&tmp.cat()).unwrap().unwrap();
    assert_eq!(catalog.dir_count(), 1);
    assert_eq!(catalog.name_count(), 0);
    catalog.load_all().unwrap();
    assert_eq!(catalog.entry_count(InoId(0)), None);
}

/// A `readlink` failure is a coverage fault (the walker reads through a
/// held descriptor, so it is never a deletion race): retain that old edge.
#[test]
fn a_readlink_failure_retains_the_old_edge_and_publishes_trustworthy_siblings() {
    let tmp = Tmp::new("readlink");
    tmp.write("f.txt", b"f\n");
    std::os::unix::fs::symlink("f.txt", tmp.at("link")).unwrap();
    let roots = [tmp.tree()];
    run(&tmp, &roots, Refresh::All, 1);
    let before = fs::read(Catalog::snapshot_path(&tmp.cat()).unwrap().unwrap()).unwrap();

    tmp.write("new.txt", b"new\n");
    crate::walk::FAIL_READLINK.set(Some(Box::new(|name| name == "link")));
    let result = index(&tmp.cat(), &roots, Refresh::All, &options(1));
    crate::walk::FAIL_READLINK.set(None);
    let report = result.unwrap();
    assert!(report.published.is_some());
    let faults = &report.coverage_faults;
    let (_, rows) = published(&tmp);
    assert!(rows.contains_key(&tmp.at("link")));
    assert!(rows.contains_key(&tmp.at("new.txt")));
    assert!(
        matches!(faults.as_slice(), [f] if f.op == IoOp::Readlink && f.path.ends_with("link")),
        "{faults:?}"
    );
    assert_eq!(
        fs::read(Catalog::snapshot_path(&tmp.cat()).unwrap().unwrap()).unwrap(),
        before
    );
}

#[test]
fn an_unreadable_file_publishes_unhashed() {
    let tmp = Tmp::new("content");
    let shut = tmp.write("shut.txt", b"secret\n");
    tmp.write("open.txt", b"open\n");
    chmod(&shut, 0o000);
    let report = run(&tmp, &[tmp.tree()], Refresh::All, 2);
    assert_eq!(report.counts.content_faults, 1);
    let (path, fault) = &report.content_faults[0];
    assert_eq!(path, &shut);
    assert!(
        matches!(fault, ContentFault::Open(e) if e.kind() == std::io::ErrorKind::PermissionDenied)
    );
    let (_, rows) = published(&tmp);
    assert_eq!(rows[&shut].state, ContentState::Fault);
    assert_eq!(rows[&shut].doc, None);
    assert_eq!(rows[&tmp.at("open.txt")].state, ContentState::Hashed);

    // A fault is never carried: once readable, the next run reads it.
    chmod(&shut, 0o644);
    let again = run(&tmp, &[tmp.tree()], Refresh::All, 2);
    assert_eq!(again.counts.files_read, 1);
    assert_eq!(published(&tmp).1[&shut].hash, Some(blake3_128(b"secret\n")));
}

#[test]
fn a_file_deleted_between_listing_and_lstat_is_a_deletion() {
    let tmp = Tmp::new("vanish");
    let victim = tmp.write("victim.txt", b"doomed\n");
    tmp.write("stays.txt", b"stays\n");
    let _hook = Hook::set(&tmp.tree(), {
        let victim = victim.clone();
        move |probe| {
            // The root is entered, so listed, before any child is statted.
            if matches!(probe, Probe::Entered) {
                let _ = fs::remove_file(&victim);
            }
        }
    });
    let report = run(&tmp, &[tmp.tree()], Refresh::All, 1);
    assert_eq!(report.counts.vanished, 1);
    let (_, rows) = published(&tmp);
    assert!(!rows.contains_key(&victim));
    assert!(rows.contains_key(&tmp.at("stays.txt")));
}

#[test]
fn refreshing_one_root_leaves_another_untouched() {
    let tmp = Tmp::new("retain");
    tmp.write("a/one.txt", b"one\n");
    let b_file = tmp.write("b/two.txt", b"two\n");
    let (a, b) = (tmp.at("a"), tmp.at("b"));
    let roots = [a.clone(), b.clone()];
    run(&tmp, &roots, Refresh::All, 2);
    let (_, before) = published(&tmp);

    fs::write(&b_file, b"two, edited\n").unwrap();
    tmp.write("a/three.txt", b"three\n");
    let report = run(&tmp, &roots, Refresh::Only(std::slice::from_ref(&a)), 2);
    assert_eq!(report.refreshed, vec![a.clone()]);
    assert_eq!(report.kept, vec![b.clone()]);
    let (_, after) = published(&tmp);
    // InoIds renumber every commit (D27); everything else is as it was.
    assert_eq!(
        after[&b_file].stable(),
        before[&b_file].stable(),
        "kept as it was, edit unseen"
    );
    assert!(after.contains_key(&tmp.at("a/three.txt")));
}

/// Paths under `root` in a listing, relative to it.
fn under(rows: &BTreeMap<PathBuf, Row>, root: &Path) -> Vec<PathBuf> {
    rows.keys()
        .filter_map(|p| p.strip_prefix(root).ok())
        .map(Path::to_path_buf)
        .collect()
}

#[test]
fn adding_an_inner_root_refreshes_the_outer_one() {
    let tmp = Tmp::new("add-inner");
    tmp.write("top.txt", b"top\n");
    let inner_file = tmp.write("in/f.txt", b"f\n");
    let (outer, inner) = (tmp.tree(), tmp.at("in"));
    run(&tmp, std::slice::from_ref(&outer), Refresh::All, 2);

    let roots = [outer.clone(), inner.clone()];
    let report = run(&tmp, &roots, Refresh::Only(std::slice::from_ref(&inner)), 2);
    assert_eq!(report.refreshed, vec![outer.clone(), inner.clone()]);
    assert!(report.kept.is_empty());
    assert_eq!(report.counts.boundaries, 1);
    let (catalog, rows) = published(&tmp);
    assert_eq!(catalog.roots().count(), 2);
    // `in` appears once, as the inner root; the outer walk stopped there.
    assert_eq!(
        under(&rows, &outer),
        ["in/f.txt", "top.txt"].map(PathBuf::from).to_vec()
    );
    assert_eq!(rows[&inner_file].state, ContentState::Hashed);
}

#[test]
fn removing_an_inner_root_refreshes_the_outer_one() {
    let tmp = Tmp::new("drop-inner");
    tmp.write("top.txt", b"top\n");
    tmp.write("in/f.txt", b"f\n");
    let (outer, inner) = (tmp.tree(), tmp.at("in"));
    run(&tmp, &[outer.clone(), inner.clone()], Refresh::All, 2);

    let report = run(&tmp, std::slice::from_ref(&outer), Refresh::Only(&[]), 2);
    assert_eq!(report.refreshed, vec![outer.clone()]);
    assert_eq!(report.dropped, vec![inner.clone()]);
    let (catalog, rows) = published(&tmp);
    assert_eq!(catalog.roots().count(), 1);
    assert_eq!(
        under(&rows, &outer),
        ["in", "in/f.txt", "top.txt"].map(PathBuf::from).to_vec()
    );
}

#[test]
// The CLI's add and remove are changes to the roots the previous generation
// holds, read under the lock. A caller passing a full set it read earlier
// would drop a root another run added in between; here the "earlier read"
// is simply not having `a` in the change, and `a` must survive.
fn a_root_change_applies_to_the_previous_generation_s_roots() {
    let tmp = Tmp::new("root-change");
    tmp.write("a/f.txt", b"a\n");
    tmp.write("b/g.txt", b"b\n");
    let (a, b) = (tmp.at("a"), tmp.at("b"));
    run(&tmp, std::slice::from_ref(&a), Refresh::All, 1);

    let change = |add: &[PathBuf], remove: &[PathBuf], refresh| {
        index_change(&tmp.cat(), RootChange { add, remove }, refresh, &options(1))
    };
    let added = std::slice::from_ref(&b);
    let report = change(added, &[], Refresh::Only(added)).unwrap();
    assert_eq!(
        (report.refreshed, report.kept),
        (vec![b.clone()], vec![a.clone()])
    );

    let report = change(&[], &[], Refresh::All).unwrap();
    assert_eq!(report.refreshed, vec![a.clone(), b.clone()], "bare refresh");

    let gone = std::slice::from_ref(&a);
    let report = change(&[], gone, Refresh::Only(&[])).unwrap();
    assert_eq!(
        (report.dropped, report.kept),
        (vec![a.clone()], vec![b.clone()])
    );
    let (catalog, _) = published(&tmp);
    let roots: Vec<&[u8]> = catalog.roots().map(|(_, p)| p).collect();
    assert_eq!(roots, [b.as_os_str().as_bytes()]);

    match change(&[], gone, Refresh::Only(&[])) {
        Err(IndexError::NotConfigured(path)) => assert_eq!(path, a),
        other => panic!("removing an unknown root: {other:?}"),
    }
    let relative = [PathBuf::from("rel")];
    assert!(matches!(
        change(&relative, &[], Refresh::All),
        Err(IndexError::BadRoot(_))
    ));
}

#[test]
fn refreshing_the_outer_root_stops_at_the_inner_one_and_keeps_it() {
    let tmp = Tmp::new("outer-only");
    tmp.write("top.txt", b"top\n");
    let inner_file = tmp.write("in/f.txt", b"f\n");
    let (outer, inner) = (tmp.tree(), tmp.at("in"));
    let roots = [outer.clone(), inner.clone()];
    run(&tmp, &roots, Refresh::All, 2);
    let (_, before) = published(&tmp);

    fs::write(&inner_file, b"f, edited\n").unwrap();
    let report = run(&tmp, &roots, Refresh::Only(std::slice::from_ref(&outer)), 2);
    assert_eq!(report.refreshed, vec![outer.clone()]);
    assert_eq!(report.kept, vec![inner.clone()]);
    assert_eq!(report.counts.boundaries, 1);
    assert_eq!(
        report.counts.files_read, 0,
        "top.txt carried, in/ not walked"
    );
    let (_, after) = published(&tmp);
    assert_eq!(after[&inner_file].stable(), before[&inner_file].stable());
    assert_eq!(
        under(&after, &outer),
        ["in/f.txt", "top.txt"].map(PathBuf::from).to_vec()
    );
}

#[test]
fn a_sniffer_version_change_refreshes_every_root() {
    let tmp = Tmp::new("sniffer");
    tmp.write("a/x.txt", b"x\n");
    tmp.write("b/y.bin", b"\0y");
    let roots = [tmp.at("a"), tmp.at("b")];
    run(&tmp, &roots, Refresh::All, 2);

    let bumped = IndexOptions {
        workers: 2,
        sniffer: ferret_policy::SNIFFER_VERSION + 1,
        ..IndexOptions::default()
    };
    let report = index(&tmp.cat(), &roots, Refresh::Only(&[]), &bumped).unwrap();
    assert_eq!(report.refreshed, roots.to_vec());
    assert!(report.kept.is_empty());
    assert_eq!(
        report.counts.files_read, 2,
        "nothing carried across versions"
    );
    let catalog = Catalog::open(&tmp.cat()).unwrap().unwrap();
    assert_eq!(
        catalog.sniffer_version(),
        ferret_policy::SNIFFER_VERSION + 1
    );
}

#[test]
fn a_second_index_during_a_run_is_locked_out() {
    let tmp = Tmp::new("lock");
    tmp.write("f.txt", b"f\n");
    let cat = tmp.cat();
    let tree = tmp.tree();
    let nested = Arc::new(Mutex::new(None));
    let _hook = Hook::set(&tree, {
        let nested = Arc::clone(&nested);
        let tree = tree.clone();
        move |probe| {
            if matches!(probe, Probe::Read(_)) {
                let result = index(&cat, std::slice::from_ref(&tree), Refresh::All, &options(1));
                *nested.lock().unwrap() = Some(result);
            }
        }
    });
    run(&tmp, std::slice::from_ref(&tree), Refresh::All, 1);
    let nested = nested.lock().unwrap().take().expect("the hook ran");
    assert!(matches!(nested, Err(IndexError::Begin(BeginError::Locked))));
}

#[test]
fn roots_are_checked_and_normalised() {
    let tmp = Tmp::new("roots");
    tmp.write("f.txt", b"f\n");
    let relative = PathBuf::from("tree");
    let dotted = tmp.base.join("tree/../tree");
    for bad in [relative, dotted] {
        let error = index(&tmp.cat(), &[bad], Refresh::All, &options(1)).unwrap_err();
        assert!(matches!(error, IndexError::BadRoot(_)), "{error}");
    }
    let spelled = PathBuf::from(format!("{}/./", tmp.tree().display()));
    let report = run(&tmp, &[spelled], Refresh::All, 1);
    assert_eq!(report.refreshed, vec![tmp.tree()]);
    let stranger = tmp.at("elsewhere");
    let error = index(
        &tmp.cat(),
        &[tmp.tree()],
        Refresh::Only(std::slice::from_ref(&stranger)),
        &options(1),
    )
    .unwrap_err();
    assert!(matches!(error, IndexError::NotConfigured(_)));
}

#[test]
fn a_missing_root_blocks_publication() {
    let tmp = Tmp::new("missing-root");
    let gone = tmp.at("gone");
    let error = index(&tmp.cat(), &[gone], Refresh::All, &options(1)).unwrap_err();
    let IndexError::Coverage { faults, .. } = error else {
        panic!("expected a coverage fault");
    };
    assert!(faults[0].on_root);
    assert!(Catalog::open(&tmp.cat()).unwrap().is_none());
}

#[test]
fn the_cache_hands_out_one_claim_per_inode() {
    use crate::observe::{Cache, Lookup, Observation};
    let cache = Cache::new();
    let key = (3, 77);
    assert!(matches!(cache.claim(key), Lookup::Claimed));
    assert!(matches!(cache.claim(key), Lookup::InFlight));
    assert!(cache.finished(key).is_none());
    let stat = ferret_catalog::Stat {
        dev: 3,
        ino: 77,
        size: 5,
        ..Default::default()
    };
    cache.complete(
        key,
        Observation {
            stat,
            content: Some(ferret_catalog::Content::Binary),
        },
    );
    assert!(matches!(cache.claim(key), Lookup::Done(o) if o.stat == stat));
    assert_eq!(cache.len(), 1);
}

/// An alias that meets its inode in flight on another worker is recorded
/// after the walk from the finished observation, without a second read and
/// without the reader waiting on anyone.
#[test]
fn an_alias_that_meets_its_inode_in_flight_is_recorded_after_the_walk() {
    let tmp = Tmp::new("deferred");
    let mut pairs = Vec::new();
    for i in 0..8 {
        let x = tmp.write(&format!("x{i}/f"), format!("in flight {i}\n").as_bytes());
        let y = tmp.at(&format!("y{i}/g"));
        fs::create_dir_all(y.parent().unwrap()).unwrap();
        fs::hard_link(&x, &y).unwrap();
        pairs.push((x, y));
    }
    for i in 0..8 {
        tmp.write(&format!("pad{i}/p"), b"padding\n");
    }
    let tree = tmp.tree();
    // The claimant holds its claim until some alias has been deferred (or a
    // second passes), so the other workers reach the alias meanwhile.
    let deferred = Arc::new(Mutex::new(0u32));
    let _hook = Hook::set(&tree, {
        let deferred = Arc::clone(&deferred);
        move |probe| match probe {
            Probe::Deferred => *deferred.lock().unwrap() += 1,
            Probe::Claimed => {
                let until = Instant::now() + Duration::from_secs(1);
                while *deferred.lock().unwrap() == 0 && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            _ => {}
        }
    });
    let report = run(&tmp, std::slice::from_ref(&tree), Refresh::All, 8);
    assert!(
        report.counts.deferred > 0,
        "no alias met its inode in flight"
    );
    assert_eq!(report.counts.files_read, 8 + 8);
    assert_eq!(report.counts.content_faults, 0);
    let (_, rows) = published(&tmp);
    for (x, y) in &pairs {
        assert_eq!(rows[y].ino, rows[x].ino);
        assert_eq!(rows[y].state, ContentState::Hashed);
        assert_eq!(rows[y].doc, rows[x].doc);
    }
}

/// The deferred backlog holds only aliases of inodes still in flight. It
/// once kept every deferred alias, with its full path, until all roots were
/// walked, so slow reads of many linked inodes grew it without bound.
/// Driven on one worker's output in a schedule the walk cannot force: 50
/// inodes in turn, each met by 4 aliases while in flight, then finished.
#[test]
fn the_deferred_backlog_is_drained_as_inodes_finish() {
    use crate::observe::{Cache, Lookup, Observation};
    use std::ffi::OsStr;

    let tmp = Tmp::new("drain");
    let txn =
        ferret_catalog::Transaction::begin(&tmp.cat(), ferret_policy::SNIFFER_VERSION).unwrap();
    let cache = Cache::new();
    let root = tmp.tree();
    let mut hasher = Hasher::new(&txn, &cache, &root);
    let parent = hasher
        .out
        .batch
        .root(b"/r", ferret_catalog::Stat::default());
    let mut peak = 0;
    for ino in 0..50 {
        let stat = ferret_catalog::Stat {
            dev: 1,
            ino,
            size: 5,
            ..Default::default()
        };
        assert!(matches!(cache.claim((1, ino)), Lookup::Claimed));
        for alias in 0..4 {
            let name = format!("{ino}-{alias}").into_bytes();
            let path = root.join(OsStr::from_bytes(&name));
            let deferred = Deferred {
                parent,
                name,
                stat,
                path,
            };
            hasher.out.defer(deferred, &cache);
            peak = peak.max(hasher.out.deferred.len());
        }
        let content = Some(ferret_catalog::Content::Binary);
        cache.complete((1, ino), Observation { stat, content });
    }
    assert!(peak <= DRAIN_MIN, "backlog peaked at {peak}");
    hasher.out.resolve(&cache);
    assert!(hasher.out.deferred.is_empty());
    assert_eq!(
        (hasher.out.counts.deferred, hasher.out.counts.aliased),
        (200, 200)
    );
    assert!(hasher.out.content_faults.is_empty());
}

/// Each root's deferred aliases are recorded when that root's walk ends. They
/// were held until every root had been walked, so a run over many roots, each
/// with a slow inode that finished after its last deferral, carried every
/// root's backlog to the end. The peak backlog is now at most one root's.
#[test]
fn the_deferred_backlog_does_not_carry_across_roots() {
    let tmp = Tmp::new("deferred-roots");
    let mut roots = Vec::new();
    let mut counters = Vec::new();
    let mut hooks = Vec::new();
    for r in 0..4 {
        for i in 0..8 {
            let x = tmp.write(
                &format!("r{r}/x{i}/f"),
                format!("root {r} file {i}\n").as_bytes(),
            );
            let y = tmp.at(&format!("r{r}/y{i}/g"));
            fs::create_dir_all(y.parent().unwrap()).unwrap();
            fs::hard_link(&x, &y).unwrap();
            tmp.write(&format!("r{r}/pad{i}/p"), b"padding\n");
        }
        let root = tmp.at(&format!("r{r}"));
        // The claimant holds its read until an alias has been deferred on
        // this root (or a second passes), so the inode finishes after it.
        let deferred = Arc::new(Mutex::new(0u64));
        hooks.push(Hook::set(&root, {
            let deferred = Arc::clone(&deferred);
            move |probe| match probe {
                Probe::Deferred => *deferred.lock().unwrap() += 1,
                Probe::Claimed => {
                    let until = Instant::now() + Duration::from_secs(1);
                    while *deferred.lock().unwrap() == 0 && Instant::now() < until {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
                _ => {}
            }
        }));
        roots.push(root);
        counters.push(deferred);
    }
    let report = run(&tmp, &roots, Refresh::All, 8);
    let per_root: Vec<u64> = counters.iter().map(|c| *c.lock().unwrap()).collect();
    assert!(
        per_root.iter().filter(|&&n| n > 0).count() >= 2,
        "too few roots deferred anything: {per_root:?}"
    );
    assert_eq!(report.counts.deferred, per_root.iter().sum::<u64>());
    assert!(
        report.counts.deferred_peak <= *per_root.iter().max().unwrap(),
        "peak {} across roots that deferred {per_root:?}",
        report.counts.deferred_peak
    );
    assert_eq!(report.counts.content_faults, 0);
    drop(hooks);
}

/// Times the post-commit content-fault pass on a large catalog, such as the
/// 10M one from `ferret-catalog`'s `synthetic` example with `faults=N`. Run
/// by hand, in release:
///
/// ```text
/// FERRET_FAULT_BENCH=<dir> cargo test --release -p ferret-crawl -- --ignored --nocapture fault_pass
/// ```
///
/// Every section is loaded first, as it is on the catalog `commit` returns,
/// so the time is the pass's alone.
#[test]
#[ignore = "a measurement: needs FERRET_FAULT_BENCH naming a catalog directory"]
fn fault_pass_timing() {
    let dir = PathBuf::from(std::env::var_os("FERRET_FAULT_BENCH").unwrap());
    let catalog = Catalog::open(&dir).unwrap().unwrap();
    catalog.load_all().unwrap();
    let roots: Vec<PathBuf> = catalog
        .roots()
        .map(|(_, path)| PathBuf::from(std::ffi::OsStr::from_bytes(path)))
        .collect();
    let faults = (catalog.dir_count()..catalog.inode_count())
        .filter(|&i| catalog.inode(InoId(i)).state == ContentState::Fault)
        .count();
    for _ in 0..3 {
        let started = Instant::now();
        let listed = content_faults(&catalog, &roots, Vec::new());
        println!(
            "{} names, {} inodes, {faults} Fault inodes: {} paths listed in {:?}",
            catalog.name_count(),
            catalog.inode_count(),
            listed.len(),
            started.elapsed()
        );
    }
}

// ── link counts and raw entry counts (D47) ──

/// A file's link count is read from the walk's `lstat`, so it counts names
/// outside the root as well as inside it; a directory's is its own.
#[test]
fn link_counts_come_from_lstat_including_names_outside_the_root() {
    let tmp = Tmp::new("nlink");
    let inside = tmp.write("root/inside.txt", b"x\n");
    fs::hard_link(&inside, tmp.at("root/second.txt")).unwrap();
    fs::hard_link(&inside, tmp.at("outside.txt")).unwrap();
    tmp.write("root/solo.txt", b"y\n");
    fs::create_dir_all(tmp.at("root/sub/deeper")).unwrap();
    fs::create_dir_all(tmp.at("root/sub/other")).unwrap();
    let root = tmp.at("root");
    run(&tmp, std::slice::from_ref(&root), Refresh::All, 2);

    let (catalog, rows) = published(&tmp);
    let nlink = |path: &Path| catalog.inode(rows[path].ino).stat.nlink;
    assert_eq!(nlink(&inside), 3, "two names inside, one outside");
    assert_eq!(nlink(&tmp.at("root/second.txt")), 3);
    assert_eq!(nlink(&tmp.at("root/solo.txt")), 1);
    for dir in ["root/sub", "root/sub/deeper"] {
        let real = fs::metadata(tmp.at(dir)).unwrap().nlink();
        assert_eq!(nlink(&tmp.at(dir)), real, "{dir}");
    }
    assert!(
        nlink(&tmp.at("root/sub")) >= 2,
        "a directory has . and its name"
    );
}

/// The count is taken before ignore rules, so a directory whose children are
/// all ignored is not empty, and a directory nothing listed reads unknown.
#[test]
fn entry_counts_survive_the_pipeline_and_count_ignored_children() {
    let tmp = Tmp::new("entries");
    tmp.write(".ferretignore", b"*.log\n");
    tmp.write("mixed/keep.txt", b"k\n");
    tmp.write("mixed/drop.log", b"d\n");
    tmp.write("allignored/a.log", b"a\n");
    tmp.write("allignored/b.log", b"b\n");
    fs::create_dir(tmp.at("empty")).unwrap();
    run(&tmp, &[tmp.tree()], Refresh::All, 4);

    let (catalog, rows) = published(&tmp);
    assert!(!rows.contains_key(&tmp.at("allignored/a.log")), "ignored");
    let count = |rel: &str| catalog.entry_count(rows[&tmp.at(rel)].ino);
    assert_eq!(count("mixed"), Some(2));
    assert_eq!(count("allignored"), Some(2));
    assert_eq!(count("empty"), Some(0));
    assert_eq!(
        catalog.entry_count(catalog.roots().next().unwrap().0),
        Some(4),
        ".ferretignore and the three directories"
    );
}

/// A root copied forward by `keep` keeps the counts and link counts it had.
/// (A directory the walk cannot list blocks publication, so unknown counts
/// are covered in the catalog's own tests.)
#[test]
fn a_kept_root_keeps_its_entry_and_link_counts() {
    let tmp = Tmp::new("entries-keep");
    let (outer, inner) = (tmp.at("outer"), tmp.at("inner"));
    tmp.write("outer/top.txt", b"t\n");
    tmp.write("inner/f.txt", b"f\n");
    tmp.write("inner/g.log", b"g\n");
    let f = tmp.at("inner/f.txt");
    fs::hard_link(&f, tmp.at("inner/f2.txt")).unwrap();
    fs::create_dir(tmp.at("inner/nothing")).unwrap();
    let roots = [outer.clone(), inner.clone()];
    run(&tmp, &roots, Refresh::All, 2);
    let (before, rows) = published(&tmp);
    let root_of = |catalog: &Catalog, path: &Path| {
        catalog
            .roots()
            .find(|&(_, p)| p == path.as_os_str().as_bytes())
            .map(|(id, _)| id)
            .unwrap_or_else(|| panic!("no root {}", path.display()))
    };
    assert_eq!(before.entry_count(root_of(&before, &inner)), Some(4));
    assert_eq!(before.inode(rows[&f].ino).stat.nlink, 2);

    fs::write(tmp.at("outer/new.txt"), b"n\n").unwrap();
    run(&tmp, &roots, Refresh::Only(std::slice::from_ref(&outer)), 2);
    let (after, rows) = published(&tmp);
    let count = |rel: &Path| after.entry_count(rows[rel].ino);
    assert_eq!(
        after.entry_count(root_of(&after, &inner)),
        Some(4),
        "kept root"
    );
    assert_eq!(
        count(&tmp.at("inner/nothing")),
        Some(0),
        "kept subdirectory"
    );
    assert_eq!(
        after.entry_count(root_of(&after, &outer)),
        Some(2),
        "refreshed root"
    );
    assert_eq!(after.inode(rows[&f].ino).stat.nlink, 2, "kept file");
}

/// 4a: ignored rows must never acquire stat/content, and unsuccessful anchored
/// traversal must collapse to one marker rather than leak its ignored children.
#[test]
fn ignored_names_opaque_directories_and_special_stats_round_trip() {
    use ferret_catalog::{Contents, Target};
    use std::os::unix::net::UnixListener;

    for workers in [1, 4] {
        let tmp = Tmp::new(&format!("ignored-specials-{workers}"));
        tmp.write(
            ".ferretignore",
            b"*.ignored\ndrop/\nprobe/\n!/probe/missing.txt\nkeep/\n!/keep/deep/ok.txt\n",
        );
        tmp.write("regular.ignored", b"never read");
        tmp.write("drop/hidden.txt", b"never read");
        tmp.write("probe/hidden.ignored", b"never read");
        tmp.write("keep/deep/ok.txt", b"visible");
        tmp.write("keep/deep/no.ignored", b"never read");
        tmp.write("only/child.ignored", b"never read");
        fs::create_dir(tmp.at("dir.ignored")).unwrap();
        std::os::unix::fs::symlink("missing", tmp.at("link.ignored")).unwrap();
        super::mkfifo(&tmp.at("pipe.ignored"));
        super::mkfifo(&tmp.at("pipe"));
        let _ignored_socket = UnixListener::bind(tmp.at("socket.ignored")).unwrap();
        let _socket = UnixListener::bind(tmp.at("socket")).unwrap();
        tmp.write("unreadable/secret", b"never read");
        chmod(&tmp.at("unreadable"), 0o000);
        let report = run(&tmp, &[tmp.tree()], Refresh::All, workers);
        assert_eq!(
            report.counts.files_read, 2,
            "only rules and re-included file read"
        );
        assert_eq!(report.counts.specials, 2);
        let catalog = Catalog::open(&tmp.cat()).unwrap().unwrap();
        catalog.load_all().unwrap();
        let root = catalog.roots().next().unwrap().0;
        let entries: BTreeMap<_, _> = catalog
            .entries(root)
            .map(|e| (e.bytes.to_vec(), e))
            .collect();
        for (name, kind) in [
            ("regular.ignored", Kind::File),
            ("dir.ignored", Kind::Dir),
            ("link.ignored", Kind::Symlink),
            ("pipe.ignored", Kind::Fifo),
            ("socket.ignored", Kind::Socket),
            ("drop", Kind::Dir),
            ("probe", Kind::Dir),
        ] {
            let entry = entries[name.as_bytes()];
            assert_eq!(entry.target, Target::Ignored(kind));
            assert_eq!(entry.kind, kind);
        }
        assert_eq!(
            catalog.contents(entries[b"drop".as_slice()].target),
            Some(Contents::Ignored)
        );
        let path = tmp.at("drop/hidden.txt");
        let resolved = catalog.resolve(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(resolved.target, Target::Ignored(Kind::Dir));
        assert_eq!(resolved.remainder, b"hidden.txt");
        let mut paths = Vec::new();
        for (id, _) in catalog.names() {
            let mut path = Vec::new();
            catalog.path(id, &mut path);
            paths.push(path);
        }
        for absent in [
            "drop/hidden.txt",
            "probe/hidden.ignored",
            "unreadable/secret",
        ] {
            assert!(!paths.contains(&tmp.at(absent).as_os_str().as_bytes().to_vec()));
        }
        for dir in ["keep", "keep/deep"] {
            let path = tmp.at(dir);
            let resolved = catalog.resolve(path.as_os_str().as_bytes()).unwrap();
            assert!(matches!(resolved.target, Target::Inode(_)));
            assert_eq!(
                catalog.contents(resolved.target),
                Some(Contents::Catalogued)
            );
        }
        let Target::Inode(only) = entries[b"only".as_slice()].target else {
            panic!("visible dir");
        };
        assert_eq!(catalog.has_children(only), Some(true));
        let child = catalog.entries(only).collect::<Vec<_>>();
        assert_eq!(child.len(), 1);
        assert_eq!(child[0].target, Target::Ignored(Kind::File));
        let Target::Inode(unreadable) = entries[b"unreadable".as_slice()].target else {
            panic!("visible denied dir");
        };
        assert_eq!(
            catalog.contents(Target::Inode(unreadable)),
            Some(Contents::Unreadable)
        );
        assert_eq!(catalog.has_children(unreadable), None);
        for (name, kind) in [("pipe", Kind::Fifo), ("socket", Kind::Socket)] {
            let entry = entries[name.as_bytes()];
            assert_eq!(entry.kind, kind);
            let Target::Inode(inode) = entry.target else {
                panic!("special stat row");
            };
            assert_eq!(Kind::from_mode(catalog.inode(inode).stat.mode), kind);
            assert_eq!(catalog.state(inode), ContentState::Unindexed);
            assert_eq!(catalog.doc(inode), None);
        }
        // Root keep must copy ignored rows without indexing their reserved
        // child ids.
        let head = catalog.head_len() as usize;
        drop(catalog);
        let before = fs::read(Catalog::snapshot_path(&tmp.cat()).unwrap().unwrap()).unwrap();
        // An ignored file's content, size and timestamps have no snapshot
        // representation: changing it must neither read content nor churn
        // bytes.
        tmp.write("regular.ignored", b"changed ignored content and size");
        let report = run(&tmp, &[tmp.tree()], Refresh::All, workers);
        assert_eq!(report.counts.files_read, 0);
        let after = fs::read(Catalog::snapshot_path(&tmp.cat()).unwrap().unwrap()).unwrap();
        assert_eq!(&after[head..], &before[head..]);
        let effective = Catalog::open(&tmp.cat()).unwrap().unwrap();
        let want = checkpoint_oracle::listings(&effective);
        let mut txn = ferret_catalog::Transaction::begin(&tmp.cat(), 1).unwrap();
        txn.keep(tmp.tree().as_os_str().as_bytes()).unwrap();
        let materialised = txn.commit().unwrap();
        assert_eq!(checkpoint_oracle::listings(&materialised), want);
    }
}

#[test]
fn effective_content_faults_use_live_names_and_graph_roots() {
    use ferret_catalog::log::{ChangeSet, Published, Record, Writer};
    use ferret_catalog::{Content, Stat, Transaction};
    let tmp = Tmp::new("fault-overlay-graph");
    let st = |ino, mode| Stat {
        dev: 1,
        ino,
        mode,
        nlink: 1,
        ..Stat::default()
    };
    let mut tx = Transaction::begin(&tmp.cat(), 1).unwrap();
    let mut b = tx.batch();
    let r = b.root(b"/r", st(1, 0o040755));
    b.entry_count(r, 1);
    let a = b.dir(r, b"a", st(2, 0o040755));
    b.entry_count(a, 1);
    b.file(a, b"old", st(3, 0o100644), Content::Fault);
    tx.add(b);
    tx.commit().unwrap();
    let mut w = Writer::open(&tmp.cat()).unwrap();
    let c = w.view();
    let r = c.roots().next().unwrap().0;
    let own = c.lookup(r, b"a").unwrap();
    let a = c.name(own).child;
    let file = c.lookup(a, b"old").unwrap();
    let child = c.name(file).child;
    let id = c.next_inode().0;
    let edge = c.next_name().0;
    let p = Published::open(&tmp.cat()).unwrap().unwrap();
    let mut counters = p.counters();
    let mut counts = p.counts();
    counters[0] += 1;
    counters[1] += 1;
    counts[0] += 1;
    counts[1] += 1;
    counts[2] += 1;
    w.commit(
        w.generation(),
        &ChangeSet {
            counters,
            counts,
            records: vec![
                Record::LifePut {
                    id,
                    kind: Kind::Dir,
                    flags: 0,
                    names: 1,
                },
                Record::InodePut {
                    id,
                    kind: Kind::Dir,
                    state: ContentState::Unindexed,
                    doc: None,
                    stat: st(4, 0o040755),
                },
                Record::NamePut {
                    id: edge,
                    parent: r.0,
                    child: id,
                    name: b"later".to_vec(),
                },
                Record::DirPut {
                    id,
                    name: Some(edge),
                    entries: Some(1),
                    flags: 4,
                    retained_at: None,
                },
                Record::NamePut {
                    id: own.0,
                    parent: id,
                    child: a.0,
                    name: b"a".to_vec(),
                },
                Record::NamePut {
                    id: file.0,
                    parent: a.0,
                    child: child.0,
                    name: b"renamed".to_vec(),
                },
            ],
        },
    )
    .unwrap();
    let effective = w.view();
    let oracle = tmp.base.join("oracle");
    let mut tx = Transaction::begin(&oracle, 1).unwrap();
    let mut b = tx.batch();
    let r = b.root(b"/r", st(1, 0o040755));
    b.entry_count(r, 1);
    let d = b.dir(r, b"later", st(4, 0o040755));
    b.entry_count(d, 1);
    let a = b.dir(d, b"a", st(2, 0o040755));
    b.entry_count(a, 1);
    b.file(a, b"renamed", st(3, 0o100644), Content::Fault);
    tx.add(b);
    let materialised = tx.commit().unwrap();
    for roots in [vec![PathBuf::from("/r")], vec![PathBuf::from("/other")]] {
        let actual = content_faults(&effective, &roots, Vec::new());
        let expected = content_faults(&materialised, &roots, Vec::new());
        assert_eq!(
            actual.iter().map(|(p, _)| p).collect::<Vec<_>>(),
            expected.iter().map(|(p, _)| p).collect::<Vec<_>>()
        );
        assert!(actual.iter().all(|(_, f)| matches!(f, ContentFault::Alias)));
    }
}
