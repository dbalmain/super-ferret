//! D109 regressions through the real scheduler, renderer and publisher.

use super::*;
use crate::grep::groups::capture::{self, Budget};
use std::sync::Condvar;
use std::time::Duration;

struct ObservedBudget(Arc<Mutex<Option<Arc<Budget>>>>);

impl ObservedBudget {
    fn new() -> Self {
        let shared = Arc::new(Mutex::new(None));
        let seen = shared.clone();
        capture::CREATED.with_borrow_mut(|hook| {
            *hook = Some(Box::new(move |budget| *lock(&seen) = Some(budget.clone())));
        });
        Self(shared)
    }

    fn get(&self) -> Arc<Budget> {
        lock(&self.0).as_ref().unwrap().clone()
    }
}

impl Drop for ObservedBudget {
    fn drop(&mut self) {
        capture::CREATED.with_borrow_mut(|hook| *hook = None);
    }
}

#[derive(Default)]
struct Started(Mutex<bool>, Condvar);

impl Started {
    fn signal(&self) {
        *lock(&self.0) = true;
        self.1.notify_all();
    }

    fn wait(&self) {
        let (started, _) = self
            .1
            .wait_timeout_while(lock(&self.0), Duration::from_secs(10), |started| !*started)
            .unwrap();
        assert!(*started, "the first file never started streaming");
    }
}

struct Pausing<'a> {
    bytes: &'a [u8],
    position: usize,
    at: usize,
    resume: &'a mut dyn FnMut(),
}

impl Read for Pausing<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.position == self.at {
            (self.resume)();
            self.at = usize::MAX;
        }
        let count = output
            .len()
            .min(self.bytes.len() - self.position)
            .min(self.at - self.position);
        output[..count].copy_from_slice(&self.bytes[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

fn records(size: usize) -> Vec<u8> {
    let mut line = vec![b'x'; CHUNK];
    line[..3].copy_from_slice(b"red");
    line[CHUNK - 1] = b'\n';
    line.repeat(size / CHUNK)
}

// Blocking at 4 MiB strands the other readers before their closing checks.
// Hold the first stream's checked read open until three captures pass 4 MiB.
#[test]
fn heavy_files_capture_concurrently_behind_a_stream() {
    let plan = plan(&["--no-filename", "red"]);
    let bytes = records(6 << 20);
    let started = Started::default();
    let (sender, receiver) = mpsc::channel();
    let receiver = Mutex::new(receiver);
    let cancelled = AtomicBool::new(false);
    let completed = std::sync::atomic::AtomicUsize::new(0);
    let fork = || -> Option<Box<CheckedReader<'_>>> {
        Some(Box::new(|name, request| {
            let ReadRequest::Stream(each, _) = request else {
                panic!("stream expected")
            };
            if name.0 == 0 {
                let mut resume = || {
                    for _ in 0..3 {
                        if lock(&receiver)
                            .recv_timeout(Duration::from_secs(10))
                            .is_err()
                        {
                            cancelled.store(true, Ordering::Release);
                            break;
                        }
                        completed.fetch_add(1, Ordering::Relaxed);
                    }
                };
                let mut input = Pausing {
                    bytes: &bytes,
                    position: 0,
                    at: GROUP_BYTES + 2 * CHUNK,
                    resume: &mut resume,
                };
                let _ = each(&mut input, bytes.len() as u64);
            } else {
                started.wait();
                if each(&mut &bytes[..], bytes.len() as u64).is_ok() {
                    sender.send(()).unwrap();
                }
            }
            ReadOutcome::Stable(ReadVersion::Current)
        }))
    };
    struct Sink<'a>(&'a Started, usize);
    impl Write for Sink<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.signal();
            self.1 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut host = host(&plan, Sink(&started, 0));
    host.filename = false;
    let mut jobs = (0..4).map(|n| Job::Checked(format!("file-{n}").into(), NameId(n), n));
    run(
        &mut host,
        |_| jobs.next().map(|job| (0, job)),
        &fork,
        None,
        4,
        &cancelled,
    )
    .unwrap();
    assert_eq!(completed.load(Ordering::Relaxed), 3);
    assert_eq!(host.writer.1, 4 * bytes.len());
}

// Fill the real allocation budget, including queued/in-progress captures,
// then make the active reader emit more bytes before its closing check.
#[test]
fn full_capture_budget_still_lets_the_streamer_finish() {
    let observed = ObservedBudget::new();
    let plan = plan(&["--no-filename", "red"]);
    let bytes = records(32 << 20);
    let started = Started::default();
    let cancelled = AtomicBool::new(false);
    let reached = AtomicBool::new(false);
    let fork = || -> Option<Box<CheckedReader<'_>>> {
        Some(Box::new(|name, request| {
            let ReadRequest::Stream(each, _) = request else {
                panic!("stream expected")
            };
            if name.0 == 0 {
                let mut resume = || {
                    let full = observed.get().wait_for_full();
                    reached.store(full, Ordering::Release);
                    if !full {
                        cancelled.store(true, Ordering::Release);
                    }
                };
                let mut input = Pausing {
                    bytes: &bytes[..8 << 20],
                    position: 0,
                    at: GROUP_BYTES + 2 * CHUNK,
                    resume: &mut resume,
                };
                let _ = each(&mut input, (8 << 20) as u64);
            } else {
                started.wait();
                let _ = each(&mut &bytes[..], bytes.len() as u64);
            }
            ReadOutcome::Stable(ReadVersion::Current)
        }))
    };
    struct Sink<'a>(&'a Started, usize);
    impl Write for Sink<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.signal();
            self.1 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut host = host(&plan, Sink(&started, 0));
    host.filename = false;
    let mut jobs = (0..3).map(|n| Job::Checked(format!("file-{n}").into(), NameId(n), n));
    run(
        &mut host,
        |_| {
            let job = jobs.next()?;
            // At limit 64, two waiters can occupy both read admissions and
            // prevent the owner they wait for from ever starting. Admit the
            // owner before offering jobs that wait for its publication.
            if !matches!(job, Job::Checked(_, NameId(0), _)) {
                started.wait();
            }
            Some((0, job))
        },
        &fork,
        None,
        3,
        &cancelled,
    )
    .unwrap();
    let (left, peak, waits) = observed.get().usage();
    assert!(
        reached.load(Ordering::Acquire),
        "the shared budget never filled"
    );
    assert_eq!(peak, (2 * 3 + 1) * GROUP_BYTES);
    assert!(waits > 0);
    assert_eq!(left, 0, "publication must return every capture allocation");
    assert_eq!(host.writer.1, (8 << 20) + 2 * bytes.len());

    // Two complete read admissions fit at limit 64. The waiting capturer
    // must keep its permit without preventing the active stream's progress.
    const MARKER: &str = "FERRET_TEST_CAPTURE_LOW_DESCRIPTORS";
    if std::env::var_os(MARKER).is_none() {
        let output = std::process::Command::new("sh")
            .args(["-c", "ulimit -n 64 && exec \"$0\" \"$@\""])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "grep::groups::tests::capture::full_capture_budget_still_lets_the_streamer_finish",
                "--nocapture",
            ])
            .env(MARKER, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
}

// Stop with captures asleep on a full budget. The publisher has not released
// its prefix or streaming slot, so only cancellation polling can wake them.
#[test]
fn full_capture_waiters_stop_on_cancellation_and_broken_pipe() {
    for broken_pipe in [false, true] {
        let observed = ObservedBudget::new();
        let plan = plan(&["--no-filename", "red"]);
        let bytes = records(32 << 20);
        let started = Started::default();
        let cancelled = AtomicBool::new(false);
        let reached = AtomicBool::new(false);
        let woke = AtomicBool::new(false);
        let (sender, receiver) = mpsc::channel();
        let receiver = Mutex::new(receiver);
        let fork = || -> Option<Box<CheckedReader<'_>>> {
            Some(Box::new(|name, request| {
                let ReadRequest::Stream(each, _) = request else {
                    panic!("stream expected")
                };
                if name.0 != 0 {
                    started.wait();
                }
                let _ = each(&mut &bytes[..], bytes.len() as u64);
                if name.0 != 0 {
                    sender.send(()).unwrap();
                }
                ReadOutcome::Stable(ReadVersion::Current)
            }))
        };
        struct Stop<'a> {
            started: &'a Started,
            observed: &'a ObservedBudget,
            reached: &'a AtomicBool,
            cancelled: &'a AtomicBool,
            broken_pipe: bool,
            finished: &'a Mutex<Receiver<()>>,
            woke: &'a AtomicBool,
            triggered: bool,
        }
        impl Write for Stop<'_> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.triggered {
                    return Ok(bytes.len());
                }
                self.triggered = true;
                self.started.signal();
                let full = self.observed.get().wait_for_full();
                self.reached.store(full, Ordering::Release);
                if self.broken_pipe {
                    Err(io::ErrorKind::BrokenPipe.into())
                } else {
                    self.cancelled.store(true, Ordering::Release);
                    let woke = (0..2).all(|_| {
                        lock(self.finished)
                            .recv_timeout(Duration::from_secs(10))
                            .is_ok()
                    });
                    self.woke.store(woke, Ordering::Release);
                    Ok(bytes.len())
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut host = host(
            &plan,
            Stop {
                started: &started,
                observed: &observed,
                reached: &reached,
                cancelled: &cancelled,
                broken_pipe,
                finished: &receiver,
                woke: &woke,
                triggered: false,
            },
        );
        host.filename = false;
        let mut jobs = (0..3).map(|n| Job::Checked(format!("file-{n}").into(), NameId(n), n));
        let result = run(
            &mut host,
            |_| jobs.next().map(|job| (0, job)),
            &fork,
            None,
            3,
            &cancelled,
        );
        if broken_pipe {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        } else {
            result.unwrap();
            assert!(
                woke.load(Ordering::Acquire),
                "budget waiters missed cancellation"
            );
        }
        assert!(
            reached.load(Ordering::Acquire),
            "no full-budget cancellation exercised"
        );
        assert!(cancelled.load(Ordering::Acquire));
        assert_eq!(observed.get().usage().0, 0);
    }
}

// The capture limit has a 64 KiB floor, but the allocation starts small.
// Charging the whole floor for every tiny group needlessly stalls the pool.
#[test]
fn small_captures_charge_allocations_instead_of_the_chunk_limit() {
    let budget = Budget::new(4);
    let plan = plan(&["red"]);
    let render = Render {
        plan: &plan,
        prefix: None,
        stopped: &|| false,
        capture: Some(&budget),
    };
    let mut sink = io::sink();
    let mut output = OutputCapture::new(&mut sink).pooled(&render);
    for bytes in [b"r".as_slice(), b"ed", b"\n"] {
        output.write_all(bytes).unwrap();
        assert_eq!(budget.usage().0, output.buffer.memory_bytes());
    }
    assert!(budget.usage().0 < CHUNK);
    drop(output);
    assert_eq!(budget.usage().0, 0);
}
