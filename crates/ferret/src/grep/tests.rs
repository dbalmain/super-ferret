use std::fs::OpenOptions;

use ferret_catalog::NameId;
use ferret_query::{ReadFault, ReadOperation, ReadOutcome, ReadRequest, ReadVersion};

use super::*;

// A shrinking input retries once; neither path may publish an unstable
// attempt as a search result.
#[test]
fn regular_files_discard_torn_matches_and_retry_current_bytes() {
    let mut outcomes = Vec::new();
    for recursive in [false, true] {
        let path =
            std::env::temp_dir().join(format!("ferret-torn-{}-{recursive}", std::process::id()));
        let mut bytes = vec![b'x'; 100_000];
        bytes[..3].copy_from_slice(b"red");
        bytes[99_997..].copy_from_slice(b"OLD");
        fs::write(&path, bytes).unwrap();
        let writer = OpenOptions::new().write(true).open(&path).unwrap();
        let mut attempt = 0;
        seam::AFTER_READ.with_borrow_mut(|hook| {
            *hook = Some(Box::new(move || {
                writer.set_len(50_000 >> attempt).unwrap();
                attempt += 1;
            }))
        });
        let line_grep::Command::Search(plan) = line_grep::parse(&["^red".into()]).unwrap() else {
            panic!("search")
        };
        let mut host = Host {
            plan: &plan,
            runtime: None,
            filename: false,
            writer: Vec::new(),
            buffer: Vec::new(),
            matched: false,
            emitted_group: false,
            sent_output: false,
            json_stats: json::Stats::default(),
            errors: false,
            cancelled: None,
        };
        host.file(&path, recursive).unwrap();
        seam::AFTER_READ.with_borrow_mut(|hook| *hook = None);
        fs::remove_file(path).unwrap();
        outcomes.push((recursive, host.writer.is_empty(), host.matched, host.errors));
    }
    assert_eq!(
        outcomes,
        [(false, true, false, true), (true, true, false, true)]
    );
}

// A changed attempt that crosses the output threshold cannot be retried,
// even when most output consists of before-context.
#[test]
fn changed_streamed_context_keeps_one_attempt_and_reports_the_file() {
    use std::cell::RefCell;
    use std::os::unix::fs::FileExt;
    use std::rc::Rc;
    let mut context = b"old context ".to_vec();
    context.resize(1023, b'x');
    context.push(b'\n');
    let lines = groups::GROUP_BYTES / context.len() + 1;
    for recursive in [false, true] {
        let path = std::env::temp_dir().join(format!(
            "ferret-context-retry-{}-{recursive}",
            std::process::id()
        ));
        let mut original = context.repeat(lines);
        original.extend_from_slice(b"red\n");
        fs::write(&path, &original).unwrap();
        let writer = OpenOptions::new().write(true).open(&path).unwrap();
        let mut changed = false;
        let original_len = original.len();
        seam::AFTER_READ.with_borrow_mut(|hook| {
            *hook = Some(Box::new(move || {
                if !changed {
                    writer.write_all_at(b"NEW", 0).unwrap();
                    // Shrinkage rejects the closing check even though
                    // search readers accept appends (D104).
                    writer.set_len(original_len as u64 - 1).unwrap();
                    changed = true;
                }
            }));
        });
        let spilled = Rc::new(RefCell::new(Vec::new()));
        let seen = spilled.clone();
        seam::COMMIT.with_borrow_mut(|hook| {
            *hook = Some(Box::new(move |output| {
                seen.borrow_mut().push(output.is_spilled());
            }));
        });
        let line_grep::Command::Search(plan) =
            line_grep::parse(&[format!("-B{lines}").into(), "red".into()]).unwrap()
        else {
            panic!("search")
        };
        let mut host = Host {
            plan: &plan,
            runtime: None,
            filename: false,
            writer: Vec::new(),
            buffer: Vec::new(),
            matched: false,
            emitted_group: false,
            sent_output: false,
            json_stats: json::Stats::default(),
            errors: false,
            cancelled: None,
        };
        host.file(&path, recursive).unwrap();
        seam::AFTER_READ.with_borrow_mut(|hook| *hook = None);
        seam::COMMIT.with_borrow_mut(|hook| *hook = None);
        let expected = original;
        fs::remove_file(path).unwrap();
        assert!(host.writer == expected, "replay differs");
        assert!(host.matched && host.errors);
        assert!(
            spilled.borrow().is_empty(),
            "streamed output bypasses capture commit"
        );
    }
}

// R3: size zero changes the readable extent, never the version check.
// Fill after the opening stat, then splice a nonmatching line mid-read.
#[test]
fn initially_empty_files_discard_torn_matches_and_bound_retries() {
    use std::cell::Cell;
    use std::os::unix::fs::FileExt;
    use std::rc::Rc;

    let mut outcomes = Vec::new();
    for recursive in [false, true] {
        for unstable in [false, true] {
            let path = std::env::temp_dir().join(format!(
                "ferret-empty-torn-{}-{recursive}-{unstable}",
                std::process::id()
            ));
            fs::write(&path, b"").unwrap();
            let writer = OpenOptions::new().write(true).open(&path).unwrap();
            let attempts = Rc::new(Cell::new(0));
            let observed = attempts.clone();
            seam::BEFORE_SCAN.with_borrow_mut(|hook| {
                *hook = Some(Box::new(move || {
                    let attempt = observed.get() + 1;
                    observed.set(attempt);
                    if attempt == 1 {
                        let mut bytes = vec![b'x'; 100_000];
                        bytes[..3].copy_from_slice(b"red");
                        bytes[99_997..].copy_from_slice(b"OLD");
                        writer.write_all_at(&bytes, 0).unwrap();
                    }
                    let writer = writer.try_clone().unwrap();
                    let mut changed = false;
                    seam::AFTER_READ.with_borrow_mut(|hook| {
                        *hook = Some(Box::new(move || {
                            if changed {
                                return;
                            }
                            changed = true;
                            if attempt == 1 {
                                writer.write_all_at(b"blu", 0).unwrap();
                                writer.write_all_at(b"NEW", 99_997).unwrap();
                            } else if unstable {
                                writer.set_len(50_000).unwrap();
                            }
                        }));
                    });
                }));
            });
            let line_grep::Command::Search(plan) =
                line_grep::parse(&["^red.*NEW$".into()]).unwrap()
            else {
                panic!("search")
            };
            let mut host = Host {
                plan: &plan,
                runtime: None,
                filename: false,
                writer: Vec::new(),
                buffer: Vec::new(),
                matched: false,
                emitted_group: false,
                sent_output: false,
                json_stats: json::Stats::default(),
                errors: false,
                cancelled: None,
            };
            host.file(&path, recursive).unwrap();
            seam::BEFORE_SCAN.with_borrow_mut(|hook| *hook = None);
            seam::AFTER_READ.with_borrow_mut(|hook| *hook = None);
            fs::remove_file(path).unwrap();
            outcomes.push((
                recursive,
                unstable,
                attempts.get(),
                host.writer.is_empty(),
                host.matched,
                host.errors,
            ));
        }
    }
    assert_eq!(
        outcomes,
        [
            (false, false, 2, true, false, false),
            (false, true, 2, true, false, true),
            (true, false, 2, true, false, false),
            (true, true, 2, true, false, true),
        ]
    );
}

#[test]
fn checked_streams_retry_once_and_commit_only_stable_output() {
    let line_grep::Command::Search(plan) = line_grep::parse(&["red".into()]).unwrap() else {
        panic!("search")
    };
    let mut host = Host {
        plan: &plan,
        runtime: None,
        filename: false,
        writer: Vec::new(),
        buffer: Vec::new(),
        matched: false,
        emitted_group: false,
        sent_output: false,
        json_stats: json::Stats::default(),
        errors: false,
        cancelled: None,
    };
    let mut attempts = 0;
    let mut reader = |_, request: ReadRequest<'_>| {
        attempts += 1;
        let ReadRequest::Stream(each, _) = request else {
            panic!("stream")
        };
        let bytes: &[u8] = if attempts == 1 {
            b"red partial\n"
        } else {
            b"red stable\n"
        };
        each(&mut &bytes[..], bytes.len() as u64).unwrap();
        if attempts == 1 {
            ReadOutcome::RetryChanged
        } else {
            ReadOutcome::Stable(ReadVersion::Current)
        }
    };
    host.checked(Path::new("file"), NameId(0), &mut reader)
        .unwrap();
    assert_eq!(attempts, 2);
    assert_eq!(host.writer, b"red stable\n");
    assert!(host.matched && !host.errors);
}

#[test]
fn failed_or_unstable_streams_never_commit_partial_matches() {
    let line_grep::Command::Search(plan) = line_grep::parse(&["red".into()]).unwrap() else {
        panic!("search")
    };
    for mode in 0..4 {
        let mut host = Host {
            plan: &plan,
            runtime: None,
            filename: false,
            writer: Vec::new(),
            buffer: Vec::new(),
            matched: false,
            emitted_group: false,
            sent_output: false,
            json_stats: json::Stats::default(),
            errors: false,
            cancelled: None,
        };
        let mut attempts = 0;
        let mut reader = |_, request: ReadRequest<'_>| {
            attempts += 1;
            let ReadRequest::Stream(each, _) = request else {
                panic!("stream")
            };
            each(&mut &b"red partial\n"[..], 12).unwrap();
            match mode {
                0 => ReadOutcome::RetryChanged,
                1 => ReadOutcome::Failed(ReadFault::Io(
                    ReadOperation::Read,
                    io::ErrorKind::PermissionDenied.into(),
                )),
                2 => ReadOutcome::Vanished,
                _ => ReadOutcome::Cancelled,
            }
        };
        host.checked(Path::new("file"), NameId(0), &mut reader)
            .unwrap();
        assert_eq!(attempts, if mode == 0 { 2 } else { 1 });
        assert!(host.writer.is_empty() && !host.matched);
        assert_eq!(host.errors, mode < 2);
    }
}

#[test]
fn output_failure_is_returned_to_stop_the_host() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let line_grep::Command::Search(plan) = line_grep::parse(&["red".into()]).unwrap() else {
        panic!("search")
    };
    let mut host = Host {
        plan: &plan,
        runtime: None,
        filename: false,
        writer: Broken,
        buffer: Vec::new(),
        matched: false,
        emitted_group: false,
        sent_output: false,
        json_stats: json::Stats::default(),
        errors: false,
        cancelled: None,
    };
    assert_eq!(
        host.direct(Path::new("file"), &mut &b"red\n"[..], 4)
            .unwrap_err()
            .kind(),
        io::ErrorKind::BrokenPipe
    );
}

// A deferring daemon host batches groups after its first, so the flush
// before a diagnostic is what keeps an earlier group ahead of it. When
// that flush fails, the host stops before reporting the input.
#[test]
fn deferred_groups_flush_before_diagnostics_and_stop_on_flush_failure() {
    struct Recorder<'a> {
        pending: Vec<u8>,
        log: &'a std::sync::Mutex<Vec<String>>,
        flushes: usize,
        fail_flush: Option<usize>,
    }
    impl Write for Recorder<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.pending.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.pending.is_empty() {
                return Ok(());
            }
            self.flushes += 1;
            if self.fail_flush == Some(self.flushes) {
                return Err(io::ErrorKind::StorageFull.into());
            }
            let text = String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned();
            self.log.lock().unwrap().push(format!("out {text}"));
            Ok(())
        }
    }
    let line_grep::Command::Search(plan) = line_grep::parse(&["red".into()]).unwrap() else {
        panic!("search")
    };
    for fail_flush in [None, Some(2)] {
        let log = std::sync::Mutex::new(Vec::new());
        let stderr = |message: &str| {
            log.lock().unwrap().push(format!("err {message}"));
            Ok(())
        };
        let cancelled = AtomicBool::new(false);
        let runtime = Runtime {
            cancelled: &cancelled,
            stderr: &stderr,
            tty: false,
            warned: AtomicBool::new(false),
            descriptors: None,
            defer_stdout: true,
        };
        let mut host = Host {
            plan: &plan,
            runtime: Some(&runtime),
            filename: true,
            writer: Recorder {
                pending: Vec::new(),
                log: &log,
                flushes: 0,
                fail_flush,
            },
            buffer: Vec::new(),
            matched: false,
            emitted_group: false,
            sent_output: false,
            json_stats: json::Stats::default(),
            errors: false,
            cancelled: None,
        };
        for name in ["a", "b"] {
            let line = format!("red {name}\n");
            let mut reader = |_, request: ReadRequest<'_>| {
                let ReadRequest::Stream(each, _) = request else {
                    panic!("stream")
                };
                each(&mut line.as_bytes(), line.len() as u64).unwrap();
                ReadOutcome::Stable(ReadVersion::Current)
            };
            host.checked(Path::new(name), NameId(0), &mut reader)
                .unwrap();
        }
        let mut failing = |_, _: ReadRequest<'_>| {
            ReadOutcome::Failed(ReadFault::Io(
                ReadOperation::Read,
                io::ErrorKind::PermissionDenied.into(),
            ))
        };
        let result = host.checked(Path::new("c"), NameId(0), &mut failing);
        let log = log.into_inner().unwrap();
        match fail_flush {
            None => {
                result.unwrap();
                assert_eq!(log.len(), 3, "{log:?}");
                assert_eq!(log[0], "out a:red a\n");
                assert_eq!(log[1], "out b:red b\n");
                assert!(log[2].starts_with("err grep: c: "), "{log:?}");
            }
            Some(_) => {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::StorageFull);
                assert_eq!(log, ["out a:red a\n"]);
            }
        }
    }
}

// A short group can fit entirely in the destination buffer. Its late
// failure must stop publication before a later input or diagnostic runs.
#[test]
fn final_group_flush_errors_stop_publication() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let line_grep::Command::Search(plan) = line_grep::parse(&["red".into()]).unwrap() else {
        panic!("search")
    };
    let mut host = Host {
        plan: &plan,
        runtime: None,
        filename: false,
        writer: io::BufWriter::new(Broken),
        buffer: Vec::new(),
        matched: false,
        emitted_group: false,
        sent_output: false,
        json_stats: json::Stats::default(),
        errors: false,
        cancelled: None,
    };
    assert_eq!(
        host.direct(Path::new("file"), &mut &b"red\n"[..], 4)
            .unwrap_err()
            .kind(),
        io::ErrorKind::BrokenPipe
    );
}

// A later destination fault must not
// discard an already accepted JSON begin event before its diagnostic.
#[test]
fn publication_faults_flush_the_previously_accepted_prefix() {
    #[derive(Default)]
    struct FailSecond {
        pending: Vec<u8>,
        published: Vec<u8>,
        calls: usize,
    }
    impl Write for FailSecond {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls > 1 {
                return Err(io::Error::other("late output fault"));
            }
            self.pending.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.published.append(&mut self.pending);
            Ok(())
        }
    }
    let line_grep::Command::Search(plan) =
        line_grep::parse(&["--json".into(), "red".into()]).unwrap()
    else {
        panic!("search")
    };
    let mut host = Host {
        plan: &plan,
        runtime: None,
        filename: false,
        writer: FailSecond::default(),
        buffer: Vec::new(),
        matched: false,
        emitted_group: false,
        sent_output: false,
        json_stats: json::Stats::default(),
        errors: false,
        cancelled: None,
    };
    let error = host
        .direct(Path::new("file"), &mut &b"red\n"[..], 4)
        .unwrap_err();
    assert_eq!(error.to_string(), "late output fault");
    assert_eq!(
        host.writer.published,
        b"{\"type\":\"begin\",\"data\":{\"path\":{\"text\":\"file\"}}}\n"
    );
    assert!(host.writer.pending.is_empty());
}

#[test]
fn json_retry_discards_events_offsets_binary_state_and_counts_together() {
    for multiline in [false, true] {
        let mut args = vec!["--json".into(), "-B1".into(), "red".into()];
        if multiline {
            args.insert(0, "-U".into());
        }
        let line_grep::Command::Search(mut plan) = line_grep::parse(&args).unwrap() else {
            panic!("search")
        };
        plan.line_number = true;
        for failure in [false, true] {
            let mut host = Host {
                plan: &plan,
                runtime: None,
                filename: false,
                writer: Vec::new(),
                buffer: Vec::new(),
                matched: false,
                emitted_group: false,
                sent_output: false,
                json_stats: json::Stats::default(),
                errors: false,
                cancelled: None,
            };
            let mut attempts = 0;
            let mut reader = |_, request: ReadRequest<'_>| {
                attempts += 1;
                let ReadRequest::Stream(each, _) = request else {
                    panic!("stream")
                };
                let bytes: &[u8] = if attempts == 1 {
                    b"obsolete context\nred red\0"
                } else {
                    b"blue\nred\n"
                };
                each(&mut &bytes[..], bytes.len() as u64).unwrap();
                if attempts == 1 {
                    ReadOutcome::RetryChanged
                } else if failure {
                    ReadOutcome::Failed(ReadFault::Unstable)
                } else {
                    ReadOutcome::Stable(ReadVersion::Current)
                }
            };
            host.checked(Path::new("actual-path"), NameId(0), &mut reader)
                .unwrap();
            assert_eq!(attempts, 2);
            assert_eq!(host.matched, !failure);
            assert_eq!(host.errors, failure);
            if failure {
                assert!(host.writer.is_empty());
                let mut summary = Vec::new();
                json::summary(&mut summary, &host.json_stats, std::time::Instant::now()).unwrap();
                let parsed = crate::protocol::parse_object(&summary).unwrap();
                let stats = parsed.field("data").unwrap().field("stats").unwrap();
                for key in [
                    "matches",
                    "matched_lines",
                    "searches_with_match",
                    "searches",
                    "bytes_searched",
                    "bytes_printed",
                ] {
                    assert_eq!(
                        stats.field(key).unwrap().number(),
                        Some(0),
                        "{key}: failed attempts reached summary"
                    );
                }
            } else {
                let text = String::from_utf8(host.writer).unwrap();
                assert!(!text.contains("obsolete"));
                assert!(text.contains(r#""absolute_offset":5"#));
                assert!(text.contains(r#""binary_offset":null"#));
                assert!(text.contains(r#""matched_lines":1,"matches":1"#));
                assert_eq!(text.matches(r#""type":"begin""#).count(), 1);
                assert_eq!(text.matches(r#""type":"end""#).count(), 1);
                let mut summary = Vec::new();
                json::summary(&mut summary, &host.json_stats, std::time::Instant::now()).unwrap();
                let parsed = crate::protocol::parse_object(&summary).unwrap();
                let stats = parsed.field("data").unwrap().field("stats").unwrap();
                assert_eq!(stats.field("matches").unwrap().number(), Some(1));
                assert_eq!(
                    stats.field("searches_with_match").unwrap().number(),
                    Some(1)
                );
            }
        }
    }
}

#[test]
fn decimal_matches_display_at_every_width() {
    for number in [0, 1, 9, 10, 99, 100, 12_345, u64::from(u32::MAX), u64::MAX] {
        let mut buffer = [0; 20];
        assert_eq!(
            presentation::decimal(number, &mut buffer),
            number.to_string().as_bytes()
        );
    }
}

// D111 B: drive the shared local/socket execution on cold pins and after a
// new checkpoint. Two selected hard-link names must both print; a link outside
// the operand or roots must not add a row. Equal-content copies share a DocId
// without sharing an inode, so nlink alone is not a complete cache hint.
#[test]
fn indexed_grep_keeps_each_selected_name_without_building_doc_names() {
    use ferret_crawl::{IndexOptions, Refresh};
    use std::time::{Instant, SystemTime};

    let base = std::env::temp_dir().join(format!("ferret-grep-doc-names-{}", std::process::id()));
    let root = base.join("root");
    let both = root.join("both");
    let one = root.join("one");
    let index = base.join("index");
    fs::create_dir_all(&both).unwrap();
    fs::create_dir_all(&one).unwrap();
    fs::write(both.join("a"), b"red\nblue\n").unwrap();
    fs::hard_link(both.join("a"), both.join("b")).unwrap();
    fs::write(one.join("c"), b"red\ngreen\n").unwrap();
    fs::hard_link(one.join("c"), root.join("other-operand")).unwrap();
    fs::write(one.join("d"), b"red\nyellow\n").unwrap();
    fs::hard_link(one.join("d"), base.join("outside-roots")).unwrap();
    for name in ["copy-a", "copy-b"] {
        fs::write(both.join(name), b"red\ncopy\n").unwrap();
    }
    ferret_crawl::index(
        &index,
        std::slice::from_ref(&root),
        Refresh::All,
        &IndexOptions::default(),
    )
    .unwrap();
    let resident = Engine::from_writer(ferret_catalog::WriterSession::open(&index).unwrap());
    for mode in ["one-shot", "resident", "cross-epoch"] {
        let one_shot =
            (mode == "one-shot").then(|| Engine::open_with_content(&index, true).unwrap().unwrap());
        if mode == "cross-epoch" {
            // Build the predecessor as a non-grep content search would, then
            // publish a checkpoint that cannot adopt its DocNames.
            let query = ferret_query::Query::from_args([b"text:red".as_slice()], SystemTime::now())
                .unwrap();
            resident
                .pin()
                .search_content(&query, None, None, |_| std::ops::ControlFlow::Continue(()))
                .unwrap();
            let previous = resident.pin().generation();
            resident.compact().unwrap();
            assert_ne!(previous.checkpoint, resident.pin().generation().checkpoint);
        }
        let pin = one_shot.as_ref().unwrap_or(&resident).pin();
        for (operand, names) in [
            (&both, vec!["a", "b", "copy-a", "copy-b"]),
            (&one, vec!["c", "d"]),
        ] {
            for invert in [false, true] {
                let mut args: Vec<OsString> = vec!["-n".into(), "-F".into()];
                if invert {
                    args.push("-v".into());
                }
                args.extend(["blue".into(), operand.as_os_str().to_owned()]);
                let line_grep::Command::Search(plan) = line_grep::parse(&args).unwrap() else {
                    panic!("search");
                };
                let selection = preflight::selection(plan.hidden, &plan.filters).unwrap();
                let cancelled = AtomicBool::new(false);
                let stderr = |message: &str| -> io::Result<()> { panic!("{message}") };
                let runtime = Runtime {
                    cancelled: &cancelled,
                    stderr: &stderr,
                    tty: false,
                    warned: AtomicBool::new(false),
                    descriptors: None,
                    defer_stdout: false,
                };
                engine::DOC_NAMES_BUILDS.set(0);
                let mut output = Vec::new();
                let exit = Execution {
                    plan: &plan,
                    selection: &selection,
                    pin: Some(&pin),
                    live: None,
                    cwd: &base,
                    threads: 1,
                    runtime: &runtime,
                    start: Instant::now(),
                }
                .run(vec![Input::Directory(operand.clone())], &mut output);
                assert_eq!(
                    engine::DOC_NAMES_BUILDS.get(),
                    0,
                    "{mode}: grep built DocNames"
                );
                let mut actual: Vec<_> = String::from_utf8(output)
                    .unwrap()
                    .lines()
                    .map(str::to_owned)
                    .collect();
                actual.sort();
                let mut expected = Vec::new();
                for name in &names {
                    let path = operand.join(name);
                    let bytes = fs::read_to_string(&path).unwrap();
                    for (line, text) in bytes.lines().enumerate() {
                        if text.contains("blue") != invert {
                            expected.push(format!("{}:{}:{text}", path.display(), line + 1));
                        }
                    }
                }
                expected.sort();
                assert_eq!(actual, expected, "{mode}, invert={invert}");
                if expected.is_empty() {
                    assert!(matches!(exit, Exit::NoMatch), "{mode}, invert={invert}");
                } else {
                    assert!(matches!(exit, Exit::Ok), "{mode}, invert={invert}");
                }
            }
        }
    }
    drop(resident);
    fs::remove_dir_all(base).unwrap();
}
