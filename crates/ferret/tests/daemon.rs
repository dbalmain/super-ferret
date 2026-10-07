//! Real socket/binary lifecycle tests. All environments and endpoints are
//! private; bounded reads/processes and Drop cleanup turn hangs into failures.
#![allow(clippy::unwrap_used)]

#[path = "support/fixture.rs"]
mod fixture;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const FERRET: &str = env!("CARGO_BIN_EXE_ferret");
const DAEMON: &str = env!("CARGO_BIN_EXE_ferretd");
const BOUND: Duration = Duration::from_secs(5);

struct Tree {
    base: PathBuf,
    children: Mutex<Vec<Child>>,
}
impl Tree {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!(
            "ferret-daemon-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(base.join("src/nested")).unwrap();
        fs::create_dir_all(base.join("home/runtime")).unwrap();
        fs::set_permissions(base.join("home/runtime"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(base.join("src/main.rs"), b"fn main() {}\n").unwrap();
        fs::write(base.join("src/nested/Cargo.toml"), b"[package]\n").unwrap();
        fs::write(
            base.join("src")
                .join(std::ffi::OsString::from_vec(b"bad\xff".to_vec())),
            b"x",
        )
        .unwrap();
        let tree = Self {
            base,
            children: Mutex::new(Vec::new()),
        };
        assert!(tree.local(&["index", "src"]).status.success());
        tree
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = fixture::bounded_command(FERRET, &self.base);
        cmd.args(args)
            .env_remove("FERRET_NO_DAEMON")
            .env("FERRET_DAEMON_BIN", DAEMON)
            .env("FERRET_DAEMON_STARTUP_MS", "1500")
            .env("FERRET_DAEMON_IDLE_MS", "30000");
        cmd
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
    fn local(&self, args: &[&str]) -> Output {
        self.command(args)
            .env("FERRET_NO_DAEMON", "1")
            .output()
            .unwrap()
    }
    fn sockets(&self) -> Vec<PathBuf> {
        fs::read_dir(self.base.join("home/runtime/ferret"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "sock"))
            .collect()
    }
    fn socket(&self) -> PathBuf {
        wait(|| self.sockets().len() == 1);
        self.sockets().pop().unwrap()
    }
    fn start(&self, extra: &[(&str, &str)]) -> u32 {
        let mut cmd = fixture::command(DAEMON, &self.base);
        cmd.env_remove("FERRET_NO_DAEMON")
            .env("FERRET_DAEMON_IDLE_MS", "30000")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (name, value) in extra {
            cmd.env(name, value);
        }
        let child = cmd.spawn().unwrap();
        let pid = child.id();
        self.children.lock().unwrap().push(child);
        let socket = self.socket();
        if !extra
            .iter()
            .any(|(name, _)| *name == "FERRET_DAEMON_IDLE_MS")
        {
            ready(UnixStream::connect(socket).unwrap());
        }
        pid
    }
    fn connect(&self) -> (BufReader<UnixStream>, String) {
        ready(UnixStream::connect(self.socket()).unwrap())
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        for path in self.sockets() {
            if let Ok(stream) = UnixStream::connect(&path) {
                stream
                    .set_read_timeout(Some(Duration::from_millis(500)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut hello = String::new();
                let _ = reader.read_line(&mut hello);
                let pid = number(&hello, "pid");
                let _ = reader.get_mut().write_all(b"{\"op\":\"drain\"}\n");
                drop(reader);
                let until = Instant::now() + Duration::from_millis(700);
                while path.exists() && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(10));
                }
                if path.exists()
                    && let Some(pid) = pid
                {
                    let _ = Command::new("kill").arg(pid.to_string()).status();
                }
            }
        }
        for child in self.children.lock().unwrap().iter_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        terminate_fixture_daemons(&self.base);
        assert!(
            fixture_daemons(&self.base).is_empty(),
            "fixture daemon survived Drop"
        );
        let _ = fs::remove_dir_all(&self.base);
    }
}
fn fixture_daemons(base: &std::path::Path) -> Vec<u32> {
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
fn terminate_fixture_daemons(base: &std::path::Path) {
    for pid in fixture_daemons(base) {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
    let until = Instant::now() + Duration::from_secs(2);
    while !fixture_daemons(base).is_empty() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    for pid in fixture_daemons(base) {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
}
fn wait(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + BOUND;
    while !condition() {
        assert!(Instant::now() < until, "deadline exceeded");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn line(reader: &mut BufReader<UnixStream>) -> String {
    let mut line = String::new();
    assert_ne!(
        reader.read_line(&mut line).unwrap(),
        0,
        "unexpected socket EOF"
    );
    line
}
fn number(line: &str, key: &str) -> Option<u32> {
    line.split_once(&format!("\"{key}\":"))?
        .1
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}
fn ready(stream: UnixStream) -> (BufReader<UnixStream>, String) {
    stream.set_read_timeout(Some(BOUND)).unwrap();
    stream.set_write_timeout(Some(BOUND)).unwrap();
    let mut reader = BufReader::new(stream);
    loop {
        let hello = line(&mut reader);
        if hello.contains("\"state\":\"ready\"") {
            return (reader, hello);
        }
        assert!(hello.contains("\"state\":\"loading\""), "{hello}");
    }
}
fn block(reader: &mut BufReader<UnixStream>, request: &[u8]) -> String {
    reader.get_mut().write_all(request).unwrap();
    let mut output = String::new();
    loop {
        let next = line(reader);
        let end = next.contains("\"event\":\"end\"") || next.contains("\"event\":\"status\"");
        output.push_str(&next);
        if end {
            return output;
        }
    }
}
fn parity(tree: &Tree, args: &[&str]) {
    let local = tree.local(args);
    let remote = tree.run(args);
    assert_eq!(
        remote.status.code(),
        local.status.code(),
        "{args:?}: {}",
        String::from_utf8_lossy(&remote.stderr)
    );
    assert_eq!(remote.stdout, local.stdout, "{args:?}");
    assert_eq!(remote.stderr, local.stderr, "{args:?}");
}

#[test]
fn native_search_and_find_bytes_and_status_match_local() {
    let tree = Tree::new();
    for args in [
        &["search", "case:main"][..],
        &["search", "--json", "*"],
        &["search", "--limit", "1", "*"],
        &["search", "missing"],
        &["find", "src", "-maxdepth", "2", "-print0"],
        &[
            "find",
            "./src/",
            "-maxdepth",
            "2",
            "-name",
            "*.rs",
            "-printf",
            "%p:%s\\n",
        ],
        &["find", "src", "-maxdepth", "2", "-false"],
        &["find", "absent", "-print"],
        &["find", "src", "-maxdepth", "2", "-perm", "/000", "-print"],
    ] {
        parity(&tree, args);
    }
    let socket = tree.socket();
    assert_eq!(fs::metadata(&socket).unwrap().mode() & 0o777, 0o600);
    assert_eq!(
        fs::metadata(socket.parent().unwrap()).unwrap().mode() & 0o777,
        0o700
    );
    let (mut reader, hello) = tree.connect();
    assert!(
        hello.contains("\"incarnation\":")
            && hello.contains("query-only")
            && hello.contains("\"limits\":")
    );
    let status = block(&mut reader, b"{\"id\":\"s\",\"op\":\"status\"}\n");
    assert_eq!(number(&status, "engine_opens"), Some(1));
}

#[test]
fn concurrent_first_use_has_one_host_and_all_clients_answer() {
    let tree = Tree::new();
    let barrier = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        let clients: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    tree.run(&["search", "main"])
                })
            })
            .collect();
        for client in clients {
            assert!(client.join().unwrap().status.success());
        }
    });
    let mut pids = Vec::new();
    for _ in 0..8 {
        let (_, hello) = tree.connect();
        pids.push(number(&hello, "pid").unwrap());
    }
    assert!(pids.iter().all(|pid| *pid == pids[0]));
    assert_eq!(tree.sockets().len(), 1);
    // Each losing direct starter must exit; count actual processes by their
    // selected private index rather than trusting only the winning endpoint.
    wait(|| {
        fs::read_dir("/proc")
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .chars()
                    .all(|c| c.is_ascii_digit())
            })
            .filter(|entry| {
                fs::read(entry.path().join("cmdline")).is_ok_and(|cmd| {
                    cmd.starts_with(DAEMON.as_bytes())
                        && cmd
                            .windows(tree.base.as_os_str().as_encoded_bytes().len())
                            .any(|w| w == tree.base.as_os_str().as_encoded_bytes())
                })
            })
            .count()
            == 1
    });
}

#[test]
#[cfg(debug_assertions)]
fn cold_first_use_answers_locally_while_one_daemon_loads() {
    let tree = Tree::new();
    let first = tree
        .command(&["search", "main"])
        .env("FERRET_DAEMON_LOAD_DELAY_MS", "5000")
        .output()
        .unwrap();
    assert!(first.status.success());
    assert_eq!(first.stdout, tree.local(&["search", "main"]).stdout);
    wait(|| fixture_daemons(&tree.base).len() == 1 && tree.sockets().len() == 1);

    let mut reader = BufReader::new(UnixStream::connect(tree.socket()).unwrap());
    reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    assert!(line(&mut reader).contains("\"state\":\"loading\""));
    let hello = loop {
        let mut hello = String::new();
        reader.read_line(&mut hello).unwrap();
        if hello.contains("\"state\":\"ready\"") {
            break hello;
        }
        assert!(hello.contains("\"state\":\"loading\""), "{hello}");
    };
    assert!(number(&hello, "pid").is_some());
    let second = tree.run(&["search", "main"]);
    assert!(second.status.success());
    assert_eq!(second.stdout, first.stdout);
    let log = fs::read_to_string(tree.base.join("home/state/ferret/log.jsonl")).unwrap();
    assert!(log.contains("\"host\":\"socket\""), "{log}");
    assert_eq!(fixture_daemons(&tree.base).len(), 1);
}

#[test]
#[cfg(debug_assertions)]
fn writer_loading_timeout_never_takes_direct_ownership_and_retry_routes_to_owner() {
    // A loading timeout used to fall through to a local writer and report
    // success before the daemon acquired its catalog lock.
    let tree = Tree::new();
    let pid = tree.start(&[
        ("FERRET_DAEMON_LOAD_DELAY_MS", "3000"),
        ("FERRET_DAEMON_IDLE_MS", "30000"),
        ("FERRET_WRITER_TEST_COMMAND_DELAY_MS", "1000"),
    ]);
    let current = fs::read(tree.base.join("index/current")).unwrap();
    fs::write(tree.base.join("src/during-loading.txt"), "x").unwrap();
    let indexed = tree.run(&["index", "src"]);
    assert_eq!(indexed.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&indexed.stderr).contains("daemon owner did not become ready"));
    assert_eq!(fs::read(tree.base.join("index/current")).unwrap(), current);
    assert!(
        !tree
            .local(&["search", "during-loading.txt"])
            .status
            .success()
    );

    let (mut reader, hello) = tree.connect();
    assert_eq!(number(&hello, "pid"), Some(pid));
    let child = tree
        .command(&["index", "src"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait(|| {
        let status = block(&mut reader, b"{\"id\":\"s\",\"op\":\"status\"}\n");
        number(&status, "writer_commands") == Some(1)
            && status.contains("\"current_operation\":\"index\"")
    });
    let indexed = bounded_output(child);
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    assert!(tree.run(&["search", "during-loading.txt"]).status.success());
    assert!(!exited(&tree, pid));
    assert_eq!(number(&tree.connect().1, "pid"), Some(pid));
}

#[test]
fn stale_socket_is_replaced_and_index_identities_are_isolated() {
    let tree = Tree::new();
    let directory = tree.base.join("home/runtime/ferret");
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let metadata = fs::metadata(tree.base.join("index")).unwrap();
    let stale = directory.join(format!("{:x}-{:x}.sock", metadata.dev(), metadata.ino()));
    drop(UnixListener::bind(&stale).unwrap());
    fs::set_permissions(&stale, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(tree.run(&["search", "main"]).status.success());
    fs::create_dir(tree.base.join("other")).unwrap();
    fs::write(tree.base.join("other/unique.txt"), "unique").unwrap();
    let second = tree.base.join("second-index");
    assert!(
        tree.command(&["index", "other"])
            .env("FERRET_INDEX", &second)
            .output()
            .unwrap()
            .status
            .success()
    );
    let output = tree
        .command(&["search", "unique"])
        .env("FERRET_INDEX", &second)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("main.rs"));
    assert_eq!(tree.run(&["search", "unique"]).status.code(), Some(1));
    // A cold query may answer locally before its asynchronously spawned host
    // has bound the socket. Both identities must converge within the bound.
    wait(|| tree.sockets().len() == 2);
}

#[test]
fn unsafe_runtime_and_symlink_directory_fall_back_without_creating_files() {
    for mode in [0o755, 0o770] {
        let tree = Tree::new();
        let runtime = tree.base.join("home/runtime");
        fs::set_permissions(&runtime, fs::Permissions::from_mode(mode)).unwrap();
        parity(&tree, &["search", "main"]);
        assert!(fs::read_dir(&runtime).unwrap().next().is_none());
    }
    let tree = Tree::new();
    let target = tree.base.join("target");
    fs::create_dir(&target).unwrap();
    symlink(&target, tree.base.join("home/runtime/ferret")).unwrap();
    parity(&tree, &["search", "main"]);
    assert!(fs::read_dir(target).unwrap().next().is_none());
    let tree = Tree::new();
    let runtime = tree.base.join("home/runtime");
    // root-owned /proc is an existing wrong-owner directory without privileges.
    let out = tree
        .command(&["search", "main"])
        .env("XDG_RUNTIME_DIR", "/proc")
        .output()
        .unwrap();
    assert_eq!(out.stdout, tree.local(&["search", "main"]).stdout);
    assert!(fs::read_dir(runtime).unwrap().next().is_none());
}

#[test]
fn bypass_missing_runtime_denied_spawn_and_loading_timeout_fall_back() {
    for case in 0..if cfg!(debug_assertions) { 4 } else { 3 } {
        let tree = Tree::new();
        let mut cmd = tree.command(&["search", "main"]);
        match case {
            0 => {
                cmd.env("FERRET_NO_DAEMON", "1");
            }
            1 => {
                cmd.env_remove("XDG_RUNTIME_DIR");
            }
            2 => {
                let denied = tree.base.join("non-executable");
                fs::write(&denied, "no").unwrap();
                cmd.env("FERRET_DAEMON_BIN", denied);
            }
            _ => {
                cmd.env("FERRET_DAEMON_STARTUP_MS", "30")
                    .env("FERRET_DAEMON_LOAD_DELAY_MS", "150");
            }
        }
        let out = cmd.output().unwrap();
        let local = tree.local(&["search", "main"]);
        assert_eq!(out.status.code(), local.status.code());
        assert_eq!(out.stdout, local.stdout);
        let log = fs::read_to_string(tree.base.join("home/state/ferret/log.jsonl")).unwrap();
        assert!(
            !log.contains("\"host\":\"socket\""),
            "case {case} did not fall back"
        );
        if case < 3 {
            assert!(tree.sockets().is_empty());
        }
    }
}

#[test]
#[cfg(debug_assertions)]
fn panic_isolated_in_real_query_and_effects_are_refused() {
    let tree = Tree::new();
    tree.start(&[]);
    let (mut reader, _) = tree.connect();
    let result = block(
        &mut reader,
        b"{\"id\":\"p\",\"op\":\"search\",\"args\":[\"*\"],\"capabilities\":[\"test-panic\"]}\n",
    );
    assert!(
        result.contains("\"exit\":3") && result.contains("RuntimeError"),
        "{result}"
    );
    let result = block(
        &mut reader,
        b"{\"id\":\"q\",\"op\":\"search\",\"args\":[\"main\"]}\n",
    );
    assert!(result.contains("\"exit\":0"));
    let request = format!(
        "{{\"id\":\"a\",\"op\":\"find\",\"args\":[\"{}\",\"-fprint\",\"{}\"],\"capabilities\":[\"local-effects\"]}}\n",
        tree.base.join("src").display(),
        tree.base.join("forbidden").display()
    );
    let result = block(&mut reader, request.as_bytes());
    assert!(result.contains("LocalEffectsRequired"));
    assert!(!tree.base.join("forbidden").exists());
}

#[test]
fn unchanged_queries_do_not_reopen_and_real_index_publication_is_adopted() {
    let tree = Tree::new();
    assert!(tree.run(&["search", "main"]).status.success());
    let (mut reader, _) = tree.connect();
    assert_eq!(
        number(
            &block(&mut reader, b"{\"id\":\"s\",\"op\":\"status\"}\n"),
            "engine_opens"
        ),
        Some(1)
    );
    fs::write(tree.base.join("src/new.rs"), "new").unwrap();
    assert!(tree.run(&["index", "src"]).status.success());
    assert!(tree.run(&["search", "new.rs"]).status.success());
    assert_eq!(
        number(
            &block(&mut reader, b"{\"id\":\"s\",\"op\":\"status\"}\n"),
            "engine_opens"
        ),
        Some(1)
    );
    assert!(tree.run(&["search", "new.rs"]).status.success());
    assert_eq!(
        number(
            &block(&mut reader, b"{\"id\":\"s\",\"op\":\"status\"}\n"),
            "engine_opens"
        ),
        Some(1)
    );
}

#[test]
fn routed_index_commands_bypass_background_politeness_but_keep_admission() {
    let tree = Tree::new();
    let proc = tree.base.join("signals/proc");
    let power = tree.base.join("signals/power/BAT0");
    fs::create_dir_all(proc.join("pressure")).unwrap();
    fs::create_dir_all(&power).unwrap();
    fs::write(proc.join("pressure/cpu"), "some avg10=0.00 avg60=0.00\n").unwrap();
    fs::write(proc.join("pressure/io"), "some avg10=11.00 avg60=0.00\n").unwrap();
    fs::write(proc.join("meminfo"), "MemAvailable: 1073741824 kB\n").unwrap();
    fs::write(proc.join("loadavg"), "0.00 0.00 0.00 1/100 1\n").unwrap();
    fs::write(power.join("type"), "Battery\n").unwrap();
    fs::write(power.join("status"), "Discharging\n").unwrap();
    tree.start(&[
        ("FERRET_SIGNAL_PROC", proc.to_str().unwrap()),
        (
            "FERRET_SIGNAL_POWER",
            tree.base.join("signals/power").to_str().unwrap(),
        ),
        ("FERRET_BULK_BYTES_PER_SECOND", "1024"),
        ("FERRET_BACKSTOP_MS", "20"),
    ]);
    let (mut reader, _) = tree.connect();
    wait(|| {
        block(&mut reader, b"{\"id\":\"status\",\"op\":\"status\"}\n").contains("battery-paused")
    });

    let large = tree.base.join("src/command-content.bin");
    fs::write(&large, vec![b'x'; 64 << 10]).unwrap();
    let started = Instant::now();
    let indexed = tree.run(&["index", "src"]);
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "command was paced"
    );
    assert!(
        tree.run(&["search", "command-content.bin"])
            .status
            .success()
    );

    // A later filesystem change remains pending under the background gate.
    fs::write(
        tree.base.join("src/background-pending.bin"),
        vec![b'y'; 64 << 10],
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        !tree
            .run(&["search", "background-pending.bin"])
            .status
            .success()
    );
    let status = block(&mut reader, b"{\"id\":\"status2\",\"op\":\"status\"}\n");
    assert!(status.contains("battery-paused"), "{status}");
}

#[test]
fn deferred_watch_burst_retries_its_scopes_after_battery_pause() {
    let tree = Tree::new();
    let proc = tree.base.join("signals/proc");
    let power = tree.base.join("signals/power/BAT0");
    fs::create_dir_all(proc.join("pressure")).unwrap();
    fs::create_dir_all(&power).unwrap();
    fs::write(proc.join("pressure/cpu"), "some avg10=0.00 avg60=0.00\n").unwrap();
    fs::write(proc.join("pressure/io"), "some avg10=0.00 avg60=0.00\n").unwrap();
    fs::write(proc.join("meminfo"), "MemAvailable: 1073741824 kB\n").unwrap();
    fs::write(proc.join("loadavg"), "0.00 0.00 0.00 1/100 1\n").unwrap();
    fs::write(power.join("type"), "Battery\n").unwrap();
    fs::write(power.join("status"), "Charging\n").unwrap();
    tree.start(&[
        ("FERRET_SIGNAL_PROC", proc.to_str().unwrap()),
        (
            "FERRET_SIGNAL_POWER",
            tree.base.join("signals/power").to_str().unwrap(),
        ),
        ("FERRET_BACKSTOP_MS", "60000"),
    ]);
    let (mut reader, _) = tree.connect();
    wait(|| {
        let status = block(&mut reader, b"{\"id\":\"s\",\"op\":\"status\"}\n");
        status.contains("\"current_operation\":\"idle\"")
            && number(&status, "refreshes").unwrap_or_default() > 0
    });

    fs::write(power.join("status"), "Discharging\n").unwrap();
    wait(|| block(&mut reader, b"{\"id\":\"s\",\"op\":\"status\"}\n").contains("battery-paused"));
    fs::write(
        tree.base.join("src/deferred-burst.txt"),
        "visible after resume",
    )
    .unwrap();
    wait(|| {
        let status = block(&mut reader, b"{\"id\":\"s\",\"op\":\"status\"}\n");
        status.contains("battery-paused")
            && number(&status, "pending_scopes").unwrap_or_default() > 0
    });
    fs::write(power.join("status"), "Charging\n").unwrap();
    wait(|| tree.run(&["search", "deferred-burst.txt"]).status.success());
    let status = block(&mut reader, b"{\"id\":\"s\",\"op\":\"status\"}\n");
    assert!(
        status.contains("\"last_refresh_reason\":\"Burst\""),
        "deferred work escalated instead of resuming its scopes: {status}"
    );
}

#[test]
fn routed_full_rebuild_reports_memory_admission_reason_and_keeps_generation() {
    let tree = Tree::new();
    let proc = tree.base.join("signals/proc");
    fs::create_dir_all(proc.join("pressure")).unwrap();
    fs::write(proc.join("pressure/cpu"), "some avg10=0.00 avg60=0.00\n").unwrap();
    fs::write(proc.join("pressure/io"), "some avg10=0.00 avg60=0.00\n").unwrap();
    fs::write(proc.join("meminfo"), "MemAvailable: 1024 kB\n").unwrap();
    fs::write(proc.join("loadavg"), "0.00 0.00 0.00 1/100 1\n").unwrap();
    let power = tree.base.join("signals/power");
    fs::create_dir_all(&power).unwrap();
    tree.start(&[
        ("FERRET_SIGNAL_PROC", proc.to_str().unwrap()),
        ("FERRET_SIGNAL_POWER", power.to_str().unwrap()),
    ]);
    fs::write(tree.base.join("src/refused.rs"), "new content").unwrap();
    let refused = tree.run(&["index", "src"]);
    assert!(!refused.status.success());
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(
        message.contains("insufficient memory for a full rebuild"),
        "{message}"
    );
    assert!(
        message.contains("need ") && message.contains("have 1048576"),
        "{message}"
    );
    assert!(tree.run(&["search", "main.rs"]).status.success());
    assert!(!tree.run(&["search", "refused.rs"]).status.success());
}

#[test]
fn short_idle_exit_unlinks_own_socket() {
    let tree = Tree::new();
    let pid = tree.start(&[("FERRET_DAEMON_IDLE_MS", "100")]);
    let socket = tree.socket();
    {
        let (_, hello) = tree.connect();
        assert_eq!(number(&hello, "pid"), Some(pid));
    }
    wait(|| !socket.exists());
    wait(|| {
        tree.children
            .lock()
            .unwrap()
            .iter_mut()
            .find(|child| child.id() == pid)
            .unwrap()
            .try_wait()
            .unwrap()
            .is_some()
    });
}

#[test]
fn idle_exit_discards_a_permanently_failing_root_retry() {
    let tree = Tree::new();
    fs::remove_dir_all(tree.base.join("src")).unwrap();
    tree.start(&[("FERRET_DAEMON_IDLE_MS", "2000")]);
    let socket = tree.socket();
    wait(|| !socket.exists());
}

#[test]
fn effectful_find_child_parent_is_the_client_and_local_modes_do_not_attach() {
    let tree = Tree::new();
    let pid = tree.start(&[]);
    let mut cmd = fixture::command(FERRET, &tree.base);
    cmd.env_remove("FERRET_NO_DAEMON")
        .env("FERRET_DAEMON_BIN", DAEMON)
        .args([
            "find",
            "src",
            "-maxdepth",
            "0",
            "-exec",
            "sh",
            "-c",
            "printf '%s' \"$PPID\"",
            ";",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn().unwrap();
    let client_pid = child.id();
    let output = bounded_output(child);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        client_pid.to_string()
    );
    assert_ne!(pid, client_pid);
    let tree = Tree::new();
    assert!(tree.run(&["find", "-I", "src", "-print"]).status.success());
    assert!(tree.run(&["find", "--help"]).status.success());
    assert!(tree.sockets().is_empty());
}

fn bounded_output(mut child: Child) -> Output {
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    std::thread::scope(|scope| {
        let stdout = scope.spawn(|| {
            let mut bytes = Vec::new();
            let mut pipe = stdout;
            pipe.read_to_end(&mut bytes).unwrap();
            bytes
        });
        let stderr = scope.spawn(|| {
            let mut bytes = Vec::new();
            let mut pipe = stderr;
            pipe.read_to_end(&mut bytes).unwrap();
            bytes
        });
        let until = Instant::now() + BOUND;
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= until {
                child.kill().unwrap();
                panic!("client timeout");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        Output {
            status,
            stdout: stdout.join().unwrap(),
            stderr: stderr.join().unwrap(),
        }
    })
}

fn large_request(tree: &Tree) -> Vec<u8> {
    format!("{{\"id\":\"large\",\"op\":\"find\",\"cwd\":\"{}\",\"args\":[\"src\",\"-printf\",\"{}\"]}}\n", tree.base.display(), "x".repeat(100_000)).into_bytes()
}
fn begin_large(tree: &Tree) -> BufReader<UnixStream> {
    let (mut reader, _) = tree.connect();
    reader.get_mut().write_all(&large_request(tree)).unwrap();
    assert!(line(&mut reader).contains("\"event\":\"begin\""));
    assert!(line(&mut reader).contains("\"event\":\"stdout\""));
    reader
}

#[test]
fn cancelling_blocked_output_and_hangup_release_all_query_slots() {
    let tree = Tree::new();
    tree.start(&[]);
    let mut readers: Vec<_> = (0..4).map(|_| begin_large(&tree)).collect();
    // Each query emits > its socket buffer. Cancel two by control message and
    // two by hangup, without draining their remaining output.
    for reader in &mut readers[..2] {
        reader
            .get_mut()
            .write_all(b"{\"op\":\"cancel\"}\n")
            .unwrap();
    }
    readers.clear();
    assert!(tree.run(&["search", "main"]).status.success());
}

#[test]
#[cfg(debug_assertions)]
fn build_mismatch_drains_finishes_in_flight_and_restarts_client_binary() {
    let tree = Tree::new();
    let old_pid = tree.start(&[("FERRET_DAEMON_TEST_BUILD", "old-build")]);
    let mut active = begin_large(&tree);
    let socket = tree.socket();
    let child = tree
        .command(&["search", "main"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The old query is blocked, so graceful replacement must still be waiting.
    std::thread::sleep(Duration::from_millis(60));
    assert!(socket.exists());
    assert!(
        tree.children.lock().unwrap()[0]
            .try_wait()
            .unwrap()
            .is_none()
    );
    let mut ended = false;
    loop {
        let mut next = String::new();
        if active.read_line(&mut next).unwrap() == 0 {
            break;
        }
        if next.contains("\"event\":\"end\"") {
            assert!(next.contains("\"exit\":0"), "{next}");
            ended = true;
        }
    }
    assert!(ended);
    let output = bounded_output(child);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let (_, hello) = tree.connect();
    assert_ne!(number(&hello, "pid"), Some(old_pid));
    assert!(!hello.contains("old-build"));
}

#[test]
fn socket_loss_after_first_bytes_is_transport_failure_and_never_replays() {
    let tree = Tree::new();
    let pid = tree.start(&[]);
    let format = "y".repeat(100_000);
    let mut command = tree.command(&["find", "src", "-printf", &format]);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    // A bounded reader gets the first bytes; the rest fills the client pipe and
    // daemon socket. Kill only this explicitly started host, then drain output.
    let first = std::thread::spawn(move || {
        let mut bytes = [0; 4096];
        stdout.read_exact(&mut bytes).unwrap();
        (stdout, bytes)
    });
    let until = Instant::now() + BOUND;
    wait(|| first.is_finished() || Instant::now() >= until);
    assert!(first.is_finished());
    let (stdout, bytes) = first.join().unwrap();
    assert!(bytes.iter().all(|byte| *byte == b'y'));
    assert!(
        Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    child.stdout = Some(stdout);
    let output = bounded_output(child);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("transport failure"));
    assert!(
        output.stdout.len() + bytes.len() < 500_000,
        "query was replayed"
    );
    // No replacement may appear after any query output was delivered.
    assert!(UnixStream::connect(tree.socket()).is_err());
}

#[test]
fn find_warnings_render_identically_on_local_socket_and_json_hosts() {
    let tree = Tree::new();
    for args in [
        vec!["src", "-path", "x/", "-print"],
        vec!["src", "-printf", "\\q"],
    ] {
        let mut local_args = vec!["find"];
        local_args.extend(args.iter().copied());
        let local = tree.local(&local_args);
        let mut socket_args = vec!["find"];
        socket_args.extend(args.iter().copied());
        let socket = tree.run(&socket_args);
        let mut json_args = vec!["--json", "find"];
        json_args.extend(args.iter().copied());
        let json = tree.run(&json_args);

        assert_eq!(socket.status.code(), local.status.code());
        assert_eq!(json.status.code(), local.status.code());
        assert_eq!(socket.stderr, local.stderr);
        assert_eq!(json.stderr, local.stderr);
        assert!(!local.stderr.windows(8).any(|bytes| bytes == b"find: : "));
        let json_stdout = String::from_utf8(json.stdout).unwrap();
        assert!(
            json_stdout.contains("\"code\":\"warning\""),
            "{json_stdout}"
        );
        assert!(
            json_stdout.contains("\"severity\":\"warning\""),
            "{json_stdout}"
        );
        assert!(json_stdout.contains("\"message\":"), "{json_stdout}");
    }
}

#[test]
#[cfg(debug_assertions)]
fn incompatible_context_falls_back_and_format_mismatch_replaces_host() {
    let tree = Tree::new();
    tree.start(&[("FERRET_DAEMON_TEST_CONTEXT", "different-groups-or-mount")]);
    parity(&tree, &["search", "main"]);
    let log = fs::read_to_string(tree.base.join("home/state/ferret/log.jsonl")).unwrap();
    assert!(
        !log.contains("\"host\":\"socket\""),
        "incompatible context did not fall back"
    );
    let tree = Tree::new();
    let old_pid = tree.start(&[("FERRET_DAEMON_TEST_FORMAT", "3")]);
    assert!(tree.run(&["search", "main"]).status.success());
    let (_, hello) = tree.connect();
    assert_ne!(number(&hello, "pid"), Some(old_pid));
    assert_eq!(number(&hello, "format"), Some(4));
}

#[test]
#[cfg(debug_assertions)]
fn find_worker_panic_wakes_siblings_and_next_query_succeeds() {
    let tree = Tree::new();
    tree.start(&[]);
    let (mut reader, _) = tree.connect();
    let request = format!(
        "{{\"id\":\"p\",\"op\":\"find\",\"cwd\":\"{}\",\"args\":[\"src\",\"-print\"],\"capabilities\":[\"test-find-panic\"]}}\n",
        tree.base.display()
    );
    let result = block(&mut reader, request.as_bytes());
    assert!(result.contains("RuntimeError"), "{result}");
    let result = block(
        &mut reader,
        b"{\"id\":\"q\",\"op\":\"search\",\"args\":[\"main\"]}\n",
    );
    assert!(result.contains("\"exit\":0"));
}

#[test]
fn structured_read_only_find_attaches_and_keeps_its_tagged_codec() {
    let tree = Tree::new();
    tree.start(&[]);
    let output = tree.run(&["--json", "find", "src", "-maxdepth", "0", "-print0"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("\"id\":\"find\",\"event\":\"begin\",\"generation\":{")
            && text.contains("c3JjAA==")
            && text.contains("\"event\":\"end\",\"exit\":0"),
        "{text}"
    );
    assert_eq!(tree.sockets().len(), 1);
}

#[test]
fn drain_on_active_connection_finishes_the_query_before_exit() {
    let tree = Tree::new();
    tree.start(&[]);
    let socket = tree.socket();
    let mut reader = begin_large(&tree);
    reader.get_mut().write_all(b"{\"op\":\"drain\"}\n").unwrap();
    loop {
        let next = line(&mut reader);
        if next.contains("\"event\":\"end\"") {
            assert!(
                next.contains("\"exit\":0") && next.contains("\"cancelled\":false"),
                "{next}"
            );
            break;
        }
    }
    drop(reader);
    wait(|| !socket.exists());
}

#[test]
fn idle_cleanup_does_not_unlink_a_replacement_socket() {
    let tree = Tree::new();
    tree.start(&[("FERRET_DAEMON_IDLE_MS", "150")]);
    let socket = tree.socket();
    {
        let _ = tree.connect();
    }
    fs::remove_file(&socket).unwrap();
    let replacement = UnixListener::bind(&socket).unwrap();
    let ino = fs::symlink_metadata(&socket).unwrap().ino();
    wait(|| {
        tree.children.lock().unwrap()[0]
            .try_wait()
            .unwrap()
            .is_some()
    });
    assert_eq!(fs::symlink_metadata(&socket).unwrap().ino(), ino);
    drop(replacement);
}

#[test]
fn search_broken_pipe_is_quiet_and_sigint_cancels_without_killing_host() {
    use std::os::unix::process::ExitStatusExt;
    let tree = Tree::new();
    let pid = tree.start(&[]);
    let mut command = tree.command(&["search", "*"]);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    drop(child.stdout.take());
    wait(|| child.try_wait().unwrap().is_some());
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let format = "z".repeat(100_000);
    let mut command = fixture::command(FERRET, &tree.base);
    command
        .env_remove("FERRET_NO_DAEMON")
        .env("FERRET_DAEMON_BIN", DAEMON)
        .args(["find", "src", "-printf", &format])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let first = std::thread::spawn(move || {
        let mut bytes = [0; 4096];
        stdout.read_exact(&mut bytes).unwrap();
        stdout
    });
    wait(|| first.is_finished());
    child.stdout = Some(first.join().unwrap());
    assert!(
        Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let output = bounded_output(child);
    assert_eq!(output.status.signal(), Some(2));
    assert!(tree.run(&["search", "main"]).status.success());
    let (_, hello) = tree.connect();
    assert_eq!(number(&hello, "pid"), Some(pid));
}

#[test]
fn failed_manifest_read_keeps_the_daemons_last_checked_view() {
    let tree = Tree::new();
    assert!(tree.run(&["search", "main"]).status.success());
    tree.connect();
    let current = tree.base.join("index/current");
    let original = fs::read(&current).unwrap();
    // M5a owns publication; a failed manifest read must not invalidate an
    // already checked query pin. Recovery may retain this view until
    // successful.
    fs::write(&current, b"not a current header").unwrap();
    let search = tree.run(&["search", "main"]);
    assert!(search.status.success());
    assert!(!search.stdout.is_empty());
    let find = tree.run(&["find", "src", "-print"]);
    assert!(find.status.success());
    assert!(!find.stdout.is_empty());
    fs::write(current, original).unwrap();
    assert!(tree.run(&["search", "main"]).status.success());
}

#[test]
fn a_rebuilt_catalog_incarnation_is_confirmed_and_adopted() {
    let tree = Tree::new();
    assert!(tree.run(&["search", "main"]).status.success());
    let (_, old) = tree.connect();
    // M5a retains the writer lock: rebuilding the catalog now requires an
    // explicit drain/restart, rather than an unrelated second writer.
    let socket = tree.socket();
    let (mut control, _) = tree.connect();
    control
        .get_mut()
        .write_all(b"{\"op\":\"drain\"}\n")
        .unwrap();
    drop(control);
    wait(|| !socket.exists());
    fs::remove_file(tree.base.join("index/current")).unwrap();
    fs::write(tree.base.join("src/reborn.rs"), "reborn").unwrap();
    assert!(tree.run(&["index", "src"]).status.success());
    assert!(tree.run(&["search", "reborn"]).status.success());
    let (mut reader, new) = tree.connect();
    assert!(tree.run(&["search", "reborn"]).status.success());
    let incarnation = |hello: String| {
        hello
            .split_once("\"incarnation\":\"")
            .unwrap()
            .1
            .split('"')
            .next()
            .unwrap()
            .to_owned()
    };
    assert_ne!(incarnation(old), incarnation(new));
    assert_eq!(
        number(
            &block(&mut reader, b"{\"id\":\"s\",\"op\":\"status\"}\n"),
            "engine_opens"
        ),
        Some(1)
    );
    let log = fs::read_to_string(tree.base.join("home/state/ferret/log.jsonl")).unwrap();
    assert!(
        log.lines().last().unwrap().contains("\"host\":\"socket\""),
        "incarnation change stranded the client on fallback"
    );
}

#[test]
fn socket_request_line_limit_matches_batch_excluding_the_newline() {
    let tree = Tree::new();
    tree.start(&[]);
    let (mut reader, _) = tree.connect();
    let prefix = b"{\"id\":\"boundary\",\"op\":\"search\",\"args\":[\"main\"],\"padding\":\"";
    let suffix = b"\"}";
    let mut request = prefix.to_vec();
    request.extend(std::iter::repeat_n(
        b'x',
        (1 << 20) - prefix.len() - suffix.len(),
    ));
    request.extend_from_slice(suffix);
    assert_eq!(request.len(), 1 << 20);
    request.push(b'\n');
    assert!(block(&mut reader, &request).contains("\"exit\":0"));
    request.insert(request.len() - 3, b'x');
    reader.get_mut().write_all(&request).unwrap();
    let error = line(&mut reader);
    assert!(
        error.contains("\"id\":\"boundary\"") && error.contains("LineTooLong"),
        "{error}"
    );
    assert!(
        block(
            &mut reader,
            b"{\"id\":\"q\",\"op\":\"search\",\"args\":[\"main\"]}\n"
        )
        .contains("\"exit\":0")
    );
}

fn start_on_battery(tree: &Tree, extra: &[(&str, &str)]) -> u32 {
    let power = tree.base.join("signals/power/BAT0");
    fs::create_dir_all(&power).unwrap();
    fs::write(power.join("type"), "Battery\n").unwrap();
    fs::write(power.join("status"), "Discharging\n").unwrap();
    let power_root = tree.base.join("signals/power");
    let mut environment = vec![("FERRET_SIGNAL_POWER", power_root.to_str().unwrap())];
    environment.extend_from_slice(extra);
    tree.start(&environment)
}
fn exited(tree: &Tree, pid: u32) -> bool {
    tree.children
        .lock()
        .unwrap()
        .iter_mut()
        .find(|child| child.id() == pid)
        .unwrap()
        .try_wait()
        .unwrap()
        .is_some()
}

#[test]
fn battery_paused_startup_idle_exits_without_a_writer_command() {
    // A paused startup backstop used to leave writer_running latched forever.
    let tree = Tree::new();
    let pid = start_on_battery(&tree, &[("FERRET_DAEMON_IDLE_MS", "500")]);
    let socket = tree.socket();
    let (mut reader, _) = tree.connect();
    let status = block(&mut reader, b"{\"id\":\"s\",\"op\":\"status\"}\n");
    assert!(status.contains("battery-paused"), "{status}");
    assert_eq!(number(&status, "writer_commands"), Some(0));
    drop(reader);
    wait(|| !socket.exists() && exited(&tree, pid));
}

#[test]
fn battery_paused_startup_drains_without_a_writer_command() {
    let tree = Tree::new();
    let pid = start_on_battery(&tree, &[]);
    let socket = tree.socket();
    let (mut reader, _) = tree.connect();
    let status = block(&mut reader, b"{\"id\":\"s\",\"op\":\"status\"}\n");
    assert!(status.contains("battery-paused"), "{status}");
    assert_eq!(number(&status, "writer_commands"), Some(0));
    reader.get_mut().write_all(b"{\"op\":\"drain\"}\n").unwrap();
    // Keep the control connection alive; shutdown belongs to the host.
    wait(|| !socket.exists() && exited(&tree, pid));
}

/// User plus system CPU ticks consumed by a process, from /proc.
fn cpu_ticks(pid: u32) -> u64 {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let fields: Vec<_> = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .collect();
    fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
}

#[test]
#[cfg(debug_assertions)]
fn loading_blocks_expired_idle_deadline_without_spinning() {
    // /proc ticks observe CPU consumption, rather than inferring it from
    // elapsed time. Loading must keep ownership alive without polling.
    let tree = Tree::new();
    let pid = start_on_battery(
        &tree,
        &[
            ("FERRET_DAEMON_IDLE_MS", "100"),
            ("FERRET_DAEMON_LOAD_DELAY_MS", "3000"),
        ],
    );
    let ticks = || cpu_ticks(pid);
    let mut loading = BufReader::new(UnixStream::connect(tree.socket()).unwrap());
    loading.get_ref().set_read_timeout(Some(BOUND)).unwrap();
    assert!(line(&mut loading).contains("\"state\":\"loading\""));
    drop(loading);
    std::thread::sleep(Duration::from_millis(200));
    let before = ticks();
    std::thread::sleep(Duration::from_millis(500));
    let consumed = ticks() - before;
    assert!(
        consumed <= 2,
        "waiting daemon consumed {consumed} CPU ticks"
    );
    wait(|| exited(&tree, pid));
}

#[test]
fn drain_closes_a_separate_idle_connection_and_finishes_active_query() {
    // A handler blocked on its empty request channel used to miss drain.
    let tree = Tree::new();
    let pid = tree.start(&[]);
    let socket = tree.socket();
    let (mut idle, _) = tree.connect();
    assert!(
        block(
            &mut idle,
            b"{\"id\":\"q\",\"op\":\"search\",\"args\":[\"main\"]}\n"
        )
        .contains("\"exit\":0")
    );
    let mut active = begin_large(&tree);
    let (mut control, _) = tree.connect();
    control
        .get_mut()
        .write_all(b"{\"op\":\"drain\"}\n")
        .unwrap();
    let mut eof = String::new();
    assert_eq!(idle.read_line(&mut eof).unwrap(), 0);
    loop {
        let next = line(&mut active);
        if next.contains("\"event\":\"end\"") {
            assert!(
                next.contains("\"exit\":0") && next.contains("\"cancelled\":false"),
                "{next}"
            );
            break;
        }
    }
    wait(|| !socket.exists() && exited(&tree, pid));
}

#[test]
fn oversized_read_only_requests_answer_locally_without_reaching_or_starting_a_daemon() {
    // A request the protocol rejects, past its argv element or line limit,
    // used to become a transport failure on a ready daemon's connection.
    let tree = Tree::new();
    let search: Vec<&str> = std::iter::once("search")
        .chain(std::iter::repeat_n("main", 16_385))
        .collect();
    let pattern = "x".repeat(120_000);
    let mut find = vec!["find", "src", "-name", "main.rs"];
    for _ in 0..9 {
        find.extend(["-o", "-name", pattern.as_str()]);
    }
    assert!(tree.local(&search).status.success());
    // With no owner, nothing starts a daemon for a request it cannot send.
    parity(&tree, &search);
    assert!(tree.sockets().is_empty(), "{:?}", tree.sockets());
    tree.start(&[]);
    for args in [&search, &find] {
        parity(&tree, args);
    }
    let log = fs::read_to_string(tree.base.join("home/state/ferret/log.jsonl")).unwrap();
    assert!(
        !log.contains("\"host\":\"socket\""),
        "an oversized search reached the daemon: {log}"
    );
}

#[test]
fn draining_writer_waits_for_a_message_past_expired_background_deadlines() {
    // Drain forbids background work, so the writer used to spin on a zero
    // timeout once a poll deadline expired while a query kept drain open.
    let tree = Tree::new();
    let pid = tree.start(&[("FERRET_POLL_MS", "50")]);
    let socket = tree.socket();
    let mut active = begin_large(&tree);
    let (mut control, _) = tree.connect();
    control
        .get_mut()
        .write_all(b"{\"op\":\"drain\"}\n")
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let before = cpu_ticks(pid);
    std::thread::sleep(Duration::from_millis(500));
    let consumed = cpu_ticks(pid) - before;
    assert!(
        consumed <= 2,
        "draining daemon consumed {consumed} CPU ticks"
    );
    loop {
        if line(&mut active).contains("\"event\":\"end\"") {
            break;
        }
    }
    drop(active);
    wait(|| !socket.exists() && exited(&tree, pid));
}

#[test]
#[cfg(debug_assertions)]
fn a_reader_exit_with_a_queued_bad_line_never_strands_its_handler() {
    // The drain registry holds a clone of each handler's request sender, so a
    // reader exit no longer disconnects the channel. When the exit wake met a
    // full channel, the handler answered the queued line and blocked forever,
    // holding its client count and so the daemon's idle exit.
    let tree = Tree::new();
    let pid = tree.start(&[
        ("FERRET_DAEMON_IDLE_MS", "300"),
        ("FERRET_DAEMON_TEST_READER_EXIT_DELAY_MS", "300"),
    ]);
    let socket = tree.socket();
    let (mut client, _) = tree.connect();
    client
        .get_mut()
        .write_all(&b"not json\n".repeat(1_000))
        .unwrap();
    // Half-close: the handler's error replies still succeed.
    client
        .get_ref()
        .shutdown(std::net::Shutdown::Write)
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    drop(client);
    wait(|| !socket.exists() && exited(&tree, pid));
}

/// Exercise the parser on the daemon's request thread, not CLI parsing.
#[test]
fn socket_search_rejects_excessive_boolean_nesting_and_stays_alive() {
    let tree = Tree::new();
    tree.start(&[]);
    for (nots, groups) in [(33, 0), (0, 33), (17, 16), (4096, 0)] {
        let args: Vec<_> = std::iter::repeat_n("\"NOT\"", nots)
            .chain(std::iter::repeat_n("\"(\"", groups))
            .chain(["\"text:x\""])
            .chain(std::iter::repeat_n("\")\"", groups))
            .collect();
        let request = format!(
            "{{\"id\":\"nest\",\"op\":\"search\",\"args\":[{}]}}\n",
            args.join(",")
        );
        let (mut reader, _) = tree.connect();
        let reply = block(&mut reader, request.as_bytes());
        assert!(
            reply.contains("query nesting exceeds 32 NOT/parenthesis levels"),
            "{reply}"
        );
    }
    let (mut reader, _) = tree.connect();
    assert!(
        block(
            &mut reader,
            b"{\"id\":\"nest\",\"op\":\"search\",\"args\":[\"main\"]}\n"
        )
        .contains("\"event\":\"row\"")
    );
}

/// Flat siblings must not become recursive cursor wrappers on the real daemon
/// stack, including materialisation, cost estimation and destruction.
#[test]
fn socket_search_handles_eight_thousand_flat_content_siblings() {
    let tree = Tree::new();
    // Explicit indexing also follows content, so absent terms have exact empty
    // postings rather than an uncovered bitmap that hides the cursor shape.
    tree.start(&[]);
    for shape in ["exclusions", "and", "or"] {
        let mut args = vec!["(".to_owned(), "text:absentpositive".to_owned()];
        for _ in 0..8000 {
            match shape {
                "exclusions" => args.push("NOT".to_owned()),
                "or" => args.push("OR".to_owned()),
                _ => {}
            }
            args.push("text:absentsibling".to_owned());
        }
        args.extend([")", "OR", "text:absentouter"].map(str::to_owned));
        let quoted = args
            .iter()
            .map(|a| format!("\"{a}\""))
            .collect::<Vec<_>>()
            .join(",");
        let request = format!("{{\"id\":\"wide\",\"op\":\"search\",\"args\":[{quoted}]}}\n");
        let (mut reader, _) = tree.connect();
        let reply = block(&mut reader, request.as_bytes());
        assert!(
            reply.contains("\"exit\":1"),
            "{shape}: {}",
            &reply[..reply.len().min(400)]
        );
        assert!(!reply.contains("\"event\":\"row\""), "{shape}: {reply}");
    }
    let (mut reader, _) = tree.connect();
    assert!(
        block(
            &mut reader,
            b"{\"id\":\"alive\",\"op\":\"search\",\"args\":[\"main\"]}\n"
        )
        .contains("\"event\":\"row\"")
    );
}
