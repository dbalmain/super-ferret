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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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
        let indexed = tree.local(&["index", "src"]);
        assert!(indexed.status.success(), "{indexed:?}");
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
        self.start_at(extra, &self.base)
    }
    fn start_at(&self, extra: &[(&str, &str)], cwd: &std::path::Path) -> u32 {
        self.start_command(extra, cwd, fixture::command(DAEMON, &self.base))
    }
    fn start_limited(&self, limits: &str) -> u32 {
        let mut command = fixture::command("bash", &self.base);
        command.args(["-c", &format!("{limits}; exec \"$@\""), "bash", DAEMON]);
        self.start_command(&[], &self.base, command)
    }
    fn start_command(
        &self,
        extra: &[(&str, &str)],
        cwd: &std::path::Path,
        mut cmd: Command,
    ) -> u32 {
        cmd.current_dir(cwd);
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
    fn fake_owner(&self) -> (UnixListener, String, PathBuf) {
        self.start(&[]);
        let (_, hello) = self.connect();
        let socket = self.socket();
        {
            let mut children = self.children.lock().unwrap();
            children[0].kill().unwrap();
            children[0].wait().unwrap();
        }
        fs::remove_file(&socket).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        (listener, hello, socket)
    }
    fn connect(&self) -> (BufReader<UnixStream>, String) {
        ready(UnixStream::connect(self.socket()).unwrap())
    }
    fn try_wait(&self, pid: u32) -> Option<std::process::ExitStatus> {
        self.children
            .lock()
            .unwrap()
            .iter_mut()
            .find(|child| child.id() == pid)?
            .try_wait()
            .unwrap()
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
fn accept_peer(listener: &UnixListener) -> UnixStream {
    let mut peer = None;
    wait(|| match listener.accept() {
        Ok((stream, _)) => {
            peer = Some(stream);
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(error) => panic!("accept: {error}"),
    });
    peer.unwrap()
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
    for args in [
        &["search", "main"][..],
        &["find", "src", "-name", "main.rs"],
        &["grep", "main", "src"],
        &["grep", "--files", "src"],
    ] {
        for mode in [0o755, 0o770] {
            let tree = Tree::new();
            let runtime = tree.base.join("home/runtime");
            fs::set_permissions(&runtime, fs::Permissions::from_mode(mode)).unwrap();
            parity(&tree, args);
            assert!(fs::read_dir(&runtime).unwrap().next().is_none());
        }
        let tree = Tree::new();
        let target = tree.base.join("target");
        fs::create_dir(&target).unwrap();
        symlink(&target, tree.base.join("home/runtime/ferret")).unwrap();
        parity(&tree, args);
        assert!(fs::read_dir(target).unwrap().next().is_none());
        let tree = Tree::new();
        let runtime = tree.base.join("home/runtime");
        // root-owned /proc is an existing wrong-owner directory without
        // privileges.
        let out = tree
            .command(args)
            .env("XDG_RUNTIME_DIR", "/proc")
            .output()
            .unwrap();
        assert_eq!(out.stdout, tree.local(args).stdout);
        assert!(fs::read_dir(runtime).unwrap().next().is_none());
    }
}

#[test]
fn bypass_missing_runtime_denied_spawn_and_loading_timeout_fall_back() {
    for args in [&["search", "main"][..], &["grep", "main", "src"]] {
        for case in 0..if cfg!(debug_assertions) { 4 } else { 3 } {
            let tree = Tree::new();
            let mut cmd = tree.command(args);
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
            let local = tree.local(args);
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
    for args in [
        &["search", "main"][..],
        &["find", "src", "-name", "main.rs"],
        &["grep", "main", "src"],
        &["grep", "--files", "src"],
    ] {
        let tree = Tree::new();
        let old_pid = tree.start(&[("FERRET_DAEMON_TEST_BUILD", "old-build")]);
        let mut active = begin_large(&tree);
        let socket = tree.socket();
        let child = tree
            .command(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // The old query is blocked, so graceful replacement must still be
        // waiting.
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
        let local = tree.local(args);
        assert_eq!(output.stdout, local.stdout, "{args:?}");
        assert_eq!(output.stderr, local.stderr, "{args:?}");
        let (_, hello) = tree.connect();
        assert_ne!(number(&hello, "pid"), Some(old_pid));
        assert!(!hello.contains("old-build"));
    }
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
    // Rust's spawn keeps an inherited SIGINT ignore and an inherited block; it
    // only resets SIGPIPE. A non-interactive shell cannot undo an ignore it was
    // started with (`trap - INT` leaves SIG_IGN), so `kill -INT` is a no-op and
    // find exits 0 once the pipe drains. Establish the default in this child.
    let mut command = fixture::command(std::env::var_os("FERRET_PYTHON").unwrap(), &tree.base);
    command
        .env_remove("FERRET_NO_DAEMON")
        .env("FERRET_DAEMON_BIN", DAEMON)
        .args([
            "-c",
            "import os, signal, sys\n\
             signal.pthread_sigmask(signal.SIG_UNBLOCK, {signal.SIGINT})\n\
             signal.signal(signal.SIGINT, signal.SIG_DFL)\n\
             os.execvp(sys.argv[1], sys.argv[1:])\n",
            FERRET,
            "find",
            "src",
            "-printf",
            &format,
        ])
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
fn a_writer_that_meets_a_draining_owner_waits_and_then_indexes_in_process() {
    // A drained owner closes new connections before hello; writer commands
    // used to fail with exit 3 instead of waiting for it to go (D110).
    let tree = Tree::new();
    let pid = tree.start(&[]);
    let socket = tree.socket();
    let (mut idle, _) = tree.connect();
    let mut active = begin_large(&tree);
    let (mut control, _) = tree.connect();
    control
        .get_mut()
        .write_all(b"{\"op\":\"drain\"}\n")
        .unwrap();
    let mut eof = String::new();
    assert_eq!(idle.read_line(&mut eof).unwrap(), 0);
    fs::write(tree.base.join("src/after-drain.txt"), "x").unwrap();
    let mut child = tree
        .command(&["index", "src"])
        .env("FERRET_DAEMON_STARTUP_MS", "10000")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "the writer did not wait for the draining owner"
    );
    while !line(&mut active).contains("\"event\":\"end\"") {}
    drop(active);
    wait(|| !socket.exists() && exited(&tree, pid));
    let indexed = bounded_output(child);
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    assert!(tree.local(&["search", "after-drain.txt"]).status.success());
}

/// An owner that accepts and closes every connection before hello, as an
/// exiting or draining ferretd does, until `stop` is set. Returns the socket
/// and the number of connections it has closed.
fn closing_owner(
    tree: &Tree,
) -> (
    PathBuf,
    Arc<AtomicUsize>,
    Arc<AtomicBool>,
    std::thread::JoinHandle<()>,
) {
    let (listener, _, socket) = tree.fake_owner();
    let closed = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let owner = {
        let closed = closed.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        closed.fetch_add(1, Ordering::SeqCst);
                        drop(stream);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            }
        })
    };
    (socket, closed, stop, owner)
}

#[test]
fn a_query_falls_back_at_once_and_a_writer_waits_for_a_leaving_owner() {
    let tree = Tree::new();
    let (socket, closed, stop, owner) = closing_owner(&tree);
    let started = Instant::now();
    let searched = tree
        .command(&["search", "main"])
        .env("FERRET_DAEMON_STARTUP_MS", "10000")
        .output()
        .unwrap();
    assert!(searched.status.success());
    assert!(started.elapsed() < BOUND, "a query waited for the owner");
    assert_eq!(closed.load(Ordering::SeqCst), 1);

    fs::write(tree.base.join("src/after-exit.txt"), "x").unwrap();
    let child = tree
        .command(&["index", "src"])
        .env("FERRET_DAEMON_STARTUP_MS", "10000")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait(|| closed.load(Ordering::SeqCst) == 2);
    fs::remove_file(&socket).unwrap();
    stop.store(true, Ordering::SeqCst);
    owner.join().unwrap();
    let indexed = bounded_output(child);
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    assert_eq!(closed.load(Ordering::SeqCst), 2);
    assert!(tree.local(&["search", "after-exit.txt"]).status.success());
}

#[test]
fn a_writer_times_out_on_an_owner_that_closes_and_never_leaves() {
    // The client cap closes without hello and keeps its socket; the wait is
    // bounded by the startup deadline and reports the owner timeout.
    let tree = Tree::new();
    let (_, closed, stop, owner) = closing_owner(&tree);
    let started = Instant::now();
    let indexed = tree
        .command(&["index", "src"])
        .env("FERRET_DAEMON_STARTUP_MS", "400")
        .output()
        .unwrap();
    let elapsed = started.elapsed();
    stop.store(true, Ordering::SeqCst);
    owner.join().unwrap();
    assert_eq!(indexed.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&indexed.stderr).contains("daemon owner did not become ready"),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    assert!(
        (Duration::from_millis(400)..BOUND).contains(&elapsed),
        "{elapsed:?}"
    );
    assert_eq!(closed.load(Ordering::SeqCst), 1);
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
    let mut grep = vec!["grep", "main", "src"];
    grep.extend(std::iter::repeat_n("--scan", 16_385));
    assert!(tree.local(&search).status.success());
    // With no owner, nothing starts a daemon for a request it cannot send.
    parity(&tree, &search);
    parity(&tree, &grep);
    assert!(tree.sockets().is_empty(), "{:?}", tree.sockets());
    tree.start(&[]);
    for args in [&search, &find, &grep] {
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

#[test]
fn grep_argv_through_socket_returns_verified_rows_and_offsets() {
    let tree = Tree::new();
    fs::write(tree.base.join("src/main.rs"), b"before\nNeedle123\nafter\n").unwrap();
    fs::write(
        tree.base.join("src/false-positive.txt"),
        b"needle xxx 123\n",
    )
    .unwrap();
    for i in 0..8 {
        fs::write(
            tree.base.join(format!("src/other{i}.txt")),
            format!("other{i}\n"),
        )
        .unwrap();
    }
    assert!(tree.local(&["index", "src"]).status.success());
    tree.start(&[]);
    // D67 A2 must survive argv forwarding through the daemon protocol.
    let sensitive = tree.run(&["search", "grep:^needle[0-9]+$"]);
    assert_eq!(sensitive.status.code(), Some(1));
    let got = tree.run(&["search", "--json", "grep:(?i)^needle[0-9]+$"]);
    assert!(
        got.status.success(),
        "{}",
        String::from_utf8_lossy(&got.stderr)
    );
    let output = String::from_utf8(got.stdout).unwrap();
    assert_eq!(output.lines().count(), 1);
    assert!(output.contains("main.rs"));
    assert!(output.contains("\"offset\":7"));
    let refused = tree.run(&["search", r"grep:\d+"]);
    assert_eq!(refused.status.code(), Some(3));
    let scanned = tree.run(&["search", "--scan", r"grep:\d+"]);
    assert!(scanned.status.success());
}

// D111: both hosts must render the same resolved plan. JSON includes measured
// durations, so only those volatile values are removed before byte comparison.
fn without_grep_durations(bytes: &[u8]) -> Vec<u8> {
    let mut text = String::from_utf8(bytes.to_vec()).unwrap();
    for key in ["\"elapsed\":{", "\"elapsed_total\":{"] {
        let mut offset = 0;
        while let Some(found) = text[offset..].find(key) {
            let start = offset + found + key.len();
            let end = start + text[start..].find('}').unwrap();
            text.replace_range(start..end, "");
            offset = start + 1;
        }
    }
    text.into_bytes()
}

#[test]
fn directory_grep_matches_local_output_errors_and_exits() {
    let tree = Tree::new();
    fs::write(
        tree.base.join("src/main.rs"),
        b"before\nred red\nafter\nblue\nred\n",
    )
    .unwrap();
    fs::write(tree.base.join("src/binary"), b"red\0blue\n").unwrap();
    fs::write(tree.base.join("patterns"), b"red\n").unwrap();
    fs::write(tree.base.join("unreadable"), b"red\n").unwrap();
    fs::set_permissions(
        tree.base.join("unreadable"),
        fs::Permissions::from_mode(0o0),
    )
    .unwrap();
    assert!(tree.local(&["index", "src"]).status.success());
    tree.start_at(
        &[
            ("FERRET_SEARCH_THREADS", "7"),
            ("NO_COLOR", "1"),
            ("TERM", "dumb"),
        ],
        std::path::Path::new("/"),
    );
    let matrix: &[&[&str]] = &[
        &["--files", "src"],
        &["--files", "-0", "src"],
        &["--files", "--color=always", "src"],
        &["--files", "-g", "*.rs", "src"],
        &["--files", "--hidden", "src"],
        &["--files", "src", "missing", "src/main.rs"],
        &["--files", "--max-depth", "0", "src"],
        &["--files", "-q", "missing", "src"],
        &["--files", "-q", "src"],
        &["--files", "-f", "missing-patterns", "src"],
        &["--files", "src", "-"],
        &["--files", "src/main.rs", "src"],
        &["red", "src"],
        &["-l", "red", "src"],
        &["-c", "red", "src"],
        &["--json", "red", "src"],
        &["-A1", "-B1", "red", "src"],
        &["red", "missing", "src", "src/main.rs"],
        &["-a", "red", "src"],
        &["--color=always", "-n", "--heading", "red", "src"],
        &["--color=ansi", "-0l", "red", "src", "src/main.rs"],
        &["--include-zero", "-c", "no-such-match", "src"],
        &["-q", "red", "missing", "src"],
        &["-q", "red", "src"],
        &["-f", "patterns", "src"],
        &["-l", "red", "src", "src/main.rs"],
        &["--files-without-match", "red", "src"],
        &["-l", "red", "src/main.rs", "unreadable", "src"],
        &["-0l", "red", "src/main.rs", "unreadable", "src"],
    ];
    for case in matrix {
        let mut args = vec!["grep"];
        args.extend_from_slice(case);
        let run = |local| {
            let mut command = tree.command(&args);
            command
                .env("FERRET_SEARCH_THREADS", "1")
                .env_remove("NO_COLOR")
                .env("TERM", "xterm");
            if local {
                command.env("FERRET_NO_DAEMON", "1");
            }
            command.output().unwrap()
        };
        let local = run(true);
        let remote = run(false);
        assert_eq!(remote.status.code(), local.status.code(), "{case:?}");
        assert_eq!(remote.stderr, local.stderr, "{case:?}");
        if case.contains(&"--json") {
            assert_eq!(
                without_grep_durations(&remote.stdout),
                without_grep_durations(&local.stdout),
                "{case:?}"
            );
        } else {
            assert_eq!(remote.stdout, local.stdout, "{case:?}");
            let merged = |local| {
                let mut command = fixture::bounded_command("bash", &tree.base);
                command
                    .env_remove("FERRET_NO_DAEMON")
                    .env("FERRET_SEARCH_THREADS", "1")
                    .args(["-c", r#"exec "$@" 2>&1"#, "grep-order", FERRET])
                    .args(&args);
                if local {
                    command.env("FERRET_NO_DAEMON", "1");
                }
                command.output().unwrap()
            };
            assert_eq!(merged(false).stdout, merged(true).stdout, "merged {case:?}");
        }
    }
    // --files must still default to '.' with piped stdin, under client cwd.
    let default_files = |local| {
        let mut command = tree.command(&["grep", "--files"]);
        command
            .current_dir(tree.base.join("src"))
            .stdin(Stdio::piped());
        if local {
            command.env("FERRET_NO_DAEMON", "1");
        }
        command.output().unwrap()
    };
    let local = default_files(true);
    let remote = default_files(false);
    assert_eq!(remote.status.code(), local.status.code());
    assert_eq!(remote.stdout, local.stdout);
    assert_eq!(remote.stderr, local.stderr);
    let closed_stderr = |local| {
        let mut command = fixture::bounded_command("bash", &tree.base);
        command
            .env_remove("FERRET_NO_DAEMON")
            .env("FERRET_SEARCH_THREADS", "1")
            .args([
                "-c",
                r#"exec "$@" 2>&-"#,
                "grep-stderr",
                FERRET,
                "grep",
                "red",
                "missing",
                "src",
            ]);
        if local {
            command.env("FERRET_NO_DAEMON", "1");
        }
        command.output().unwrap()
    };
    let local = closed_stderr(true);
    let remote = closed_stderr(false);
    assert_eq!(remote.status.code(), local.status.code());
    assert_eq!(remote.stdout, local.stdout);
    // Pattern stdin belongs to the client too; it cannot be read by ferretd.
    let mut command = tree.command(&["grep", "-f", "-", "src"]);
    command
        .env("FERRET_SEARCH_THREADS", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"red\n").unwrap();
    let output = bounded_output(child);
    assert!(output.status.success());
    assert_eq!(output.stdout, tree.local(&["grep", "red", "src"]).stdout);
}

#[test]
fn grep_pipe_close_and_disconnect_release_workers_and_all_query_slots() {
    let tree = Tree::new();
    // Explicit inputs have no indexing size cap. Mixing in a directory routes
    // this large input through the daemon without a large index fixture.
    fs::write(
        tree.base.join("dense"),
        b"red red red red\n".repeat(2_000_000),
    )
    .unwrap();
    let pid = tree.start(&[]);
    let tasks = || fs::read_dir(format!("/proc/{pid}/task")).unwrap().count();
    let baseline = tasks();
    let mut clients = Vec::new();
    for _ in 0..4 {
        let mut child = tree
            .command(&["grep", "red", "dense", "src"])
            .env("FERRET_SEARCH_THREADS", "2")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let first = std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.contains("red"));
            reader
        });
        wait(|| first.is_finished());
        clients.push((child, first.join().unwrap()));
    }
    for (mut child, pipe) in clients {
        drop(pipe); // the same close as `| head -1`
        wait(|| child.try_wait().unwrap().is_some());
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty(), "{:?}", output.stderr);
    }
    wait(|| tasks() <= baseline);
    assert!(tree.run(&["grep", "main", "src"]).status.success());
    // Also exercise a raw peer hangup while the socket writer is blocked.
    let mut peers = Vec::new();
    for _ in 0..4 {
        let (mut peer, _) = tree.connect();
        let request = format!(
            r#"{{"id":"g","op":"grep","args":["red","dense","src"],"cwd":"{}","grep":{{"color":false,"heading":false,"numbers":false,"tty":false,"threads":2,"pattern_files":[],"temp_dir":"/tmp"}}}}"#,
            tree.base.display()
        );
        peer.get_mut()
            .write_all(format!("{request}\n").as_bytes())
            .unwrap();
        assert!(line(&mut peer).contains("\"event\":\"begin\""));
        assert!(line(&mut peer).contains("\"event\":\"stdout\""));
        peers.push(peer);
    }
    drop(peers);
    wait(|| tasks() <= baseline);
    assert!(tree.run(&["search", "main"]).status.success());
}

#[test]
fn daemon_grep_uses_the_clients_terminal_defaults() {
    let tree = Tree::new();
    tree.start(&[("NO_COLOR", "1"), ("TERM", "dumb")]);
    for args in [
        &["grep", "main", "src"][..],
        &["grep", "--files", "src"],
        &["grep", "--files", "-0", "src"],
    ] {
        for (term, no_color) in [("xterm", false), ("dumb", false), ("xterm", true)] {
            let run = |local| {
                let mut command = fixture::bounded_command(
                    std::env::var_os("FERRET_PYTHON").unwrap(),
                    &tree.base,
                );
                command
                    .env_remove("FERRET_NO_DAEMON")
                    .env("FERRET_SEARCH_THREADS", "1")
                    .env("TERM", term)
                    .env_remove("NO_COLOR")
                    .args([
                        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/grep_pty.py"),
                        FERRET,
                    ])
                    .args(args);
                if no_color {
                    command.env("NO_COLOR", "1");
                }
                if local {
                    command.env("FERRET_NO_DAEMON", "1");
                }
                command.output().unwrap()
            };
            let local = run(true);
            let remote = run(false);
            assert!(remote.status.success(), "{remote:?}");
            assert_eq!(remote.stdout, local.stdout, "{term}, NO_COLOR={no_color}");
            assert_eq!(remote.stderr, local.stderr);
        }
    }
}

#[test]
#[cfg(debug_assertions)]
fn daemon_grep_reports_thread_shortfalls_once_per_request() {
    let tree = Tree::new();
    tree.start(&[("FERRET_TEST_REFUSE_THREADS", "ferret-verify=0")]);
    let mut command = tree.command(&["grep", "main", "src"]);
    command.env("FERRET_SEARCH_THREADS", "4");
    for _ in 0..2 {
        let actual = command.output().unwrap();
        let expected = tree
            .command(&["grep", "main", "src"])
            .env("FERRET_SEARCH_THREADS", "4")
            .env("FERRET_TEST_REFUSE_THREADS", "ferret-verify=0")
            .output()
            .unwrap();
        assert!(actual.status.success());
        assert_eq!(actual.stdout, expected.stdout);
        assert_eq!(actual.stderr, expected.stderr);
        assert!(String::from_utf8_lossy(&actual.stderr).contains("searching with 1 of 4 threads"));
    }
}

#[test]
fn grep_falls_back_when_a_peer_closes_before_hello_or_refuses_it() {
    for args in [&["grep", "main", "src"][..], &["grep", "--files", "src"]] {
        for refuse in [false, true] {
            let tree = Tree::new();
            let directory = tree.base.join("home/runtime/ferret");
            fs::create_dir(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            let metadata = fs::metadata(tree.base.join("index")).unwrap();
            let socket = directory.join(format!("{:x}-{:x}.sock", metadata.dev(), metadata.ino()));
            let listener = UnixListener::bind(&socket).unwrap();
            fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let peer = std::thread::spawn(move || {
                let until = Instant::now() + BOUND;
                loop {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            if refuse {
                                stream.write_all(b"{}\n").unwrap();
                            }
                            return;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < until,
                                "grep never attempted its daemon route"
                            );
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("accept: {error}"),
                    }
                }
            });
            parity(&tree, args);
            peer.join().unwrap();
        }
    }
}

#[test]
fn a_client_accepts_legacy_frames_when_raw_output_is_not_advertised() {
    let tree = Tree::new();
    let (listener, hello, socket) = tree.fake_owner();
    let server = std::thread::spawn(move || {
        let mut stream = accept_peer(&listener);
        stream.set_read_timeout(Some(BOUND)).unwrap();
        stream
            .write_all(hello.replace(",\"raw-output-v1\"", "").as_bytes())
            .unwrap();
        let mut reader = BufReader::new(stream);
        let request = line(&mut reader);
        assert!(request.contains("\"op\":\"grep\""));
        reader.get_mut().write_all(b"{\"id\":\"grep\",\"event\":\"begin\"}\n{\"id\":\"grep\",\"event\":\"stdout\",\"bytes_base64\":\"bGVnYWN5Cg==\"}\n{\"id\":\"grep\",\"event\":\"end\",\"exit\":0}\n").unwrap();
    });
    let output = tree.run(&["grep", "main", "src"]);
    server.join().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"legacy\n");
    assert!(output.stderr.is_empty());
    fs::remove_file(socket).unwrap();
}

// Opt-in is per request, including when a legacy request follows a raw one
// on the same connection. Existing clients must never receive raw payloads.
#[test]
fn raw_output_negotiation_is_per_request_and_preserves_legacy_peers() {
    let tree = Tree::new();
    tree.start(&[]);
    let (mut peer, hello) = tree.connect();
    assert!(hello.contains("raw-output-v1"));
    for raw in [false, true, false] {
        let capabilities = if raw { r#"["raw-output-v1"]"# } else { "[]" };
        let request = format!(
            r#"{{"id":"g","op":"grep","args":["main","src"],"cwd":"{}","capabilities":{},"grep":{{"color":false,"heading":false,"numbers":false,"tty":false,"threads":1,"pattern_files":[],"temp_dir":"/tmp"}}}}"#,
            tree.base.display(),
            capabilities
        );
        peer.get_mut()
            .write_all(format!("{request}\n").as_bytes())
            .unwrap();
        assert!(line(&mut peer).contains("\"event\":\"begin\""));
        let header = line(&mut peer);
        assert!(header.contains("\"event\":\"stdout\""));
        if raw {
            let expected = b"src/main.rs:fn main() {}\n";
            assert_eq!(number(&header, "length"), Some(expected.len() as u32));
            let mut bytes = vec![0; expected.len()];
            peer.read_exact(&mut bytes).unwrap();
            assert_eq!(bytes, expected);
        } else {
            assert!(header.contains("\"bytes_base64\":"));
            assert!(!header.contains("\"length\":"));
        }
        loop {
            let next = line(&mut peer);
            if next.contains("\"event\":\"end\"") {
                assert_eq!(number(&next, "exit"), Some(0));
                break;
            }
        }
        // A normal quiet listing must not cancel the connection; the next
        // iteration reuses this same peer and can negotiate another codec.
        let quiet = request.replace(r#"["main","src"]"#, r#"["--files","-q","src"]"#);
        peer.get_mut()
            .write_all(format!("{quiet}\n").as_bytes())
            .unwrap();
        assert!(line(&mut peer).contains("\"event\":\"begin\""));
        let end = line(&mut peer);
        assert!(end.contains("\"event\":\"end\""));
        assert_eq!(number(&end, "exit"), Some(0));
        assert!(end.contains("\"cancelled\":false"), "{end}");
    }
}

// Search starts a daemon and answers its first query locally. Grep must do the
// same, including --files, rather than waiting for the new owner to load.
#[test]
#[cfg(debug_assertions)]
fn directory_grep_starts_one_daemon_and_answers_first_locally() {
    for args in [&["grep", "main", "src"][..], &["grep", "--files", "src"]] {
        let tree = Tree::new();
        assert!(tree.sockets().is_empty());
        let first = tree
            .command(args)
            .env("FERRET_DAEMON_LOAD_DELAY_MS", "5000")
            .output()
            .unwrap();
        let local = tree.local(args);
        assert!(first.status.success(), "{first:?}");
        assert_eq!(first.stdout, local.stdout);
        assert_eq!(first.stderr, local.stderr);
        wait(|| fixture_daemons(&tree.base).len() == 1 && tree.sockets().len() == 1);
        let stream = UnixStream::connect(tree.socket()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut peer = BufReader::new(stream);
        // The first query has already completed while the server cannot yet
        // execute any query: its output necessarily came from the local host.
        assert!(line(&mut peer).contains("\"state\":\"loading\""));
        loop {
            if line(&mut peer).contains("\"state\":\"ready\"") {
                break;
            }
        }
        let second = tree.run(args);
        assert!(second.status.success(), "{second:?}");
        assert_eq!(second.stdout, first.stdout);
        assert_eq!(second.stderr, first.stderr);
        assert_eq!(fixture_daemons(&tree.base).len(), 1);
    }
}

#[test]
fn find_and_grep_refuse_the_same_incompatible_owner_hellos() {
    let tree = Tree::new();
    let (listener, hello, socket) = tree.fake_owner();
    let mutations = [
        ("\"major\":1", "\"major\":99"),
        ("\"index\":\"", "\"index\":\"wrong-"),
        ("\"context\":\"", "\"context\":\"wrong-"),
        ("\"incarnation\":\"", "\"incarnation\":\"wrong-"),
    ];
    let server = std::thread::spawn(move || {
        for (from, to) in mutations {
            assert!(hello.contains(from));
            for _ in 0..3 {
                let mut stream = accept_peer(&listener);
                stream.set_read_timeout(Some(BOUND)).unwrap();
                stream
                    .write_all(hello.replace(from, to).as_bytes())
                    .unwrap();
                let mut request = Vec::new();
                stream.read_to_end(&mut request).unwrap();
                assert!(request.is_empty(), "query sent to an incompatible owner");
            }
        }
    });
    for _ in mutations {
        for args in [
            &["find", "src", "-name", "main.rs"][..],
            &["grep", "main", "src"],
            &["grep", "--files", "src"],
        ] {
            parity(&tree, args);
        }
    }
    server.join().unwrap();
    fs::remove_file(socket).unwrap();
}

// A socket caller used to size verifier/capture pools before any server grant,
// so a typo could abort every connection and the resident writer.
#[test]
fn daemon_grep_bounds_client_threads_before_allocating_and_survives_bad_counts() {
    let tree = Tree::new();
    tree.start_limited("ulimit -v 2097152; ulimit -c 0");
    let (mut peer, _) = tree.connect();
    for threads in [100_000_000_u64, 0, u64::MAX, 1] {
        let request = format!(
            r#"{{"id":"g","op":"grep","args":["main","src"],"cwd":"{}","grep":{{"color":false,"heading":false,"numbers":false,"tty":false,"threads":{},"pattern_files":[],"temp_dir":"/tmp"}}}}"#,
            tree.base.display(),
            threads
        );
        peer.get_mut()
            .write_all(format!("{request}\n").as_bytes())
            .unwrap();
        if threads == 0 || threads == u64::MAX {
            let response = line(&mut peer);
            assert!(response.contains("\"event\":\"error\""), "{response}");
            assert!(response.contains("invalid field"), "{response}");
        } else {
            let response = block(&mut peer, b"");
            assert!(response.contains("\"event\":\"end\""), "{response}");
            assert_eq!(number(&response, "exit"), Some(0), "{response}");
        }
    }
    assert!(tree.run(&["grep", "main", "src"]).status.success());
}

// Under NOFILE=64 the process has two read admissions. A stopped publisher
// must not hold both while an independent admitted request has no way forward.
#[test]
fn stopped_grep_consumer_leaves_a_descriptor_share_for_another_request() {
    let tree = Tree::new();
    fs::write(tree.base.join("src/a"), b"red\n").unwrap();
    assert!(tree.local(&["index", "src"]).status.success());
    let mut dense = fs::File::create(tree.base.join("dense")).unwrap();
    let mut record = b"red ".to_vec();
    record.extend(std::iter::repeat_n(b'x', 8187));
    record.push(b'\n');
    for _ in 0..3072 {
        dense.write_all(&record).unwrap();
    }
    drop(dense);
    tree.start_limited("ulimit -n 64");
    let (mut slow, _) = tree.connect();
    let request = format!(
        r#"{{"id":"slow","op":"grep","args":["red","dense","dense","src"],"cwd":"{}","grep":{{"color":false,"heading":false,"numbers":false,"tty":false,"threads":2,"pattern_files":[],"temp_dir":"/tmp"}}}}"#,
        tree.base.display()
    );
    slow.get_mut()
        .write_all(format!("{request}\n").as_bytes())
        .unwrap();
    assert!(line(&mut slow).contains("\"event\":\"begin\""));
    assert!(line(&mut slow).contains("\"event\":\"stdout\""));
    // Leave this socket open without consuming any more output. Give the
    // second reader time to fill capture space behind the blocked publisher.
    std::thread::sleep(Duration::from_millis(250));
    let (mut quiet, _) = tree.connect();
    quiet
        .get_mut()
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let request = request
        .replace("\"slow\"", "\"quiet\"")
        .replace(r#"["red","dense","dense","src"]"#, r#"["-q","red","src"]"#);
    let response = block(&mut quiet, format!("{request}\n").as_bytes());
    assert!(response.contains("\"event\":\"end\""), "{response}");
    assert_eq!(number(&response, "exit"), Some(0), "{response}");
    drop(slow);
    assert!(tree.run(&["grep", "-q", "red", "src"]).status.success());
}

// A before-context window over long lines spills to disk. The daemon must
// spill where the client's own run would, never into its startup TMPDIR,
// which here no longer exists.
#[test]
fn daemon_grep_spills_context_into_the_clients_temporary_directory() {
    let tree = Tree::new();
    fs::write(tree.base.join("src/a"), b"red\n").unwrap();
    let mut context = Vec::new();
    for _ in 0..100 {
        context.extend_from_slice(&[b'x'; 1024]);
        context.push(b'\n');
    }
    context.extend_from_slice(b"red\n");
    fs::write(tree.base.join("src/context"), context).unwrap();
    assert!(tree.local(&["index", "src"]).status.success());
    fs::create_dir(tree.base.join("client-tmp")).unwrap();
    let missing = tree.base.join("missing-tmp");
    tree.start_at(
        &[("TMPDIR", missing.to_str().unwrap())],
        std::path::Path::new("/"),
    );
    let client = tree.base.join("client-tmp");
    let gone = tree.base.join("client-gone");
    for temp in [client.as_os_str(), "client-tmp".as_ref(), gone.as_os_str()] {
        let grep = |local: bool| {
            let mut command = tree.command(&["grep", "-B100", "red", "src"]);
            // Both hosts publish files as workers finish. A byte-for-byte
            // compare is the spill check, so one thread keeps walk order.
            command.env("FERRET_SEARCH_THREADS", "1");
            command.env("TMPDIR", temp);
            if local {
                command.env("FERRET_NO_DAEMON", "1");
            }
            command.output().unwrap()
        };
        let (local, daemon) = (grep(true), grep(false));
        assert_eq!(
            daemon.status.code(),
            local.status.code(),
            "{temp:?} {daemon:?}"
        );
        assert_eq!(daemon.stdout, local.stdout, "{temp:?}");
        let stderr = String::from_utf8_lossy(&daemon.stderr);
        if temp == gone.as_os_str() {
            assert_eq!(daemon.status.code(), Some(2));
            assert!(
                stderr.contains(&format!("create output spill {}/", gone.display())),
                "{stderr}"
            );
        } else {
            assert_eq!(daemon.status.code(), Some(0), "{stderr}");
            assert!(stderr.is_empty(), "{stderr}");
        }
    }
}

// A file operand's identity belongs to the client: `/proc/self/fd/N` and its
// aliases name the client's descriptor, and `/proc/self/cwd` its cwd. Mixed
// requests and process-relative directories therefore stay in process; the
// daemon here runs from `/`, without descriptor 99.
#[test]
fn mixed_grep_operands_keep_the_clients_process_identity() {
    let tree = descriptor_tree();
    tree.start_at(&[], std::path::Path::new("/"));
    let mut differing = Vec::new();
    for case in descriptor_cases() {
        let local = grep_with_descriptor(&tree, &case, true);
        let remote = grep_with_descriptor(&tree, &case, false);
        if (remote.status.code(), &remote.stdout, &remote.stderr)
            != (local.status.code(), &local.stdout, &local.stderr)
        {
            differing.push(format!("{case:?}: local {local:?}, daemon {remote:?}"));
        }
    }
    assert!(differing.is_empty(), "{differing:#?}");
    let local = grep_with_descriptor(&tree, &["red", "/proc/self/fd/99", "src"], true);
    assert!(local.status.success(), "{local:?}");
    assert_eq!(
        local.stdout,
        b"/proc/self/fd/99:red descriptor-only\nsrc/main.rs:red main\n"
    );
}

// The routing half of the test above, observed at the socket: those requests
// never connect, while covered directory-only content and --files requests do.
#[test]
fn only_directory_only_grep_requests_reach_the_daemon() {
    let tree = descriptor_tree();
    let (listener, hello, socket) = tree.fake_owner();
    let hello = hello.replace(",\"raw-output-v1\"", "");
    for case in descriptor_cases() {
        let output = grep_with_descriptor(&tree, &case, false);
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "{case:?} connected to the daemon: {output:?}"
        );
    }
    // D112: an uncovered operand keeps the whole mixed request local.
    fs::create_dir(tree.base.join("uncovered")).unwrap();
    fs::write(tree.base.join("uncovered/file"), b"red live\n").unwrap();
    for case in [
        &["red", "src", "uncovered"][..],
        &["--files", "uncovered", "src"],
    ] {
        let output = grep_with_descriptor(&tree, case, false);
        assert!(output.status.success(), "{output:?}");
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "{case:?} connected: {output:?}"
        );
    }
    for case in [
        &["red", "src"][..],
        &["--files", "src"],
        &["red", "src", "src/nested"],
    ] {
        let server = std::thread::scope(|scope| {
            let server = scope.spawn(|| {
                let mut stream = accept_peer(&listener);
                stream.set_read_timeout(Some(BOUND)).unwrap();
                stream.write_all(hello.as_bytes()).unwrap();
                let mut reader = BufReader::new(stream);
                let request = line(&mut reader);
                reader.get_mut().write_all(b"{\"id\":\"grep\",\"event\":\"begin\"}\n{\"id\":\"grep\",\"event\":\"stdout\",\"bytes_base64\":\"c2VydmVkCg==\"}\n{\"id\":\"grep\",\"event\":\"end\",\"exit\":0}\n").unwrap();
                request
            });
            let output = grep_with_descriptor(&tree, case, false);
            assert!(output.status.success(), "{case:?} {output:?}");
            assert_eq!(output.stdout, b"served\n", "{case:?}");
            server.join().unwrap()
        });
        assert!(server.contains("\"op\":\"grep\""), "{server}");
    }
    fs::remove_file(socket).unwrap();
}

fn descriptor_tree() -> Tree {
    let tree = Tree::new();
    fs::write(tree.base.join("src/main.rs"), b"red main\n").unwrap();
    fs::write(tree.base.join("fdinput"), b"red descriptor-only\n").unwrap();
    symlink("/proc/self/fd/99", tree.base.join("fdlink")).unwrap();
    symlink("/proc/self/cwd/src", tree.base.join("cwdlink")).unwrap();
    assert!(tree.local(&["index", "src"]).status.success());
    tree
}

// Inherited regular-file descriptor 99 by its aliases, and the client's cwd
// as a directory, each alone and beside the indexed `src`.
fn descriptor_cases() -> Vec<Vec<&'static str>> {
    let mut cases = Vec::new();
    for operand in [
        "/proc/self/fd/99",
        "/dev/fd/99",
        "fdlink",
        "/proc/self/cwd/src",
        "cwdlink",
    ] {
        for mode in [&["red"][..], &["--files"]] {
            cases.push([mode, &[operand, "src"]].concat());
            cases.push([mode, &[operand]].concat());
        }
    }
    cases
}

fn grep_with_descriptor(tree: &Tree, args: &[&str], local: bool) -> Output {
    let mut command = fixture::bounded_command("bash", &tree.base);
    command
        .current_dir(&tree.base)
        .env_remove("FERRET_NO_DAEMON")
        .env("FERRET_DAEMON_BIN", DAEMON)
        .env("FERRET_DAEMON_STARTUP_MS", "1500")
        .env("FERRET_SEARCH_THREADS", "1")
        .args(["-c", r#"exec "$@" 99<fdinput"#, "grep-fd", FERRET, "grep"])
        .args(args);
    if local {
        command.env("FERRET_NO_DAEMON", "1");
    }
    command.output().unwrap()
}

// D113: these names have no content document, but their catalogued sizes
// require a checked live read beside the postings, in both hosts.
#[test]
fn indexed_grep_reads_over_cap_text_locally_and_through_the_daemon() {
    compare_over_cap_grep("text", None);
}

#[test]
fn indexed_grep_applies_rg_binary_gate_to_over_cap_files_in_both_hosts() {
    compare_over_cap_grep("binary", Some(1));
}

#[test]
fn indexed_grep_keeps_rg_early_matches_before_a_late_binary_byte_in_both_hosts() {
    compare_over_cap_grep("late-binary", Some(128 * 1024));
}

fn compare_over_cap_grep(name: &str, nul: Option<usize>) {
    let tree = Tree::new();
    let path = format!("src/large-{name}");
    let mut bytes = vec![b'x'; 8 * 1024 * 1024 + 1];
    if let Some(at) = nul {
        bytes[..7].copy_from_slice(b"needle\n");
        bytes[at] = 0;
    }
    bytes.extend_from_slice(b"\nneedle tail\n");
    fs::write(tree.base.join(&path), bytes).unwrap();
    assert!(tree.local(&["index", "src"]).status.success());
    tree.start(&[]);
    let rg =
        std::env::var_os("FERRET_RG").unwrap_or_else(|| panic!("dev shell supplies pinned rg"));
    let version = Command::new(&rg).arg("--version").output().unwrap();
    assert!(version.stdout.starts_with(b"ripgrep 15.2.0\n"));
    // Unlimited late-NUL counts retain GREP.md's established first-NUL
    // stopping policy; cap removal does not change that separate contract.
    let profiles: &[&[&str]] = if nul.is_some_and(|at| at > 64 * 1024) {
        &[
            &[],
            &["-l"],
            &["-q"],
            &["--files-without-match"],
            &["-a", "-l"],
            &["-a", "--count"],
        ]
    } else {
        &[
            &[],
            &["-l"],
            &["-q"],
            &["--count"],
            &["--files-without-match"],
            &["-a", "-l"],
            &["-a", "--count"],
        ]
    };
    for &flags in profiles {
        let args = [flags, &["needle", "src"]].concat();
        let oracle = fixture::command(&rg, &tree.base)
            .args(&args)
            .output()
            .unwrap();
        for local in [true, false] {
            let mut command = tree.command(&["grep"]);
            command.args(&args).env("FERRET_SEARCH_THREADS", "1");
            if local {
                command.env("FERRET_NO_DAEMON", "1");
            }
            let actual = command.output().unwrap();
            assert_eq!(
                actual.status.code(),
                oracle.status.code(),
                "{name} {flags:?} local={local}: {actual:?} vs {oracle:?}"
            );
            if flags.is_empty() && nul.is_some_and(|at| at > 64 * 1024) {
                // GREP.md discards buffered plain lines at the first NUL and
                // uses its established diagnostic instead of rg's warning.
                assert!(
                    oracle
                        .stdout
                        .windows(b": WARNING:".len())
                        .any(|part| part == b": WARNING:")
                );
                assert_eq!(
                    actual.stdout,
                    format!("{path}: binary file matches\n").as_bytes()
                );
            } else {
                let lines = |bytes: &[u8]| {
                    let mut lines: Vec<_> = bytes
                        .split(|&byte| byte == b'\n')
                        .map(<[u8]>::to_vec)
                        .collect();
                    lines.sort();
                    lines
                };
                assert_eq!(
                    lines(&actual.stdout),
                    lines(&oracle.stdout),
                    "{name} {flags:?} local={local}"
                );
            }
            // Both hosts may render the same binary warning with their own
            // wording; the suppression and match decisions must agree.
            assert_eq!(
                actual.stderr.is_empty(),
                oracle.stderr.is_empty(),
                "{name} {flags:?} local={local}: {actual:?} vs {oracle:?}"
            );
        }
    }
}

/// Resigns `current` so its incarnation no longer matches the snapshot.
/// `Catalog::open` then fails checkpoint identity, while the manifest alone
/// still yields the new incarnation.
fn resign_incarnation(index: &std::path::Path) {
    let path = index.join("current");
    let mut bytes = fs::read(&path).unwrap();
    assert_eq!(bytes.len(), 128, "manifest length");
    bytes[16] ^= 0x01;
    assert_ne!(&bytes[16..32], &[0; 16], "incarnation must stay nonzero");
    let digest = ferret_catalog::checkpoint_checksum(&bytes[..112]);
    bytes[112..128].copy_from_slice(&digest);
    fs::write(path, bytes).unwrap();
}

#[test]
fn client_compares_the_manifest_incarnation_without_a_full_open() {
    let tree = Tree::new();
    let pid = tree.start(&[]);
    let status = tree.run(&["status", "--json"]);
    assert!(status.status.success(), "{status:?}");
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("\"host_running\":true"),
        "{}",
        String::from_utf8_lossy(&status.stdout)
    );

    // No published manifest: the client skips the check, as it does for a
    // missing index, and the already-loaded daemon still answers.
    let current = tree.base.join("index/current");
    let saved = fs::read(&current).unwrap();
    fs::remove_file(&current).unwrap();
    let missing = tree.run(&["status", "--json"]);
    assert!(missing.status.success(), "{missing:?}");
    assert!(
        String::from_utf8_lossy(&missing.stdout).contains("\"host_running\":true"),
        "{}",
        String::from_utf8_lossy(&missing.stdout)
    );
    fs::write(&current, saved).unwrap();

    resign_incarnation(&tree.base.join("index"));
    let mismatched = tree.run(&["status", "--json"]);
    assert_eq!(mismatched.status.code(), Some(3), "{mismatched:?}");
    assert!(
        String::from_utf8_lossy(&mismatched.stderr).contains("checkpoint identity"),
        "{}",
        String::from_utf8_lossy(&mismatched.stderr)
    );
    assert_eq!(number(&tree.connect().1, "pid"), Some(pid));
}

fn release_spawned(tree: &Tree) {
    terminate_fixture_daemons(&tree.base);
    wait(|| fixture_daemons(&tree.base).is_empty());
    for path in tree.sockets() {
        let _ = fs::remove_file(path);
    }
}

fn assert_index_owner(tree: &Tree) {
    let (_, hello) = tree.connect();
    let meta = fs::metadata(tree.base.join("index")).unwrap();
    let identity = format!("{:x}-{:x}", meta.dev(), meta.ino());
    assert!(
        hello.contains(&format!("\"index\":\"{identity}\"")),
        "{hello}"
    );
    assert_eq!(fixture_daemons(&tree.base).len(), 1);
}

fn assert_no_daemon(tree: &Tree, output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fixture_daemons(&tree.base).is_empty(),
        "FERRET_NO_DAEMON still spawned {:?}",
        fixture_daemons(&tree.base)
    );
}

/// `ferret index` / `roots remove` with no daemon publish in process, drop
/// the writer lock, then start ferretd for that index. `FERRET_NO_DAEMON`
/// does not. Directory grep on a miss starts the same kind of host.
#[test]
fn in_process_index_roots_remove_and_grep_spawn_that_index_daemon() {
    let tree = Tree::new();
    assert!(fixture_daemons(&tree.base).is_empty());
    assert!(tree.sockets().is_empty());

    let indexed = tree.run(&["index", "src"]);
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    assert_index_owner(&tree);
    release_spawned(&tree);

    let grep = tree.run(&["grep", "fn", "src"]);
    assert!(
        grep.status.success(),
        "{}",
        String::from_utf8_lossy(&grep.stderr)
    );
    assert!(
        String::from_utf8_lossy(&grep.stdout).contains("fn main"),
        "{grep:?}"
    );
    assert_index_owner(&tree);
    release_spawned(&tree);

    assert_no_daemon(&tree, &tree.local(&["grep", "fn", "src"]));
    assert_no_daemon(&tree, &tree.local(&["index", "src"]));

    let removed = tree.run(&["roots", "remove", "src"]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert_index_owner(&tree);
}

fn census_count(text: &str, label: &str) -> u32 {
    text.lines()
        .find(|line| line.starts_with(label))
        .unwrap_or_else(|| panic!("no {label} line:\n{text}"))
        .split_whitespace()
        .nth(1)
        .unwrap_or_else(|| panic!("{label} has no count:\n{text}"))
        .parse()
        .unwrap_or_else(|error| panic!("{label}: {error}"))
}

/// Text stats asks the running daemon and does not start one. Hiding the
/// published snapshot makes a local open fail while the paused daemon's pin
/// still answers, so text and JSON describe that pin. There is no stable
/// window where the pin's generation is ahead of `Published::open`: select
/// runs after commit.
#[test]
fn text_stats_reads_the_daemon_pin_when_the_snapshot_is_hidden() {
    let tree = Tree::new();
    let power = tree.base.join("power");
    fs::create_dir_all(power.join("BAT0")).unwrap();
    fs::write(power.join("BAT0/type"), "Battery\n").unwrap();
    fs::write(power.join("BAT0/status"), "Discharging\n").unwrap();
    let indexed = tree
        .command(&["index", "src"])
        .env("FERRET_SIGNAL_POWER", &power)
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    assert_index_owner(&tree);

    let remote = tree.run(&["stats"]);
    let local = tree.local(&["stats"]);
    assert!(
        remote.status.success(),
        "{}",
        String::from_utf8_lossy(&remote.stderr)
    );
    assert!(
        local.status.success(),
        "{}",
        String::from_utf8_lossy(&local.stderr)
    );
    assert_eq!(
        remote.stdout,
        local.stdout,
        "daemon text\n{}\nlocal text\n{}",
        String::from_utf8_lossy(&remote.stdout),
        String::from_utf8_lossy(&local.stdout)
    );
    let json = tree.run(&["stats", "--json"]);
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let json_line = String::from_utf8(json.stdout).unwrap();
    let text = String::from_utf8(remote.stdout.clone()).unwrap();
    assert_eq!(
        number(&json_line, "names"),
        Some(census_count(&text, "entries"))
    );
    assert_eq!(
        number(&json_line, "documents"),
        Some(census_count(&text, "documents"))
    );

    let index = tree.base.join("index");
    let mut hidden = 0;
    for entry in fs::read_dir(&index).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("snapshot.") || name.starts_with("changes.") {
            let mut aside = entry.path();
            aside.set_extension("aside");
            fs::rename(entry.path(), aside).unwrap();
            hidden += 1;
        }
    }
    assert!(hidden > 0, "index published no snapshot to hide");

    let after = tree.run(&["stats"]);
    assert!(
        after.status.success(),
        "{}",
        String::from_utf8_lossy(&after.stderr)
    );
    assert_eq!(after.stdout, remote.stdout);
    let json = tree.run(&["stats", "--json"]);
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let json_line = String::from_utf8(json.stdout).unwrap();
    let text = String::from_utf8(after.stdout).unwrap();
    assert_eq!(
        number(&json_line, "names"),
        Some(census_count(&text, "entries"))
    );
    assert_eq!(
        number(&json_line, "documents"),
        Some(census_count(&text, "documents"))
    );
    let local = tree.local(&["stats"]);
    assert_eq!(
        local.status.code(),
        Some(3),
        "local stats still opened the hidden snapshot: {}",
        String::from_utf8_lossy(&local.stderr)
    );
    assert_eq!(fixture_daemons(&tree.base).len(), 1);
}

fn publish_accept_error(path: &std::path::Path, name: &str) {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, name).unwrap();
    fs::rename(&temporary, path).unwrap();
}

fn task_names(pid: u32) -> Vec<String> {
    fs::read_dir(format!("/proc/{pid}/task"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| fs::read_to_string(entry.path().join("comm")).ok())
        .map(|name| name.trim().to_owned())
        .collect()
}

fn ready_within(tree: &Tree, bound: Duration) -> String {
    let socket = tree.socket();
    let stream = UnixStream::connect(&socket).unwrap();
    stream.set_read_timeout(Some(bound)).unwrap();
    stream.set_write_timeout(Some(bound)).unwrap();
    let mut reader = BufReader::new(stream);
    let until = Instant::now() + bound;
    loop {
        assert!(Instant::now() < until, "daemon did not become ready");
        let hello = line(&mut reader);
        if hello.contains("\"state\":\"ready\"") {
            return hello;
        }
        assert!(hello.contains("\"state\":\"loading\""), "{hello}");
    }
}

/// Fatal accept must join the writer (and therefore its merge worker) and
/// release the catalog lock before the socket disappears. Process exit would
/// drop the flock anyway, so the check is the window while ferretd is still
/// alive: the merge thread is gone, the socket is still there, and another
/// writer cannot take the lock.
#[test]
#[cfg(debug_assertions)]
fn fatal_accept_error_joins_the_merge_before_removing_the_socket() {
    let tree = Tree::new();
    for i in 0..600 {
        let text = (0..400)
            .map(|word| format!("word{i}part{word} "))
            .collect::<String>();
        fs::write(tree.base.join(format!("src/merge-{i}.txt")), text).unwrap();
    }
    let indexed = fixture::command(FERRET, &tree.base)
        .args(["index", "src"])
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "index: {}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let gate = tree.base.join("content-gate");
    fs::create_dir_all(&gate).unwrap();
    fs::write(gate.join("merge"), b"hold").unwrap();
    let hook = tree.base.join("accept-error");
    let pid = tree.start(&[
        ("FERRET_DAEMON_IDLE_MS", "0"),
        ("FERRET_BACKSTOP_MS", "3600000"),
        ("FERRET_POLL_MS", "300000"),
        ("FERRET_CONTENT_TEST_GATE", gate.to_str().unwrap()),
        ("FERRET_DAEMON_TEST_ACCEPT_ERROR", hook.to_str().unwrap()),
        ("FERRET_DAEMON_TEST_STOP_DELAY_MS", "3000"),
    ]);
    ready_within(&tree, Duration::from_secs(60));
    for i in 0..240 {
        fs::remove_file(tree.base.join(format!("src/merge-{i}.txt"))).unwrap();
    }
    let merge_by = Instant::now() + Duration::from_secs(60);
    while !gate.join("merge.reached").exists() {
        assert!(
            tree.try_wait(pid).is_none(),
            "daemon exited before the merge was held"
        );
        assert!(Instant::now() < merge_by, "merge did not reach the gate");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        task_names(pid).iter().any(|name| name == "ferret-merge"),
        "merge worker was not running"
    );
    publish_accept_error(&hook, "EBADF");
    let _ = UnixStream::connect(tree.socket());
    let observed = Instant::now() + Duration::from_secs(15);
    while tree.try_wait(pid).is_some()
        || tree.sockets().is_empty()
        || task_names(pid).iter().any(|name| name == "ferret-merge")
    {
        assert!(
            Instant::now() < observed,
            "merge stayed up, or the socket vanished, before the writer was joined"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    match ferret_catalog::WriterSession::open(&tree.base.join("index")) {
        Err(ferret_catalog::log::Error::Locked) => {}
        Err(error) => panic!("catalog lock released while the socket was still present: {error}"),
        Ok(_) => panic!("catalog lock released while the socket was still present"),
    }
    let exited = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = tree.try_wait(pid) {
            break status;
        }
        assert!(Instant::now() < exited, "daemon did not finish teardown");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(3), "{status}");
    assert!(tree.sockets().is_empty(), "socket survived teardown");
    assert!(
        gate.join("merge").exists(),
        "the test must not release the merge itself"
    );
    let writer = ferret_catalog::WriterSession::open(&tree.base.join("index"))
        .unwrap_or_else(|error| panic!("catalog lock survived teardown: {error}"));
    drop(writer);
}

#[test]
#[cfg(debug_assertions)]
fn transient_accept_errors_do_not_stop_the_daemon() {
    let tree = Tree::new();
    let hook = tree.base.join("accept-error");
    let pid = tree.start(&[("FERRET_DAEMON_TEST_ACCEPT_ERROR", hook.to_str().unwrap())]);
    for name in [
        "ECONNABORTED",
        "EINTR",
        "EPROTO",
        "EMFILE",
        "ENFILE",
        "ENOBUFS",
        "ENOMEM",
    ] {
        publish_accept_error(&hook, name);
        let _ = UnixStream::connect(tree.socket());
        wait(|| !hook.exists());
        let output = tree.run(&["search", "case:main"]);
        assert!(
            output.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.windows(4).any(|part| part == b"main"),
            "{name}"
        );
        let (_, hello) = tree.connect();
        assert_eq!(
            number(&hello, "pid"),
            Some(pid),
            "{name} replaced the daemon"
        );
        assert_eq!(tree.sockets().len(), 1, "{name}");
    }
}
