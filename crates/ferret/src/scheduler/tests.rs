//! Injected signals drive the production scheduler and retained crawl writer.
use std::fs;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use ferret_catalog::bulk::{Blocked, Clock, Control, Kind};
use ferret_catalog::{CompactionLimits, InputLimits, WriterSession};
use ferret_crawl::watch::{Config as WatchConfig, Watch};
use ferret_crawl::{
    IndexOptions, Refresh, RefreshOutcome, RefreshReason, RefreshRequest, RefreshScope,
};
use ferret_query::Query;

use super::{Config, Idle, Sample, Scheduler, Signals};
use crate::engine::Engine;

#[derive(Debug, Default)]
struct Time(Mutex<Duration>);
impl Time {
    fn advance(&self, seconds: u64) {
        *self.0.lock().unwrap_or_else(|e| panic!("fixture: {e:?}")) += Duration::from_secs(seconds);
    }
}
impl Clock for Time {
    fn now(&self) -> Duration {
        *self.0.lock().unwrap_or_else(|e| panic!("fixture: {e:?}"))
    }
    fn sleep(&self, duration: Duration) {
        *self.0.lock().unwrap_or_else(|e| panic!("fixture: {e:?}")) += duration;
    }
}
#[derive(Debug, Clone)]
struct Source(Arc<Mutex<Sample>>);
impl Signals for Source {
    fn sample(&mut self) -> Sample {
        self.0
            .lock()
            .unwrap_or_else(|e| panic!("fixture: {e:?}"))
            .clone()
    }
}
fn calm() -> Sample {
    Sample {
        cpu: Some(0.0),
        io: Some(0.0),
        battery: Some(false),
        load: Some(1.0),
        memory: Some(10 << 30),
        idle: Idle::Headless,
    }
}
fn scheduler(config: Config, index: std::path::PathBuf) -> (Arc<Scheduler>, Source, Arc<Time>) {
    let source = Source(Arc::new(Mutex::new(calm())));
    let clock = Arc::new(Time::default());
    (
        Arc::new(Scheduler::new(
            config,
            32,
            index,
            Box::new(source.clone()),
            clock.clone(),
        )),
        source,
        clock,
    )
}
#[test]
fn ratchet_raises_one_each_ten_calm_seconds_and_drops_at_once() {
    let (scheduler, source, time) = scheduler(
        Config {
            concurrency: 4,
            ..Config::default()
        },
        "/tmp".into(),
    );
    time.advance(9);
    scheduler.sample();
    assert_eq!(scheduler.status().workers, 1);
    time.advance(1);
    scheduler.sample();
    assert_eq!(scheduler.status().workers, 2);
    time.advance(10);
    scheduler.sample();
    assert_eq!(scheduler.status().workers, 3);
    time.advance(100);
    scheduler.sample();
    assert_eq!(
        scheduler.status().workers,
        4,
        "one step even after a long gap"
    );
    source
        .0
        .lock()
        .unwrap_or_else(|e| panic!("fixture: {e:?}"))
        .cpu = Some(20.1);
    scheduler.sample();
    assert_eq!(scheduler.status().workers, 1);
    source
        .0
        .lock()
        .unwrap_or_else(|e| panic!("fixture: {e:?}"))
        .cpu = Some(0.0);
    scheduler.sample();
    time.advance(9);
    scheduler.sample();
    assert_eq!(scheduler.status().workers, 1);
    time.advance(1);
    scheduler.sample();
    assert_eq!(scheduler.status().workers, 2);
}
#[test]
fn idle_boundaries_missing_psi_and_battery_use_the_real_policy() {
    let (scheduler, source, time) = scheduler(
        Config {
            concurrency: 20,
            ..Config::default()
        },
        "/tmp".into(),
    );
    for (idle, expected) in [
        (Idle::Unknown, 1),
        (Idle::Desktop(Duration::from_secs(30)), 1),
        (Idle::Desktop(Duration::from_secs(31)), 8),
        (Idle::Desktop(Duration::from_secs(300)), 8),
        (Idle::Desktop(Duration::from_secs(301)), 16),
        (Idle::Headless, 16),
    ] {
        source
            .0
            .lock()
            .unwrap_or_else(|e| panic!("fixture: {e:?}"))
            .idle = idle;
        for _ in 0..20 {
            time.advance(10);
            scheduler.sample();
        }
        assert_eq!(scheduler.status().workers, expected);
    }
    source
        .0
        .lock()
        .unwrap_or_else(|e| panic!("fixture: {e:?}"))
        .cpu = None;
    scheduler.sample();
    assert_eq!(scheduler.status().workers, 1);
    source
        .0
        .lock()
        .unwrap_or_else(|e| panic!("fixture: {e:?}"))
        .io = None;
    scheduler.sample();
    assert_eq!(scheduler.status().paused, None);
    source
        .0
        .lock()
        .unwrap_or_else(|e| panic!("fixture: {e:?}"))
        .io = Some(0.0);
    source
        .0
        .lock()
        .unwrap_or_else(|e| panic!("fixture: {e:?}"))
        .battery = Some(true);
    scheduler.sample();
    assert_eq!(scheduler.status().paused, Some(Blocked::Battery));
    source
        .0
        .lock()
        .unwrap_or_else(|e| panic!("fixture: {e:?}"))
        .battery = None;
    scheduler.sample();
    assert_eq!(scheduler.status().paused, None);
}
struct Tree(std::path::PathBuf);
impl Tree {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "ferret-controller-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(base.join("tree")).unwrap_or_else(|e| panic!("fixture: {e:?}"));
        fs::write(base.join("tree/a.txt"), b"alpha").unwrap_or_else(|e| panic!("fixture: {e:?}"));
        ferret_crawl::index(
            &base.join("index"),
            &[base.join("tree")],
            Refresh::All,
            &IndexOptions::default(),
        )
        .unwrap_or_else(|e| panic!("fixture: {e:?}"));
        Self(base)
    }
    fn root(&self) -> std::path::PathBuf {
        self.0.join("tree")
    }
    fn index(&self) -> std::path::PathBuf {
        self.0.join("index")
    }
    fn writer(&self) -> WriterSession {
        WriterSession::open(&self.index()).unwrap_or_else(|e| panic!("fixture: {e:?}"))
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn request(engine: &Engine, root: std::path::PathBuf, reason: RefreshReason) -> RefreshRequest {
    RefreshRequest {
        expected_generation: engine.generation(),
        scopes: vec![RefreshScope::Root(root)],
        rename_hints: vec![],
        reason,
    }
}
fn query(engine: &Engine) -> usize {
    let query = Query::from_args([b"*.txt".as_slice()], SystemTime::now())
        .unwrap_or_else(|e| panic!("fixture: {e:?}"));
    let mut rows = 0;
    engine
        .pin()
        .search(&query, |_| {
            rows += 1;
            ControlFlow::Continue(())
        })
        .unwrap_or_else(|e| panic!("fixture: {e:?}"));
    rows
}
#[test]
fn memory_deferred_fallback_keeps_generation_planner_and_writer_then_recovers() {
    let tree = Tree::new();
    let mut writer = tree.writer();
    writer.set_input_limits(InputLimits {
        records: 1,
        owned_bytes: 1,
    });
    let engine = Engine::from_writer(writer);
    let old = engine.pin();
    let (scheduler, source, _) = scheduler(
        Config {
            memory_floor: 1 << 30,
            rate: 0,
            ..Config::default()
        },
        tree.index(),
    );
    source
        .0
        .lock()
        .unwrap_or_else(|e| panic!("fixture: {e:?}"))
        .memory = Some(1 << 20);
    scheduler.sample();
    let options = IndexOptions {
        bulk: Some(scheduler.clone()),
        ..IndexOptions::default()
    };
    fs::write(tree.root().join("a.txt"), b"replacement")
        .unwrap_or_else(|e| panic!("fixture: {e:?}"));
    let root_id = engine
        .pin()
        .catalog()
        .roots()
        .next()
        .unwrap_or_else(|| panic!("root"))
        .0;
    let small = RefreshRequest {
        expected_generation: engine.generation(),
        scopes: vec![RefreshScope::Entry {
            parent: root_id,
            basename: b"a.txt".to_vec(),
        }],
        rename_hints: vec![],
        reason: RefreshReason::Burst,
    };
    let report = engine
        .refresh(small, &options)
        .unwrap_or_else(|e| panic!("fixture: {e:?}"));
    assert!(matches!(
        report.outcome,
        RefreshOutcome::DeferredBulk(Blocked::Memory)
    ));
    assert_eq!(engine.generation(), old.generation());
    assert!(
        std::ptr::eq(engine.pin().name_index(), old.name_index()),
        "current planner must be kept"
    );
    assert_eq!(query(&engine), 1);
    assert_eq!(
        scheduler.status().admission,
        Some((Kind::FullRewalk, Err(Blocked::Memory)))
    );
    source
        .0
        .lock()
        .unwrap_or_else(|e| panic!("fixture: {e:?}"))
        .memory = Some(10 << 30);
    scheduler.sample();
    let report = engine
        .refresh(
            request(&engine, tree.root(), RefreshReason::Backstop),
            &options,
        )
        .unwrap_or_else(|e| panic!("fixture: {e:?}"));
    assert!(matches!(report.outcome, RefreshOutcome::Checkpointed));
    assert!(report.report.input_fallback);
    assert_ne!(engine.generation(), old.generation());
    assert_eq!(query(&engine), 1);
    assert!(matches!(
        engine
            .refresh(
                request(&engine, tree.root(), RefreshReason::Burst),
                &options
            )
            .unwrap_or_else(|e| panic!("fixture: {e:?}"))
            .outcome,
        RefreshOutcome::Unchanged
    ));
}
#[test]
fn disk_refusal_and_live_watch_resources_are_admission_inputs() {
    let tree = Tree::new();
    let view = tree.writer().view();
    let (scheduler, _, _) = scheduler(
        Config {
            disk: u64::MAX,
            rate: 0,
            ..Config::default()
        },
        tree.index(),
    );
    assert_eq!(scheduler.admit(Kind::Checkpoint, &view), Err(Blocked::Disk));
    let (scheduler, source, _) = self::scheduler(
        Config {
            rate: 0,
            memory_floor: 100,
            additional_memory: 2 << 20,
            ..Config::default()
        },
        tree.index(),
    );
    source
        .0
        .lock()
        .unwrap_or_else(|e| panic!("fixture: {e:?}"))
        .memory = Some(1 << 20);
    scheduler.sample();
    assert_eq!(
        scheduler.admit(Kind::FullRewalk, &view),
        Err(Blocked::Memory)
    );
    // Actual sparse alias and policy registrations also consume admission
    // headroom, not just the optional fixed reserve.
    let watch = Arc::new(
        Watch::new(WatchConfig {
            scopes: 100,
            bytes: 4096,
            watch_cap: 100,
        })
        .unwrap_or_else(|e| panic!("watch: {e:?}")),
    );
    fs::hard_link(tree.root().join("a.txt"), tree.root().join("alias.txt"))
        .unwrap_or_else(|e| panic!("alias: {e}"));
    let mut writer = tree.writer();
    ferret_crawl::recrawl(
        &mut writer,
        &[tree.root()],
        Refresh::All,
        &IndexOptions {
            watch: Some(watch.clone()),
            ..IndexOptions::default()
        },
    )
    .unwrap_or_else(|e| panic!("observe: {e}"));
    watch.policy_path(&tree.root(), &tree.0.join("outside-ignore"));
    assert!(watch.resource_bytes() > 1024);
    let (scheduler, source, _) = self::scheduler(
        Config {
            rate: 0,
            full_memory: 0,
            memory_floor: 100,
            ..Config::default()
        },
        tree.index(),
    );
    scheduler.watch(Some(watch.clone()));
    source
        .0
        .lock()
        .unwrap_or_else(|e| panic!("signals: {e}"))
        .memory = Some(1024);
    scheduler.sample();
    assert_eq!(
        scheduler.admit(Kind::FullRewalk, &writer.view()),
        Err(Blocked::Memory)
    );
    assert_eq!(
        scheduler.status().required_memory,
        100 + watch.resource_bytes()
    );
}
#[test]
fn high_or_missing_io_psi_and_unknown_battery_admit_background_work() {
    for (io, battery) in [(Some(66.0), Some(false)), (None, None)] {
        let tree = Tree::new();
        let engine = Engine::from_writer(tree.writer());
        let (scheduler, source, _) = scheduler(
            Config {
                rate: 0,
                ..Config::default()
            },
            tree.index(),
        );
        {
            let mut sample = source.0.lock().unwrap_or_else(|e| panic!("fixture: {e:?}"));
            sample.io = io;
            sample.battery = battery;
        }
        scheduler.sample();
        assert_eq!(scheduler.status().paused, None);
        assert_eq!(
            scheduler.admit(Kind::FullRewalk, engine.pin().catalog()),
            Ok(()),
            "I/O PSI {io:?}, battery {battery:?}"
        );
    }
}
#[test]
fn battery_still_pauses_background_checkpoint_while_queries_keep_serving() {
    let tree = Tree::new();
    let engine = Engine::from_writer(tree.writer());
    let (scheduler, source, _) = scheduler(
        Config {
            rate: 0,
            ..Config::default()
        },
        tree.index(),
    );
    source
        .0
        .lock()
        .unwrap_or_else(|e| panic!("fixture: {e:?}"))
        .battery = Some(true);
    scheduler.sample();
    let before = engine.generation();
    fs::write(tree.root().join("a.txt"), b"change").unwrap_or_else(|e| panic!("fixture: {e:?}"));
    let options = IndexOptions {
        bulk: Some(scheduler),
        ..IndexOptions::default()
    };
    let result = engine
        .refresh(
            request(&engine, tree.root(), RefreshReason::Burst),
            &options,
        )
        .unwrap_or_else(|e| panic!("fixture: {e:?}"));
    assert!(matches!(result.outcome, RefreshOutcome::DeferredBulk(_)));
    assert_eq!(engine.generation(), before);
    assert_eq!(query(&engine), 1);
}
#[test]
fn paused_intake_is_bounded_and_sustained_arrivals_do_not_postpone_d51() {
    let tree = Tree::new();
    let watch = Arc::new(
        Watch::new(WatchConfig {
            scopes: 2,
            bytes: 1024,
            watch_cap: 100,
        })
        .unwrap_or_else(|e| panic!("fixture: {e:?}")),
    );
    let (scheduler, source, _) = scheduler(
        Config {
            rate: 0,
            ..Config::default()
        },
        tree.index(),
    );
    let mut writer = tree.writer();
    writer.set_compaction_limits(CompactionLimits {
        records: 1,
        ..CompactionLimits::default()
    });
    let engine = Engine::from_writer(writer);
    let options = IndexOptions {
        watch: Some(watch.clone()),
        bulk: Some(scheduler.clone()),
        ..IndexOptions::default()
    };
    engine
        .refresh(
            request(&engine, tree.root(), RefreshReason::Backstop),
            &options,
        )
        .unwrap_or_else(|e| panic!("fixture: {e:?}"));
    source
        .0
        .lock()
        .unwrap_or_else(|e| panic!("fixture: {e:?}"))
        .battery = Some(true);
    scheduler.sample();
    watch.backstop(RefreshReason::Backstop);
    for n in 0..20 {
        fs::write(tree.root().join(format!("event{n}.txt")), b"new")
            .unwrap_or_else(|e| panic!("fixture: {e:?}"));
        watch.drain().unwrap_or_else(|e| panic!("fixture: {e:?}"));
        assert!(
            watch
                .take_admitted(scheduler.status().paused.is_none())
                .is_none()
        );
        let state = watch.status();
        assert!(state.bytes <= 1024);
        assert!(state.pending <= 3, "two scopes plus a complete marker");
    }
    assert!(
        watch.status().backstop.is_some(),
        "overflow must collapse into a complete marker"
    );
    assert_eq!(query(&engine), 1);
    source
        .0
        .lock()
        .unwrap_or_else(|e| panic!("fixture: {e:?}"))
        .battery = Some(false);
    scheduler.sample();
    let pending = watch
        .take_admitted(true)
        .unwrap_or_else(|| panic!("missing queued burst"));
    // Another arrival remains queued while the real commit crosses its budget.
    fs::write(tree.root().join("during.txt"), b"another")
        .unwrap_or_else(|e| panic!("fixture: {e:?}"));
    watch.drain().unwrap_or_else(|e| panic!("fixture: {e:?}"));
    let report = engine
        .refresh(pending.request(engine.pin().catalog()), &options)
        .unwrap_or_else(|e| panic!("fixture: {e:?}"));
    assert!(matches!(report.outcome, RefreshOutcome::Checkpointed));
    assert!(watch.status().busy);
    watch.finish(pending, true);
    assert_eq!(query(&engine), 22);
}
