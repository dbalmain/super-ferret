//! Resident engine tests use the production crawl, writer, query and reader.
#![allow(clippy::unwrap_used)] // A fixture failure should stop the test.

use std::fs;
use std::io;
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

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
    fs::write(tree.root().join(".ferretignore"), b"secret/\n").unwrap();
    fs::create_dir(tree.root().join("secret")).unwrap();
    fs::write(tree.root().join("secret/hidden.txt"), b"hidden by policy").unwrap();
    index(&tree.index(), &[tree.root()], Refresh::All, &options()).unwrap();
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
    assert!(!lines(&cli.stdout).contains(&b"tree/secret/hidden.txt".to_vec()));
    let live_plan = Plan::parse_at(
        &["-I".into(), "tree".into(), "-type".into(), "f".into()],
        &tree.0,
        SystemTime::now(),
    )
    .unwrap();
    let live = Output::default();
    pin.find(&live_plan, live.clone(), 4).unwrap();
    let live_cli = fixture::bounded_command(env!("CARGO_BIN_EXE_ferret"), &tree.0)
        .args(["find", "-I", "tree", "-type", "f"])
        .output()
        .unwrap();
    assert!(live_cli.status.success(), "{live_cli:?}");
    assert_eq!(lines(&live.0.lock().unwrap()), lines(&live_cli.stdout));
    assert!(lines(&live_cli.stdout).contains(&b"tree/secret/hidden.txt".to_vec()));
    assert_eq!(engine.pin().catalog().bytes_read(), loaded);
    // A counter on a newly reopened catalog could start over at the same
    // value. Remove the backing paths to prove new queries share this load.
    fs::remove_dir_all(tree.index()).unwrap();
    assert_eq!(search(&engine.pin()), expected);
    let repeated = Output::default();
    engine.pin().find(&plan, repeated.clone(), 4).unwrap();
    assert_eq!(lines(&repeated.0.lock().unwrap()), lines(&cli.stdout));
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
        assert_eq!(
            engine.pinned_epochs(),
            [old.generation().checkpoint, compact.checkpoint]
        );
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
    drop(old);
    assert_eq!(engine.pinned_epochs(), [engine.generation().checkpoint]);
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
    assert_projection(&retained, &tree);
    assert!(!retained.name_index().can_accelerate_find());
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
    assert_projection(&opaque, &tree);
    assert!(!opaque.name_index().can_accelerate_find());
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

#[test]
fn a_cli_exec_that_renames_its_cwd_keeps_that_cwd_for_later_commands() {
    let tree = Tree::new();
    let renamed = tree.0.with_extension("renamed");
    let output = fixture::bounded_command(env!("CARGO_BIN_EXE_ferret"), &tree.0)
        .args([
            "find",
            "tree/a.txt",
            "-exec",
            "sh",
            "-c",
            "mv -- \"$1\" \"$2\"",
            "sh",
        ])
        .arg(&tree.0)
        .arg(&renamed)
        .args([";", "-exec", "pwd", ";"])
        .output()
        .unwrap();
    fs::rename(&renamed, &tree.0).unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        lines(&output.stdout),
        vec![renamed.as_os_str().as_bytes().to_vec()]
    );
}

struct MovedCwd(PathBuf, PathBuf);
impl MovedCwd {
    fn new(from: &Path) -> Self {
        let to = from.with_extension("captured-cwd-moved");
        fs::rename(from, &to).unwrap();
        Self(from.to_owned(), to)
    }
}
impl Drop for MovedCwd {
    fn drop(&mut self) {
        fs::rename(&self.1, &self.0).unwrap();
    }
}

#[test]
fn a_captured_nonprocess_cwd_survives_a_move_before_reference_output_and_exec() {
    let tree = Tree::new();
    fs::write(tree.root().join("a.txt"), b"alpha\n").unwrap();
    fs::write(tree.root().join("sub/b.txt"), b"beta\n").unwrap();
    index(&tree.index(), &[tree.root()], Refresh::All, &options()).unwrap();
    let engine = Engine::open(&tree.index()).unwrap().unwrap();
    let reference = Plan::parse_at(
        &[
            "tree/a.txt".into(),
            "-samefile".into(),
            "tree/a.txt".into(),
            "-fprint".into(),
            "result".into(),
        ],
        &tree.0,
        SystemTime::now(),
    )
    .unwrap();
    let mut plans = Vec::new();
    for action in ["-exec", "-execdir"] {
        plans.push(
            Plan::parse_at(
                &[
                    "tree".into(),
                    "-type".into(),
                    "f".into(),
                    action.into(),
                    "cat".into(),
                    "{}".into(),
                    "+".into(),
                ],
                &tree.0,
                SystemTime::now(),
            )
            .unwrap(),
        );
    }
    let moved = MovedCwd::new(&tree.0);
    assert_eq!(
        engine
            .pin()
            .find(&reference, Output::default(), 4)
            .unwrap()
            .errors,
        0
    );
    assert_eq!(fs::read(moved.1.join("result")).unwrap(), b"tree/a.txt\n");
    for plan in plans {
        let output = Output::default();
        assert_eq!(
            engine.pin().find(&plan, output.clone(), 4).unwrap().errors,
            0
        );
        assert_eq!(
            lines(&output.0.lock().unwrap()),
            vec![b"alpha".to_vec(), b"beta".to_vec()]
        );
    }
}

#[derive(Clone, Default)]
struct TraversalOutput {
    output: Output,
    errors: Arc<Mutex<Vec<PathBuf>>>,
}
impl Effects for TraversalOutput {
    fn print(&mut self, path: &Path, nul: bool) -> io::Result<()> {
        self.output.print(path, nul)
    }
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.output.write(bytes)
    }
    fn error(&mut self, error: &WalkError) {
        self.errors.lock().unwrap().push(error.path.clone());
    }
}

#[test]
fn a_moved_cwd_with_an_external_symlink_back_to_a_catalog_ancestor_reports_the_first_loop() {
    let tree = Tree::new();
    std::os::unix::fs::symlink("../..", tree.root().join("sub/outside")).unwrap();
    index(&tree.index(), &[tree.root()], Refresh::All, &options()).unwrap();
    let engine = Engine::open(&tree.index()).unwrap().unwrap();
    tree.oracle(&engine.pin());
    let plan = Plan::parse_at(
        &[
            "-L".into(),
            "tree".into(),
            "-maxdepth".into(),
            "6".into(),
            "-type".into(),
            "d".into(),
        ],
        &tree.0,
        SystemTime::now(),
    )
    .unwrap();
    let _moved = MovedCwd::new(&tree.0);
    let output = TraversalOutput::default();
    let result = engine.pin().find(&plan, output.clone(), 4).unwrap();
    assert!(result.errors > 0);
    assert!(
        output
            .errors
            .lock()
            .unwrap()
            .contains(&PathBuf::from("tree/sub/outside/tree"))
    );
    assert!(!lines(&output.output.0.lock().unwrap()).contains(&b"tree/sub/outside/tree".to_vec()));
}

fn assert_projection(pin: &QuerySession, tree: &Tree) {
    let disk = Catalog::open(&tree.index()).unwrap().unwrap();
    disk.load_all().unwrap();
    let base = disk.checkpoint_base();
    let names = pin.catalog().resident_names().unwrap();
    let mut distinct: Vec<_> = base.names().map(|(_, name)| name.to_vec()).collect();
    distinct.sort();
    distinct.dedup();
    assert_eq!(
        (0..names.distinct_count())
            .map(|key| names.distinct_name(key).to_vec())
            .collect::<Vec<_>>(),
        distinct
    );
    for key in 0..names.distinct_count() {
        let mut postings = Vec::new();
        names.postings(key, &mut postings);
        let expected: Vec<_> = base
            .names()
            .filter(|(_, name)| *name == names.distinct_name(key))
            .map(|(id, _)| id.0)
            .collect();
        assert_eq!(postings, expected);
        assert_eq!(names.count(key), expected.len() as u32);
    }
    for text in [
        "*",
        "cache",
        "*.rs",
        "name-term:cache",
        "name-term:http",
        "name-term:b",
        "name-term:OpenHTTP",
    ] {
        let query = Query::parse(text, SystemTime::now()).unwrap();
        let mut actual = Vec::new();
        let mut expected = Vec::new();
        pin.search(&query, |row| {
            actual.push(row.path.to_vec());
            ControlFlow::Continue(())
        })
        .unwrap();
        query
            .run(&disk, |row| {
                expected.push(row.path.to_vec());
                ControlFlow::Continue(())
            })
            .unwrap();
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected, "{text}");
    }
}

#[test]
fn generated_create_move_hardlink_ignore_retention_and_epoch_name_sequences_match_the_full_oracle()
{
    let tree = Tree::new();
    let engine = writer_engine(&tree);
    let mut files = vec![tree.root().join("a.txt"), tree.root().join("sub/b.txt")];
    for step in 0..24 {
        let old = engine.pin();
        let old_rows = search(&old);
        let selected = step as usize % files.len();
        match step % 8 {
            0 => {
                let path = tree.root().join(format!("sub/OpenHTTP{step}_cache.rs"));
                fs::write(&path, b"birth").unwrap();
                files.push(path);
            }
            1 => {
                let path = tree.root().join(format!("movedCache{step}.txt"));
                fs::rename(&files[selected], &path).unwrap();
                files[selected] = path;
            }
            2 => {
                let path = tree.root().join(format!("sub/cacheAlias{step}.rs"));
                fs::hard_link(&files[selected], &path).unwrap();
                files.push(path);
            }
            3 => {
                fs::write(
                    tree.root().join(".ferretignore"),
                    if step % 16 == 3 {
                        b"hidden*\n".as_slice()
                    } else {
                        b"".as_slice()
                    },
                )
                .unwrap();
                fs::write(tree.root().join(format!("hidden{step}.txt")), b"ignored").unwrap();
            }
            4 => {
                fs::remove_file(files.remove(selected)).unwrap();
            }
            5 => {
                fs::write(&files[selected], format!("changed {step}")).unwrap();
                let ignore = tree.root().join("sub/.ferretignore");
                fs::write(&ignore, b"ignored\n").unwrap();
                engine.refresh(request(&engine, &tree), &options()).unwrap();
                let before_fault = engine.pin();
                let denied = Denied::new(ignore);
                let late = tree.root().join(format!("sub/lateCache{step}.rs"));
                fs::write(&late, b"retained until recovery").unwrap();
                files.push(late);
                engine.refresh(request(&engine, &tree), &options()).unwrap();
                let faulted = engine.pin();
                let mut expected = oracle::listings(before_fault.catalog());
                let scope = tree.root().join("sub");
                for row in &mut expected {
                    if row.path == scope.as_os_str().as_bytes() {
                        row.entries = None;
                        row.retained_at = Some(before_fault.generation().sequence);
                    }
                }
                assert_eq!(oracle::listings(faulted.catalog()), expected);
                assert_projection(&faulted, &tree);
                drop(denied);
            }
            6 => {
                let dir = tree.root().join(format!("directory{step}"));
                fs::create_dir(&dir).unwrap();
                let path = dir.join("HTTPServer_cache.rs");
                fs::write(&path, b"nested birth").unwrap();
                files.push(path);
            }
            _ => {
                let sequence = old.generation().sequence;
                engine.compact().unwrap();
                let current = engine.pin();
                assert_eq!(sequence, current.generation().sequence);
                assert_ne!(old.generation().checkpoint, current.generation().checkpoint);
                assert!(matches!(
                    current.search_in(
                        ferret_catalog::Handle {
                            generation: old.generation(),
                            id: ferret_catalog::InoId(u32::MAX)
                        },
                        &query(),
                        |_| ControlFlow::Continue(())
                    ),
                    Err(ferret_query::RunError::Stale(_))
                ));
                assert!(
                    old.name_index()
                        .select(current.catalog(), None, &[], |_| true)
                        .is_err()
                );
            }
        }
        engine.refresh(request(&engine, &tree), &options()).unwrap();
        assert_eq!(search(&old), old_rows, "pinned step {step}");
        let pin = engine.pin();
        assert_projection(&pin, &tree);
        tree.oracle(&pin);
        // Fresh full-index query is independent of the resident dictionary.
        let fresh = Catalog::open(&tree.0.join("oracle")).unwrap().unwrap();
        for text in ["name-term:cache", "name-term:http", "*.rs", "cache"] {
            let query = Query::parse(text, SystemTime::now()).unwrap();
            let mut expected = Vec::new();
            let mut actual = Vec::new();
            query
                .run(&fresh, |row| {
                    expected.push(row.path.to_vec());
                    ControlFlow::Continue(())
                })
                .unwrap();
            pin.search(&query, |row| {
                actual.push(row.path.to_vec());
                ControlFlow::Continue(())
            })
            .unwrap();
            expected.sort();
            actual.sort();
            assert_eq!(actual, expected, "step {step}: {text}");
        }
    }
}

#[test]
fn explicit_name_token_is_distinct_from_substring_and_gnu_glob() {
    let tree = Tree::new();
    for name in [
        "cache.rs",
        "cacheable.rs",
        "HTTPServer_cache2.rs",
        "mycache.rs",
    ] {
        fs::write(tree.root().join(name), b"text").unwrap();
    }
    index(&tree.index(), &[tree.root()], Refresh::All, &options()).unwrap();
    let pin = Engine::open(&tree.index()).unwrap().unwrap().pin();
    let run = |text| {
        let mut rows = Vec::new();
        pin.search(&Query::parse(text, SystemTime::now()).unwrap(), |row| {
            rows.push(row.path.to_vec());
            ControlFlow::Continue(())
        })
        .unwrap();
        rows.sort();
        rows
    };
    assert_eq!(run("cache").len(), 4);
    assert_eq!(run("cache*.rs").len(), 2);
    let token = run("name-term:cache");
    assert_eq!(token.len(), 2);
    assert!(
        token
            .iter()
            .any(|path| path.ends_with(b"HTTPServer_cache2.rs"))
    );
    assert!(!token.iter().any(|path| path.ends_with(b"cacheable.rs")));
    assert_eq!(run("name-term:http").len(), 1);
    assert_eq!(run("name-term:server").len(), 1);
    assert!(Query::parse("name-term:cache*", SystemTime::now()).is_err());
    assert_projection(&pin, &tree);
    tree.oracle(&pin);
}

#[test]
fn rare_scoped_postings_become_a_common_scope_walk_when_delta_births_change_the_stored_counts() {
    use ferret_catalog::{Handle, Target};
    use ferret_query::NamePlan;
    let tree = Tree::new();
    for i in 0..80 {
        fs::write(tree.root().join(format!("sub/ordinary{i}.rs")), b"text").unwrap();
    }
    fs::write(tree.root().join("sub/RareAtom.rs"), b"rare").unwrap();
    fs::write(tree.root().join("sub/common.rs"), b"common").unwrap();
    for i in 0..60 {
        let dir = tree.root().join(format!("outside{i}"));
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("common.rs"), b"common").unwrap();
    }
    index(&tree.index(), &[tree.root()], Refresh::All, &options()).unwrap();
    let engine = writer_engine(&tree);
    let plan = |pin: &QuerySession, needle: &[u8]| {
        let Target::Inode(dir) = pin
            .catalog()
            .resolve(tree.root().join("sub").as_os_str().as_bytes())
            .unwrap()
            .target
        else {
            panic!("scope")
        };
        pin.name_index()
            .select(
                pin.catalog(),
                Some(Handle {
                    generation: pin.generation(),
                    id: dir,
                }),
                &[],
                |name| name.starts_with(needle),
            )
            .unwrap()
    };
    let old = engine.pin();
    let rare = plan(&old, b"RareAtom");
    assert_eq!(rare.estimate.hits, 1);
    assert_eq!(rare.estimate.plan, NamePlan::Postings);
    assert_eq!(rare.rows(old.catalog()).unwrap().len(), 1);
    assert_eq!(plan(&old, b"common").estimate.plan, NamePlan::ScopeWalk);
    let args = ["tree/sub".into(), "-name".into(), "RareAtom*".into()];
    let find = Plan::parse_at(&args, &tree.0, SystemTime::now()).unwrap();
    let output = Output::default();
    old.find(&find, output.clone(), 4).unwrap();
    assert_eq!(lines(&output.0.lock().unwrap()).len(), 1);
    for i in 0..12 {
        fs::write(
            tree.root().join(format!("sub/RareAtom-born-{i}.rs")),
            b"birth",
        )
        .unwrap();
    }
    engine.refresh(request(&engine, &tree), &options()).unwrap();
    let new = engine.pin();
    let common = plan(&new, b"RareAtom");
    assert_eq!(common.estimate.hits, 13);
    assert_eq!(common.estimate.scope_rows, Some(95)); // b.txt + 80 ordinary + RareAtom + common + 12 births.
    assert_eq!(common.estimate.plan, NamePlan::ScopeWalk);
    assert_eq!(plan(&old, b"RareAtom").estimate.plan, NamePlan::Postings);
    let output = Output::default();
    new.find(&find, output.clone(), 4).unwrap();
    assert_eq!(lines(&output.0.lock().unwrap()).len(), 13);
    assert_projection(&new, &tree);
    tree.oracle(&new);
}

#[test]
fn pure_name_postings_preserve_operand_spelling_and_live_fault_fallback() {
    let tree = Tree::new();
    for i in 0..40 {
        fs::write(tree.root().join(format!("sub/ordinary{i}.bin")), b"text").unwrap();
    }
    fs::write(tree.root().join("sub/RareAtom.rs"), b"rare").unwrap();
    let ignore = tree.root().join("sub/.ferretignore");
    fs::write(&ignore, b"ignored\n").unwrap();
    index(&tree.index(), &[tree.root()], Refresh::All, &options()).unwrap();
    let engine = writer_engine(&tree);
    let compare = |pin: &QuerySession, start: &str| {
        let args = [start.into(), "-name".into(), "RareAtom*".into()];
        let plan = Plan::parse_at(&args, &tree.0, SystemTime::now()).unwrap();
        let expected = Output::default();
        plan.run(
            &mut plan.catalog_source(pin.catalog().clone()),
            &mut expected.clone(),
        )
        .unwrap();
        let actual = Output::default();
        pin.find(&plan, actual.clone(), 4).unwrap();
        let expected = lines(&expected.0.lock().unwrap());
        assert_eq!(lines(&actual.0.lock().unwrap()), expected, "{start}");
        expected
    };
    for start in ["tree/sub", "tree//sub/", "tree/sub/."] {
        assert_eq!(compare(&engine.pin(), start).len(), 1);
    }
    let denied = Denied::new(ignore);
    engine.refresh(request(&engine, &tree), &options()).unwrap();
    let retained = engine.pin();
    drop(denied);
    fs::write(tree.root().join("sub/RareAtom-late.rs"), b"live fallback").unwrap();
    assert_eq!(compare(&retained, "tree/sub").len(), 2);
    engine.refresh(request(&engine, &tree), &options()).unwrap();
    tree.oracle(&engine.pin());
    let denied = Denied::new(tree.root().join("sub"));
    engine.refresh(request(&engine, &tree), &options()).unwrap();
    let opaque = engine.pin();
    drop(denied);
    assert_eq!(compare(&opaque, "tree/sub").len(), 2);
    assert_projection(&opaque, &tree);
    engine.refresh(request(&engine, &tree), &options()).unwrap();
    tree.oracle(&engine.pin());
}

#[test]
fn an_independent_catalog_incarnation_cannot_reuse_another_name_term_base() {
    let left = Tree::new();
    let right = Tree::new();
    fs::write(left.root().join("HTTPServer_cache.rs"), b"left").unwrap();
    fs::write(right.root().join("unrelatedReceipt.txt"), b"right").unwrap();
    for tree in [&left, &right] {
        index(&tree.index(), &[tree.root()], Refresh::All, &options()).unwrap();
    }
    let old = Engine::open(&left.index()).unwrap().unwrap().pin();
    let current = Engine::open(&right.index()).unwrap().unwrap().pin();
    assert_eq!(old.generation().checkpoint, current.generation().checkpoint);
    assert_ne!(
        old.generation().incarnation,
        current.generation().incarnation
    );
    let adopted = ferret_query::NameIndex::adopt(current.catalog(), Some(old.name_index()));
    assert!(
        old.name_index()
            .select(current.catalog(), None, &[], |_| true)
            .is_err()
    );
    let query = Query::parse("name-term:receipt", SystemTime::now()).unwrap();
    let mut rows = Vec::new();
    query
        .run_indexed(current.catalog(), &adopted, None, |row| {
            rows.push(row.path.to_vec());
            ControlFlow::Continue(())
        })
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].ends_with(b"unrelatedReceipt.txt"));
}

#[test]
fn host_cancellation_stops_search_before_the_next_candidate_is_evaluated() {
    use std::sync::atomic::AtomicBool;
    let tree = Tree::new();
    let engine = Engine::open(&tree.index()).unwrap().unwrap();
    let pin = engine.pin();
    for atom in [b"*.txt".as_slice(), b"type:f"] {
        let query = Query::from_args([atom], SystemTime::now()).unwrap();
        let mut full_rows = 0;
        let full = pin
            .search(&query, |_| {
                full_rows += 1;
                ControlFlow::Continue(())
            })
            .unwrap();
        assert_eq!(full_rows, 2);
        let cancelled = AtomicBool::new(false);
        let mut rows = 0;
        // Returning Continue is essential: the cancellation flag, rather than
        // the output callback's ordinary limit break, must stop the evaluator.
        let result = pin
            .search_until(&query, Some(&cancelled), |_| {
                rows += 1;
                cancelled.store(true, Ordering::Release);
                ControlFlow::Continue(())
            })
            .unwrap();
        assert_eq!(rows, 1);
        assert!(result.candidates <= full.candidates);
        assert_eq!(search(&engine.pin()).len(), 2);
    }
}

/// Live content matches for `word` in a pin: the index view's postings
/// filtered by the pinned catalog view's liveness.
fn content_docs(pin: &QuerySession, word: &[u8]) -> Vec<u32> {
    let live = ferret::engine::live_documents(pin.catalog());
    let view = pin.content().unwrap();
    let mut docs = view.lookup(word).unwrap();
    docs.retain(|&doc| live.contains(doc));
    docs
}

#[test]
fn a_pin_pairs_its_catalog_view_with_the_content_view_published_beside_it() {
    use ferret_index::Budget;
    let tree = Tree::new();
    let engine = writer_engine(&tree);
    assert!(engine.pin().content().is_none());
    assert!(matches!(
        engine.follow_content(&Budget::unbounded(), None),
        Err(ferret::engine::Error::NoContentIndex)
    ));
    engine.attach_content(&tree.index()).unwrap();
    let empty = engine.pin();
    let live = ferret::engine::live_documents(empty.catalog());
    assert_eq!(empty.content().unwrap().uncovered(&live).len(), 2);

    let followed = engine.follow_content(&Budget::unbounded(), None).unwrap();
    assert_eq!((followed.docs, followed.unreadable), (2, 0));
    let first = engine.pin();
    let alpha = content_docs(&first, b"alpha");
    assert_eq!((alpha.len(), content_docs(&first, b"beta").len()), (1, 1));

    // The catalog moves on; the content view stays with it until a pass.
    fs::remove_file(tree.root().join("a.txt")).unwrap();
    fs::write(tree.root().join("new.txt"), b"gamma alpha").unwrap();
    engine.refresh(request(&engine, &tree), &options()).unwrap();
    let refreshed = engine.pin();
    let live = ferret::engine::live_documents(refreshed.catalog());
    let uncovered = refreshed.content().unwrap().uncovered(&live);
    assert_eq!(uncovered.len(), 1, "only the new document");
    assert!(
        content_docs(&refreshed, b"alpha").is_empty(),
        "a.txt is dead"
    );

    let followed = engine.follow_content(&Budget::unbounded(), None).unwrap();
    assert_eq!(followed.docs, 1);
    let last = engine.pin();
    assert_eq!(content_docs(&last, b"alpha"), uncovered);
    assert_eq!(content_docs(&last, b"gamma"), uncovered);
    // Older pins keep their own pairing.
    assert_eq!(content_docs(&first, b"alpha"), alpha);
    assert!(content_docs(&refreshed, b"gamma").is_empty());

    let query = Query::from_args([b"text:alpha".as_slice()], SystemTime::now()).unwrap();
    let answer = |pin: &QuerySession| {
        let mut rows = Vec::new();
        pin.search(&query, |row| {
            rows.push(row.path.to_vec());
            ControlFlow::Continue(())
        })
        .unwrap();
        rows
    };
    let before_merge = answer(&first);
    let input = first.content().unwrap().manifest().segments[0].file_name();
    // The first segment is half dead, past the trigger.
    let merged = engine.merge_content(&Budget::unbounded()).unwrap().unwrap();
    assert_eq!((merged.inputs, merged.purged), (1, 1));
    assert!(
        !tree
            .index()
            .join(ferret::engine::CONTENT_DIR)
            .join(input)
            .exists()
    );
    assert_eq!(answer(&first), before_merge);
    assert_eq!(content_docs(&engine.pin(), b"alpha"), uncovered);
    assert_eq!(content_docs(&engine.pin(), b"beta").len(), 1);
    assert_eq!(
        content_docs(&first, b"alpha"),
        alpha,
        "the old view kept its files"
    );

    // A reopened writer finds the same index.
    drop(engine);
    let engine = writer_engine(&tree);
    engine.attach_content(&tree.index()).unwrap();
    assert_eq!(content_docs(&engine.pin(), b"gamma"), uncovered);
    let live = ferret::engine::live_documents(engine.pin().catalog());
    assert!(engine.pin().content().unwrap().uncovered(&live).is_empty());
}

/// Reproducible D63 measurement: production refresh warms each edited document,
/// then the real follow reads it a second time. No log or daemon is involved.
#[test]
#[ignore = "S2 M5 scratch-tree measurement; run release with --ignored --nocapture"]
fn measure_steady_content_follow() {
    use ferret_index::Budget;
    fn counter(path: &str, key: &str) -> u64 {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .find_map(|line| {
                line.strip_prefix(key)
                    .and_then(|s| s.split_whitespace().next())
                    .and_then(|s| s.parse().ok())
            })
            .unwrap()
    }
    fn cpu() -> u64 {
        fs::read_to_string("/proc/thread-self/schedstat")
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }
    let tree = Tree::new();
    let document = |file, round| {
        let head = format!("document{file} revision{round} common requestHandler ");
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(&b"common requestHandler alpha beta ".repeat(2048));
        bytes.truncate(64 << 10);
        bytes
    };
    for file in 0..512 {
        fs::write(
            tree.root().join(format!("measure-{file}.txt")),
            document(file, 0),
        )
        .unwrap();
    }
    let engine = writer_engine(&tree);
    engine.refresh(request(&engine, &tree), &options()).unwrap();
    engine.attach_content(&tree.index()).unwrap();
    while engine
        .follow_content(&Budget::unbounded(), None)
        .unwrap()
        .remaining
        > 0
    {}
    while engine
        .merge_content(&Budget::unbounded())
        .unwrap()
        .is_some()
    {}
    let (mut nanos, mut chars, mut device, mut changed, mut wall) =
        (0u64, 0u64, 0u64, 0u64, Duration::ZERO);
    for round in 1..=40 {
        for offset in 0..8 {
            let file = (round * 8 + offset) % 512;
            fs::write(
                tree.root().join(format!("measure-{file}.txt")),
                document(file, round),
            )
            .unwrap();
        }
        engine.refresh(request(&engine, &tree), &options()).unwrap();
        let before_chars = counter("/proc/self/io", "rchar:");
        let before_device = counter("/proc/self/io", "read_bytes:");
        let before_cpu = cpu();
        let started = std::time::Instant::now();
        loop {
            let followed = engine.follow_content(&Budget::unbounded(), None).unwrap();
            changed += followed.bytes;
            if followed.remaining == 0 {
                break;
            }
        }
        wall += started.elapsed();
        nanos += cpu() - before_cpu;
        chars += counter("/proc/self/io", "rchar:") - before_chars;
        device += counter("/proc/self/io", "read_bytes:") - before_device;
        while engine
            .merge_content(&Budget::unbounded())
            .unwrap()
            .is_some()
        {}
    }
    let before_rss = counter("/proc/self/status", "VmRSS:");
    let pins: Vec<_> = (0..128).map(|_| engine.pin()).collect();
    let after_rss = counter("/proc/self/status", "VmRSS:");
    println!(
        "M5_FOLLOW refreshes=40 changed_bytes={changed} rchar={chars} read_bytes={device} cpu_ns={nanos} wall_ns={} view_bytes={} pin_count={} pin_size={} rss_delta_kib={}",
        wall.as_nanos(),
        pins[0].content().unwrap().resident_bytes(),
        pins.len(),
        std::mem::size_of::<QuerySession>(),
        after_rss.saturating_sub(before_rss)
    );
    assert_eq!(changed, 40 * 8 * (64 << 10));
}
