//! `index` end to end, on real temp trees: every test drives the real
//! pipeline and reads the published catalog back.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferret_catalog::{BeginError, Catalog, ContentState, DocId, InoId, Kind};
use ferret_policy::Config;

use crate::index::{PROBES, Probe};
use crate::{ContentFault, IndexError, IndexOptions, IoOp, Refresh, Report, index};

/// A temp directory holding the walked tree (`tree/`) and the catalog
/// (`cat/`), outside the tree. Cleanup is pure Rust: a spawned `chmod` would
/// inherit a catalog lock descriptor between fork and exec, and another test's
/// `begin` would see `Locked`.
struct Tmp {
    base: PathBuf,
    _fds: std::sync::RwLockReadGuard<'static, ()>,
}

impl Tmp {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!("ferret-index-{}-{name}", std::process::id()));
        unlock(&base);
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("tree")).unwrap();
        let fds = super::FDS.read().unwrap_or_else(|e| e.into_inner());
        Self { base, _fds: fds }
    }

    fn tree(&self) -> PathBuf {
        self.base.join("tree")
    }

    fn at(&self, rel: &str) -> PathBuf {
        self.tree().join(rel)
    }

    fn cat(&self) -> PathBuf {
        self.base.join("cat")
    }

    fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
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
        let ino = catalog.name(id).child;
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
    let rows = listing(&catalog);
    assert_eq!(rows.len(), catalog.name_count() as usize);
    (catalog, rows)
}

fn blake3_128(bytes: &[u8]) -> [u8; 16] {
    let mut hash = [0; 16];
    hash.copy_from_slice(&blake3::hash(bytes).as_bytes()[..16]);
    hash
}

/// Installs a probe on walks of `root` until dropped.
struct Hook {
    root: PathBuf,
}

impl Hook {
    fn set(root: &Path, hook: impl Fn(Probe<'_>) + Send + Sync + 'static) -> Self {
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
    let first = fs::read(tmp.cat().join("catalog")).unwrap();
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
    let second = fs::read(tmp.cat().join("catalog")).unwrap();
    assert!(
        first == second,
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
        assert!(reported.contains(&x) && reported.contains(&y), "{}", x.display());
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

#[test]
fn an_unreadable_directory_blocks_publication_and_keeps_the_old_generation() {
    let tmp = Tmp::new("coverage");
    tmp.write("open/a.txt", b"a\n");
    let roots = [tmp.tree()];
    run(&tmp, &roots, Refresh::All, 2);
    let before = fs::read(tmp.cat().join("catalog")).unwrap();

    tmp.write("shut/b.txt", b"b\n");
    chmod(&tmp.at("shut"), 0o000);
    tmp.write("open/new.txt", b"new\n");
    let error = index(&tmp.cat(), &roots, Refresh::All, &options(2)).unwrap_err();
    let IndexError::Coverage { faults, report } = error else {
        panic!("expected a coverage fault, got {error}");
    };
    assert_eq!(faults.len(), 1);
    assert_eq!(faults[0].op, IoOp::OpenDir);
    assert_eq!(faults[0].path, Path::new("shut"));
    assert_eq!(faults[0].error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(report.published.is_none());
    assert_eq!(fs::read(tmp.cat().join("catalog")).unwrap(), before);
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
