mod capture;
use super::job::{Cache, Cached, Job, Worker, observe};
use super::render::{Cancellable, OutputCapture, Record, write_parts};
use super::*;
use ferret_query::{ReadOperation, ReadVersion};
use std::cell::Cell;
use std::io::IoSlice;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};

fn host<W: Write>(plan: &Plan, writer: W) -> Host<'_, W> {
    Host {
        plan,
        runtime: None,
        filename: true,
        writer,
        buffer: Vec::new(),
        matched: false,
        emitted_group: false,
        sent_output: false,
        json_stats: json::Stats::default(),
        errors: false,
        cancelled: None,
    }
}

// D111: a spilled fallback parent must reopen under the client cwd,
// while output keeps the original operand spelling.
#[test]
fn a_closed_listed_parent_uses_the_lookup_path_not_the_display_path() {
    use std::os::unix::fs::MetadataExt;
    let directory =
        std::env::temp_dir().join(format!("ferret-grep-listed-cwd-{}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    let actual = directory.join("file");
    fs::write(&actual, b"red\n").unwrap();
    let stat = directory.metadata().unwrap();
    let plan = plan(&["red"]);
    let mut host = host(&plan, Vec::new());
    let mut job = Some(Job::Listed(
        "display/file".into(),
        actual,
        engine::reader::Parent::Closed(stat.dev(), stat.ino()),
        false,
        false,
    ));
    let fork = || -> Option<Box<CheckedReader<'_>>> { None };
    run(
        &mut host,
        |_| job.take().map(|job| (4, job)),
        &fork,
        None,
        1,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(host.writer, b"display/file:red\n");
    assert!(!host.errors);
    fs::remove_dir_all(directory).unwrap();
}

// Drive the real handoff with groups just over the capture threshold.
// Groups may finish capturing behind a stream (D109); all must stay
// whole and none may spill.
#[test]
fn parallel_publication_keeps_large_groups_whole() {
    use std::cell::RefCell;
    use std::rc::Rc;
    // Checks each 20-byte "file-NN:red payload\n" line as it arrives and
    // records runs of one prefix, without holding the output.
    #[derive(Default)]
    struct Runs {
        carry: Vec<u8>,
        runs: Vec<(Vec<u8>, usize)>,
    }
    impl Write for Runs {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.carry.extend_from_slice(bytes);
            let whole = self.carry.len() / 20 * 20;
            for line in self.carry[..whole].chunks_exact(20) {
                assert_eq!(&line[8..], b"red payload\n");
                match self.runs.last_mut() {
                    Some((prefix, count)) if prefix[..] == line[..8] => *count += 1,
                    _ => self.runs.push((line[..8].to_vec(), 1)),
                }
            }
            self.carry.drain(..whole);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    const FILES: u32 = 24;
    let plan = plan(&["red"]);
    let lines = GROUP_BYTES / 20 + 1;
    let bytes = b"red payload\n".repeat(lines);
    let fork = || -> Option<Box<CheckedReader<'_>>> {
        Some(Box::new(|_, request| {
            let ReadRequest::Stream(each, _) = request else {
                panic!("stream")
            };
            each(&mut &bytes[..], bytes.len() as u64).unwrap();
            ReadOutcome::Stable(ReadVersion::Current)
        }))
    };
    let spilled = Rc::new(RefCell::new(Vec::new()));
    let seen = spilled.clone();
    super::super::seam::COMMIT.with_borrow_mut(|hook| {
        *hook = Some(Box::new(move |output| {
            seen.borrow_mut().push(output.is_spilled());
        }));
    });
    let mut host = host(&plan, Runs::default());
    let mut jobs = (0..FILES).map(|n| Job::Checked(format!("file-{n:02}").into(), NameId(n), n));
    let result = run(
        &mut host,
        |_| jobs.next().map(|job| (bytes.len() as u64, job)),
        &fork,
        None,
        8,
        &AtomicBool::new(false),
    );
    super::super::seam::COMMIT.with_borrow_mut(|hook| *hook = None);
    result.unwrap();
    assert!(host.writer.carry.is_empty());
    assert!(
        spilled.borrow().iter().all(|spilled| !spilled),
        "waiting captures must remain in memory"
    );
    let mut runs = std::mem::take(&mut host.writer.runs);
    runs.sort();
    assert_eq!(
        runs,
        (0..FILES)
            .map(|n| (format!("file-{n:02}:").into_bytes(), lines))
            .collect::<Vec<_>>()
    );
}

// One thread must preserve producer order, even when it is not sorted.
#[test]
fn single_thread_publication_keeps_walk_order() {
    let plan = plan(&["red"]);
    let fork = || -> Option<Box<CheckedReader<'_>>> {
        Some(Box::new(|_, request| {
            let ReadRequest::Stream(each, _) = request else {
                panic!("stream")
            };
            each(&mut &b"red\n"[..], 4).unwrap();
            ReadOutcome::Stable(ReadVersion::Current)
        }))
    };
    let mut host = host(&plan, Vec::new());
    let mut jobs = ["z", "a", "m"]
        .into_iter()
        .map(|p| Job::Checked(p.into(), NameId(0), 0));
    run(
        &mut host,
        |_| jobs.next().map(|job| (4, job)),
        &fork,
        None,
        1,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(host.writer, b"z:red\na:red\nm:red\n");
}

// A publication fault must break the completion drain. Other finished
// captures already waiting in the queue cannot write afterwards.
#[test]
fn publication_fault_stops_waiting_workers() {
    #[derive(Default)]
    struct Fault(usize);
    impl Write for Fault {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            self.0 += 1;
            // Let the other prepared captures reach the real queue while
            // this first publication is failing.
            std::thread::sleep(std::time::Duration::from_millis(10));
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let plan = plan(&["red"]);
    let barrier = std::sync::Barrier::new(8);
    let fork = || -> Option<Box<CheckedReader<'_>>> {
        Some(Box::new(|_, request| {
            let ReadRequest::Stream(each, _) = request else {
                panic!("stream")
            };
            each(&mut &b"red\n"[..], 4).unwrap();
            barrier.wait();
            ReadOutcome::Stable(ReadVersion::Current)
        }))
    };
    let mut host = host(&plan, Fault::default());
    let mut jobs = (0..8).map(|n| Job::Checked(format!("file-{n}").into(), NameId(n), n));
    let cancelled = AtomicBool::new(false);
    let error = run(
        &mut host,
        |_| jobs.next().map(|job| (4, job)),
        &fork,
        None,
        8,
        &cancelled,
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert!(cancelled.load(Ordering::Acquire));
    assert_eq!(host.writer.0, 1);

    // Dropping an active stream receiver wakes its sender before the
    // caller records the stdout fault. That relay error must never win.
    let mut bytes = b"red ".to_vec();
    bytes.resize(GROUP_BYTES + CHUNK, b'x');
    bytes.push(b'\n');
    let fork = || -> Option<Box<CheckedReader<'_>>> {
        Some(Box::new(|_, request| {
            let ReadRequest::Stream(each, _) = request else {
                panic!("stream")
            };
            let _ = each(&mut &bytes[..], bytes.len() as u64);
            ReadOutcome::Stable(ReadVersion::Current)
        }))
    };
    for _ in 0..8 {
        let mut streaming_host = self::host(&plan, Fault::default());
        let mut jobs = (0..16).map(|n| Job::Checked(format!("file-{n}").into(), NameId(n), n));
        let cancelled = AtomicBool::new(false);
        let error = run(
            &mut streaming_host,
            |_| jobs.next().map(|job| (bytes.len() as u64, job)),
            &fork,
            None,
            4,
            &cancelled,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(cancelled.load(Ordering::Acquire));
        assert_eq!(streaming_host.writer.0, 1);
    }
}

// A slow first read cannot hold back another file's completed transaction.
#[test]
fn completed_group_publishes_before_a_slow_earlier_file() {
    struct Signal<'a> {
        bytes: Vec<u8>,
        published: &'a AtomicBool,
    }
    impl Write for Signal<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            self.published.store(true, Ordering::Release);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let plan = plan(&["red"]);
    let published = AtomicBool::new(false);
    let fork = || -> Option<Box<CheckedReader<'_>>> {
        Some(Box::new(|name, request| {
            let ReadRequest::Stream(each, _) = request else {
                panic!("stream")
            };
            if name.0 == 0 {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                while !published.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                    std::thread::yield_now();
                }
            }
            each(&mut &b"red\n"[..], 4).unwrap();
            ReadOutcome::Stable(ReadVersion::Current)
        }))
    };
    let mut host = host(
        &plan,
        Signal {
            bytes: Vec::new(),
            published: &published,
        },
    );
    let mut jobs = [
        Job::Checked("slow".into(), NameId(0), 0),
        Job::Checked("fast".into(), NameId(1), 1),
    ]
    .into_iter();
    run(
        &mut host,
        |_| jobs.next().map(|job| (4, job)),
        &fork,
        None,
        2,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(host.writer.bytes, b"fast:red\nslow:red\n");
}

// Batched handoffs: the worker that finished "fast" takes the next file,
// which is slow too. Its group must not wait in the worker's batch; it
// publishes before either slow file finishes.
#[test]
fn held_group_publishes_while_its_worker_reads_a_slow_file() {
    held_group_before_slow_file(&["red"], ["fast:red", "later:red", "slow:red"]);
}

// Counts render on the publisher, so their capture weight is zero even
// when they print. Both slow reads wait for the fast count to publish:
// gating idle handoff on capture bytes strands that count behind them.
#[test]
fn zero_weight_count_publishes_while_its_worker_reads_a_slow_file() {
    held_group_before_slow_file(&["-c", "red"], ["fast:1", "later:1", "slow:1"]);
}

fn held_group_before_slow_file(args: &[&str], expected: [&str; 3]) {
    struct Signal<'a> {
        bytes: Vec<u8>,
        published: &'a AtomicBool,
    }
    impl Write for Signal<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            self.published.store(true, Ordering::Release);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let plan = plan(args);
    let published = AtomicBool::new(false);
    let waited_out = AtomicBool::new(false);
    let fork = || -> Option<Box<CheckedReader<'_>>> {
        Some(Box::new(|name, request| {
            let ReadRequest::Stream(each, _) = request else {
                panic!("stream")
            };
            if name.0 != 1 {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                while !published.load(Ordering::Acquire) {
                    if std::time::Instant::now() >= deadline {
                        waited_out.store(true, Ordering::Release);
                        break;
                    }
                    std::thread::yield_now();
                }
            }
            each(&mut &b"red\n"[..], 4).unwrap();
            ReadOutcome::Stable(ReadVersion::Current)
        }))
    };
    let mut host = host(
        &plan,
        Signal {
            bytes: Vec::new(),
            published: &published,
        },
    );
    let mut jobs = [
        Job::Checked("slow".into(), NameId(0), 0),
        Job::Checked("fast".into(), NameId(1), 1),
        Job::Checked("later".into(), NameId(2), 2),
    ]
    .into_iter();
    run(
        &mut host,
        |_| jobs.next().map(|job| (4, job)),
        &fork,
        None,
        2,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(!waited_out.load(Ordering::Acquire));
    let output = std::str::from_utf8(&host.writer.bytes).unwrap();
    let mut lines: Vec<_> = output.lines().collect();
    assert_eq!(lines[0], expected[0]);
    lines.sort_unstable();
    assert_eq!(lines, expected);
}

// Policy can diagnose while advancing a live source. It must use the same
// lock as real publication, including its final destination flush.
#[test]
fn live_source_diagnostics_wait_for_the_published_group() {
    struct Publishing<'a>(&'a AtomicBool);
    impl Write for Publishing<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.store(true, Ordering::Release);
            std::thread::sleep(std::time::Duration::from_millis(20));
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0.store(false, Ordering::Release);
            Ok(())
        }
    }
    let plan = plan(&["--live", "red"]);
    let publishing = AtomicBool::new(false);
    let mut host = host(&plan, Publishing(&publishing));
    let fork = || -> Option<Box<CheckedReader<'_>>> {
        Some(Box::new(|_, request| {
            let ReadRequest::Stream(each, _) = request else {
                panic!("stream")
            };
            each(&mut &b"red\n"[..], 4).unwrap();
            ReadOutcome::Stable(ReadVersion::Current)
        }))
    };
    let mut jobs = (0..32).map(|n| Job::Checked(format!("file-{n}").into(), NameId(n), n));
    run(
        &mut host,
        |_| {
            // Advance slowly enough for an uncoordinated publisher to
            // begin while policy is still loading this directory.
            std::thread::sleep(std::time::Duration::from_millis(5));
            assert!(
                !publishing.load(Ordering::Acquire),
                "live warning would split a group"
            );
            jobs.next().map(|job| (4, job))
        },
        &fork,
        None,
        8,
        &AtomicBool::new(false),
    )
    .unwrap();
}

// D102: a real context write fault wins over a later reader error and
// remains file-local. Isolate RLIMIT_FSIZE in a child process.
#[test]
fn late_spill_failure_remains_a_capture_fault() {
    const MARKER: &str = "FERRET_TEST_LATE_SPILL";
    if std::env::var_os(MARKER).is_none() {
        // The limit is the capture size, so the final spill bytes fail.
        let launch = format!(
            "import os, resource, signal, sys; \
                 resource.setrlimit(resource.RLIMIT_FSIZE, ({GROUP_BYTES}, {GROUP_BYTES})); \
                 signal.signal(signal.SIGXFSZ, signal.SIG_IGN); \
                 os.execv(sys.argv[1], [sys.argv[1], '--exact', \
                 'grep::groups::tests::late_spill_failure_remains_a_capture_fault', '--nocapture'])"
        );
        let output = std::process::Command::new(
            std::env::var_os("FERRET_PYTHON").expect("dev shell supplies Python"),
        )
        .args(["-c", &launch])
        .arg(std::env::current_exe().unwrap())
        .env(MARKER, "1")
        .output()
        .unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    }
    let line_grep::Command::Search(plan) = line_grep::parse(&["-B1".into(), "red".into()]).unwrap()
    else {
        panic!("search")
    };
    let render = Render {
        plan: &plan,
        prefix: None,
        stopped: &|| false,
        capture: None,
    };
    let mut bytes = vec![b'x'; GROUP_BYTES];
    bytes.extend_from_slice(b"\nred\n");
    let observed = observe(
        &render,
        &mut Vec::new(),
        &mut |each| {
            let _ = each(&mut &bytes[..], bytes.len() as u64);
            ReadOutcome::Failed(ReadFault::Io(
                ReadOperation::Read,
                io::ErrorKind::PermissionDenied.into(),
            ))
        },
        false,
    )
    .unwrap();
    let Observed::Spool(error) = observed else {
        panic!("context spool fault must fail only the file")
    };
    assert!(
        error.to_string().starts_with("write output spill "),
        "{error}"
    );
    assert!(
        error.to_string().contains("File too large (os error 27)"),
        "{error}"
    );
}

// Three quarter-limit records stay in memory; three half-limit records
// cross the real streaming transition and never create a rendered spill.
#[test]
fn vectored_plain_records_replay_exactly_before_and_after_streaming() {
    for (size, streams) in [
        (0, false),
        (11, false),
        (GROUP_BYTES / 4, false),
        (GROUP_BYTES / 2, true),
    ] {
        let bytes = vec![b'x'; size];
        let mut replay = Vec::new();
        let mut output = OutputCapture::new(&mut replay);
        for _ in 0..3 {
            let mut writer = Cancellable {
                inner: &mut output,
                stopped: &|| false,
            };
            write_parts(
                &mut writer,
                &mut [
                    IoSlice::new(b"path"),
                    IoSlice::new(b":"),
                    IoSlice::new(&bytes),
                    IoSlice::new(b"\n"),
                ],
            )
            .unwrap();
        }
        output.flush().unwrap();
        assert_eq!(output.streamed, streams, "{size}-byte records");
        assert!(!output.buffer.is_spilled());
        output.buffer.write_to(output.target).unwrap();
        assert!(replay == [b"path:".as_slice(), &bytes, b"\n"].concat().repeat(3));
    }
}

#[test]
fn vectored_records_handle_partial_interrupted_zero_and_cancelled_writes() {
    struct Short {
        bytes: Vec<u8>,
        interrupt: bool,
        limit: usize,
    }
    impl Write for Short {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if std::mem::take(&mut self.interrupt) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = bytes.len().min(self.limit);
            self.bytes.extend_from_slice(&bytes[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut short = Short {
        bytes: Vec::new(),
        interrupt: true,
        limit: 2,
    };
    write_parts(
        &mut short,
        &mut [
            IoSlice::new(b""),
            IoSlice::new(b"abc"),
            IoSlice::new(b"def"),
            IoSlice::new(b"\n"),
        ],
    )
    .unwrap();
    assert_eq!(short.bytes, b"abcdef\n");
    short.limit = 0;
    let error = write_parts(&mut short, &mut [IoSlice::new(b"x")]).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WriteZero);
    let mut writer = Cancellable {
        inner: &mut short,
        stopped: &|| true,
    };
    let error = write_parts(&mut writer, &mut [IoSlice::new(b"x")]).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
    assert_eq!(short.bytes, b"abcdef\n");
}

fn plan(args: &[&str]) -> Plan {
    let line_grep::Command::Search(plan) =
        line_grep::parse(&args.iter().map(OsString::from).collect::<Vec<_>>()).unwrap()
    else {
        panic!("search")
    };
    *plan
}

// M2: a copy may reuse catalogue bytes only after its own full-key stat.
// Current bytes never become a document-wide answer for another alias.
#[test]
fn duplicate_records_require_fresh_stats_and_current_results_stay_local() {
    let streams = Cell::new(0);
    let stats = Cell::new(0);
    let large = [b"red".to_vec(), vec![b'x'; ENTRY_BYTES + 100]].concat();
    let read = |name: NameId, request: ReadRequest<'_>| {
        let version = if name.0 == 1 {
            ReadVersion::Current
        } else {
            ReadVersion::Catalogued
        };
        match request {
            ReadRequest::Stat => {
                stats.set(stats.get() + 1);
            }
            ReadRequest::Stream(each, _) => {
                streams.set(streams.get() + 1);
                let bytes: &[u8] = if name.0 == 1 {
                    b"blue\n"
                } else if name.0 == 3 {
                    &large
                } else {
                    b"red\n"
                };
                each(&mut &bytes[..], bytes.len() as u64).unwrap();
            }
            ReadRequest::Bytes(_, _) => panic!("stream expected"),
        }
        ReadOutcome::Stable(version)
    };
    let mut worker = Worker::new(plan(&["red"]), Some(Box::new(read)));
    let cache = Mutex::new(Cache::default());
    let mut outputs = Vec::new();
    for (name, path, doc) in [
        (0, "copy-a", 0),
        (1, "copy-b", 0),
        (2, "copy-c", 0),
        (3, "large", 1),
    ] {
        let observed = worker
            .prepare(
                &Job::Checked(path.into(), NameId(name), doc),
                true,
                &cache,
                true,
                (&|| false, &mut io::sink(), None),
            )
            .unwrap();
        let Observed::Ready(mut group) = observed else {
            panic!("ready")
        };
        let mut bytes = Vec::new();
        group.output.write_to(&mut bytes).unwrap();
        outputs.push(bytes);
    }
    assert_eq!(
        &outputs[..3],
        &[
            b"copy-a:red\n".to_vec(),
            Vec::new(),
            b"copy-c:red\n".to_vec()
        ]
    );
    assert!(outputs[3].len() > ENTRY_BYTES);
    assert_eq!(streams.get(), 3);
    assert_eq!(stats.get(), 2);
    assert_eq!(lock(&cache).entries.len(), 1);
}

#[test]
fn cached_context_replays_kinds_gaps_and_each_paths_heading() {
    let streams = Cell::new(0);
    let stats = Cell::new(0);
    let read = |_: NameId, request: ReadRequest<'_>| {
        match request {
            ReadRequest::Stat => stats.set(stats.get() + 1),
            ReadRequest::Stream(each, _) => {
                streams.set(streams.get() + 1);
                let bytes = b"red\na\nb\nc\nd\ne\nred\n";
                each(&mut &bytes[..], bytes.len() as u64).unwrap();
            }
            ReadRequest::Bytes(_, _) => panic!("stream expected"),
        }
        ReadOutcome::Stable(ReadVersion::Catalogued)
    };
    let mut worker = Worker::new(plan(&["--heading", "-nC1", "red"]), Some(Box::new(read)));
    let cache = Mutex::new(Cache::default());
    for (name, path) in [(0, "copy-a"), (1, "copy-b")] {
        let Observed::Ready(mut group) = worker
            .prepare(
                &Job::Checked(path.into(), NameId(name), 0),
                true,
                &cache,
                true,
                (&|| false, &mut io::sink(), None),
            )
            .unwrap()
        else {
            panic!("ready")
        };
        let mut actual = Vec::new();
        group.output.write_to(&mut actual).unwrap();
        assert_eq!(
            actual,
            format!("{path}\n1:red\n2-a\n--\n6-e\n7:red\n").as_bytes()
        );
    }
    assert_eq!(streams.get(), 1);
    assert_eq!(stats.get(), 1);
}

#[test]
fn multiline_cache_replays_original_cross_line_highlights() {
    let streams = Cell::new(0);
    let stats = Cell::new(0);
    let read = |_: NameId, request: ReadRequest<'_>| {
        match request {
            ReadRequest::Stat => stats.set(stats.get() + 1),
            ReadRequest::Stream(each, _) => {
                streams.set(streams.get() + 1);
                let bytes = b"red\nblue\n";
                each(&mut &bytes[..], bytes.len() as u64).unwrap();
            }
            ReadRequest::Bytes(_, _) => panic!("stream expected"),
        }
        ReadOutcome::Stable(ReadVersion::Catalogued)
    };
    let mut worker = Worker::new(
        plan(&["-Un", "--color=always", "red\\nblue"]),
        Some(Box::new(read)),
    );
    let cache = Mutex::new(Cache::default());
    for (name, path) in [(0, "copy-a"), (1, "copy-b")] {
        let Observed::Ready(mut group) = worker
            .prepare(
                &Job::Checked(path.into(), NameId(name), 0),
                false,
                &cache,
                true,
                (&|| false, &mut io::sink(), None),
            )
            .unwrap()
        else {
            panic!("ready")
        };
        let mut actual = Vec::new();
        group.output.write_to(&mut actual).unwrap();
        assert_eq!(actual, b"\x1b[0m\x1b[32m1\x1b[0m:\x1b[0m\x1b[1m\x1b[31mred\x1b[0m\n\x1b[0m\x1b[32m2\x1b[0m:\x1b[0m\x1b[1m\x1b[31mblue\x1b[0m\n");
    }
    assert_eq!(streams.get(), 1);
    assert_eq!(stats.get(), 1);
    // Two owned lines carry one cross-line highlight each. The shared
    // cache ledger must account for both allocated Range payloads.
    assert_eq!(
        lock(&cache).bytes,
        std::mem::size_of::<Cached>()
            + 2 * std::mem::size_of::<Record>()
            + 7
            + 2 * std::mem::size_of::<std::ops::Range<usize>>()
    );
}

#[test]
fn replacement_cache_retains_source_boundaries_and_expanded_highlights() {
    let streams = Cell::new(0);
    let stats = Cell::new(0);
    let read = |_: NameId, request: ReadRequest<'_>| {
        match request {
            ReadRequest::Stat => stats.set(stats.get() + 1),
            ReadRequest::Stream(each, _) => {
                streams.set(streams.get() + 1);
                let bytes = b"a\nred\nblue\nz\n";
                each(&mut &bytes[..], bytes.len() as u64).unwrap();
            }
            ReadRequest::Bytes(_, _) => panic!("stream expected"),
        }
        ReadOutcome::Stable(ReadVersion::Catalogued)
    };
    let mut worker = Worker::new(
        plan(&["-UnC1", "--color=always", "-r", "<$2/$1>", "(red)\\n(blue)"]),
        Some(Box::new(read)),
    );
    let cache = Mutex::new(Cache::default());
    for name in [0, 1] {
        let Observed::Ready(mut group) = worker
            .prepare(
                &Job::Checked("copy".into(), NameId(name), 0),
                false,
                &cache,
                true,
                (&|| false, &mut io::sink(), None),
            )
            .unwrap()
        else {
            panic!("ready")
        };
        let mut actual = Vec::new();
        group.output.write_to(&mut actual).unwrap();
        assert_eq!(actual, b"\x1b[0m\x1b[32m1\x1b[0m-a\n\x1b[0m\x1b[32m2\x1b[0m:\x1b[0m\x1b[1m\x1b[31m<blue/red>\x1b[0m\n\x1b[0m\x1b[32m4\x1b[0m-z\n");
    }
    assert_eq!(streams.get(), 1);
    assert_eq!(stats.get(), 1);
}

// M2: a later quiet hit must stop a read already in progress, before the
// ordered emitter can reach that hit. This runs the real group host.
#[test]
fn quiet_hit_cancels_an_earlier_in_progress_reader() {
    use std::sync::{Barrier, mpsc};
    use std::time::Duration;
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = cancelled.clone();
    let (finished, completion) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let plan = plan(&["-q", "red"]);
        let barrier = Barrier::new(2);
        let fork = || -> Option<Box<CheckedReader<'_>>> {
            Some(Box::new(|name: NameId, request: ReadRequest<'_>| {
                let ReadRequest::Stream(each, stopped) = request else {
                    panic!("stream")
                };
                barrier.wait();
                if name.0 == 0 {
                    while !stopped() {
                        std::thread::yield_now();
                    }
                    ReadOutcome::Cancelled
                } else {
                    each(&mut &b"red\n"[..], 4).unwrap();
                    ReadOutcome::Stable(ReadVersion::Catalogued)
                }
            }))
        };
        let mut host = Host {
            runtime: None,
            plan: &plan,
            filename: true,
            writer: Vec::new(),
            buffer: Vec::new(),
            matched: false,
            emitted_group: false,
            sent_output: false,
            json_stats: json::Stats::default(),
            errors: false,
            cancelled: Some(&flag),
        };
        let mut jobs = [
            Job::Checked("slow".into(), NameId(0), 0),
            Job::Checked("hit".into(), NameId(1), 1),
        ]
        .into_iter();
        run(
            &mut host,
            move |_| jobs.next().map(|job| (4, job)),
            &fork,
            None,
            2,
            &flag,
        )
        .unwrap();
        finished
            .send((host.matched, host.errors, host.writer.is_empty()))
            .unwrap();
    });
    let observed = completion.recv_timeout(Duration::from_secs(5));
    // Release the blocked reader for cleanup if the quiet signal regresses.
    cancelled.store(true, Ordering::Release);
    thread.join().unwrap();
    assert_eq!(observed.unwrap(), (true, false, true));
}

// The actual scan must stop between chunks, even though its transaction
// will also suppress publication when cancellation is seen at the end.
#[test]
fn cancellation_reaches_the_real_scanner_before_its_next_read() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let flag = cancelled.clone();
    let observed_reads = reads.clone();
    super::super::seam::AFTER_READ.with_borrow_mut(|hook| {
        *hook = Some(Box::new(move || {
            observed_reads.fetch_add(1, Ordering::Relaxed);
            flag.store(true, Ordering::Release);
        }));
    });
    let plan = plan(&["red"]);
    let stopped = || cancelled.load(Ordering::Acquire);
    let render = Render {
        plan: &plan,
        prefix: None,
        stopped: &stopped,
        capture: None,
    };
    let bytes = [b"red".to_vec(), vec![b'x'; 200_000]].concat();
    let mut input = &bytes[..];
    let observed = observe(
        &render,
        &mut Vec::new(),
        &mut |each| stream(&mut input, bytes.len() as u64, each, &stopped),
        false,
    )
    .unwrap();
    super::super::seam::AFTER_READ.with_borrow_mut(|hook| *hook = None);
    assert!(matches!(observed, Observed::Skipped));
    assert_eq!(reads.load(Ordering::Relaxed), 1);
}

// Cancellation interrupts the initial in-memory prefix during the real
// streaming transition, without Interrupted/write_all retry loops.
#[test]
fn cancellation_interrupts_streaming_prefix_between_chunks() {
    struct Sink<'a> {
        flag: &'a AtomicBool,
        bytes: usize,
    }
    impl Write for Sink<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes += bytes.len();
            self.flag.store(true, Ordering::Release);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let flag = AtomicBool::new(false);
    let mut sink = Sink {
        flag: &flag,
        bytes: 0,
    };
    let stopped = || flag.load(Ordering::Acquire);
    let mut destination = Cancellable {
        inner: &mut sink,
        stopped: &stopped,
    };
    let mut output = OutputCapture::new(&mut destination);
    output.write_all(&vec![b'x'; GROUP_BYTES]).unwrap();
    let error = output.write_all(b"x").unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
    assert_eq!(sink.bytes, CHUNK);
}
// Review R1: a producer filtering posting-negative entries must observe a
// quiet hit in another worker. Count visits, rather than racing a timer.
#[test]
fn quiet_hit_stops_a_producer_filtering_catalog_entries() {
    use std::sync::Barrier;
    use std::sync::atomic::AtomicUsize;
    let base =
        std::env::temp_dir().join(format!("ferret-grep-filter-cancel-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let hits = base.join("hits");
    let negatives = base.join("negatives");
    fs::create_dir_all(&hits).unwrap();
    fs::create_dir_all(&negatives).unwrap();
    fs::write(hits.join("hit"), b"red\n").unwrap();
    for i in 0..1000 {
        let dir = negatives.join(format!("d{}", i / 20));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("negative-{i}")), b"blue\n").unwrap();
    }
    let index = base.join("index");
    ferret_crawl::index(
        &index,
        &[hits.clone(), negatives.clone()],
        ferret_crawl::Refresh::All,
        &ferret_crawl::IndexOptions::default(),
    )
    .unwrap();
    let catalog = ferret_catalog::Catalog::open(&index)
        .unwrap()
        .unwrap()
        .into_resident()
        .unwrap();
    let (name, _) = catalog.names().find(|(_, name)| *name == b"hit").unwrap();
    let doc = catalog.doc(catalog.name(name).child).unwrap();
    // Supply the scheduler's posting result: the hit is its sole candidate.
    // Source still performs the real traversal and membership filtering.
    let candidates = ferret_index::DocSet::new(catalog.doc_count(), [doc.0]);
    let selection = GrepSelection::default();
    let mut source = Source::new(
        vec![Input::Directory(hits), Input::Directory(negatives.clone())],
        Some(&catalog),
        Some(&candidates),
        &selection,
        base.clone(),
    );
    let flag = Arc::new(AtomicBool::new(false));
    let visits = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Barrier::new(2));
    let count = visits.clone();
    let cancelled = flag.clone();
    let producer_gate = gate.clone();
    source.on_entry = Some(Box::new(move |path| {
        if path.starts_with(&negatives)
            && path
                .file_name()
                .unwrap()
                .as_bytes()
                .starts_with(b"negative-")
            && count.fetch_add(1, Ordering::Relaxed) == 0
        {
            producer_gate.wait();
            while !cancelled.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
    }));
    let fork = || -> Option<Box<CheckedReader<'_>>> {
        Some(Box::new(|_, request| {
            let ReadRequest::Stream(each, _) = request else {
                panic!("stream")
            };
            gate.wait();
            each(&mut &b"red\n"[..], 4).unwrap();
            ReadOutcome::Stable(ReadVersion::Catalogued)
        }))
    };
    let plan = plan(&["-q", "red"]);
    let mut host = Host {
        runtime: None,
        plan: &plan,
        filename: true,
        writer: Vec::new(),
        buffer: Vec::new(),
        matched: false,
        emitted_group: false,
        sent_output: false,
        json_stats: json::Stats::default(),
        errors: false,
        cancelled: Some(&flag),
    };
    run(
        &mut host,
        move |stopped| source.next(stopped),
        &fork,
        None,
        2,
        &flag,
    )
    .unwrap();
    assert!(host.matched);
    assert!(!host.errors);
    assert!(host.writer.is_empty());
    assert_eq!(
        visits.load(Ordering::Relaxed),
        1,
        "continued filtering after cancellation"
    );
    fs::remove_dir_all(base).unwrap();
}
// Review R2: a real quiet hit must reach the shared listing predicate.
// If the host still calls next_with, release the hit at the first child
// instead, so the regression fails an assertion instead of hanging.
fn check_quiet_listing(opaque: bool) {
    use std::sync::Barrier;
    use std::sync::atomic::AtomicUsize;
    let base = std::env::temp_dir().join(format!(
        "ferret-grep-listing-{}-{opaque}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&base);
    let root = base.join("tree");
    let hits = root.join("hits");
    let directory = root.join("listed");
    fs::create_dir_all(&hits).unwrap();
    fs::create_dir(&directory).unwrap();
    fs::write(hits.join("hit"), b"red\n").unwrap();
    for i in 0..2048 {
        fs::write(directory.join(format!("child-{i:04}")), b"blue\n").unwrap();
    }
    let index = base.join("index");
    let options = ferret_crawl::IndexOptions {
        global: opaque.then(|| "listed/\n".into()),
        ..ferret_crawl::IndexOptions::default()
    };
    ferret_crawl::index(&index, &[root], ferret_crawl::Refresh::All, &options).unwrap();
    let catalog = ferret_catalog::Catalog::open(&index)
        .unwrap()
        .unwrap()
        .into_resident()
        .unwrap();
    let resolved = catalog.resolve(directory.as_os_str().as_bytes()).unwrap();
    assert_eq!(
        catalog.contents(resolved.target),
        Some(if opaque {
            ferret_catalog::Contents::Ignored
        } else {
            ferret_catalog::Contents::Catalogued
        })
    );
    let selection = GrepSelection::default();
    let mut source = Source::new(
        vec![Input::Directory(hits), Input::Directory(directory.clone())],
        Some(&catalog),
        None,
        &selection,
        base.clone(),
    );
    let flag = Arc::new(AtomicBool::new(false));
    let gate = Arc::new(Barrier::new(2));
    let polls = Arc::new(AtomicUsize::new(0));
    // Source's hooks must own their captures; both routes release once.
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new({
        let (started, gate, flag) = (started.clone(), gate.clone(), flag.clone());
        move || {
            if !started.swap(true, Ordering::AcqRel) {
                gate.wait();
                while !flag.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
            }
        }
    });
    let armed = Arc::new(AtomicBool::new(false));
    let (count, unblock, listing) = (polls.clone(), release.clone(), armed.clone());
    source.on_listing_poll = Some(Box::new(move || {
        if listing.load(Ordering::Acquire) {
            count.fetch_add(1, Ordering::Relaxed);
            unblock();
        }
    }));
    source.on_entry = Some(Box::new(move |path| {
        if path == directory {
            armed.store(true, Ordering::Release);
        }
        if path.file_name().unwrap().as_bytes().starts_with(b"child-") {
            release();
        }
    }));
    let fork = || -> Option<Box<CheckedReader<'_>>> {
        Some(Box::new(|_, request| {
            let ReadRequest::Stream(each, _) = request else {
                panic!("stream")
            };
            gate.wait();
            each(&mut &b"red\n"[..], 4).unwrap();
            ReadOutcome::Stable(ReadVersion::Catalogued)
        }))
    };
    let plan = plan(&["-q", "red"]);
    let mut host = Host {
        runtime: None,
        plan: &plan,
        filename: true,
        writer: Vec::new(),
        buffer: Vec::new(),
        matched: false,
        emitted_group: false,
        sent_output: false,
        json_stats: json::Stats::default(),
        errors: false,
        cancelled: Some(&flag),
    };
    run(
        &mut host,
        move |stopped| source.next(stopped),
        &fork,
        None,
        2,
        &flag,
    )
    .unwrap();
    assert!(host.matched);
    assert!(!host.errors);
    assert!(host.writer.is_empty());
    assert_eq!(
        polls.load(Ordering::Relaxed),
        1,
        "quiet cancellation never reached listing"
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn quiet_hit_cancels_catalog_listing() {
    check_quiet_listing(false);
}

#[test]
fn quiet_hit_cancels_opaque_listing() {
    check_quiet_listing(true);
}

#[test]
fn json_cached_aliases_retain_offsets_submatches_and_their_own_paths() {
    let streams = Cell::new(0);
    let stats = Cell::new(0);
    let read = |_: NameId, request: ReadRequest<'_>| {
        match request {
            ReadRequest::Stat => stats.set(stats.get() + 1),
            ReadRequest::Stream(each, _) => {
                streams.set(streams.get() + 1);
                let bytes = b"blue\nred red\n";
                each(&mut &bytes[..], bytes.len() as u64).unwrap();
            }
            ReadRequest::Bytes(_, _) => panic!("stream expected"),
        }
        ReadOutcome::Stable(ReadVersion::Catalogued)
    };
    let mut worker = Worker::new(plan(&["--json", "-B1", "red"]), Some(Box::new(read)));
    let cache = Mutex::new(Cache::default());
    for (name, path) in [(0, "first"), (1, "second")] {
        let Observed::Ready(mut group) = worker
            .prepare(
                &Job::Checked(path.into(), NameId(name), 0),
                false,
                &cache,
                true,
                (&|| false, &mut io::sink(), None),
            )
            .unwrap()
        else {
            panic!("ready")
        };
        let mut actual = Vec::new();
        group.output.write_to(&mut actual).unwrap();
        let text = String::from_utf8(actual).unwrap();
        assert_eq!(
            text.matches(&format!(r#""path":{{"text":"{path}"}}"#))
                .count(),
            2
        );
        assert!(text.contains(r#""absolute_offset":5"#));
        assert!(text.contains(r#""start":4,"end":7"#));
        assert_eq!(group.result.stats.matched_lines, 1);
        assert_eq!(group.result.stats.matches, 2);
    }
    assert_eq!(streams.get(), 1);
    assert_eq!(stats.get(), 1);
}

// A checked reader can panic after streaming has acquired the publisher.
// Its sender must drop so the writer leaves its receive loop before join.
// The scope re-raises a worker's panic as its own, so the injected one is
// seen by a panic hook, which is process-wide: run in a child process.
#[test]
fn panicking_stream_worker_releases_the_publisher() {
    const MARKER: &str = "FERRET_TEST_STREAM_PANIC";
    const INJECTED: &str = "injected panic after streaming";
    if std::env::var_os(MARKER).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "grep::groups::tests::panicking_stream_worker_releases_the_publisher",
                "--nocapture",
            ])
            .env(MARKER, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    }
    let plan = plan(&["red"]);
    let mut bytes = b"red ".to_vec();
    bytes.resize(GROUP_BYTES + CHUNK, b'x');
    bytes.push(b'\n');
    let reached = AtomicBool::new(false);
    let fork = || -> Option<Box<CheckedReader<'_>>> {
        Some(Box::new(|_, request| {
            let ReadRequest::Stream(each, _) = request else {
                panic!("stream")
            };
            each(&mut &bytes[..], bytes.len() as u64).unwrap();
            reached.store(true, Ordering::SeqCst);
            panic!("{INJECTED}")
        }))
    };
    let mut host = host(&plan, Vec::new());
    let mut jobs = [Job::Checked("panic-file".into(), NameId(0), 0)].into_iter();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new({
        let seen = seen.clone();
        move |info| {
            let payload = info.payload();
            let message = payload
                .downcast_ref::<&str>()
                .map(|message| (*message).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            lock(&seen).push(message);
        }
    }));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(
            &mut host,
            |_| jobs.next().map(|job| (bytes.len() as u64, job)),
            &fork,
            None,
            2,
            &AtomicBool::new(false),
        )
        .unwrap();
    }));
    std::panic::set_hook(previous);
    let seen = std::mem::take(&mut *lock(&seen));
    assert!(result.is_err(), "the run did not panic");
    assert!(
        reached.load(Ordering::SeqCst),
        "the injection was not reached: {seen:?}"
    );
    // The injected panic first, then only the scopes re-raising it.
    assert!(
        seen.first().is_some_and(|first| first == INJECTED)
            && seen[1..]
                .iter()
                .all(|rest| rest == "a scoped thread panicked"),
        "expected only {INJECTED:?}, saw {seen:?}"
    );
    assert!(host.errors);
    assert!(host.writer.starts_with(b"panic-file:red "));
}

// A small cached record set can render past 4 MiB when an alias's byte
// path needs JSON escaping. Re-rendering must use the streaming transition.
#[test]
fn cached_records_stream_when_an_escaped_alias_amplifies_output() {
    use std::os::unix::ffi::OsStringExt;
    let streams = Cell::new(0);
    let stats = Cell::new(0);
    let bytes = b"red\n".repeat(500);
    let read = |_: NameId, request: ReadRequest<'_>| {
        match request {
            ReadRequest::Stat => stats.set(stats.get() + 1),
            ReadRequest::Stream(each, _) => {
                streams.set(streams.get() + 1);
                each(&mut &bytes[..], bytes.len() as u64).unwrap();
            }
            _ => panic!("stream expected"),
        }
        ReadOutcome::Stable(ReadVersion::Catalogued)
    };
    let mut worker = Worker::new(plan(&["--json", "red"]), Some(Box::new(read)));
    let cache = Mutex::new(Cache::default());
    let first = worker
        .prepare(
            &Job::Checked("short".into(), NameId(0), 0),
            true,
            &cache,
            true,
            (&|| false, &mut io::sink(), None),
        )
        .unwrap();
    assert!(matches!(first, Observed::Ready(_)));
    let path = PathBuf::from(OsString::from_vec(vec![1; 4000]));
    let mut output = Vec::new();
    let second = worker
        .prepare(
            &Job::Checked(path, NameId(1), 0),
            true,
            &cache,
            true,
            (&|| false, &mut output, None),
        )
        .unwrap();
    assert!(matches!(second, Observed::Streamed(_, None, _)));
    assert!(output.len() > GROUP_BYTES);
    assert_eq!(streams.get(), 1);
    assert_eq!(stats.get(), 1);
    assert_eq!(
        String::from_utf8(output)
            .unwrap()
            .matches("\\u0001")
            .count(),
        4000 * 500
    );
}
