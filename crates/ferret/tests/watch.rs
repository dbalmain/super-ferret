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
        wait(|| {
            self.socket()
                .is_some_and(|p| UnixStream::connect(p).is_ok())
        });
    }
    fn stop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
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
    fn status(&self) -> String {
        self.event("{\"id\":\"s\",\"op\":\"status\"}\n")
    }
    fn quiet(&self) {
        wait(|| {
            let s = self.status();
            s.contains("\"pending_scopes\":0")
                && s.contains("\"writer_busy\":false")
                && !s.contains("\"last_complete_backstop\":null")
        });
    }
    fn oracle(&self) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let oracle = self.base.join("oracle");
        if oracle.exists() {
            fs::remove_dir_all(&oracle)
                .unwrap_or_else(|error| panic!("remove old oracle: {error:?}"));
        }
        let output = self
            .command(&["index", "src"])
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
        (
            records(query(&["search", "*"]), b'\n'),
            records(query(&["find", "src", "-printf", "%y %p\\0"]), 0),
        )
    }
    fn converges(&self) {
        let expected = self.oracle();
        let until = Instant::now() + BOUND;
        loop {
            let actual = (
                records(self.run(&["search", "*"]), b'\n'),
                records(self.run(&["find", "src", "-printf", "%y %p\\0"]), 0),
            );
            let s = self.status();
            if actual == expected
                && s.contains("\"pending_scopes\":0")
                && s.contains("\"writer_busy\":false")
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
impl Drop for Tree {
    fn drop(&mut self) {
        self.stop();
        let _ = Command::new("chmod")
            .args(["-R", "u+rwx"])
            .arg(&self.base)
            .status();
        let _ = fs::remove_dir_all(&self.base);
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

#[test]
fn idle_exit_waits_for_startup_and_pending_publication() {
    let mut tree = Tree::new();
    tree.spawn(&[
        ("FERRET_DAEMON_IDLE_MS", "80"),
        ("FERRET_WATCH_TEST_REFRESH_DELAY_MS", "300"),
    ]);
    assert!(tree.status().contains("\"writer_busy\":true"));
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        tree.daemon
            .as_mut()
            .unwrap_or_else(|| panic!("daemon"))
            .try_wait()
            .unwrap_or_else(|error| panic!("try wait: {error:?}"))
            .is_none()
    );
    tree.quiet();
    write(tree.path("src/left/deep/pending.txt"), "pending");
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        tree.daemon
            .as_mut()
            .unwrap_or_else(|| panic!("daemon"))
            .try_wait()
            .unwrap_or_else(|error| panic!("try wait: {error:?}"))
            .is_none()
    );
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
