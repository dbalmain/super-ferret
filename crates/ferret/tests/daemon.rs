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
            .env("FERRET_DAEMON_IDLE_MS", "3000");
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
            .env("FERRET_DAEMON_IDLE_MS", "3000")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (name, value) in extra {
            cmd.env(name, value);
        }
        let child = cmd.spawn().unwrap();
        let pid = child.id();
        self.children.lock().unwrap().push(child);
        self.socket();
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
        let _ = fs::remove_dir_all(&self.base);
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
    assert_eq!(tree.sockets().len(), 2);
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
    for case in 0..4 {
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
        if case < 3 {
            assert!(tree.sockets().is_empty());
        }
    }
}

#[test]
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
        Some(2)
    );
    assert!(tree.run(&["search", "new.rs"]).status.success());
    assert_eq!(
        number(
            &block(&mut reader, b"{\"id\":\"s\",\"op\":\"status\"}\n"),
            "engine_opens"
        ),
        Some(2)
    );
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
