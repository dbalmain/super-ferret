//! Real inotify/ferretd tests. The oracle is a fresh full production index of
//! the same temporary tree; search paths and find path/type records must agree.

#[path = "support/fixture.rs"]
mod fixture;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const FERRET: &str = env!("CARGO_BIN_EXE_ferret");
const DAEMON: &str = env!("CARGO_BIN_EXE_ferretd");
const CONTENT: [&str; 14] = [
    "text:original",
    "text:modified",
    "text:create",
    "text:atomic save",
    "text:0",
    "text:1",
    "NOT text:original",
    "text:absent OR text:modified",
    "text:needle",
    // Last file `unfollowed_burst` writes (`BURST_FILES - 1`).
    "text:unique23",
    "text:word",
    "text:word17part3",
    "text:word500part3",
    "case:text:word500part3",
];
type ContentAnswers = Vec<(Vec<Vec<u8>>, Vec<u8>, Option<i32>)>;

const BOUND: Duration = Duration::from_secs(8);

struct Tree {
    base: PathBuf,
    daemon: Option<Child>,
}
impl Tree {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!(
            "ferret-watch-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_else(|error| panic!("clock: {error:?}"))
                .as_nanos()
        ));
        for dir in ["src/left/deep", "src/right", "outside", "home/runtime"] {
            fs::create_dir_all(base.join(dir))
                .unwrap_or_else(|error| panic!("fixture directories: {error:?}"));
        }
        fs::set_permissions(base.join("home/runtime"), fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|error| panic!("runtime permissions: {error:?}"));
        fs::write(base.join("src/left/original.txt"), "original")
            .unwrap_or_else(|error| panic!("fixture file: {error:?}"));
        let tree = Self { base, daemon: None };
        tree.success(tree.local(&["index", "src"]));
        assert!(tree.socket().is_none(), "index must not spawn a daemon");
        tree
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = fixture::bounded_command(FERRET, &self.base);
        cmd.args(args)
            .env_remove("FERRET_NO_DAEMON")
            .env("FERRET_DAEMON_BIN", self.base.join("no-spawn-binary"))
            .env("FERRET_DAEMON_STARTUP_MS", "2000");
        cmd
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args)
            .output()
            .unwrap_or_else(|error| panic!("bounded command: {error:?}"))
    }
    fn local(&self, args: &[&str]) -> Output {
        self.command(args)
            .env("FERRET_NO_DAEMON", "1")
            .output()
            .unwrap_or_else(|error| panic!("bounded local command: {error:?}"))
    }
    fn success(&self, output: Output) -> Output {
        assert!(
            output.status.success(),
            "status {:?}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
    fn socket(&self) -> Option<PathBuf> {
        fs::read_dir(self.base.join("home/runtime/ferret"))
            .ok()?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "sock"))
    }
    fn start(&mut self, extra: &[(&str, &str)]) {
        self.spawn(extra);
        self.quiet();
    }
    fn spawn(&mut self, extra: &[(&str, &str)]) {
        let mut cmd = fixture::command(DAEMON, &self.base);
        cmd.env_remove("FERRET_NO_DAEMON")
            .env("FERRET_DAEMON_IDLE_MS", "0")
            .env("FERRET_BACKSTOP_MS", "3600000")
            .env("FERRET_POLL_MS", "300000")
            .stdout(Stdio::null());
        let log = fs::File::create(self.base.join("daemon.log"))
            .unwrap_or_else(|error| panic!("daemon log: {error:?}"));
        cmd.stderr(log);
        for (name, value) in extra {
            cmd.env(name, value);
        }
        self.daemon = Some(
            cmd.spawn()
                .unwrap_or_else(|error| panic!("daemon spawn: {error:?}")),
        );
        let until = Instant::now() + BOUND;
        while self
            .socket()
            .is_none_or(|p| UnixStream::connect(p).is_err())
        {
            assert!(
                Instant::now() < until,
                "daemon did not bind: {}",
                fs::read_to_string(self.base.join("daemon.log")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn stop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
    }
    fn voluntary_switches(&self) -> u64 {
        let pid = self
            .daemon
            .as_ref()
            .map(Child::id)
            .unwrap_or_else(|| panic!("daemon was not started"));
        fs::read_dir(format!("/proc/{pid}/task"))
            .unwrap_or_else(|error| panic!("read daemon tasks: {error}"))
            .map(|task| {
                let task = task.unwrap_or_else(|error| panic!("read daemon task entry: {error}"));
                let status = fs::read_to_string(task.path().join("status"))
                    .unwrap_or_else(|error| panic!("read daemon task status: {error}"));
                status
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("voluntary_ctxt_switches:")
                            .and_then(|value| value.trim().parse::<u64>().ok())
                    })
                    .unwrap_or(0)
            })
            .sum()
    }
    fn event(&self, request: &str) -> String {
        let stream = UnixStream::connect(self.socket().unwrap_or_else(|| panic!("socket")))
            .unwrap_or_else(|error| panic!("connect: {error:?}"));
        stream
            .set_read_timeout(Some(BOUND))
            .unwrap_or_else(|error| panic!("read timeout: {error:?}"));
        stream
            .set_write_timeout(Some(BOUND))
            .unwrap_or_else(|error| panic!("write timeout: {error:?}"));
        let mut reader = BufReader::new(stream);
        loop {
            let mut hello = String::new();
            reader
                .read_line(&mut hello)
                .unwrap_or_else(|error| panic!("hello: {error:?}"));
            assert!(!hello.is_empty());
            if hello.contains("\"state\":\"ready\"") {
                break;
            }
            assert!(hello.contains("\"state\":\"loading\""), "{hello}");
        }
        reader
            .get_mut()
            .write_all(request.as_bytes())
            .unwrap_or_else(|error| panic!("request: {error:?}"));
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .unwrap_or_else(|error| panic!("event: {error:?}"));
        line
    }
    /// Opens a connection and waits for its hello, so the daemon has counted
    /// it as a client: until it is dropped, the daemon cannot idle out.
    fn hold(&self) -> UnixStream {
        let stream = UnixStream::connect(self.socket().unwrap_or_else(|| panic!("socket")))
            .unwrap_or_else(|error| panic!("hold connect: {error:?}"));
        stream
            .set_read_timeout(Some(BOUND))
            .unwrap_or_else(|error| panic!("hold timeout: {error:?}"));
        let mut hello = String::new();
        BufReader::new(&stream)
            .read_line(&mut hello)
            .unwrap_or_else(|error| panic!("hold hello: {error:?}"));
        assert!(
            hello.contains("\"event\":\"hello\""),
            "hold hello: {hello:?}"
        );
        stream
    }
    fn exited(&mut self) -> bool {
        self.daemon
            .as_mut()
            .unwrap_or_else(|| panic!("daemon"))
            .try_wait()
            .unwrap_or_else(|error| panic!("try wait: {error:?}"))
            .is_some()
    }
    fn status(&self) -> String {
        self.event("{\"id\":\"s\",\"op\":\"status\"}\n")
    }
    fn quiet(&self) {
        let until = Instant::now() + BOUND;
        loop {
            let s = self.status();
            if s.contains("\"pending_scopes\":0")
                && s.contains("\"writer_busy\":false")
                && !s.contains("\"last_complete_backstop\":null")
            {
                break;
            }
            assert!(Instant::now() < until, "daemon did not become quiet: {s}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn roots(&self) -> Vec<String> {
        let catalog = ferret_catalog::Catalog::open(&self.path("index"))
            .unwrap_or_else(|error| panic!("roots catalog: {error}"))
            .unwrap_or_else(|| panic!("roots catalog absent"));
        catalog
            .load(&[ferret_catalog::Section::Roots])
            .unwrap_or_else(|error| panic!("roots load: {error}"));
        catalog
            .roots()
            .map(|(_, p)| {
                String::from_utf8(p.to_vec())
                    .unwrap_or_else(|error| panic!("fixture root UTF-8: {error}"))
            })
            .collect()
    }
    fn find_args(&self) -> Vec<String> {
        let mut args = vec!["find".to_owned()];
        args.extend(self.roots());
        args.extend(["-printf".to_owned(), "%y %p %s %T@\\0".to_owned()]);
        args
    }
    fn oracle(&self) -> (Vec<Vec<u8>>, Vec<Vec<u8>>, ContentAnswers) {
        let oracle = self.base.join("oracle");
        if oracle.exists() {
            fs::remove_dir_all(&oracle)
                .unwrap_or_else(|error| panic!("remove old oracle: {error:?}"));
        }
        let mut index_args = vec!["index".to_owned()];
        index_args.extend(self.roots());
        let output = self
            .command(&index_args.iter().map(String::as_str).collect::<Vec<_>>())
            .env("FERRET_NO_DAEMON", "1")
            .env("FERRET_INDEX", &oracle)
            .output()
            .unwrap_or_else(|error| panic!("oracle index: {error:?}"));
        self.success(output);
        let query = |args: &[&str]| {
            self.command(args)
                .env("FERRET_NO_DAEMON", "1")
                .env("FERRET_INDEX", &oracle)
                .output()
                .unwrap_or_else(|error| panic!("oracle query: {error:?}"))
        };
        let content = content_answers(|q| query(&["search", "--scan", q]));
        (
            records(query(&["search", "*"]), b'\n'),
            records(
                query(
                    &self
                        .find_args()
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                ),
                0,
            ),
            content,
        )
    }
    fn converges(&self) {
        let expected = self.oracle();
        let until = Instant::now() + BOUND;
        loop {
            let actual = (
                records(self.run(&["search", "*"]), b'\n'),
                records(
                    self.run(
                        &self
                            .find_args()
                            .iter()
                            .map(String::as_str)
                            .collect::<Vec<_>>(),
                    ),
                    0,
                ),
                content_answers(|q| self.run(&["search", "--scan", q])),
            );
            let s = self.status();
            if actual == expected
                && s.contains("\"pending_scopes\":0")
                && s.contains("\"writer_busy\":false")
                && number(&s, "uncovered") == 0
            {
                return;
            }
            assert!(
                Instant::now() < until,
                "oracle mismatch: {actual:?} vs {expected:?}; {s}; {}",
                fs::read_to_string(self.base.join("daemon.log")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn path(&self, name: &str) -> PathBuf {
        self.base.join(name)
    }
}

#[test]
fn idle_daemon_blocks_without_waking_its_threads() {
    let mut tree = Tree::new();
    tree.start(&[("FERRET_POLL_MS", "300000")]);
    std::thread::sleep(Duration::from_millis(250));
    let before = tree.voluntary_switches();
    std::thread::sleep(Duration::from_secs(2));
    let after = tree.voluntary_switches();
    let switches = after.saturating_sub(before);
    eprintln!("idle daemon voluntary context switches in 2 seconds: {switches}");
    assert!(
        switches <= 20,
        "idle daemon made {switches} voluntary context switches in 2 seconds"
    );
}
impl Drop for Tree {
    fn drop(&mut self) {
        self.stop();
        terminate_fixture_daemons(&self.base);
        assert!(
            fixture_daemons(&self.base).is_empty(),
            "fixture daemon survived Drop"
        );
        let _ = Command::new("chmod")
            .args(["-R", "u+rwx"])
            .arg(&self.base)
            .status();
        let _ = fs::remove_dir_all(&self.base);
    }
}
fn fixture_daemons(base: &Path) -> Vec<u32> {
    let prefix = base.as_os_str().as_encoded_bytes();
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pid = entry.file_name().to_string_lossy().parse::<u32>().ok()?;
            let command = fs::read(entry.path().join("cmdline")).ok()?;
            (command.starts_with(DAEMON.as_bytes())
                && command.windows(prefix.len()).any(|part| part == prefix))
            .then_some(pid)
        })
        .collect()
}
fn terminate_fixture_daemons(base: &Path) {
    for pid in fixture_daemons(base) {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
    let until = Instant::now() + Duration::from_secs(2);
    while !fixture_daemons(base).is_empty() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    for pid in fixture_daemons(base) {
        let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
    }
}
fn wait(mut predicate: impl FnMut() -> bool) {
    let until = Instant::now() + BOUND;
    while !predicate() {
        assert!(Instant::now() < until, "deadline exceeded");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn records(output: Output, delimiter: u8) -> Vec<Vec<u8>> {
    assert!(
        matches!(output.status.code(), Some(0 | 1)),
        "query failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut rows: Vec<_> = output
        .stdout
        .split(|&b| b == delimiter)
        .filter(|r| !r.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    rows.sort();
    rows
}
fn write(path: impl AsRef<Path>, value: &str) {
    fs::write(path, value).unwrap_or_else(|error| panic!("write: {error:?}"));
}

#[test]
fn ordinary_edits_and_atomic_save_match_a_fresh_index() {
    let mut tree = Tree::new();
    tree.start(&[]);
    // Modification alone changes no namespace. Compare stored size/mtime too,
    // so an unrelated create/rename cannot mask a missing MODIFY subscription.
    write(tree.path("src/left/original.txt"), "modified content alone");
    tree.converges();
    write(tree.path("src/created.txt"), "create");
    write(tree.path("src/left/original.txt"), "modified content");
    write(tree.path("src/temp-save"), "atomic save");
    fs::rename(
        tree.path("src/temp-save"),
        tree.path("src/left/original.txt"),
    )
    .unwrap_or_else(|error| panic!("atomic replace: {error:?}"));
    symlink("left/original.txt", tree.path("src/link"))
        .unwrap_or_else(|error| panic!("symlink: {error:?}"));
    tree.converges();
    fs::rename(tree.path("src/created.txt"), tree.path("src/renamed.txt"))
        .unwrap_or_else(|error| panic!("rename within: {error:?}"));
    fs::rename(
        tree.path("src/left/original.txt"),
        tree.path("src/right/moved.txt"),
    )
    .unwrap_or_else(|error| panic!("rename across: {error:?}"));
    tree.converges();
    fs::remove_file(tree.path("src/right/moved.txt"))
        .unwrap_or_else(|error| panic!("delete: {error:?}"));
    tree.converges();
}

#[test]
fn moved_in_directory_is_armed_while_it_is_still_populating() {
    let mut tree = Tree::new();
    tree.start(&[]);
    fs::create_dir_all(tree.path("outside/new/inner"))
        .unwrap_or_else(|error| panic!("outside directory: {error:?}"));
    fs::rename(tree.path("outside/new"), tree.path("src/arrived"))
        .unwrap_or_else(|error| panic!("move in: {error:?}"));
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for i in 0..80 {
                write(
                    tree.path(&format!("src/arrived/inner/{i}.txt")),
                    "populating",
                );
                std::thread::sleep(Duration::from_millis(3));
            }
        });
    });
    tree.converges();
    write(tree.path("src/arrived/inner/after-arm.txt"), "after");
    tree.converges();
}

#[test]
fn moved_out_directory_then_deleted_does_not_leave_old_children() {
    let mut tree = Tree::new();
    tree.start(&[]);
    fs::rename(tree.path("src/left"), tree.path("outside/gone"))
        .unwrap_or_else(|error| panic!("move out: {error:?}"));
    fs::remove_dir_all(tree.path("outside/gone"))
        .unwrap_or_else(|error| panic!("delete outside: {error:?}"));
    tree.converges();
}

#[test]
fn unpaired_and_wrong_cookies_only_request_disk_observation() {
    let mut tree = Tree::new();
    tree.start(&[]);
    write(tree.path("outside/incoming.txt"), "outside");
    fs::rename(
        tree.path("outside/incoming.txt"),
        tree.path("src/right/incoming.txt"),
    )
    .unwrap_or_else(|error| panic!("unpaired in: {error:?}"));
    fs::rename(
        tree.path("src/left/original.txt"),
        tree.path("outside/outgoing.txt"),
    )
    .unwrap_or_else(|error| panic!("unpaired out: {error:?}"));
    let left = tree.path("src/left");
    let right = tree.path("src/right");
    for (path, name, cookie, endpoint) in [
        (left, "original.txt", 17, "from"),
        (right, "incoming.txt", 93, "to"),
    ] {
        tree.event(&format!("{{\"id\":\"s\",\"op\":\"status\",\"capabilities\":[\"test-watch-move\"],\"args\":[\"{}\",\"{name}\",\"{cookie}\",\"{endpoint}\"]}}\n", path.display()));
    }
    tree.converges();
}

#[test]
fn kernel_overflow_and_new_loss_during_a_backstop_converge() {
    let mut tree = Tree::new();
    tree.start(&[("FERRET_WATCH_TEST_REFRESH_DELAY_MS", "100")]);
    write(tree.path("src/lost.txt"), "lost notification");
    let injection = "{\"id\":\"s\",\"op\":\"status\",\"capabilities\":[\"test-watch-overflow\"]}\n";
    let s = tree.event(injection);
    assert!(s.contains("Overflow"), "{s}");
    wait(|| tree.status().contains("\"writer_busy\":true"));
    write(tree.path("src/during.txt"), "new loss");
    tree.event(injection);
    tree.converges();
    assert!(!tree.status().contains("\"last_complete_backstop\":null"));
}

#[test]
fn userspace_bound_collapses_to_a_complete_backstop() {
    let mut tree = Tree::new();
    tree.start(&[
        ("FERRET_WATCH_TEST_SCOPES", "1"),
        ("FERRET_WATCH_TEST_REFRESH_DELAY_MS", "100"),
    ]);
    for i in 0..40 {
        write(tree.path(&format!("src/left/{i}.txt")), "burst");
    }
    wait(|| tree.status().contains("Overflow"));
    tree.converges();
}

#[test]
fn watch_cap_reports_uncovered_and_polling_preserves_the_tree() {
    let mut tree = Tree::new();
    tree.start(&[("FERRET_WATCH_CAP", "1"), ("FERRET_POLL_MS", "500")]);
    let s = tree.status();
    assert!(s.contains("\"watch_installed\":1"), "{s}");
    assert!(s.contains("\"watch_uncovered\":true"), "{s}");
    assert!(!s.contains("\"watch_failed\":0"), "{s}");
    write(tree.path("src/left/deep/unwatched.txt"), "polled");
    tree.converges();
    assert!(tree.run(&["search", "original.txt"]).status.success());
}

#[test]
fn a_daemon_started_before_any_index_serves_the_first_index() {
    let mut tree = Tree::new();
    fs::remove_dir_all(tree.path("index")).unwrap_or_else(|error| panic!("index: {error:?}"));
    tree.start(&[]);
    tree.success(tree.run(&["index", "src"]));
    assert!(tree.run(&["search", "original.txt"]).status.success());
}

#[test]
fn a_poll_that_outlasts_its_interval_is_spaced_out() {
    let mut tree = Tree::new();
    tree.start(&[
        ("FERRET_WATCH_CAP", "1"),
        ("FERRET_POLL_MS", "100"),
        ("FERRET_WATCH_TEST_REFRESH_DELAY_MS", "300"),
    ]);
    let before = number(&tree.status(), "refreshes");
    std::thread::sleep(Duration::from_secs(3));
    let status = tree.status();
    // Back to back, 3 s holds about ten of these refreshes. Spaced by four
    // times a refresh's duration, it holds at most three.
    let refreshes = number(&status, "refreshes") - before;
    assert!((1..=4).contains(&refreshes), "{refreshes}: {status}");
}

#[test]
fn repeated_polls_do_not_accumulate_watch_failures() {
    let mut tree = Tree::new();
    // Past 1,024 failures a root's are counted rather than kept by key.
    for i in 0..1100 {
        fs::create_dir_all(tree.path(&format!("src/many/{i}")))
            .unwrap_or_else(|error| panic!("fixture: {error:?}"));
    }
    tree.success(tree.local(&["index", "src"]));
    tree.start(&[("FERRET_WATCH_CAP", "1"), ("FERRET_POLL_MS", "100")]);
    let first = number(&tree.status(), "watch_failed");
    assert!(first > 0);
    let before = number(&tree.status(), "refreshes");
    wait(|| number(&tree.status(), "refreshes") >= before + 3);
    // Each poll observes the whole root, so it replaces the failures the
    // previous one recorded rather than adding to them.
    let status = tree.status();
    assert_eq!(number(&status, "watch_failed"), first, "{status}");
}

#[test]
fn polling_does_not_refresh_a_fully_covered_tree() {
    let mut tree = Tree::new();
    tree.start(&[("FERRET_POLL_MS", "500")]);
    let status = tree.status();
    let last = status
        .split("\"last_successful_refresh\":")
        .nth(1)
        .and_then(|tail| tail.split([',', '}']).next())
        .unwrap()
        .to_owned();
    std::thread::sleep(Duration::from_millis(2200));
    let after = tree.status();
    let last_after = after
        .split("\"last_successful_refresh\":")
        .nth(1)
        .and_then(|tail| tail.split([',', '}']).next())
        .unwrap();
    assert_eq!(
        last, last_after,
        "covered roots were refreshed by polling: {after}"
    );
}

#[test]
fn denied_directory_is_opaque_and_permission_recovery_is_polled() {
    let mut tree = Tree::new();
    tree.start(&[("FERRET_POLL_MS", "500")]);
    fs::set_permissions(tree.path("src/left"), fs::Permissions::from_mode(0o000))
        .unwrap_or_else(|error| panic!("deny: {error:?}"));
    tree.converges();
    assert_eq!(tree.run(&["search", "original.txt"]).status.code(), Some(1));
    fs::set_permissions(tree.path("src/left"), fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|error| panic!("restore: {error:?}"));
    tree.converges();
    assert!(tree.run(&["search", "original.txt"]).status.success());
    // A denied configured root still has its root and global rule parents
    // watched. Its failed installation must still be reported and recovered
    // by the polling timer.
    fs::set_permissions(tree.path("src"), fs::Permissions::from_mode(0o000))
        .unwrap_or_else(|error| panic!("deny root: {error:?}"));
    tree.stop();
    tree.start(&[("FERRET_POLL_MS", "500")]);
    let denied_root = tree.status();
    assert!(
        denied_root.contains("\"watch_installed\":2"),
        "{denied_root}"
    );
    assert!(denied_root.contains("\"watch_failed\":1"), "{denied_root}");
    assert!(
        denied_root.contains("\"watch_uncovered\":true"),
        "{denied_root}"
    );
    fs::set_permissions(tree.path("src"), fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|error| panic!("restore root: {error:?}"));
    tree.converges();
}

#[test]
fn a_denied_watch_is_reported_separately_from_opaque_crawl_state() {
    let mut tree = Tree::new();
    fs::create_dir(tree.path("src/denied"))
        .unwrap_or_else(|error| panic!("denied directory: {error:?}"));
    write(tree.path("src/denied/secret.txt"), "secret");
    fs::set_permissions(tree.path("src/denied"), fs::Permissions::from_mode(0o000))
        .unwrap_or_else(|error| panic!("deny: {error:?}"));
    tree.start(&[]);
    let status = tree.status();
    assert!(!status.contains("\"watch_failed\":0"), "{status}");
    assert!(status.contains("\"watch_uncovered\":true"), "{status}");
    assert_eq!(tree.run(&["search", "secret.txt"]).status.code(), Some(1));
    fs::set_permissions(tree.path("src/denied"), fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|error| panic!("restore: {error:?}"));
    tree.converges();
    let recovered = tree.status();
    assert!(recovered.contains("\"watch_failed\":0"), "{recovered}");
    assert!(
        recovered.contains("\"watch_uncovered\":false"),
        "{recovered}"
    );
}

#[test]
fn ignore_read_denial_retains_old_children_until_recovery() {
    let mut tree = Tree::new();
    write(tree.path("src/left/.ferretignore"), "# readable rules");
    tree.success(tree.local(&["index", "src"]));
    tree.start(&[("FERRET_POLL_MS", "500")]);
    fs::set_permissions(
        tree.path("src/left/.ferretignore"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap_or_else(|error| panic!("deny rules: {error:?}"));
    wait(|| tree.status().contains("\"fault_retained\":true"));
    assert!(tree.run(&["search", "original.txt"]).status.success());
    fs::set_permissions(
        tree.path("src/left/.ferretignore"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap_or_else(|error| panic!("restore rules: {error:?}"));
    tree.converges();
    assert!(tree.status().contains("\"fault_retained\":false"));
}

#[test]
fn explicit_index_and_root_commands_publish_before_reply_with_one_lock() {
    let mut tree = Tree::new();
    tree.start(&[]);
    write(tree.path("src/explicit.txt"), "explicit");
    let remote = tree.success(tree.run(&["index", "src"]));
    assert!(String::from_utf8_lossy(&remote.stdout).contains("indexed "));
    assert!(tree.run(&["search", "explicit.txt"]).status.success());
    write(tree.path("outside/root.txt"), "new root");
    let added = tree.success(tree.run(&["index", "outside"]));
    assert!(String::from_utf8_lossy(&added.stdout).contains("indexed "));
    assert!(tree.run(&["search", "root.txt"]).status.success());
    tree.success(tree.run(&["roots", "remove", "outside"]));
    assert_eq!(tree.run(&["search", "root.txt"]).status.code(), Some(1));
    assert!(tree.status().contains("\"engine_opens\":1"));
    tree.converges();
    // No daemon means the same unchanged producer/report, byte for byte.
    let remote = tree.success(tree.run(&["index", "src"]));
    tree.stop();
    let local = tree.success(tree.local(&["index", "src"]));
    assert_eq!(remote.stdout, local.stdout);
}

#[test]
fn queued_writer_commands_never_consume_read_query_permits() {
    let mut tree = Tree::new();
    tree.start(&[("FERRET_WRITER_TEST_COMMAND_DELAY_MS", "1000")]);
    let mut readers = Vec::new();
    for i in 0..4 {
        let stream = UnixStream::connect(tree.socket().unwrap_or_else(|| panic!("socket")))
            .unwrap_or_else(|error| panic!("connect: {error:?}"));
        stream
            .set_read_timeout(Some(BOUND))
            .unwrap_or_else(|error| panic!("timeout: {error:?}"));
        let mut reader = BufReader::new(stream);
        let mut hello = String::new();
        reader
            .read_line(&mut hello)
            .unwrap_or_else(|error| panic!("hello: {error:?}"));
        assert!(hello.contains("\"state\":\"ready\""));
        let request = format!(
            "{{\"id\":\"w{i}\",\"op\":\"index\",\"args\":[\"\",\"{}\"]}}\n",
            tree.path("src").display()
        );
        reader
            .get_mut()
            .write_all(request.as_bytes())
            .unwrap_or_else(|error| panic!("writer request: {error:?}"));
        readers.push(reader);
    }
    wait(|| tree.status().contains("\"writer_commands\":4"));
    tree.success(tree.run(&["search", "original.txt"]));
    assert!(
        tree.status().contains("\"writer_commands\":4"),
        "a read query waited for a writer command to finish"
    );
    for mut reader in readers {
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .unwrap_or_else(|error| panic!("reply: {error:?}"));
            assert!(!line.is_empty());
            if line.contains("\"event\":\"end\"") {
                assert!(line.contains("\"exit\":0"), "{line}");
                break;
            }
        }
    }
}

#[test]
fn no_daemon_writer_fails_promptly_with_owner_advice() {
    let mut tree = Tree::new();
    tree.start(&[]);
    let started = Instant::now();
    let output = tree.local(&["index", "src"]);
    assert_eq!(output.status.code(), Some(3));
    assert!(started.elapsed() < Duration::from_secs(2));
    let advice = String::from_utf8_lossy(&output.stderr);
    assert!(
        advice.contains("ferretd") && advice.contains("status --json"),
        "{advice}"
    );
    assert!(tree.run(&["search", "original.txt"]).status.success());
}

#[test]
fn restart_after_kill_mid_burst_rearms_and_backstops() {
    let mut tree = Tree::new();
    tree.start(&[("FERRET_WATCH_TEST_REFRESH_DELAY_MS", "100")]);
    write(tree.path("src/before-kill.txt"), "kill");
    wait(|| tree.status().contains("\"writer_busy\":true"));
    tree.stop();
    write(tree.path("src/after-kill.txt"), "restart");
    tree.start(&[]);
    tree.converges();
    write(
        tree.path("src/left/deep/rearmed.txt"),
        "watch after restart",
    );
    tree.converges();
}

#[test]
fn hourly_backstop_observes_an_unwatched_tree() {
    let mut tree = Tree::new();
    tree.start(&[("FERRET_WATCH_CAP", "0"), ("FERRET_BACKSTOP_MS", "350")]);
    write(
        tree.path("src/left/deep/hourly.txt"),
        "only the timer can observe this",
    );
    tree.converges();
}

/// Idle exit is driven by events, not sleeps: a held connection keeps the
/// daemon from idling out while the test looks, and each check is anchored to
/// a refresh the test saw begin. The only sleep is the negative check, which
/// spans many idle periods inside that refresh.
#[test]
fn idle_exit_waits_for_publication_and_restart_backstops_debounced_hints() {
    let env = [
        ("FERRET_DAEMON_IDLE_MS", "20"),
        // Loading counts as busy, so the first held connection lands before
        // idle can expire even if this thread stalls after the bind probe.
        ("FERRET_DAEMON_LOAD_DELAY_MS", "500"),
        ("FERRET_WATCH_TEST_REFRESH_DELAY_MS", "2000"),
    ];
    let mut tree = Tree::new();
    tree.spawn(&env);
    let hold = tree.hold();
    let refreshing = |s: &str| {
        s.contains("\"current_operation\":\"refresh\"") && s.contains("\"writer_busy\":true")
    };
    let mut seen = tree.status();
    let until = Instant::now() + BOUND;
    while !refreshing(&seen) {
        assert!(Instant::now() < until, "refresh never began: {seen}");
        std::thread::sleep(Duration::from_millis(10));
        seen = tree.status();
    }
    drop(hold);
    std::thread::sleep(Duration::from_millis(250));
    assert!(!tree.exited(), "daemon idled out during a refresh");
    let after = tree.status();
    assert!(
        refreshing(&after) && number(&after, "refreshes") == number(&seen, "refreshes"),
        "the refresh ended before the check, so the check proved nothing; \
         raise the refresh delay: {after}"
    );
    // Debounced hints are not busy work: the daemon may exit with one pending,
    // and the next start's backstop must pick it up. Whether the debounce
    // (200 ms) fires before the idle exit (20 ms) is a race the product
    // allows, so retry until an exit leaves the hint unpublished.
    for attempt in 0.. {
        let hold = tree.hold();
        tree.quiet();
        drop(hold);
        let name = format!("pending-{attempt}.txt");
        write(tree.path(&format!("src/left/deep/{name}")), "pending");
        let until = Instant::now() + BOUND;
        while !tree.exited() {
            assert!(Instant::now() < until, "daemon did not idle out");
            std::thread::sleep(Duration::from_millis(10));
        }
        tree.stop();
        let published = records(tree.local(&["search", "*"]), b'\n')
            .iter()
            .any(|path| path.ends_with(name.as_bytes()));
        if !published {
            break;
        }
        assert!(attempt < 5, "every idle exit waited for the debounced hint");
        tree.spawn(&env);
    }
    tree.start(&[]);
    tree.converges();
}

fn generated(rounds: usize) {
    let mut tree = Tree::new();
    tree.start(&[]);
    let mut seed = 0x5eed_1a71_u64;
    for round in 0..rounds {
        for step in 0..12 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let a = tree.path(&format!("src/left/file-{}.txt", seed % 9));
            let b = tree.path(&format!("src/right/file-{}.txt", seed % 9));
            match seed % 4 {
                0 => write(&a, &format!("{round}-{step}")),
                1 => {
                    let _ = fs::remove_file(&a);
                    let _ = fs::remove_file(&b);
                }
                2 => {
                    if a.exists() {
                        fs::rename(a, b)
                            .unwrap_or_else(|error| panic!("generated rename: {error:?}"));
                    }
                }
                _ => {
                    let from = tree.path("src/left/deep");
                    let to = tree.path("src/right/deep");
                    if from.exists() {
                        fs::rename(from, to).unwrap_or_else(|error| panic!("move dir: {error:?}"));
                    } else {
                        fs::rename(to, from)
                            .unwrap_or_else(|error| panic!("move dir back: {error:?}"));
                    }
                }
            }
        }
        tree.converges();
    }
}
#[test]
fn generated_bursts_match_full_index_after_each_quiescent_publication() {
    generated(5);
}
/// FERRET_WATCH_BURST_ROUNDS=1000 cargo test -p ferret --test watch --
/// --ignored generated_bursts_long. Each round compares against a new full real
/// index.
#[test]
#[ignore]
fn generated_bursts_long() {
    generated(
        std::env::var("FERRET_WATCH_BURST_ROUNDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1000),
    );
}

fn number(json: &str, key: &str) -> u64 {
    json.split(&format!("\"{key}\":"))
        .nth(1)
        .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("missing number {key}: {json}"))
}

#[test]
fn linked_worktree_commondir_changes_are_watched() {
    let mut tree = Tree::new();
    for dir in [
        "outside/gitdir",
        "outside/common-a/info",
        "outside/common-b/info",
        "outside/policy-a",
        "outside/policy-b",
    ] {
        fs::create_dir_all(tree.path(dir)).unwrap();
    }
    std::os::unix::fs::symlink(
        tree.path("outside/policy-a"),
        tree.path("outside/policy-link"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        tree.path("outside/policy-link/exclude"),
        tree.path("outside/common-a/info/exclude"),
    )
    .unwrap();
    write(
        tree.path("src/.git"),
        &format!("gitdir: {}\n", tree.path("outside/gitdir").display()),
    );
    write(tree.path("outside/gitdir/commondir"), "../common-a\n");
    write(tree.path("outside/policy-a/exclude"), "");
    write(tree.path("outside/policy-b/exclude"), "original.txt\n");
    write(tree.path("outside/common-b/info/exclude"), "original.txt\n");
    tree.success(tree.local(&["index"]));
    tree.start(&[]);
    let before_info = tree.status();
    write(tree.path("outside/policy-a/exclude"), "original.txt\n");
    tree.converges();
    assert_eq!(tree.run(&["search", "original.txt"]).status.code(), Some(1));
    let after_info = tree.status();
    assert!(number(&after_info, "refreshes") > number(&before_info, "refreshes"));
    assert!(after_info.contains("\"last_refresh_reason\":\"Burst\""));
    write(tree.path("outside/policy-a/exclude"), "");
    tree.converges();
    fs::remove_file(tree.path("outside/policy-link")).unwrap();
    std::os::unix::fs::symlink(
        tree.path("outside/policy-b"),
        tree.path("outside/policy-link"),
    )
    .unwrap();
    tree.converges();
    assert_eq!(tree.run(&["search", "original.txt"]).status.code(), Some(1));
    fs::remove_file(tree.path("outside/policy-link")).unwrap();
    std::os::unix::fs::symlink(
        tree.path("outside/policy-a"),
        tree.path("outside/policy-link"),
    )
    .unwrap();
    tree.converges();
    let before = tree.status();
    write(tree.path("outside/gitdir/commondir"), "../common-b\n");
    tree.converges();
    assert_eq!(tree.run(&["search", "original.txt"]).status.code(), Some(1));
    let after = tree.status();
    assert!(number(&after, "refreshes") > number(&before, "refreshes"));
    assert!(
        after.contains("\"last_refresh_reason\":\"Burst\""),
        "{after}"
    );
}

#[test]
fn ignored_name_churn_preserves_the_full_raw_count_census() {
    let mut tree = Tree::new();
    write(tree.path("src/.ferretignore"), "*.ignored\n");
    tree.success(tree.local(&["index"]));
    tree.start(&[]);
    for i in 0..25 {
        write(tree.path(&format!("src/left/{i}.ignored")), "ignored");
    }
    for i in 0..12 {
        fs::remove_file(tree.path(&format!("src/left/{i}.ignored"))).unwrap();
    }
    tree.converges();
    let live = String::from_utf8(tree.success(tree.run(&["stats", "--json"])).stdout).unwrap();
    let expected = String::from_utf8(
        tree.success(
            tree.command(&["stats", "--json"])
                .env("FERRET_NO_DAEMON", "1")
                .env("FERRET_INDEX", tree.path("oracle"))
                .output()
                .unwrap(),
        )
        .stdout,
    )
    .unwrap();
    for key in ["raw_entries", "unknown_entry_counts", "names", "ignored"] {
        assert_eq!(
            number(&live, key),
            number(&expected, key),
            "{key}: {live} vs {expected}"
        );
    }
}

#[test]
fn json_status_and_stats_have_typed_fields_and_live_values() {
    let mut tree = Tree::new();
    let local = tree.success(tree.local(&["status", "--json"]));
    assert!(String::from_utf8_lossy(&local.stdout).contains("\"host_running\":false"));
    tree.start(&[]);
    let before = tree.status();
    write(tree.path("src/left/new.txt"), "new");
    tree.converges();
    let after = tree.status();
    assert!(number(&after, "refreshes") > number(&before, "refreshes"));
    assert!(
        number(&after, "last_successful_refresh") >= number(&before, "last_successful_refresh")
    );
    let output = tree.success(tree.run(&["stats", "--json"]));
    let mut check = fixture::bounded_command("python3", &tree.base);
    check.args(["-c", r#"
import json, sys
docs = [json.loads(line) for line in sys.stdin]
local, s = docs
assert local['host_running'] is False
assert isinstance(local['generation'], dict)
assert local['last_successful_refresh'] is None
assert local['last_complete_backstop'] is None
assert local['watch_uncovered'] is None
for key in ['current_operation', 'protected_scopes', 'watch_installed', 'watch_needed', 'watch_failed', 'oldest_pending_ms', 'pending_scopes', 'pending_bytes', 'backstop_reason', 'writer_input_budget', 'writer_log_budget', 'current_rss_kb', 'peak_rss_kb', 'pinned_internal_epochs']:
    assert key in local, (key, local)
for key in ['generation', 'writer_input_budget', 'writer_log_budget', 'census', 'd54']:
    assert isinstance(s[key], dict), (key, s)
for key in ['watch_installed', 'watch_needed', 'watch_failed', 'pending_scopes', 'pending_bytes', 'protected_scopes', 'opaque_directories', 'current_rss_kb', 'peak_rss_kb', 'last_successful_refresh', 'last_complete_backstop']:
    assert type(s[key]) is int, (key, s)
assert isinstance(s['current_operation'], str)
assert s['oldest_pending_ms'] is None or type(s['oldest_pending_ms']) is int
assert s['backstop_reason'] is None or isinstance(s['backstop_reason'], str)
assert type(s['host_running']) is bool and s['host_running']
assert type(s['watch_uncovered']) is bool
assert s['refresh_error'] is None or isinstance(s['refresh_error'], str)
assert isinstance(s['polling_roots'], list)
assert isinstance(s['writer_input_usage'], dict)
assert isinstance(s['pinned_internal_epochs'], list) and s['pinned_internal_epochs']
assert all(type(n) is int for n in s['pinned_internal_epochs'])
assert isinstance(s['census']['extensions'], list)
assert s['d54']['scope_walk_plans'] + s['d54']['postings_plans'] > 0
"#]).stdin(Stdio::piped());
    let mut child = check.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&[local.stdout, output.stdout].concat())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
fn nested_roots_and_cross_root_hard_links_use_all_checked_occurrences() {
    let mut tree = Tree::new();
    fs::hard_link(
        tree.path("src/left/original.txt"),
        tree.path("outside/linked.txt"),
    )
    .unwrap();
    tree.success(tree.local(&["index", "src/left", "outside"]));
    tree.start(&[]);
    write(
        tree.path("outside/linked.txt"),
        "changed through kept root alias",
    );
    tree.converges();
    let after = tree.status();
    assert!(
        after.contains("\"last_refresh_reason\":\"Burst\""),
        "{after}"
    );
    assert!(after.contains("\"watch_uncovered\":false"), "{after}");
    write(
        tree.path("src/left/original.txt"),
        "changed through nested root",
    );
    tree.converges();
    fs::remove_file(tree.path("src/left/original.txt")).unwrap();
    tree.converges();
}

#[test]
fn bind_aliases_receive_edits_through_every_occurrence() {
    const CHILD: &str = "FERRET_TEST_BIND_NAMESPACE";
    if std::env::var_os(CHILD).is_none() {
        let tree = Tree::new();
        let available = fixture::bounded_command("unshare", &tree.base)
            .args(["-rm", "true"])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !available {
            // Same kernel descriptor fanout is also injected in intake tests.
            eprintln!(
                "unshare -rm unavailable; real bind fixture skipped, intake occurrence seam covered"
            );
            return;
        }
        let output = fixture::bounded_command("unshare", &tree.base)
            .args(["-rm", "--"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "bind_aliases_receive_edits_through_every_occurrence",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let mut tree = Tree::new();
    fs::create_dir_all(tree.path("src/bound")).unwrap();
    let mounted = fixture::bounded_command("mount", &tree.base)
        .arg("--bind")
        .arg(tree.path("src/left"))
        .arg(tree.path("src/bound"))
        .status()
        .unwrap();
    assert!(mounted.success());
    tree.success(tree.local(&["index"]));
    tree.start(&[]);
    write(tree.path("src/bound/original.txt"), "through bind alias");
    tree.converges();
    let after = tree.status();
    assert!(
        after.contains("\"last_refresh_reason\":\"Burst\""),
        "{after}"
    );
    assert!(after.contains("\"watch_uncovered\":false"), "{after}");
    write(
        tree.path("src/left/deep/new.txt"),
        "through original occurrence",
    );
    tree.converges();
    tree.stop();
    assert!(
        fixture::bounded_command("umount", &tree.base)
            .arg(tree.path("src/bound"))
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn unwatchable_external_policy_marks_its_root_for_polling_and_recovers() {
    let mut tree = Tree::new();
    fs::create_dir_all(tree.path("outside/private/gitdir/info")).unwrap();
    write(
        tree.path("src/.git"),
        &format!(
            "gitdir: {}\n",
            tree.path("outside/private/gitdir").display()
        ),
    );
    write(tree.path("outside/private/gitdir/info/exclude"), "");
    tree.success(tree.local(&["index"]));
    fs::set_permissions(
        tree.path("outside/private"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    tree.spawn(&[("FERRET_POLL_MS", "100")]);
    let until = Instant::now() + BOUND;
    loop {
        let status = tree.status();
        if status.contains("\"fault_retained\":true") {
            break;
        }
        assert!(
            Instant::now() < until,
            "policy denial did not retain: {status}; {}",
            fs::read_to_string(tree.path("daemon.log")).unwrap()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let denied = tree.status();
    assert!(denied.contains("\"watch_uncovered\":true"), "{denied}");
    assert!(number(&denied, "watch_failed") > 0, "{denied}");
    fs::set_permissions(
        tree.path("outside/private"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    write(
        tree.path("outside/private/gitdir/info/exclude"),
        "original.txt\n",
    );
    tree.converges();
    wait(|| tree.status().contains("\"watch_uncovered\":false"));
}

#[test]
fn an_unobserved_hard_link_is_polling_dependent_until_all_occurrences_are_known() {
    let mut tree = Tree::new();
    fs::hard_link(
        tree.path("src/left/original.txt"),
        tree.path("outside/unobserved.txt"),
    )
    .unwrap();
    tree.success(tree.local(&["index"]));
    tree.start(&[("FERRET_POLL_MS", "100")]);
    let before = tree.status();
    assert!(!before.contains("\"polling_roots\":[]"), "{before}");
    write(
        tree.path("outside/unobserved.txt"),
        "changed outside watched parents",
    );
    tree.converges();
    tree.success(tree.run(&["index", "outside"]));
    wait(|| tree.status().contains("\"polling_roots\":[]"));
    // A rename cannot leave an old physical name in the completeness proof.
    fs::rename(
        tree.path("outside/unobserved.txt"),
        tree.path("outside/moved.txt"),
    )
    .unwrap();
    tree.converges();
    let after = tree.status();
    assert!(after.contains("\"polling_roots\":[]"), "{after}");
}

#[test]
fn global_ferret_rules_wait_for_protected_scope_recovery() {
    let mut tree = Tree::new();
    write(tree.path("src/left/.ferretignore"), "");
    tree.success(tree.local(&["index"]));
    tree.start(&[("FERRET_POLL_MS", "100")]);
    fs::set_permissions(
        tree.path("src/left/.ferretignore"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    wait(|| tree.status().contains("\"fault_retained\":true"));
    let before = tree.status();
    let output = tree.success(tree.run(&["search", "original.txt"]));
    write(
        tree.path("home/config/ferret/ignore"),
        ".git/\noriginal.txt\n",
    );
    wait(|| tree.status().contains("\"refresh_error\":\""));
    let blocked = tree.status();
    assert_eq!(number(&blocked, "sequence"), number(&before, "sequence"));
    assert!(blocked.contains("\"fault_retained\":true"), "{blocked}");
    assert_eq!(
        tree.success(tree.run(&["search", "original.txt"])).stdout,
        output.stdout
    );
    fs::set_permissions(
        tree.path("src/left/.ferretignore"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    tree.converges();
    assert_eq!(tree.run(&["search", "original.txt"]).status.code(), Some(1));
    let recovered = tree.status();
    assert!(recovered.contains("\"refresh_error\":null"), "{recovered}");
    assert!(recovered.contains("\"protected_scopes\":0"), "{recovered}");
}

#[test]
fn a_slow_query_socket_keeps_its_old_epoch_without_blocking_publication() {
    let mut tree = Tree::new();
    let suffix = "x".repeat(180);
    for n in 0..3000 {
        write(tree.path(&format!("src/left/{n:04}-{suffix}.txt")), "small");
    }
    tree.success(tree.local(&["index", "src"]));
    tree.start(&[]);
    let before = tree.status();
    let old_epoch = number(&before, "checkpoint");
    let stream = UnixStream::connect(tree.socket().unwrap_or_else(|| panic!("socket")))
        .unwrap_or_else(|e| panic!("slow client: {e}"));
    stream
        .set_read_timeout(Some(BOUND))
        .unwrap_or_else(|e| panic!("timeout: {e}"));
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .unwrap_or_else(|e| panic!("hello: {e}"));
    reader
        .get_mut()
        .write_all(b"{\"id\":\"slow\",\"op\":\"search\",\"args\":[\"*.txt\"]}\n")
        .unwrap_or_else(|e| panic!("query: {e}"));
    line.clear();
    reader
        .read_line(&mut line)
        .unwrap_or_else(|e| panic!("begin: {e}"));
    assert!(line.contains("begin"), "{line}");
    line.clear();
    reader
        .read_line(&mut line)
        .unwrap_or_else(|e| panic!("row: {e}"));
    assert!(line.contains("row"), "{line}");
    // Leave more than a socket buffer of output unread. This pins the query's
    // epoch while a serial writer command must still publish its successor.
    write(
        tree.path("src/left/original.txt"),
        "changed while output is blocked",
    );
    for n in 0..40 {
        write(
            tree.path(&format!("src/left/{n:04}-{suffix}.txt")),
            "changed",
        );
    }
    tree.success(tree.run(&["index", "src"]));
    let after = tree.status();
    assert_ne!(number(&after, "checkpoint"), old_epoch, "{after}");
    let epochs = after
        .split("\"pinned_internal_epochs\":")
        .nth(1)
        .unwrap_or_else(|| panic!("epochs: {after}"));
    let epochs = epochs
        .split(']')
        .next()
        .unwrap_or_else(|| panic!("epochs: {after}"));
    assert!(
        epochs.contains(&old_epoch.to_string()),
        "old query pin was lost: {after}"
    );
    tree.success(tree.run(&["search", "original.txt"]));
    drop(reader);
}

fn content_answers(mut query: impl FnMut(&str) -> Output) -> ContentAnswers {
    CONTENT
        .iter()
        .map(|q| {
            let output = query(q);
            let mut rows: Vec<_> = output
                .stdout
                .split(|&b| b == b'\n')
                .filter(|r| !r.is_empty())
                .map(<[u8]>::to_vec)
                .collect();
            rows.sort();
            (rows, output.stderr, output.status.code())
        })
        .collect()
}

fn gate(tree: &Tree, phase: &str) -> PathBuf {
    let path = tree.path("content-gate");
    fs::create_dir_all(&path).unwrap_or_else(|e| panic!("gate: {e}"));
    fs::write(path.join(phase), b"hold").unwrap_or_else(|e| panic!("gate: {e}"));
    path
}
/// Burst files an uncovered follow must still be short of finishing.
///
/// The background gate parks on the 10th cancellation check of one turn: one
/// check before `follow`, then one per document. Releasing it publishes those
/// documents, about eight (a stubbed command follow left 9,995 of 10,002
/// uncovered). The explicit-follow gate parks on its own 10th per-document
/// check, so at least ten documents have to remain ahead of that frontier.
/// 24 burst files plus `original.txt` is 25 live documents: past that floor
/// of 18, and still uncovered if the command replies before following. The
/// daemon's startup walk of this tree stays inside the suite's 8s bound.
const BURST_FILES: usize = 24;
fn unfollowed_burst(tree: &Tree) {
    for i in 0..BURST_FILES {
        write(
            tree.path(&format!("src/burst-{i}.txt")),
            &format!("needle unique{i} words"),
        );
    }
    tree.success(tree.local(&["index", "src"]));
    fs::remove_dir_all(tree.path("index/index")).unwrap_or_else(|e| panic!("remove content: {e}"));
}
fn drain_content(tree: &mut Tree) {
    let _ = tree.event("{\"op\":\"drain\"}\n");
    let until = Instant::now() + BOUND;
    loop {
        let done = tree
            .daemon
            .as_mut()
            .unwrap_or_else(|| panic!("daemon"))
            .try_wait()
            .unwrap_or_else(|e| panic!("wait: {e}"));
        if let Some(status) = done {
            assert!(status.success(), "drain: {status}");
            break;
        }
        assert!(Instant::now() < until, "drain exceeded bound");
        std::thread::sleep(Duration::from_millis(5));
    }
    tree.daemon.take();
}
fn manifest_equals_clean(tree: &Tree) {
    let expected = tree.oracle().2;
    let actual = content_answers(|q| tree.local(&["search", "--scan-uncovered", q]));
    assert_eq!(actual, expected);
}
/// Threads exit between `readdir` and the `stat` read. A missing row is a
/// thread that is already gone, so the sample skips it.
fn task_rows(pid: u32) -> Vec<(String, String, i32)> {
    let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    tasks
        .flatten()
        .filter_map(|entry| {
            let tid = entry.file_name().to_string_lossy().into_owned();
            let comm = fs::read_to_string(entry.path().join("comm")).ok()?;
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            let nice = stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().nth(16))
                .and_then(|field| field.parse().ok())?;
            Some((tid, comm.trim().to_owned(), nice))
        })
        .collect()
}

#[test]
fn queued_command_covers_content_while_background_follow_is_held() {
    let mut tree = Tree::new();
    unfollowed_burst(&tree);
    let gate = gate(&tree, "follow");
    let gate_text = gate.to_str().unwrap_or_else(|| panic!("gate path"));
    tree.spawn(&[
        ("FERRET_CONTENT_TEST_GATE", gate_text),
        ("FERRET_CRAWL_TEST_GATE", gate_text),
        ("FERRET_INDEX_WORKERS", "4"),
    ]);
    wait(|| gate.join("follow.reached").exists());
    write(tree.path("src/urgent.txt"), "urgent");
    let pid = tree
        .daemon
        .as_ref()
        .unwrap_or_else(|| panic!("daemon"))
        .id();
    let before = task_rows(pid);
    let before_tids: std::collections::HashSet<_> =
        before.iter().map(|(tid, _, _)| tid.clone()).collect();
    let command_nice = before
        .iter()
        .find(|(_, comm, _)| comm == "ferret-command")
        .map(|(_, _, nice)| *nice)
        .unwrap_or_else(|| panic!("command thread missing: {before:?}"));
    let lowered: Vec<_> = before
        .iter()
        .filter(|(_, _, nice)| *nice == 19)
        .map(|(tid, _, _)| tid.clone())
        .collect();
    assert!(
        !lowered.is_empty(),
        "writer was not at nice 19 before the command: {before:?}"
    );
    // Hold the command's crawl workers once each has made its priority
    // decision, and read their nice while they are held: a worker that
    // lowered itself cannot be sampled before it did so.
    let worker_gate = gate.join("worker");
    write(&worker_gate, "held");
    let (output, seen) = std::thread::scope(|scope| {
        let index = scope.spawn(|| tree.run(&["index", "src"]));
        // Only an explicit command's crawl stops at this gate. Read the
        // markers before the task list: a worker that reaches the gate after
        // the snapshot is not in it.
        let reached = || -> Vec<String> {
            std::fs::read_dir(&gate)
                .unwrap_or_else(|error| panic!("read gate: {error}"))
                .filter_map(|entry| {
                    let name = entry.ok()?.file_name().into_string().ok()?;
                    let tid = name.strip_prefix("worker.")?.strip_suffix(".reached")?;
                    Some(tid.to_owned())
                })
                .collect()
        };
        wait(|| !reached().is_empty());
        let tids = reached();
        let rows = task_rows(pid);
        let seen: Vec<_> = tids
            .into_iter()
            .map(|tid| {
                let (_, comm, nice) = rows
                    .iter()
                    .find(|(row, _, _)| *row == tid)
                    .unwrap_or_else(|| panic!("held worker {tid} missing: {rows:?}"));
                (tid, (comm.clone(), *nice))
            })
            .collect();
        for (tid, _) in &seen {
            assert!(
                !before_tids.contains(tid),
                "held worker {tid} predates the command"
            );
        }
        std::fs::remove_file(&worker_gate).unwrap_or_else(|error| panic!("release: {error}"));
        let output = index
            .join()
            .unwrap_or_else(|_| panic!("index thread panicked"));
        (output, seen)
    });
    tree.success(output);
    assert!(
        gate.join("follow").exists(),
        "command follow waited in the background content gate"
    );
    assert_eq!(
        number(&tree.status(), "uncovered"),
        0,
        "index returned with content still uncovered: {}",
        tree.status()
    );
    let last = BURST_FILES - 1;
    let search = tree.success(tree.run(&["search", &format!("text:unique{last}")]));
    let stdout = String::from_utf8_lossy(&search.stdout);
    assert!(
        stdout.contains(&format!("burst-{last}.txt")),
        "indexed content missing from search: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&search.stderr);
    assert!(
        !stderr.contains("does not yet cover"),
        "search scanned or refused: {stderr}"
    );
    assert!(
        !seen.is_empty(),
        "command crawl spawned no worker; nice inheritance was not observed"
    );
    for (tid, (comm, nice)) in &seen {
        assert_eq!(
            *nice, command_nice,
            "worker {tid} ({comm}) nice {nice}, command thread {command_nice}"
        );
        assert_ne!(*nice, 19, "worker {tid} ({comm}) ran at background nice");
    }
    let after = task_rows(pid);
    for tid in &lowered {
        if let Some((_, comm, nice)) = after.iter().find(|(id, _, _)| id == tid) {
            assert_eq!(*nice, 19, "background thread {tid} ({comm}) left nice 19");
        }
    }
    fs::remove_file(gate.join("follow")).unwrap_or_else(|e| panic!("release: {e}"));
    tree.converges();
}
#[test]
fn drain_during_explicit_follow_exits_incomplete() {
    let mut tree = Tree::new();
    unfollowed_burst(&tree);
    // The background gate keeps documents uncovered until the command is
    // queued. The explicit gate then holds that command's own follow, so
    // drain overlaps it instead of landing after it has already finished.
    let gate = gate(&tree, "follow");
    fs::write(gate.join("explicit"), b"hold").unwrap_or_else(|e| panic!("gate: {e}"));
    let gate_text = gate.to_str().unwrap_or_else(|| panic!("gate path"));
    tree.spawn(&[("FERRET_CONTENT_TEST_GATE", gate_text)]);
    wait(|| gate.join("follow.reached").exists());
    let output = std::thread::scope(|scope| {
        let index = scope.spawn(|| tree.run(&["index", "src"]));
        wait(|| gate.join("explicit.reached").exists());
        let _ = tree.event("{\"op\":\"drain\"}\n");
        index
            .join()
            .unwrap_or_else(|_| panic!("index thread panicked"))
    });
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout {:?}\nstderr {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "content error printed a report: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "ferret: content index: interrupted (daemon stopping); run ferret index again\n"
    );
    let until = Instant::now() + BOUND;
    loop {
        let done = tree
            .daemon
            .as_mut()
            .unwrap_or_else(|| panic!("daemon"))
            .try_wait()
            .unwrap_or_else(|e| panic!("wait: {e}"));
        if let Some(status) = done {
            assert!(status.success(), "drain: {status}");
            break;
        }
        assert!(Instant::now() < until, "drain exceeded bound");
        std::thread::sleep(Duration::from_millis(5));
    }
    tree.daemon.take();
}
#[test]
fn drain_during_follow_keeps_a_queryable_manifest_and_restart_finishes_it() {
    let mut tree = Tree::new();
    unfollowed_burst(&tree);
    let gate = gate(&tree, "follow");
    let gate_text = gate.to_str().unwrap_or_else(|| panic!("gate path"));
    tree.spawn(&[("FERRET_CONTENT_TEST_GATE", gate_text)]);
    wait(|| gate.join("follow.reached").exists());
    drain_content(&mut tree);
    assert!(
        number(
            &String::from_utf8(tree.local(&["status", "--json"]).stdout).unwrap_or_default(),
            "covered"
        ) > 0
    );
    manifest_equals_clean(&tree);
    fs::remove_file(gate.join("follow")).unwrap_or_else(|e| panic!("release: {e}"));
    tree.start(&[]);
    tree.converges();
}
#[test]
fn drain_during_streaming_merge_keeps_inputs_and_restart_finishes_it() {
    let mut tree = Tree::new();
    // Distinct dictionary entries force spilled output, so the barrier is
    // inside the streaming merge after it has written temporary bytes.
    for i in 0..600 {
        let text = (0..400)
            .map(|j| format!("word{i}part{j} "))
            .collect::<String>();
        write(tree.path(&format!("src/merge-{i}.txt")), &text);
    }
    tree.success(tree.local(&["index", "src"]));
    let gate = gate(&tree, "merge");
    let gate_text = gate.to_str().unwrap_or_else(|| panic!("gate path"));
    tree.start(&[("FERRET_CONTENT_TEST_GATE", gate_text)]);
    for i in 0..240 {
        fs::remove_file(tree.path(&format!("src/merge-{i}.txt")))
            .unwrap_or_else(|e| panic!("delete: {e}"));
    }
    wait(|| gate.join("merge.reached").exists());
    drain_content(&mut tree);
    assert!(
        !fs::read_dir(tree.path("index/index"))
            .unwrap_or_else(|e| panic!("index: {e}"))
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with("tmp-"))
    );
    assert!(tree.socket().is_none(), "drain left its socket behind");
    let writer = ferret_catalog::WriterSession::open(&tree.path("index"))
        .unwrap_or_else(|e| panic!("catalog lock survived merge drain: {e}"));
    drop(writer);
    manifest_equals_clean(&tree);
    fs::remove_file(gate.join("merge")).unwrap_or_else(|e| panic!("release: {e}"));
    tree.start(&[]);
    tree.converges();
}
#[test]
fn battery_pauses_follow_and_socket_incomplete_matches_local() {
    let mut tree = Tree::new();
    unfollowed_burst(&tree);
    let power = tree.path("power/BAT0");
    fs::create_dir_all(&power).unwrap_or_else(|e| panic!("power: {e}"));
    write(power.join("type"), "Battery");
    write(power.join("status"), "Discharging");
    let root = tree.path("power");
    let root_text = root.to_str().unwrap_or_else(|| panic!("power path"));
    tree.spawn(&[("FERRET_SIGNAL_POWER", root_text)]);
    let status = tree.status();
    assert!(status.contains("battery-paused"), "{status}");
    assert_eq!(number(&status, "covered"), 0);
    for args in [
        vec!["search", "text:needle"],
        vec!["search", "--scan-uncovered", "text:needle"],
        vec!["--json", "search", "text:needle"],
    ] {
        let daemon = tree.run(&args);
        let local = tree.local(&args);
        assert_eq!(
            (daemon.stdout, daemon.stderr, daemon.status.code()),
            (local.stdout, local.stderr, local.status.code())
        );
    }
    assert_eq!(number(&tree.status(), "covered"), 0);
    write(power.join("status"), "Charging");
    tree.converges();
}

#[test]
fn covered_socket_content_rows_and_diagnostics_are_native_byte_identical() {
    let mut tree = Tree::new();
    write(tree.path("src/right/a.txt"), "alpha requestHandler");
    write(tree.path("src/right/b.txt"), "beta request handler");
    tree.success(tree.local(&["index", "src"]));
    tree.start(&[]);
    for args in [
        vec!["search", "text:alpha"],
        vec!["search", "text:request handler"],
        vec!["search", "NOT", "text:alpha"],
        vec!["search", "case:text:requestHandler"],
        vec!["search", "text:missing"],
        vec!["search", "text:alpha", "OR", "text:beta"],
        vec!["--json", "search", "text:request handler"],
        vec!["search", "--limit", "1", "text:request handler"],
    ] {
        let daemon = tree.run(&args);
        let local = tree.local(&args);
        assert_eq!(
            (daemon.stdout, daemon.stderr, daemon.status.code()),
            (local.stdout, local.stderr, local.status.code()),
            "{args:?}"
        );
    }
    let stats = tree.run(&["stats", "--json"]);
    let status = tree.run(&["status", "--json"]);
    for output in [stats, status] {
        let json = String::from_utf8(output.stdout).unwrap_or_else(|e| panic!("JSON: {e}"));
        assert!(json.contains("\"index\":{"), "{json}");
        for field in [
            "covered",
            "uncovered",
            "unreadable",
            "segments",
            "bytes",
            "last_follow",
            "last_merge",
        ] {
            assert!(json.contains(&format!("\"{field}\":")), "{json}");
        }
        assert_eq!(number(&json, "uncovered"), 0);
        assert!(!json.contains("\"last_follow\":null"), "{json}");
    }
}

#[test]
fn deferred_catalog_retry_does_not_spin_on_an_expired_content_deadline() {
    let mut tree = Tree::new();
    let proc = tree.path("low-memory-proc");
    fs::create_dir_all(proc.join("pressure")).unwrap_or_else(|e| panic!("signals: {e}"));
    for (path, value) in [
        ("pressure/cpu", "some avg10=0.00 avg60=0.00\n"),
        ("pressure/io", "some avg10=0.00 avg60=0.00\n"),
        ("meminfo", "MemAvailable: 1 kB\n"),
        ("loadavg", "0.00 0.00 0.00 1/100 1\n"),
    ] {
        write(proc.join(path), value);
    }
    let proc_text = proc.to_str().unwrap_or_else(|| panic!("proc path"));
    tree.spawn(&[("FERRET_SIGNAL_PROC", proc_text)]);
    wait(|| tree.status().contains("memory-blocked"));
    let pid = tree
        .daemon
        .as_ref()
        .unwrap_or_else(|| panic!("daemon"))
        .id();
    let ticks = || {
        let stat =
            fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_else(|e| panic!("stat: {e}"));
        stat.rsplit_once(')')
            .unwrap_or_else(|| panic!("stat layout"))
            .1
            .split_whitespace()
            .skip(11)
            .take(2)
            .map(|n| n.parse::<u64>().unwrap_or_else(|e| panic!("ticks: {e}")))
            .sum::<u64>()
    };
    let before = ticks();
    let until = Instant::now() + Duration::from_millis(250);
    while Instant::now() < until {
        std::thread::sleep(Duration::from_millis(5));
    }
    let used = ticks() - before;
    assert!(used < 10, "deferred writer spun for {used} CPU ticks");
}

/// Hold a real streaming merge after temporary output has reached disk.
fn held_merge(tree: &mut Tree, extra: &[(&str, &str)]) -> (PathBuf, UnixStream) {
    for i in 0..600 {
        let text = (0..400)
            .map(|j| format!("word{i}part{j} "))
            .collect::<String>();
        write(tree.path(&format!("src/merge-{i}.txt")), &text);
    }
    tree.success(tree.local(&["index", "src"]));
    let gate = gate(tree, "merge");
    let gate_text = gate.to_str().unwrap_or_else(|| panic!("gate path"));
    let mut env = extra.to_vec();
    env.push(("FERRET_CONTENT_TEST_GATE", gate_text));
    tree.start(&env);
    let client = tree.hold();
    for i in 0..240 {
        fs::remove_file(tree.path(&format!("src/merge-{i}.txt")))
            .unwrap_or_else(|e| panic!("delete: {e}"));
    }
    wait(|| gate.join("merge.reached").exists());
    (gate, client)
}

/// Watch traffic used to cancel two merges, then wait behind a protected
/// third attempt. Every refresh must now publish beside the same held merge.
#[test]
fn watch_traffic_cannot_restart_the_same_merge_forever() {
    let mut tree = Tree::new();
    let (gate, _client) = held_merge(&mut tree, &[("FERRET_WATCH_TEST_REFRESH_DELAY_MS", "100")]);
    let before = number(&tree.status(), "refreshes");
    for round in 0..3 {
        write(
            tree.path("src/right/traffic.txt"),
            &format!("traffic round{round}"),
        );
        wait(|| tree.status().contains("\"current_operation\":\"refresh\""));
        let concurrent = tree.status();
        assert!(
            concurrent.contains("\"merge_running\":true"),
            "{concurrent}"
        );
        wait(|| number(&tree.status(), "refreshes") > before + round);
        let status = tree.status();
        assert!(status.contains("\"merge_running\":true"), "{status}");
        assert!(gate.join("merge").exists());
        assert!(
            tree.success(tree.run(&["search", &format!("text:round{round}")]))
                .stdout
                .ends_with(b"traffic.txt\n")
        );
    }
    fs::remove_file(gate.join("merge")).unwrap_or_else(|e| panic!("release: {e}"));
    wait(|| tree.status().contains("\"merge_running\":false"));
    wait(|| number(&tree.status(), "uncovered") == 0);
    tree.converges();
}

/// Follow used to block on the merge's mutex. It must skip while the new
/// DocId remains uncovered, letting the query verify the edited bytes directly.
#[test]
fn follow_skips_a_running_merge_and_queries_verify_the_edit() {
    let mut tree = Tree::new();
    let (gate, _client) = held_merge(&mut tree, &[]);
    let before = number(&tree.status(), "refreshes");
    write(tree.path("src/right/edit.txt"), "uncoveredneedle");
    wait(|| number(&tree.status(), "refreshes") > before);
    // Wait through several follow deadlines; follow must leave the writer
    // free even though the merge's content mutex is still held.
    std::thread::sleep(Duration::from_millis(350));
    let status = tree.status();
    assert_eq!(number(&status, "uncovered"), 1, "{status}");
    assert!(status.contains("\"writer_busy\":false"), "{status}");
    assert!(status.contains("\"merge_running\":true"), "{status}");
    let output = tree.success(tree.run(&["search", "text:uncoveredneedle"]));
    assert!(output.stdout.ends_with(b"edit.txt\n"));
    fs::remove_file(gate.join("merge")).unwrap_or_else(|e| panic!("release: {e}"));
    wait(|| number(&tree.status(), "uncovered") == 0);
    assert!(
        tree.success(tree.run(&["search", "text:uncoveredneedle"]))
            .stdout
            .ends_with(b"edit.txt\n")
    );
}

/// Idle expiry must cancel background merges rather than waiting indefinitely
/// for their content mutex. Socket removal still follows catalog lock release.
#[test]
fn idle_exit_cancels_a_streaming_merge_and_releases_the_catalog_lock() {
    let mut tree = Tree::new();
    let (gate, client) = held_merge(&mut tree, &[("FERRET_DAEMON_IDLE_MS", "1500")]);
    let socket = tree.socket().unwrap_or_else(|| panic!("socket"));
    drop(client);
    wait(|| !socket.exists());
    let daemon = tree.daemon.as_mut().unwrap_or_else(|| panic!("daemon"));
    wait(|| {
        daemon
            .try_wait()
            .unwrap_or_else(|e| panic!("wait: {e}"))
            .is_some()
    });
    assert!(
        daemon
            .wait()
            .unwrap_or_else(|e| panic!("exit: {e}"))
            .success()
    );
    assert!(
        gate.join("merge").exists(),
        "test must not release the merge"
    );
    // Opening a replacement writer after the socket vanishes pins D110's
    // lock-before-socket ordering, including the joined merge thread.
    let writer = ferret_catalog::WriterSession::open(&tree.path("index"))
        .unwrap_or_else(|e| panic!("catalog lock survived idle exit: {e}"));
    drop(writer);
    tree.daemon.take();
    manifest_equals_clean(&tree);
}

#[test]
fn slow_bursts_in_watched_directories_do_not_starve_a_poll() {
    let mut tree = Tree::new();
    // Cap 3 watches the two policy inputs' directories and `src` itself, so
    // churn in `src` makes bursts and `src/left` is seen only by polls. The
    // local search reads the publication, not a live walk.
    tree.start(&[
        ("FERRET_WATCH_CAP", "3"),
        ("FERRET_POLL_MS", "200"),
        ("FERRET_WATCH_TEST_REFRESH_DELAY_MS", "600"),
    ]);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let churn = {
        let stop = stop.clone();
        let dir = tree.path("src");
        std::thread::spawn(move || {
            let mut i = 0u32;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                fs::write(dir.join("churn.txt"), i.to_string())
                    .unwrap_or_else(|error| panic!("churn: {error:?}"));
                i += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    };
    // Let the startup walk and the bursts settle into their rhythm first, so
    // no walk that was already due finds the file.
    std::thread::sleep(Duration::from_secs(4));
    let before = number(&tree.status(), "refreshes");
    fs::write(tree.path("src/left/polled.txt"), "polled")
        .unwrap_or_else(|error| panic!("fixture: {error:?}"));
    // Each 600 ms burst would push a poll 2.4 s further out, and a burst
    // comes sooner than that, so a poll spaced by bursts never runs.
    let until = Instant::now() + Duration::from_secs(12);
    let mut found = false;
    while Instant::now() < until {
        if tree.local(&["search", "polled.txt"]).status.success() {
            found = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    churn
        .join()
        .unwrap_or_else(|_| panic!("churn thread panicked"));
    let status = tree.status();
    assert!(
        number(&status, "refreshes") > before + 1,
        "the churn made no bursts: {status}"
    );
    assert!(found, "the poll never ran: {status}");
}
