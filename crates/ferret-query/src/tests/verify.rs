//! The verification pool (`verify.rs`), driven through
//! [`Query::run_content_parallel`] over a real catalog with no content
//! index, so every document is a Maybe and every row is read. The reader is
//! scripted: each name's bytes and version, and a gate that can hold a read
//! until another has started, which forces the completion orders the
//! window has to get right instead of hoping the scheduler produces them.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ferret_catalog::{Catalog, Content, NameId, Transaction};
use ferret_index::{DocSet, Pinned};

use super::{Scratch, dir, file, lazy, now};
use crate::verify::seam::{Poll, Seams};
use crate::{
    Content as Plan, DocNames, NameIndex, Query, ReadRequest, ReadVersion, Reader, Row, Stats,
    Workers,
};

/// How long a gate waits for the event it is held for before failing the
/// test, rather than hanging it.
const PATIENCE: Duration = Duration::from_secs(20);

/// A one-way event across threads.
#[derive(Default)]
struct Signal {
    set: Mutex<bool>,
    changed: Condvar,
}

impl Signal {
    fn set(&self) {
        *self.set.lock().unwrap() = true;
        self.changed.notify_all();
    }

    fn wait(&self, what: &str) {
        let (set, timeout) = self
            .changed
            .wait_timeout_while(self.set.lock().unwrap(), PATIENCE, |set| !*set)
            .unwrap();
        assert!(*set && !timeout.timed_out(), "never happened: {what}");
    }
}

/// Files directly under `/r`: name, catalogued size, content hash byte.
/// Names with the same hash byte share a document.
struct Fixture {
    _scratch: Scratch,
    catalog: Catalog,
    names: NameIndex,
    docs: DocNames,
    live: DocSet,
}

impl Fixture {
    fn of(scratch: Scratch, catalog: Catalog) -> Self {
        let names = NameIndex::new(&catalog);
        let docs = DocNames::new(&catalog).unwrap();
        let live = DocSet::new(catalog.next_doc().0, 0..catalog.next_doc().0);
        Fixture {
            _scratch: scratch,
            catalog,
            names,
            docs,
            live,
        }
    }
}

fn fixture(name: &str, files: &[(&str, u64, u8)]) -> Fixture {
    let scratch = Scratch::new(&format!("verify-{name}"));
    let mut txn = Transaction::begin(&scratch.0, 1).unwrap();
    let mut w = txn.batch();
    let r = w.root(b"/r", dir(1));
    for (i, &(name, size, hash)) in files.iter().enumerate() {
        w.file(
            r,
            name.as_bytes(),
            file(10 + i as u64, size, 3600),
            Content::Hashed([hash; 16]),
        );
    }
    txn.add(w);
    txn.commit().unwrap();
    let catalog = lazy(&scratch).into_resident().unwrap();
    Fixture::of(scratch, catalog)
}

/// A gate: runs on the reading thread once a read has taken its bytes,
/// with the name and the read's stop predicate, and may block.
type Gate = Box<dyn Fn(&str, &dyn Fn() -> bool) + Send + Sync>;

/// A scripted file's bytes, and what a stat or read of it reports.
type File = (Vec<u8>, Option<ReadVersion>);

/// The scripted reader shared by every thread's [`Reader`].
struct Script {
    /// Each name's bytes and what its stat or read reports; `None` fails.
    files: Mutex<HashMap<String, File>>,
    /// Every request in the order made: the name, and `stat` or `read`.
    log: Mutex<Vec<(String, &'static str)>>,
    gate: Gate,
    /// Runs on the stating thread after a stat is logged.
    on_stat: Box<dyn Fn(&str) + Send + Sync>,
}

impl Script {
    /// Every name reads as `bytes`, at its catalogued version.
    fn new<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let files = files
            .into_iter()
            .map(|(name, bytes)| {
                let read = (bytes.as_bytes().to_vec(), Some(ReadVersion::Catalogued));
                (name.to_string(), read)
            })
            .collect();
        Self {
            files: Mutex::new(files),
            log: Mutex::default(),
            gate: Box::new(|_, _| {}),
            on_stat: Box::new(|_| {}),
        }
    }

    fn gated(mut self, gate: impl Fn(&str, &dyn Fn() -> bool) + Send + Sync + 'static) -> Self {
        self.gate = Box::new(gate);
        self
    }

    fn on_stat(mut self, hook: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.on_stat = Box::new(hook);
        self
    }

    fn set(&self, name: &str, bytes: &str, version: Option<ReadVersion>) {
        let read = (bytes.as_bytes().to_vec(), version);
        self.files.lock().unwrap().insert(name.into(), read);
    }

    fn request(
        &self,
        catalog: &Catalog,
        name: NameId,
        request: ReadRequest<'_>,
    ) -> Option<ReadVersion> {
        let name = String::from_utf8(catalog.name(name).bytes.to_vec()).unwrap();
        let file = |name: &str| self.files.lock().unwrap().get(name).cloned();
        match request {
            ReadRequest::Stat => {
                self.log.lock().unwrap().push((name.clone(), "stat"));
                (self.on_stat)(&name);
                file(&name)?.1
            }
            ReadRequest::Bytes(out, stop) => {
                self.log.lock().unwrap().push((name.clone(), "read"));
                // The bytes are taken before the gate: a read that has
                // signalled has seen the file.
                let file = file(&name);
                (self.gate)(&name, stop);
                let (bytes, version) = file?;
                out.clear();
                out.extend_from_slice(&bytes);
                version
            }
            ReadRequest::Stream(each, stop) => {
                self.log.lock().unwrap().push((name.clone(), "read"));
                let file = file(&name);
                (self.gate)(&name, stop);
                let (bytes, version) = file?;
                each(&mut bytes.as_slice(), bytes.len() as u64).ok()?;
                version
            }
        }
    }

    /// The requests made for `name`, in order.
    fn requests(&self, name: &str) -> Vec<&'static str> {
        let log = self.log.lock().unwrap();
        log.iter()
            .filter(|(n, _)| n == name)
            .map(|&(_, request)| request)
            .collect()
    }
}

/// One run: each emitted row's basename and offset, and the stats.
type Output = (Vec<(String, Option<usize>)>, Stats);

/// Runs `text` on `threads` through the real pool, `emit` deciding after
/// each row is recorded.
fn run(
    fx: &Fixture,
    text: &str,
    threads: usize,
    script: &Script,
    cancelled: Option<&AtomicBool>,
    mut emit: impl FnMut(&[(String, Option<usize>)]) -> ControlFlow<()>,
) -> Output {
    let query = Query::parse(text, now()).unwrap();
    let pinned = Pinned::new(None, &fx.live);
    let plan = Plan {
        pinned: &pinned,
        docs: &fx.docs,
        bound: None,
    };
    let catalog = &fx.catalog;
    let fork = || -> Box<Reader<'_>> {
        Box::new(move |name, request| script.request(catalog, name, request))
    };
    let mut rows = Vec::new();
    let stats = query
        .run_content_parallel(
            catalog,
            &fx.names,
            &plan,
            Workers {
                fork: &fork,
                threads,
            },
            cancelled,
            |row: &Row<'_>| {
                let path = String::from_utf8(row.path.to_vec()).unwrap();
                let name = path.rsplit('/').next().unwrap().to_string();
                rows.push((name, row.offset));
                emit(&rows)
            },
        )
        .unwrap();
    (rows, stats)
}

/// Stops after `limit` rows.
fn limit(limit: usize) -> impl FnMut(&[(String, Option<usize>)]) -> ControlFlow<()> {
    move |rows| match rows.len() >= limit {
        true => ControlFlow::Break(()),
        false => ControlFlow::Continue(()),
    }
}

fn all(_: &[(String, Option<usize>)]) -> ControlFlow<()> {
    ControlFlow::Continue(())
}

/// The stats with the fields the thread count may change cleared: the
/// times, and the count itself.
fn comparable((rows, mut stats): Output) -> (Vec<(String, Option<usize>)>, String) {
    if let Some(report) = &mut stats.content {
        report.verification_time = Duration::ZERO;
        report.regex_time = Duration::ZERO;
        report.threads = 0;
    }
    (rows, format!("{stats:?}"))
}

fn report(stats: &Stats) -> &crate::ContentReport {
    stats.content.as_ref().unwrap()
}

fn names(rows: &[(String, Option<usize>)]) -> Vec<&str> {
    rows.iter().map(|(name, _)| name.as_str()).collect()
}

/// Five documents, all matching `grep:needle`, and `b1hold`, which a
/// `name:hold` alternative makes hold without a read.
const FIVE: &[(&str, u64, u8)] = &[
    ("a0", 10, 1),
    ("b1hold", 10, 2),
    ("c2", 10, 3),
    ("c3", 10, 4),
    ("c4", 10, 5),
];

fn five_script() -> Script {
    Script::new(FIVE.iter().map(|&(name, ..)| (name, "a needle")))
}

/// Holds `a0`'s read until `c4`'s has started. One other worker reads
/// `c2`, `c3` and `c4` in order, so by then `c2` and `c3` have finished:
/// the head is the last of five rows to be answered.
fn head_last(script: Script) -> Script {
    let started = Arc::new(Signal::default());
    script.gated(move |name, _| match name {
        "a0" => started.wait("c4's read"),
        "c4" => started.set(),
        _ => {}
    })
}

#[test]
fn rows_leave_in_name_order_whatever_order_they_finish_in() {
    let fx = fixture("order", FIVE);
    let query = "grep:needle OR name:hold";
    let one = comparable(run(&fx, query, 1, &five_script(), None, all));
    assert_eq!(names(&one.0), ["a0", "b1hold", "c2", "c3", "c4"]);
    assert_eq!(
        comparable(run(&fx, query, 2, &head_last(five_script()), None, all)),
        one
    );
}

/// Finding 1: a thread that cannot be created after another started must
/// not leave the started one waiting for jobs, and the scope joining it.
#[test]
fn a_failed_spawn_neither_hangs_nor_changes_the_output() {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let fx = fixture("spawn", FIVE);
        let query = "grep:needle OR name:hold";
        let one = comparable(run(&fx, query, 1, &five_script(), None, all));
        for fail_at in [1, 3, 0] {
            let _seams = Seams {
                fail_spawn_at: Some(fail_at),
                ..Seams::default()
            }
            .set();
            let output = run(&fx, query, 4, &five_script(), None, all);
            assert_eq!(
                report(&output.1).threads,
                fail_at.max(1),
                "failed at {fail_at}"
            );
            assert_eq!(comparable(output), one, "failed at {fail_at}");
        }
        done.send(()).unwrap();
    });
    match finished.recv_timeout(PATIENCE) {
        Ok(()) => {}
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => panic!("the run panicked"),
        Err(timeout) => panic!("a failed spawn hung the run: {timeout}"),
    }
}

/// Finding 2: a callback that cancels the query stops the rows already
/// answered behind it, the exact `b1hold` and the read `c2`, `c3`.
#[test]
fn a_cancelling_callback_stops_buffered_rows() {
    let fx = fixture("cancel", FIVE);
    for threads in [1, 2] {
        let script = match threads {
            1 => five_script(),
            _ => head_last(five_script()),
        };
        let cancelled = AtomicBool::new(false);
        let (rows, stats) = run(
            &fx,
            "grep:needle OR name:hold",
            threads,
            &script,
            Some(&cancelled),
            |_| {
                cancelled.store(true, Ordering::Release);
                ControlFlow::Continue(())
            },
        );
        assert_eq!(names(&rows), ["a0"], "{threads} threads");
        assert_eq!(stats.rows, 1);
        // Only the head's read is counted; the window rewinds to the next.
        assert_eq!(report(&stats).verified, 1, "{threads} threads");
        assert_eq!(report(&stats).matching.regex_documents, 1);
        assert_eq!(stats.candidates, 2, "{threads} threads");
    }
}

/// The budget scene for the limit tests. `a0` (no catalogued size) heads
/// the run; `b1` holds the whole budget; `c2` waits for bytes; `zz`, which
/// the query rejects, comes last. Two workers and four positions make
/// batches of one: one worker takes `a0` and then `c2`, the other `b1`.
/// `a0`'s read waits for `b1`'s, so `b1` holds the budget before `c2` is
/// claimed.
const BUDGET: &[(&str, u64, u8)] = &[
    ("a0", 0, 1),
    ("b1", crate::IN_FLIGHT_BYTES, 2),
    ("c2", 1, 3),
    ("zz", 1, 4),
];
const BUDGET_QUERY: &str = "grep:needle AND NOT name:zz";

/// Sets `waiting` when a worker waits for bytes.
fn budget_seams(waiting: &Arc<Signal>, teardown: Option<Arc<Poll>>) -> Seams {
    let waiting = Arc::clone(waiting);
    Seams {
        on_budget_wait: Some(Arc::new(move || waiting.set())),
        on_teardown: teardown,
        ..Seams::default()
    }
}

/// The budget scene's reader: `a0` waits for `b1`'s read; `b1` runs `hold`
/// once started.
fn budget_script(hold: impl Fn(&dyn Fn() -> bool) + Send + Sync + 'static) -> Script {
    let started = Signal::default();
    Script::new(BUDGET.iter().map(|&(name, ..)| (name, "needle"))).gated(move |name, stop| {
        match name {
            "a0" => started.wait("b1's read"),
            "b1" => {
                started.set();
                hold(stop);
            }
            _ => {}
        }
    })
}

/// Finding 3: once a limit stops the run, a read in progress is told to
/// stop, and a worker waiting for budget bytes starts no read.
#[test]
fn a_limit_stops_reads_in_progress_and_starts_none() {
    // `b1` reads until told to stop. The worker that read `a0` next takes
    // `c2` and waits for bytes; `a0`'s callback waits for that, then stops
    // the run.
    let fx = fixture("budget", BUDGET);
    let waiting = Arc::new(Signal::default());
    let _seams = budget_seams(&waiting, None).set();
    let told = Arc::new(AtomicBool::new(false));
    let script = budget_script({
        let told = Arc::clone(&told);
        move |stop| {
            let deadline = Instant::now() + PATIENCE;
            while !stop() && Instant::now() < deadline {
                std::thread::yield_now();
            }
            told.store(stop(), Ordering::Release);
        }
    });
    let (rows, stats) = run(&fx, BUDGET_QUERY, 2, &script, None, |_| {
        waiting.wait("a worker waiting for bytes");
        ControlFlow::Break(())
    });
    assert_eq!(names(&rows), ["a0"]);
    assert_eq!(report(&stats).verified, 1);
    assert!(
        told.load(Ordering::Acquire),
        "b1's read was never told to stop"
    );
    assert_eq!(
        script.requests("c2"),
        Vec::<&str>::new(),
        "c2 read after the limit"
    );
}

/// Round 2: the workers stop when the head sees the limit, not when the run
/// has finished dropping its state and cache. `b1` holds the whole budget,
/// and the worker that read `a0` waits for bytes to read `c2`. Once the
/// limit is seen, the caller is held in teardown while `b1` finishes and
/// frees its bytes. The woken worker must start no read.
#[test]
fn a_limit_stops_workers_before_teardown() {
    let fx = fixture("teardown", BUDGET);
    let waiting = Arc::new(Signal::default());
    let release = Arc::new(Signal::default());
    // Teardown frees `b1`, then waits until every job has finished, read
    // or stopped, `c2`'s included.
    let teardown: Arc<Poll> = {
        let release = Arc::clone(&release);
        Arc::new(move |finished: &dyn Fn() -> u64| {
            release.set();
            let deadline = Instant::now() + PATIENCE;
            while finished() < 3 {
                assert!(Instant::now() < deadline, "jobs never finished");
                std::thread::yield_now();
            }
        })
    };
    let _seams = budget_seams(&waiting, Some(teardown)).set();
    let script = budget_script(move |_| release.wait("teardown"));
    let (rows, stats) = run(&fx, BUDGET_QUERY, 2, &script, None, |_| {
        waiting.wait("a worker waiting for bytes");
        ControlFlow::Break(())
    });
    assert_eq!(names(&rows), ["a0"]);
    assert_eq!(report(&stats).verified, 1);
    assert_eq!(
        script.requests("c2"),
        Vec::<&str>::new(),
        "c2 read during teardown"
    );
}

/// Finding 6: a limit at the head counts the candidates up to the head,
/// however far the workers have got. `a.txt`'s read is held until workers
/// have finished 30 later positions, all rejected by name; `--limit=1`
/// then stops the run at `a.txt`, with one candidate counted, as on one
/// thread.
#[test]
fn a_limit_at_the_head_ignores_positions_finished_beyond_it() {
    let mut files = vec![("a.txt".to_string(), 10, 1)];
    files.extend((0..200).map(|i| (format!("b{i:03}"), 10, 2 + (i % 200) as u8)));
    let files: Vec<_> = files.iter().map(|(n, s, h)| (n.as_str(), *s, *h)).collect();
    let fx = fixture("rejected", &files);
    let query = "grep:needle AND ( name:a.txt OR name:absent )";
    let script = || Script::new(files.iter().map(|&(name, ..)| (name, "needle")));
    let ahead = Arc::new(Signal::default());
    let finished = Arc::new(AtomicUsize::new(0));
    let _seams = Seams {
        on_publish: Some({
            let (ahead, finished) = (Arc::clone(&ahead), Arc::clone(&finished));
            Arc::new(move |count: &dyn Fn() -> u64| {
                let count = count() as usize;
                finished.fetch_max(count, Ordering::AcqRel);
                if count >= 30 {
                    ahead.set();
                }
            })
        }),
        ..Seams::default()
    }
    .set();
    let gated = script().gated(move |name, _| {
        if name == "a.txt" {
            ahead.wait("30 later positions finished");
        }
    });
    let (rows, stats) = run(&fx, query, 2, &gated, None, limit(1));
    assert_eq!(names(&rows), ["a.txt"]);
    assert!(finished.load(Ordering::Acquire) >= 30);
    assert_eq!(stats.candidates, 1);
    let one = run(&fx, query, 1, &script(), None, limit(1));
    assert_eq!(comparable((rows, stats)), comparable(one));
}

/// 64 positions on two workers make batches of two: `a0` and `a1` are
/// the first batch, read by one worker in turn. The rest are fillers.
fn pairs() -> Vec<(String, u64, u8)> {
    let mut files = vec![("a0".to_string(), 10, 1), ("a1".to_string(), 10, 2)];
    files.extend((0..62).map(|i| (format!("b{i:02}"), 10, 3 + i as u8)));
    files
}

fn pairs_fixture(name: &str) -> (Fixture, Vec<String>) {
    let files = pairs();
    let borrowed: Vec<_> = files.iter().map(|(n, s, h)| (n.as_str(), *s, *h)).collect();
    let fx = fixture(name, &borrowed);
    (fx, files.iter().map(|(n, ..)| n.clone()).collect())
}

fn pairs_script(names: &[String]) -> Script {
    Script::new(names.iter().map(|name| (name.as_str(), "needle")))
}

/// Sol round 1, P1: a finished position is taken before the rest of its
/// batch. `a1`, after `a0` in its batch, reads until told to stop, which
/// only `a0`'s row reaching `--limit=1` can do.
#[test]
fn a_ready_head_is_taken_before_the_rest_of_its_batch() {
    let (fx, files) = pairs_fixture("batch-limit");
    let told = Arc::new(AtomicBool::new(false));
    let script = pairs_script(&files).gated({
        let told = Arc::clone(&told);
        move |name, stop| {
            if name == "a1" {
                let deadline = Instant::now() + PATIENCE;
                while !stop() && Instant::now() < deadline {
                    std::thread::yield_now();
                }
                told.store(stop(), Ordering::Release);
            }
        }
    });
    let (rows, stats) = run(&fx, "grep:needle", 2, &script, None, limit(1));
    assert!(told.load(Ordering::Acquire), "a0 waited for a1's read");
    assert_eq!(names(&rows), ["a0"]);
    let one = run(&fx, "grep:needle", 1, &pairs_script(&files), None, limit(1));
    assert_eq!(comparable((rows, stats)), comparable(one));
}

/// Prompt head emission: `a0`'s row is emitted while `a1`, after it in its
/// batch, is still being read; `a1`'s read waits for that emission.
#[test]
fn a_ready_head_is_emitted_while_its_batch_is_read() {
    let (fx, files) = pairs_fixture("batch-prompt");
    let emitted = Arc::new(Signal::default());
    let script = pairs_script(&files).gated({
        let emitted = Arc::clone(&emitted);
        move |name, _| {
            if name == "a1" {
                emitted.wait("a0's row emitted");
            }
        }
    });
    let output = run(&fx, "grep:needle", 2, &script, None, |rows| {
        if rows.len() == 1 {
            emitted.set();
        }
        ControlFlow::Continue(())
    });
    assert_eq!(output.0.len(), files.len());
    let one = run(&fx, "grep:needle", 1, &pairs_script(&files), None, all);
    assert_eq!(comparable(output), comparable(one));
}

/// Finding 7: on a pool, a document is read when a worker gets to it,
/// which can be before an earlier row's callback runs. Here `b1` has been
/// read before `a0`'s callback deletes it: one thread drops `b1` as
/// changed, a pool emits it as read. This is the accepted semantics.
#[test]
fn a_pool_reads_ahead_of_earlier_callbacks() {
    let fx = fixture("read-time", &[("a0", 10, 1), ("b1", 10, 2)]);
    for threads in [1, 2] {
        let read = Arc::new(Signal::default());
        let script =
            Script::new([("a0", "needle"), ("b1", "a needle")]).gated(move |name, _| match name {
                "a0" if threads > 1 => read.wait("b1's read"),
                "b1" => read.set(),
                _ => {}
            });
        let (rows, stats) = run(&fx, "grep:needle", threads, &script, None, |rows| {
            if rows.len() == 1 {
                script.set("b1", "", None);
            }
            ControlFlow::Continue(())
        });
        let changed = report(&stats).changed;
        match threads {
            1 => assert_eq!((names(&rows), changed), (vec!["a0"], 1)),
            _ => assert_eq!(
                (rows, changed),
                (vec![("a0".into(), Some(0)), ("b1".into(), Some(2))], 0)
            ),
        }
    }
}

/// Finding 8: a second name of a document in flight is queued behind it
/// and settled at the head as the sequential order settles it, whatever
/// the first read found.
#[test]
fn a_duplicate_behind_its_document_in_flight_settles_sequentially() {
    // `a1` and `a2` are one document; `b3` is another, so its read
    // starting means `a2` is already in the window behind `a1`.
    let fx = fixture("duplicate", &[("a1", 10, 1), ("a2", 10, 1), ("b3", 10, 2)]);
    let cases = [
        // The first read fails: the duplicate reads for itself.
        ("fails", None, vec!["read"]),
        // Catalogued: the duplicate is answered by a stat.
        ("catalogued", Some(ReadVersion::Catalogued), vec!["stat"]),
        // Current: answers for that path only; the duplicate reads.
        ("current", Some(ReadVersion::Current), vec!["read"]),
    ];
    for (case, first, duplicate) in cases {
        let script = |threads: usize| {
            let started = Arc::new(Signal::default());
            let script = Script::new([("a1", "x needle"), ("a2", "x needle"), ("b3", "needle")])
                .gated(move |name, _| match name {
                    "a1" if threads > 1 => started.wait("b3's read"),
                    "b3" => started.set(),
                    _ => {}
                });
            script.set("a1", "x needle", first);
            script
        };
        let one = script(1);
        let expected = comparable(run(&fx, "grep:needle", 1, &one, None, all));
        assert_eq!(one.requests("a2"), duplicate, "{case}");
        let pool = script(2);
        let output = comparable(run(&fx, "grep:needle", 2, &pool, None, all));
        assert_eq!(output, expected, "{case}");
        assert_eq!(pool.requests("a2"), duplicate, "{case}");
        assert_eq!(pool.requests("b3"), ["read"], "{case}");
    }
}

/// A later name of a document whose read has finished is settled on a
/// worker, as the sequential order settles it at the head: `c3` is stated
/// (and read again if its stat finds another version) while `b2`, ahead of
/// it, is still being read, so the calling thread cannot have done it.
#[test]
fn a_duplicate_after_its_document_settles_on_a_worker() {
    let fx = fixture("settled", &[("a1", 10, 1), ("b2", 10, 2), ("c3", 10, 1)]);
    let cases = [
        // Still the catalogued version: the cached answers hold.
        ("catalogued", Some(ReadVersion::Catalogued), vec!["stat"]),
        // Another version: the duplicate reads for itself.
        ("current", Some(ReadVersion::Current), vec!["stat", "read"]),
        // Gone: changed, with no row.
        ("gone", None, vec!["stat"]),
    ];
    for (case, duplicate, requests) in cases {
        let script = |threads: usize| {
            let stated = Arc::new(Signal::default());
            let waited = stated.clone();
            let script = Script::new([("a1", "x needle"), ("b2", "needle"), ("c3", "x needle")])
                .gated(move |name, _| {
                    if name == "b2" && threads > 1 {
                        waited.wait("c3's stat");
                    }
                })
                .on_stat(move |name| {
                    if name == "c3" {
                        stated.set();
                    }
                });
            script.set("c3", "x needle", duplicate);
            script
        };
        let one = script(1);
        let expected = comparable(run(&fx, "grep:needle", 1, &one, None, all));
        assert_eq!(one.requests("c3"), requests, "{case}");
        let pool = script(2);
        let output = comparable(run(&fx, "grep:needle", 2, &pool, None, all));
        assert_eq!(output, expected, "{case}");
        assert_eq!(pool.requests("c3"), requests, "{case}");
        assert_eq!(pool.requests("a1"), ["read"], "{case}");
    }
}

/// Sol round 1, P2: an overlay can give a document a name whose id is below
/// its checkpoint name's, so its names are not stored in id order. The
/// lower id still reads the document, once: `included.rs`'s checkpoint
/// name sorts after the overlay's `old.rs`, which now names its file too.
/// `old.rs`'s read is held until every other position is finished, so a
/// worker that took `included.rs` for a first name would have read it.
#[test]
fn an_overlay_name_below_its_documents_checkpoint_name_reads_it_once() {
    let (scratch, mut state, mut writer) = super::overlay::setup("verify-overlay");
    let old = state.clone();
    let id = |name: &[u8]| *state.edges.iter().find(|(_, e)| e.name == name).unwrap().0;
    let (moved, kept) = (id(b"old.rs"), id(b"included.rs"));
    assert!(
        moved < kept,
        "the fixture needs old.rs's id below included.rs's"
    );
    let gone = state.edges[&moved].child;
    let target = state.edges[&kept].child;
    state.edges.get_mut(&moved).unwrap().child = target;
    state.nodes.remove(&gone);
    let view = super::overlay::publish(&mut state, &old, &mut writer);
    let fx = Fixture::of(scratch, view);
    let doc = fx.catalog.doc(ferret_catalog::InoId(target)).unwrap();
    assert_eq!(
        fx.docs.names(doc.0).len(),
        2,
        "old.rs and included.rs name one document"
    );
    let files = ["old.rs", "included.rs", "kept.rs"].map(|name| (name, "a needle"));
    let one = Script::new(files);
    let alone = run(&fx, "grep:needle", 1, &one, None, all);
    let positions = alone.1.candidates;
    let expected = comparable(alone);
    assert_eq!(one.requests("included.rs"), ["stat"]);
    let others = Arc::new(Signal::default());
    let _seams = Seams {
        on_publish: Some({
            let others = Arc::clone(&others);
            Arc::new(move |finished: &dyn Fn() -> u64| {
                if finished() + 1 >= positions {
                    others.set();
                }
            })
        }),
        ..Seams::default()
    }
    .set();
    let pool = Script::new(files).gated(move |name, _| {
        if name == "old.rs" {
            others.wait("every other position finished");
        }
    });
    let output = comparable(run(&fx, "grep:needle", 2, &pool, None, all));
    assert_eq!(output, expected);
    assert_eq!(pool.requests("included.rs"), ["stat"]);
    assert_eq!(pool.requests("old.rs"), ["read"]);
}
