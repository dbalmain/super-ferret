use std::io::BufWriter;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;

/// A reader gone in one find says nothing about the next one in the same
/// process (0db5cfb replaced a process-wide flag).
#[test]
fn a_closed_pipe_ends_only_its_own_invocation() {
    let first = Output::stdout();
    let Output::Stdout(closed) = &first else {
        unreachable!()
    };
    closed.store(true, Ordering::Relaxed);
    assert!(first.closed());
    assert!(!Output::stdout().closed());
}

/// A local plan never builds the resident names a name index is built from.
/// A plain listing checks every section, as search does (D46); an action
/// plan opens only the sections it reads.
#[test]
fn local_plans_open_the_catalog_without_resident_names() {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let directory = std::env::temp_dir().join(format!(
        "ferret-find-light-open-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let root = directory.join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("one"), b"one").unwrap();
    let index = directory.join("index");
    ferret_crawl::index(
        &index,
        std::slice::from_ref(&root),
        ferret_crawl::Refresh::All,
        &ferret_crawl::IndexOptions::default(),
    )
    .unwrap();
    let plan = |args: &[&str]| {
        let mut argv = vec![root.clone().into_os_string()];
        argv.extend(args.iter().map(OsString::from));
        Plan::parse(&argv).unwrap()
    };

    // Neither plan reads sizes, so neither loads them (D140 A).
    for args in [&["-exec", "true", "{}", "+"][..], &["-name", "one"]] {
        let Ok(FindSource::Sections(catalog)) = open_catalog(&plan(args), &index) else {
            panic!("{args:?} opened the engine");
        };
        assert!(catalog.resident_names().is_none());
        assert!(
            !catalog.is_loaded(ferret_catalog::Section::Size),
            "{args:?}"
        );
    }
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn config_accepts_one_boolean_and_rejects_ambiguous_settings() {
    assert!(!parse_config("# empty\n").unwrap());
    assert!(parse_config("find_no_ignore = true # GNU mode\n").unwrap());
    assert!(!parse_config("find_no_ignore=false\n").unwrap());
    for text in [
        "find_no_ignore = yes",
        "other = true",
        "find_no_ignore = true\nfind_no_ignore = false",
    ] {
        assert!(parse_config(text).is_err(), "{text}");
    }
}

#[test]
fn spilled_record_reaches_the_cli_destination_before_another_worker_flushes() {
    // R2 #4: force the next worker's complete short record between the
    // first worker's evaluation and completion flush, without scheduling
    // or sleeps. This drives the real evaluator and CLI Output host.
    use ferret_query::find::{Entry, EntrySource, FileKind, LiveWalk};
    struct Interleave {
        entry: Entry,
        yielded: bool,
        other: Option<(Plan, LiveWalk, Output)>,
    }
    impl EntrySource for Interleave {
        fn next(&mut self, _: bool) -> Option<Result<&Entry, WalkError>> {
            if !self.yielded {
                self.yielded = true;
                return Some(Ok(&self.entry));
            }
            if let Some((plan, mut walk, mut host)) = self.other.take() {
                assert_eq!(plan.run(&mut walk, &mut host).unwrap().errors, 0);
            }
            None
        }
    }
    let directory = std::env::temp_dir().join(format!("ferret-r2-cli-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let long = directory.join("long");
    let short = directory.join("short");
    std::os::unix::fs::symlink("L".repeat(2000), &long).unwrap();
    std::os::unix::fs::symlink("S", &short).unwrap();
    let format = OsString::from(format!("{}\\n", "%l".repeat(100)));
    let make_plan = |path: &Path| {
        Plan::parse(&[
            "-I".into(),
            path.as_os_str().to_owned(),
            "-printf".into(),
            format.clone(),
        ])
        .unwrap()
    };
    let plan = make_plan(&long);
    let other = make_plan(&short);
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let mut host = Output::test(Sink(bytes.clone()));
    let mut source = Interleave {
        entry: Entry::new(long, 0, FileKind::Symlink),
        yielded: false,
        other: Some((other, make_plan(&short).live_source(), host.clone())),
    };
    let result = plan.run(&mut source, &mut host).unwrap();
    host.flush().unwrap();
    let shared = bytes.lock().unwrap();
    let records: Vec<_> = shared.split(|&byte| byte == b'\n').collect();
    std::fs::remove_dir_all(directory).unwrap();
    assert_eq!(result.errors, 0);
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].len(), 200_000);
    assert!(records[0].iter().all(|&byte| byte == b'L'));
    assert_eq!(records[1].len(), 100);
    assert!(records[1].iter().all(|&byte| byte == b'S'));
    assert!(records[2].is_empty());
}

#[test]
fn spilled_file_record_excludes_a_short_record_between_destination_writes() {
    // R2 #4: interpose after the first actual file-destination write.
    // The old single-action path allowed a second invocation of the same
    // prepared plan to insert a short record there. Whole transactions
    // bypass that byte-write seam; the source then runs the short record
    // after the long evaluation. No thread scheduling or sleeps are used.
    use ferret_query::find::{Entry, EntrySource, FileKind};
    use std::sync::atomic::{AtomicBool, Ordering};
    struct One {
        entry: Entry,
        yielded: bool,
    }
    impl EntrySource for One {
        fn next(&mut self, _: bool) -> Option<Result<&Entry, WalkError>> {
            if self.yielded {
                None
            } else {
                self.yielded = true;
                Some(Ok(&self.entry))
            }
        }
    }
    fn short_record(plan: &Plan, path: &Path, mut host: Output) {
        let mut source = One {
            entry: Entry::new(path.to_owned(), 0, FileKind::Symlink),
            yielded: false,
        };
        assert_eq!(plan.run(&mut source, &mut host).unwrap().errors, 0);
    }
    struct Host {
        cli: Output,
        plan: Arc<Plan>,
        short: PathBuf,
        emitted: Arc<AtomicBool>,
    }
    impl Effects for Host {
        fn print(&mut self, path: &Path, nul: bool) -> io::Result<()> {
            self.cli.print(path, nul)
        }
        fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.cli.write(bytes)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.cli.flush()
        }
        fn error(&mut self, error: &WalkError) {
            panic!("{error:?}");
        }
        fn file(
            &mut self,
            file: &Arc<Mutex<Option<BufWriter<std::fs::File>>>>,
            bytes: &[u8],
        ) -> io::Result<()> {
            self.cli.file(file, bytes)?;
            if !self.emitted.swap(true, Ordering::SeqCst) {
                short_record(&self.plan, &self.short, self.cli.clone());
            }
            Ok(())
        }
    }
    struct Source {
        first: One,
        plan: Arc<Plan>,
        short: PathBuf,
        cli: Output,
        emitted: Arc<AtomicBool>,
    }
    impl EntrySource for Source {
        fn next(&mut self, _: bool) -> Option<Result<&Entry, WalkError>> {
            if !self.first.yielded {
                return self.first.next(true);
            }
            if !self.emitted.swap(true, Ordering::SeqCst) {
                short_record(&self.plan, &self.short, self.cli.clone());
            }
            None
        }
    }
    let directory = std::env::temp_dir().join(format!("ferret-r2-cli-file-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let long = directory.join("long");
    let short = directory.join("short");
    let destination = directory.join("out");
    std::os::unix::fs::symlink("L".repeat(2000), &long).unwrap();
    std::os::unix::fs::symlink("S", &short).unwrap();
    let plan = Arc::new(
        Plan::parse(&[
            "-I".into(),
            long.as_os_str().to_owned(),
            "-fprintf".into(),
            destination.as_os_str().to_owned(),
            format!("{}\\n", "%l".repeat(100)).into(),
        ])
        .unwrap(),
    );
    let cli = Output::test(Vec::new());
    let emitted = Arc::new(AtomicBool::new(false));
    let mut host = Host {
        cli: cli.clone(),
        plan: plan.clone(),
        short: short.clone(),
        emitted: emitted.clone(),
    };
    let mut source = Source {
        first: One {
            entry: Entry::new(long, 0, FileKind::Symlink),
            yielded: false,
        },
        plan: plan.clone(),
        short,
        cli,
        emitted,
    };
    assert_eq!(plan.run(&mut source, &mut host).unwrap().errors, 0);
    let bytes = std::fs::read(&destination).unwrap();
    std::fs::remove_dir_all(directory).unwrap();
    let records: Vec<_> = bytes.split(|&byte| byte == b'\n').collect();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].len(), 200_000);
    assert!(records[0].iter().all(|&byte| byte == b'L'));
    assert_eq!(records[1].len(), 100);
    assert!(records[1].iter().all(|&byte| byte == b'S'));
    assert!(records[2].is_empty());
}

#[test]
fn a_batch_child_does_not_hold_back_another_workers_output() {
    // Two executor workers share the real CLI writer. A final execdir
    // batch waits for the other worker's committed output before exiting;
    // capturing under the writer lock used to make that wait time out.
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    struct Sink(mpsc::Sender<Vec<u8>>);
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.send(bytes.to_vec()).map_err(io::Error::other)?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let directory = std::env::temp_dir().join(format!("ferret-r1-output-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let entry = directory.join("entry");
    std::fs::write(&entry, b"").unwrap();
    let started = directory.join("started");
    let release = directory.join("release");
    let args = vec![
        OsString::from("-I"),
        entry.clone().into_os_string(),
        "-maxdepth".into(),
        "0".into(),
        "-execdir".into(),
        "timeout".into(),
        "8".into(),
        "sh".into(),
        "-c".into(),
        ": > \"$1\"; while ! test -e \"$2\"; do sleep 0.01; done; head -c 70000 /dev/zero".into(),
        "sh".into(),
        started.clone().into_os_string(),
        release.clone().into_os_string(),
        "{}".into(),
        "+".into(),
    ];
    let plan = Plan::parse(&args).unwrap();
    let (sent, received) = mpsc::channel();
    let effects = Output::test(Sink(sent));
    let slow = effects.clone();
    let batch = std::thread::spawn(move || plan.run_parallel(plan.live_source(), slow, 1).unwrap());
    let deadline = Instant::now() + Duration::from_secs(3);
    while !started.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let child_started = started.exists();
    let fast = effects.clone();
    let fast_plan = Plan::parse(&[
        OsString::from("-I"),
        entry.into_os_string(),
        "-maxdepth".into(),
        "0".into(),
        "-printf".into(),
        "fast\n".into(),
    ])
    .unwrap();
    let other = std::thread::spawn(move || {
        fast_plan
            .run_parallel(fast_plan.live_source(), fast, 1)
            .unwrap()
    });
    let first = received.recv_timeout(Duration::from_secs(2));
    // Release and await both children before asserting, including failures.
    std::fs::write(&release, b"").unwrap();
    let batch = batch.join().unwrap();
    let other = other.join().unwrap();
    let mut bytes = Vec::new();
    if let Ok(first) = &first {
        bytes.extend_from_slice(first);
    }
    bytes.extend(received.try_iter().flatten());
    std::fs::remove_dir_all(directory).unwrap();
    assert!(child_started, "batch child did not start within 3 seconds");
    assert_eq!(
        first.unwrap(),
        b"fast\n",
        "another worker's output was held back"
    );
    assert_eq!(batch.errors + other.errors, 0);
    assert_eq!(bytes.len(), 5 + 70000);
    assert!(bytes[5..].iter().all(|byte| *byte == 0));
}
