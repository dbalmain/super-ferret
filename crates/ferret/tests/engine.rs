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
        let disk = Catalog::open(&self.index()).unwrap().unwrap();
        let expected = oracle::listings(&fresh);
        assert_eq!(oracle::listings(pin.catalog()), expected);
        assert_eq!(oracle::listings(&disk), expected);
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

fn writer_engine(tree: &Tree) -> Engine {
    let mut writer = ferret_catalog::WriterSession::open(&tree.index()).unwrap();
    // Keep these tiny fixtures in the append path until the explicit compact.
    writer.set_compaction_limits(ferret_catalog::CompactionLimits {
        log_bytes: u64::MAX,
        records: u64::MAX,
        dirty_percent: 100,
        dead_percent: 100,
    });
    Engine::from_writer(writer)
}
fn request(engine: &Engine, tree: &Tree) -> ferret_crawl::RefreshRequest {
    ferret_crawl::RefreshRequest {
        expected_generation: engine.pin().generation(),
        scopes: vec![ferret_crawl::RefreshScope::Root(tree.root())],
        rename_hints: Vec::new(),
        reason: ferret_crawl::RefreshReason::Burst,
    }
}

#[test]
fn a_real_writer_refresh_is_adopted_without_a_second_catalog_load() {
    let tree = Tree::new();
    let engine = writer_engine(&tree);
    let old = engine.pin();
    let bytes = old.catalog().bytes_read();
    fs::remove_file(tree.root().join("a.txt")).unwrap();
    fs::write(tree.root().join("new.txt"), b"new document").unwrap();
    let report = engine.refresh(request(&engine, &tree), &options()).unwrap();
    assert!(matches!(
        report.outcome,
        ferret_crawl::RefreshOutcome::Committed { .. }
    ));
    assert_eq!(engine.pin().generation(), report.view.generation());
    assert_eq!(
        engine.pin().generation().checkpoint,
        old.generation().checkpoint
    );
    assert!(engine.pin().generation().sequence > old.generation().sequence);
    assert_eq!(engine.pin().catalog().bytes_read(), bytes);
    assert_ne!(search(&engine.pin()), search(&old));
    tree.oracle(&engine.pin());
    let selected = tree.index().join("current");
    fs::rename(&selected, tree.index().join("hidden-current")).unwrap();
    // A reopen would now see no index. Adoption must use the returned view.
    assert_eq!(search(&engine.pin()).len(), 2);
    fs::rename(tree.index().join("hidden-current"), selected).unwrap();
}

#[test]
fn a_query_paused_on_its_first_row_survives_append_and_dense_checkpoint_remapping() {
    let tree = Tree::new();
    let engine = writer_engine(&tree);
    let old = engine.pin();
    let wanted = search(&old);
    let survivor_path = tree.root().join("sub/b.txt");
    let old_id = old
        .catalog()
        .resolve(survivor_path.as_os_str().as_bytes())
        .unwrap()
        .target;
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let running = old.clone();
        let query = scope.spawn(move || {
            let mut first = true;
            let mut rows = Vec::new();
            running
                .search(&self::query(), |row| {
                    if first {
                        first = false;
                        started_tx.send(()).unwrap();
                        resume_rx
                            .recv_timeout(std::time::Duration::from_secs(10))
                            .unwrap();
                    }
                    assert!(running.catalog().size(row.inode) > 0);
                    rows.push(row.path.to_vec());
                    ControlFlow::Continue(())
                })
                .unwrap();
            rows.sort();
            rows
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        fs::remove_file(tree.root().join("a.txt")).unwrap();
        let report = engine.refresh(request(&engine, &tree), &options()).unwrap();
        assert!(matches!(
            report.outcome,
            ferret_crawl::RefreshOutcome::Committed { .. }
        ));
        let append = engine.pin().generation();
        let compact = engine.compact().unwrap();
        assert_eq!(append.sequence, compact.sequence);
        assert_ne!(append.checkpoint, compact.checkpoint);
        let current = engine.pin();
        let new_id = current
            .catalog()
            .resolve(survivor_path.as_os_str().as_bytes())
            .unwrap()
            .target;
        assert_ne!(old_id, new_id, "the survivor must really be renumbered");
        assert_eq!(
            search(&current),
            vec![survivor_path.as_os_str().as_bytes().to_vec()]
        );
        tree.oracle(&current);
        resume_tx.send(()).unwrap();
        assert_eq!(query.join().unwrap(), wanted);
    });
    // Compaction unlinked the old files; the old pin remains usable afterwards.
    assert_eq!(search(&old), wanted);
}

#[test]
fn an_unchanged_sequence_stale_epoch_with_an_invalid_id_retries_before_dereference() {
    let tree = Tree::new();
    let engine = writer_engine(&tree);
    let old = engine.pin().generation();
    let new = engine.compact().unwrap();
    assert_eq!(new.sequence, old.sequence);
    assert_ne!(new.checkpoint, old.checkpoint);
    let mut stale = request(&engine, &tree);
    stale.expected_generation = old;
    stale.scopes = vec![ferret_crawl::RefreshScope::Entry {
        parent: ferret_catalog::InoId(u32::MAX),
        basename: b"invalid".to_vec(),
    }];
    let result = engine.refresh(stale, &options()).unwrap();
    assert!(matches!(
        result.outcome,
        ferret_crawl::RefreshOutcome::RetryFromCurrent(_)
    ));
    assert_eq!(engine.pin().generation(), new);
    tree.oracle(&engine.pin());
}

struct Denied(PathBuf, fs::Permissions);
impl Denied {
    fn new(path: PathBuf) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let previous = fs::metadata(&path).unwrap().permissions();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
        Self(path, previous)
    }
}
impl Drop for Denied {
    fn drop(&mut self) {
        fs::set_permissions(&self.0, self.1.clone()).unwrap();
    }
}
fn find_subtree(pin: &QuerySession, tree: &Tree, live: bool) -> Vec<Vec<u8>> {
    let plan = Plan::parse_at(
        &[
            "tree/sub".into(),
            "-type".into(),
            "f".into(),
            "-print".into(),
        ],
        &tree.0,
        SystemTime::now(),
    )
    .unwrap();
    let output = Output::default();
    let outcome = if live {
        plan.run_parallel(plan.live_source(), output.clone(), 4)
            .unwrap()
    } else {
        pin.find(&plan, output.clone(), 4).unwrap()
    };
    assert_eq!(outcome.errors, 0);
    lines(&output.0.lock().unwrap())
}

#[test]
fn an_unreadable_ferretignore_retains_real_old_rows_and_find_uses_live_fallback() {
    let tree = Tree::new();
    let ignore = tree.root().join("sub/.ferretignore");
    fs::write(&ignore, b"ignored\n").unwrap();
    index(&tree.index(), &[tree.root()], Refresh::All, &options()).unwrap();
    let engine = writer_engine(&tree);
    let before = engine.pin();
    let denied = Denied::new(ignore);
    assert_eq!(
        fs::read(&denied.0).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    fs::write(tree.root().join("sub/late.txt"), b"not observed").unwrap();
    let report = engine.refresh(request(&engine, &tree), &options()).unwrap();
    assert_eq!(report.report.protected_scopes, 1);
    let retained = engine.pin();
    let scope = tree.root().join("sub").as_os_str().as_bytes().to_vec();
    let resolved = retained.catalog().resolve(&scope).unwrap();
    let ferret_catalog::Target::Inode(id) = resolved.target else {
        panic!("directory")
    };
    assert_eq!(retained.catalog().entry_count(id), None);
    assert_eq!(
        retained.catalog().retained_at(id),
        Some(before.generation().sequence)
    );
    drop(denied);
    let destination = tree.0.join("oracle");
    index(&destination, &[tree.root()], Refresh::All, &options()).unwrap();
    let fresh = Catalog::open(&destination).unwrap().unwrap();
    // The retained expectation consists of actual pre-fault checkpoint rows,
    // with trustworthy rows from the independently crawled current tree.
    let inside =
        |p: &[u8]| p == scope || (p.starts_with(&scope) && p.get(scope.len()) == Some(&b'/'));
    let mut wanted: Vec<_> = oracle::listings(&fresh)
        .into_iter()
        .filter(|r| !inside(&r.path))
        .collect();
    wanted.extend(
        oracle::listings(before.catalog())
            .into_iter()
            .filter(|r| inside(&r.path)),
    );
    for row in &mut wanted {
        if row.path == scope {
            row.entries = None;
            row.retained_at = Some(before.generation().sequence);
        }
    }
    wanted.sort();
    assert_eq!(oracle::listings(retained.catalog()), wanted);
    let disk = Catalog::open(&tree.index()).unwrap().unwrap();
    assert_eq!(oracle::listings(&disk), wanted);
    assert_eq!(
        find_subtree(&retained, &tree, false),
        find_subtree(&retained, &tree, true)
    );
    assert!(find_subtree(&retained, &tree, false).contains(&b"tree/sub/late.txt".to_vec()));
    engine.refresh(request(&engine, &tree), &options()).unwrap();
    tree.oracle(&engine.pin());
}

#[test]
fn directory_eacces_removes_old_children_and_the_opaque_row_keeps_live_fallback() {
    let tree = Tree::new();
    let engine = writer_engine(&tree);
    let denied = Denied::new(tree.root().join("sub"));
    assert_eq!(
        fs::read_dir(&denied.0).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    let report = engine.refresh(request(&engine, &tree), &options()).unwrap();
    assert_eq!(report.report.protected_scopes, 0);
    let opaque = engine.pin();
    let resolved = opaque
        .catalog()
        .resolve(denied.0.as_os_str().as_bytes())
        .unwrap();
    let ferret_catalog::Target::Inode(id) = resolved.target else {
        panic!("directory")
    };
    assert_eq!(opaque.catalog().entry_count(id), None);
    assert_eq!(opaque.catalog().retained_at(id), None);
    assert_eq!(opaque.catalog().children(id).count(), 0);
    assert!(search(&opaque).iter().all(|path| !path.ends_with(b"b.txt")));
    tree.oracle(&opaque);
    let generation = opaque.generation();
    engine.refresh(request(&engine, &tree), &options()).unwrap();
    assert_eq!(engine.pin().generation(), generation);
    drop(denied);
    fs::write(tree.root().join("sub/late.txt"), b"live opaque fallback").unwrap();
    assert_eq!(
        find_subtree(&opaque, &tree, false),
        find_subtree(&opaque, &tree, true)
    );
    assert!(find_subtree(&opaque, &tree, false).contains(&b"tree/sub/late.txt".to_vec()));
    engine.refresh(request(&engine, &tree), &options()).unwrap();
    tree.oracle(&engine.pin());
}

#[test]
fn explicit_cwd_reaches_reference_files_output_files_and_single_and_batched_exec() {
    let tree = Tree::new();
    fs::write(tree.root().join("a.txt"), b"alpha\n").unwrap();
    fs::write(tree.root().join("sub/b.txt"), b"beta\n").unwrap();
    index(&tree.index(), &[tree.root()], Refresh::All, &options()).unwrap();
    let engine = Engine::open(&tree.index()).unwrap().unwrap();
    let pin = engine.pin();
    let cases: &[&[&str]] = &[
        &["tree", "-type", "f", "-exec", "cat", "{}", ";"],
        &["tree", "-type", "f", "-exec", "cat", "{}", "+"],
        &["tree", "-type", "f", "-execdir", "cat", "{}", ";"],
        &["tree", "-type", "f", "-execdir", "cat", "{}", "+"],
        &["tree", "-samefile", "tree/a.txt", "-print"],
    ];
    for args in cases {
        let args: Vec<_> = args.iter().map(std::ffi::OsString::from).collect();
        let plan = Plan::parse_at(&args, &tree.0, SystemTime::now()).unwrap();
        let output = Output::default();
        assert_eq!(pin.find(&plan, output.clone(), 4).unwrap().errors, 0);
        let cli = fixture::bounded_command(env!("CARGO_BIN_EXE_ferret"), &tree.0)
            .arg("find")
            .args(&args)
            .output()
            .unwrap();
        assert!(cli.status.success(), "{cli:?}");
        assert_eq!(
            lines(&output.0.lock().unwrap()),
            lines(&cli.stdout),
            "{args:?}"
        );
    }
    let plan = Plan::parse_at(
        &["tree/a.txt".into(), "-fprint".into(), "result".into()],
        &tree.0,
        SystemTime::now(),
    )
    .unwrap();
    pin.find(&plan, Output::default(), 4).unwrap();
    assert_eq!(fs::read(tree.0.join("result")).unwrap(), b"tree/a.txt\n");
}

#[test]
fn a_captured_start_time_controls_relative_find_dates_on_a_resident_pin() {
    let tree = Tree::new();
    let engine = Engine::open(&tree.index()).unwrap().unwrap();
    let modified = fs::metadata(tree.root().join("a.txt"))
        .unwrap()
        .modified()
        .unwrap();
    let minute = std::time::Duration::from_secs(60);
    for (started, expected) in [(modified - minute, 1), (modified + minute, 0)] {
        let plan = Plan::parse_at(
            &["tree/a.txt".into(), "-newermt".into(), "now".into()],
            &tree.0,
            started,
        )
        .unwrap();
        let output = Output::default();
        engine.pin().find(&plan, output.clone(), 4).unwrap();
        assert_eq!(lines(&output.0.lock().unwrap()).len(), expected);
    }
}

#[test]
fn a_production_limit_refresh_adopts_its_automatic_checkpoint_and_keeps_the_old_pin() {
    let tree = Tree::new();
    let engine = Engine::from_writer(ferret_catalog::WriterSession::open(&tree.index()).unwrap());
    let old = engine.pin();
    let previous = search(&old);
    fs::write(
        tree.root().join("new.txt"),
        b"new birth crosses one percent",
    )
    .unwrap();
    let report = engine.refresh(request(&engine, &tree), &options()).unwrap();
    assert!(matches!(
        report.outcome,
        ferret_crawl::RefreshOutcome::Checkpointed
    ));
    assert_ne!(
        old.generation().checkpoint,
        engine.pin().generation().checkpoint
    );
    assert_eq!(report.view.generation(), engine.pin().generation());
    assert_eq!(search(&old), previous);
    assert_eq!(search(&engine.pin()).len(), previous.len() + 1);
    tree.oracle(&engine.pin());
}
