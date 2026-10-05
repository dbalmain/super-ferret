//! The real serial daemon writer receives an injected signal source and clock.
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use ferret_catalog::bulk::{Blocked, Clock};
use ferret_crawl::{IndexError, IndexOptions, Refresh};

use super::{Command, Host, Loaded, Message, Status, serve};
use crate::config::Controller;
use crate::daemon::{Lifecycle, ServerEvent};
use crate::politeness::{Idle, Sample, Signals};
use crate::protocol::{Op, Request};
use crate::scheduler::Scheduler;

#[derive(Debug, Default)]
struct Time(Mutex<Duration>);
impl Clock for Time {
    fn now(&self) -> Duration {
        *self.0.lock().unwrap_or_else(|e| panic!("clock: {e}"))
    }
    fn sleep(&self, duration: Duration) {
        *self.0.lock().unwrap_or_else(|e| panic!("clock: {e}")) += duration;
    }
}
#[derive(Clone, Debug)]
struct Source(Arc<Mutex<Sample>>);
impl Signals for Source {
    fn sample(&mut self) -> Sample {
        self.0
            .lock()
            .unwrap_or_else(|e| panic!("signals: {e}"))
            .clone()
    }
}
struct Tree(std::path::PathBuf);
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn tree() -> Tree {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir().join(format!(
        "ferret-writer-controller-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(base.join("tree")).unwrap_or_else(|e| panic!("fixture: {e}"));
    fs::write(base.join("tree/a.txt"), b"alpha").unwrap_or_else(|e| panic!("fixture: {e}"));
    ferret_crawl::index(
        &base.join("index"),
        &[base.join("tree")],
        Refresh::All,
        &IndexOptions::default(),
    )
    .unwrap_or_else(|e| panic!("index: {e}"));
    Tree(base)
}
fn command(host: &Host, root: &std::path::Path) -> Result<ferret_crawl::Report, IndexError> {
    let (reply, receive) = mpsc::sync_channel(1);
    host.writer_pending.fetch_add(1, Ordering::AcqRel);
    host.writer_send
        .send(Message::Command(Command {
            request: Request {
                id: "index".into(),
                op: Op::Index,
                args: vec![Vec::new(), root.as_os_str().as_bytes().to_vec()],
                cwd: None,
                limit: None,
                capabilities: vec![],
                child_stdin: None,
                start_unix_ns: None,
            },
            reply,
        }))
        .unwrap_or_else(|_| panic!("writer channel"));
    receive
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("writer reply: {e}"))
}
fn deferred_writer(reason: Blocked) {
    let tree = tree();
    let clock = Arc::new(Time::default());
    let source = Source(Arc::new(Mutex::new(Sample {
        cpu: Some(0.0),
        io: Some(if reason == Blocked::IoPressure {
            11.0
        } else {
            0.0
        }),
        battery: Some(reason == Blocked::Battery),
        load: Some(0.0),
        memory: Some(if reason == Blocked::Memory {
            1 << 20
        } else {
            10 << 30
        }),
        idle: Idle::Unknown,
    })));
    let scheduler = Arc::new(Scheduler::new(
        Controller {
            memory_floor: 1 << 30,
            rate: 0,
            ..Controller::default()
        },
        32,
        tree.0.join("index"),
        Box::new(source.clone()),
        clock.clone(),
    ));
    let (send, receive) = mpsc::sync_channel(32);
    let (server_send, server_receive) = mpsc::channel();
    let host = Arc::new(Host {
        engine: Mutex::new(Loaded::Loading),
        engine_ready: Condvar::new(),
        lifecycle: Mutex::new(Lifecycle {
            draining: false,
            queries: 0,
        }),
        lifecycle_changed: Condvar::new(),
        clients: AtomicUsize::new(0),
        index: tree.0.join("index"),
        server_send,
        identity: "fixture".into(),
        context: "fixture".into(),
        build: "fixture".into(),
        query_limit: 4,
        workers: 1,
        format: ferret_catalog::FORMAT_VERSION as u64,
        writer_send: send,
        writer_status: Mutex::new(Status {
            scheduler: Some(scheduler.clone()),
            ..Status::default()
        }),
        writer_running: AtomicBool::new(false),
        writer_pending: AtomicUsize::new(0),
        stop: AtomicBool::new(false),
    });
    let writer_host = host.clone();
    let control = scheduler.clone();
    let writer = std::thread::spawn(move || serve(&writer_host, receive, control));
    let engine = {
        let mut loaded = host.engine.lock().unwrap_or_else(|e| panic!("engine: {e}"));
        while matches!(*loaded, Loaded::Loading) {
            let (next, timeout) = host
                .engine_ready
                .wait_timeout(loaded, Duration::from_secs(5))
                .unwrap_or_else(|e| panic!("ready: {e}"));
            assert!(!timeout.timed_out(), "writer load timed out");
            loaded = next;
        }
        match &*loaded {
            Loaded::Ready(engine) => engine.clone(),
            _ => panic!("writer failed to load"),
        }
    };
    let old = engine.pin();
    fs::write(tree.0.join("tree/a.txt"), b"changed").unwrap_or_else(|e| panic!("fixture: {e}"));
    let result = command(&host, &tree.0.join("tree"));
    if matches!(reason, Blocked::Battery | Blocked::IoPressure) {
        assert!(result.is_ok(), "command was politely deferred: {result:?}");
        assert_ne!(engine.generation(), old.generation());
        host.stop.store(true, Ordering::Release);
        let _ = host.writer_send.send(Message::Intake);
        writer
            .join()
            .unwrap_or_else(|_| panic!("writer panic"))
            .unwrap_or_else(|e| panic!("writer: {e}"));
        engine.close_writer();
        return;
    }
    if reason == Blocked::Memory {
        assert!(
            matches!(result, Err(IndexError::DeferredMemory { required, available }) if required > available)
        );
    } else {
        assert!(matches!(result, Err(IndexError::DeferredBulk(actual)) if actual == reason));
    }
    assert_eq!(engine.generation(), old.generation());
    let query = ferret_query::Query::from_args([b"*.txt".as_slice()], std::time::SystemTime::now())
        .unwrap_or_else(|e| panic!("query: {e}"));
    let mut rows = 0;
    engine
        .pin()
        .search(&query, |_| {
            rows += 1;
            std::ops::ControlFlow::Continue(())
        })
        .unwrap_or_else(|e| panic!("query while paused: {e}"));
    assert_eq!(rows, 1);
    assert!(std::ptr::eq(engine.pin().name_index(), old.name_index()));
    // The command's wake comes after the real deferred outcome was handled.
    server_receive
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("boundary: {e}"));
    let mut json = Vec::new();
    let mut object = crate::json::Object::new(&mut json);
    super::fields(&host, &mut object);
    object.end();
    let json = String::from_utf8(json).unwrap_or_else(|e| panic!("status: {e}"));
    assert!(json.contains(reason.status()), "{json}");
    assert!(json.contains("Backstop"), "{json}");
    source
        .0
        .lock()
        .unwrap_or_else(|e| panic!("signals: {e}"))
        .memory = Some(10 << 30);
    {
        let mut sample = source.0.lock().unwrap_or_else(|e| panic!("signals: {e}"));
        sample.io = Some(0.0);
        sample.battery = Some(false);
    }
    scheduler.sample();
    clock.sleep(Duration::from_secs(2));
    host.writer_send
        .send(Message::Intake)
        .unwrap_or_else(|_| panic!("wake writer"));
    loop {
        let event = server_receive
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|e| panic!("retry: {e}"));
        if matches!(event, ServerEvent::Wake) && engine.generation() != old.generation() {
            break;
        }
    }
    assert!(
        host.writer_status
            .lock()
            .unwrap_or_else(|e| panic!("status: {e}"))
            .blocked
            .is_none()
    );
    host.stop.store(true, Ordering::Release);
    let _ = host.writer_send.send(Message::Intake);
    writer
        .join()
        .unwrap_or_else(|_| panic!("writer panic"))
        .unwrap_or_else(|e| panic!("writer: {e}"));
    engine.close_writer();
}

#[test]
fn daemon_writer_reports_memory_blocked_preserves_caches_and_retries_the_complete_marker() {
    deferred_writer(Blocked::Memory);
}
#[test]
fn daemon_writer_command_ignores_battery_pause() {
    deferred_writer(Blocked::Battery);
}
#[test]
fn daemon_writer_command_ignores_io_pressure() {
    deferred_writer(Blocked::IoPressure);
}
