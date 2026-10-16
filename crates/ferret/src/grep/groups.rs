//! File transactions, bounded record caching and worker-owned line rendering.
//! The parent host supplies operands/policy; query's existing scheduler owns
//! parallelism. Only its emission callback touches stdout.

use super::*;
use ferret_query::{ReadOperation, ReadVersion};
use std::collections::HashMap;
use std::io::BufRead;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

const ENTRY_BYTES: usize = 64 << 10;
const CACHE_BYTES: usize = 16 << 20;
const CHUNK: usize = 64 << 10;

pub(super) struct Render<'a> {
    pub plan: &'a Plan,
    pub prefix: Option<&'a [u8]>,
    pub stopped: &'a dyn Fn() -> bool,
}

#[derive(Clone)]
struct Record {
    number: u64,
    bytes: Vec<u8>,
    kind: line_grep::LineKind,
    fragment: bool,
}

struct Capture {
    records: Option<Vec<Record>>,
    bytes: usize,
}
impl Capture {
    fn new(enabled: bool) -> Self {
        Self {
            records: enabled.then(Vec::new),
            bytes: 0,
        }
    }
    fn push(&mut self, line: line_grep::Line<'_>) {
        if self.records.is_none() {
            return;
        }
        self.bytes = self
            .bytes
            .saturating_add(line.bytes.len() + std::mem::size_of::<Record>());
        if self.bytes > ENTRY_BYTES {
            self.records = None;
        } else if let Some(records) = &mut self.records {
            records.push(Record {
                number: line.number,
                bytes: line.bytes.to_vec(),
                kind: line.kind,
                fragment: line.fragment,
            });
        }
    }
}

pub(super) struct Group {
    pub result: InputResult,
    pub output: OutputBuffer,
    version: ReadVersion,
    records: Option<Vec<Record>>,
}

pub(super) enum Observed {
    Ready(Group),
    Skipped,
    Failed(ReadFault),
}

impl Render<'_> {
    fn line(
        &self,
        output: &mut OutputBuffer,
        line: line_grep::Line<'_>,
        previous: &mut Option<u64>,
    ) -> io::Result<()> {
        let mut output = Cancellable {
            inner: output,
            stopped: self.stopped,
        };
        if previous.is_none()
            && self.plan.presentation.heading == Some(true)
            && let Some(prefix) = self.prefix
        {
            presentation::path(&self.plan.presentation, &mut output, prefix)?;
            output.write_all(if self.plan.presentation.null {
                b"\0"
            } else {
                b"\n"
            })?;
        }
        if (self.plan.before > 0 || self.plan.after > 0)
            && previous.is_some_and(|number| line.number > number.saturating_add(1))
        {
            output.write_all(b"--\n")?;
        }
        *previous = Some(line.number);
        let delimiter: &[u8] = if line.kind == line_grep::LineKind::Context {
            b"-"
        } else {
            b":"
        };
        if self.plan.presentation.heading != Some(true)
            && let Some(prefix) = self.prefix
        {
            presentation::path(&self.plan.presentation, &mut output, prefix)?;
            output.write_all(if self.plan.presentation.null {
                b"\0"
            } else {
                delimiter
            })?;
        }
        if self.plan.line_number {
            presentation::number(&self.plan.presentation, &mut output, line.number)?;
            output.write_all(delimiter)?;
        }
        presentation::text(self.plan, &mut output, line)?;
        output.write_all(b"\n")
    }

    fn scan(
        &self,
        input: &mut dyn Read,
        size: u64,
        buffer: &mut Vec<u8>,
        output: &mut OutputBuffer,
        capture: &mut Capture,
    ) -> Result<InputResult, ScanError> {
        #[cfg(test)]
        super::seam::BEFORE_SCAN.with_borrow_mut(|hook| {
            if let Some(hook) = hook {
                hook();
            }
        });
        #[cfg(test)]
        let mut observed = super::seam::Observed(input);
        #[cfg(test)]
        let input: &mut dyn Read = &mut observed;
        let mut previous = None;
        line_grep::scan_lines(self.plan, input, size, buffer, self.stopped, |line| {
            capture.push(line);
            self.line(output, line, &mut previous)
        })
    }
}

/// All paths share the same retry and output-fault boundary, including callers
/// that do not cache. Failed attempts discard both records and rendered bytes.
pub(super) fn observe(
    render: &Render<'_>,
    buffer: &mut Vec<u8>,
    read: &mut dyn FnMut(ferret_query::Consumer<'_>) -> ReadOutcome,
    cache: bool,
) -> io::Result<Observed> {
    let mut output = OutputBuffer::default();
    let mut result = InputResult::default();
    let mut capture = Capture::new(cache);
    let mut output_failure = None;
    let outcome = ferret_query::retry_read(
        || {
            output = OutputBuffer::default();
            result = InputResult::default();
            capture = Capture::new(cache);
            let mut each = |input: &mut dyn Read, size| match render.scan(
                input,
                size,
                buffer,
                &mut output,
                &mut capture,
            ) {
                Ok(scanned) => {
                    result = scanned;
                    Ok(())
                }
                Err(ScanError::Input(error)) => Err(error),
                Err(ScanError::Output(error)) => {
                    output_failure = Some(error);
                    Err(io::Error::other("grep output failed"))
                }
            };
            let outcome = read(&mut each);
            if output_failure.is_some() {
                ReadOutcome::Cancelled
            } else {
                outcome
            }
        },
        render.stopped,
    );
    if let Some(error) = output_failure {
        return Err(error);
    }
    Ok(match outcome {
        ReadOutcome::Stable(version) => Observed::Ready(Group {
            result,
            output,
            version,
            records: capture.records,
        }),
        ReadOutcome::Failed(error) => Observed::Failed(error),
        ReadOutcome::RetryChanged => Observed::Failed(ReadFault::Unstable),
        ReadOutcome::Cancelled | ReadOutcome::Vanished => Observed::Skipped,
    })
}

pub(super) fn read_file(
    path: &Path,
    recursive: bool,
    each: ferret_query::Consumer<'_>,
    stopped: &dyn Fn() -> bool,
) -> ReadOutcome {
    let cap = recursive.then(|| ferret_policy::Config::default().size_cap);
    engine::reader::direct(
        path,
        cap,
        &mut |input, size| {
            if recursive {
                let mut buffered = io::BufReader::with_capacity(ferret_policy::SNIFF_LEN, input);
                if ferret_policy::sniff(buffered.fill_buf()?) == ferret_policy::Content::Binary {
                    return Ok(());
                }
                each(&mut buffered, size)
            } else {
                each(input, size)
            }
        },
        stopped,
    )
}

pub(super) fn stream(
    input: &mut dyn Read,
    size: u64,
    each: ferret_query::Consumer<'_>,
    stopped: &dyn Fn() -> bool,
) -> ReadOutcome {
    match each(input, size) {
        Ok(()) => ReadOutcome::Stable(ReadVersion::Current),
        Err(_) if stopped() => ReadOutcome::Cancelled,
        Err(error) => ReadOutcome::Failed(ReadFault::Io(ReadOperation::Read, error)),
    }
}

// ConnectionAborted, rather than Interrupted: Write::write_all retries an
// Interrupted error, which would turn cancellation into an infinite loop.
struct Cancellable<'a, W: ?Sized> {
    inner: &'a mut W,
    stopped: &'a dyn Fn() -> bool,
}
impl<W: Write + ?Sized> Write for Cancellable<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if (self.stopped)() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "grep cancelled",
            ));
        }
        self.inner.write(&bytes[..bytes.len().min(CHUNK)])
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub(super) fn emit(
    output: &mut OutputBuffer,
    writer: &mut dyn Write,
    stopped: &dyn Fn() -> bool,
) -> io::Result<()> {
    output.write_to(&mut Cancellable {
        inner: writer,
        stopped,
    })
}

pub(super) enum Job {
    Direct(PathBuf, bool),
    Checked(PathBuf, NameId, u32),
    Failed(PathBuf, String),
    // Catalog posting absence only: explicit/live inputs always read bytes.
    Absent(PathBuf),
}
impl Job {
    fn path(&self) -> &Path {
        match self {
            Self::Direct(p, _) | Self::Checked(p, _, _) | Self::Failed(p, _) | Self::Absent(p) => p,
        }
    }
}

struct Cached {
    result: InputResult,
    records: Vec<Record>,
}
#[derive(Default)]
struct Cache {
    entries: HashMap<u32, Arc<Cached>>,
    bytes: usize,
}
impl Cache {
    fn get(&self, doc: u32) -> Option<Arc<Cached>> {
        self.entries.get(&doc).cloned()
    }
    fn remember(&mut self, doc: u32, group: &mut Group) {
        if group.version != ReadVersion::Catalogued || self.entries.contains_key(&doc) {
            return;
        }
        let Some(records) = group.records.take() else {
            return;
        };
        let bytes = records
            .iter()
            .map(|r| r.bytes.len() + std::mem::size_of::<Record>())
            .sum::<usize>()
            + std::mem::size_of::<Cached>();
        if self.bytes + bytes > CACHE_BYTES {
            self.entries.clear();
            self.bytes = 0;
        }
        self.bytes += bytes;
        self.entries.insert(
            doc,
            Arc::new(Cached {
                result: group.result,
                records,
            }),
        );
    }
}

struct Worker<'a> {
    plan: Plan,
    buffer: Vec<u8>,
    read: Option<Box<CheckedReader<'a>>>,
}
impl Worker<'_> {
    fn prepare(
        &mut self,
        job: &Job,
        filename: bool,
        cache: &Mutex<Cache>,
        eligible: bool,
        stopped: &dyn Fn() -> bool,
    ) -> io::Result<Observed> {
        let render = Render {
            plan: &self.plan,
            prefix: filename.then(|| label(job.path(), self.plan.paths.is_empty())),
            stopped,
        };
        let observed = match job {
            Job::Checked(_, name, doc) => {
                let Some(read) = self.read.as_mut() else {
                    return Err(io::Error::other("catalogued input has no checked reader"));
                };
                let cached = eligible.then(|| lock(cache).get(*doc)).flatten();
                if let Some(cached) = cached {
                    match read(*name, ReadRequest::Stat) {
                        ReadOutcome::Stable(ReadVersion::Catalogued) => {
                            let mut output = OutputBuffer::default();
                            let mut previous = None;
                            for record in &cached.records {
                                render.line(
                                    &mut output,
                                    line_grep::Line {
                                        number: record.number,
                                        bytes: &record.bytes,
                                        kind: record.kind,
                                        fragment: record.fragment,
                                    },
                                    &mut previous,
                                )?;
                            }
                            return Ok(Observed::Ready(Group {
                                result: cached.result,
                                output,
                                version: ReadVersion::Catalogued,
                                records: None,
                            }));
                        }
                        ReadOutcome::Failed(error) => return Ok(Observed::Failed(error)),
                        ReadOutcome::Cancelled | ReadOutcome::Vanished => {
                            return Ok(Observed::Skipped);
                        }
                        ReadOutcome::Stable(ReadVersion::Current) | ReadOutcome::RetryChanged => {}
                    }
                }
                let observed = observe(
                    &render,
                    &mut self.buffer,
                    &mut |each| read(*name, ReadRequest::Stream(each, stopped)),
                    eligible,
                )?;
                if let Observed::Ready(mut group) = observed {
                    if eligible {
                        lock(cache).remember(*doc, &mut group);
                    }
                    Observed::Ready(group)
                } else {
                    observed
                }
            }
            Job::Direct(path, recursive) => observe(
                &render,
                &mut self.buffer,
                &mut |each| read_file(path, *recursive, each, stopped),
                false,
            )?,
            Job::Absent(_) => Observed::Ready(Group {
                result: InputResult::empty(&self.plan),
                output: OutputBuffer::default(),
                version: ReadVersion::Catalogued,
                records: None,
            }),
            Job::Failed(_, _) => Observed::Skipped,
        };
        Ok(observed)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

type Fork<'a> = dyn Fn() -> Option<Box<CheckedReader<'a>>> + Sync + 'a;

pub(super) fn run<'a, W: Write>(
    host: &mut Host<'_, W>,
    next: impl FnMut(&dyn Fn() -> bool) -> Option<(u64, Job)> + Send,
    fork: &Fork<'a>,
    names: Option<&ferret_query::DocNames>,
    threads: usize,
    cancelled: &AtomicBool,
) -> io::Result<()> {
    let plan = host.plan;
    let filename = host.filename;
    let cache = Mutex::new(Cache::default());
    let hit = AtomicBool::new(false);
    let fatal = Mutex::new(None);
    let result = ferret_query::file_groups(
        threads,
        cancelled,
        next,
        || Worker {
            plan: plan.clone(),
            buffer: Vec::new(),
            read: fork(),
        },
        |worker, job, stopped| {
            let eligible = matches!(&job, Job::Checked(_,_,doc) if names.is_some_and(|names| names.names(*doc).len() > 1));
            let result = worker.prepare(&job, filename, &cache, eligible, stopped);
            if worker.buffer.capacity() > ferret_query::KEPT_BUFFER {
                worker.buffer = Vec::new();
            }
            match result {
                Ok(observed) => {
                    if plan.quiet
                        && matches!(&observed, Observed::Ready(group) if group.result.matched)
                    {
                        hit.store(true, Ordering::Release);
                        cancelled.store(true, Ordering::Release);
                    }
                    (job, observed)
                }
                Err(error) => {
                    if !stopped() {
                        let mut fatal = lock(&fatal);
                        if fatal.is_none() {
                            *fatal = Some(error);
                        }
                        cancelled.store(true, Ordering::Release);
                    }
                    (job, Observed::Skipped)
                }
            }
        },
        |(job, observed)| {
            if let Job::Failed(_, message) = &job {
                host.error(job.path(), message);
                return ControlFlow::Continue(());
            }
            let result = match observed {
                Observed::Ready(group) => host.commit(job.path(), group.result, group.output),
                Observed::Skipped => Ok(()),
                Observed::Failed(error) => {
                    host.error(job.path(), &error);
                    Ok(())
                }
            };
            if let Err(error) = result {
                let mut fatal = lock(&fatal);
                if fatal.is_none() {
                    *fatal = Some(error);
                }
                cancelled.store(true, Ordering::Release);
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        },
    );
    host.matched |= hit.load(Ordering::Acquire);
    if let Some(error) = lock(&fatal).take() {
        Err(error)
    } else {
        result
    }
}

#[cfg(test)]
type OnEntry = dyn FnMut(&Path) + Send;

/// The source is called by a worker while claiming its next ring position.
/// It retains find's prune feedback and live fallback, without a path backlog.
pub(super) struct Source<'a> {
    inputs: std::vec::IntoIter<Input>,
    source: Option<CatalogSource>,
    descend: bool,
    catalog: Option<&'a ferret_catalog::Catalog>,
    candidates: Option<&'a ferret_index::DocSet>,
    emit_absent: bool,
    selection: &'a GrepSelection,
    cwd: PathBuf,
    #[cfg(test)]
    on_entry: Option<Box<OnEntry>>,
    #[cfg(test)]
    on_listing_poll: Option<Box<dyn FnMut() + Send>>,
}
impl<'a> Source<'a> {
    pub fn new(
        inputs: Vec<Input>,
        catalog: Option<&'a ferret_catalog::Catalog>,
        candidates: Option<&'a ferret_index::DocSet>,
        selection: &'a GrepSelection,
        cwd: PathBuf,
    ) -> Self {
        Self {
            inputs: inputs.into_iter(),
            source: None,
            descend: true,
            catalog,
            candidates,
            emit_absent: false,
            selection,
            cwd,
            #[cfg(test)]
            on_entry: None,
            #[cfg(test)]
            on_listing_poll: None,
        }
    }

    pub(super) fn with_absence(mut self, emit: bool) -> Self {
        self.emit_absent = emit;
        self
    }

    pub fn next(&mut self, stopped: &dyn Fn() -> bool) -> Option<(u64, Job)> {
        loop {
            if stopped() {
                return None;
            }
            if let Some(source) = &mut self.source {
                loop {
                    if stopped() {
                        return None;
                    }
                    let mut cancelled = || {
                        #[cfg(test)]
                        if let Some(hook) = &mut self.on_listing_poll {
                            hook();
                        }
                        stopped()
                    };
                    let entry =
                        source.next_with_cancel(self.descend, &mut || Ok(()), &mut cancelled);
                    if stopped() {
                        return None;
                    }
                    let Some(entry) = entry else {
                        break;
                    };
                    self.descend = true;
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error) => {
                            return Some((0, Job::Failed(error.path, error.error.to_string())));
                        }
                    };
                    #[cfg(test)]
                    if let Some(hook) = &mut self.on_entry {
                        hook(entry.path());
                    }
                    let kind = match entry.kind() {
                        Ok(kind) => kind,
                        Err(error) => {
                            return Some((
                                0,
                                Job::Failed(entry.path().to_owned(), error.to_string()),
                            ));
                        }
                    };
                    let relative = entry.path().strip_prefix(&self.cwd).unwrap_or(entry.path());
                    if kind == FileKind::Directory {
                        self.descend = self
                            .selection
                            .descend(relative.as_os_str().as_bytes(), entry.depth());
                        continue;
                    }
                    if kind != FileKind::File
                        || !self
                            .selection
                            .allows(relative.as_os_str().as_bytes(), entry.depth())
                    {
                        continue;
                    }
                    if let (Some(catalog), Some(name)) = (self.catalog, entry.catalog_name()) {
                        let inode = catalog.name(name).child;
                        let Some(doc) = catalog.doc(inode) else {
                            continue;
                        };
                        if self
                            .candidates
                            .is_some_and(|candidates| !candidates.contains(doc.0))
                        {
                            if self.emit_absent {
                                return Some((0, Job::Absent(entry.path().to_owned())));
                            }
                            continue;
                        }
                        return Some((
                            catalog.inode(inode).stat.size,
                            Job::Checked(entry.path().to_owned(), name, doc.0),
                        ));
                    }
                    return Some((0, Job::Direct(entry.path().to_owned(), true)));
                }
                self.source = None;
            }
            match self.inputs.next()? {
                Input::File(path, size) => return Some((size, Job::Direct(path, false))),
                Input::Failed(path, error) => {
                    return Some((0, Job::Failed(path, error.to_string())));
                }
                Input::Directory(path) => {
                    let Some(catalog) = self.catalog else {
                        return Some((
                            0,
                            Job::Failed(path, "directory input has no pinned catalog".into()),
                        ));
                    };
                    self.source = Some(CatalogSource::paths(
                        catalog.clone(),
                        vec![path],
                        self.cwd.clone(),
                    ));
                    self.descend = true;
                }
                // The parent splits operands at stdin barriers before scheduling.
                Input::Stdin => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn plan(args: &[&str]) -> Plan {
        let line_grep::Command::Search(plan) =
            line_grep::parse(&args.iter().map(OsString::from).collect::<Vec<_>>()).unwrap()
        else {
            panic!("search")
        };
        plan
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
        let mut worker = Worker {
            plan: plan(&["red"]),
            buffer: Vec::new(),
            read: Some(Box::new(read)),
        };
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
                    &|| false,
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
        let mut worker = Worker {
            plan: plan(&["--heading", "-nC1", "red"]),
            buffer: Vec::new(),
            read: Some(Box::new(read)),
        };
        let cache = Mutex::new(Cache::default());
        for (name, path) in [(0, "copy-a"), (1, "copy-b")] {
            let Observed::Ready(mut group) = worker
                .prepare(
                    &Job::Checked(path.into(), NameId(name), 0),
                    true,
                    &cache,
                    true,
                    &|| false,
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
                plan: &plan,
                filename: true,
                writer: Vec::new(),
                buffer: Vec::new(),
                matched: false,
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

    // Cancellation must interrupt spill replay as well as matching. Returning
    // Interrupted from Write would retry forever instead of stopping here.
    #[test]
    fn cancellation_interrupts_real_spill_emission_between_chunks() {
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
        let mut output = OutputBuffer::default();
        output.write_all(&vec![b'x'; 3 * CHUNK]).unwrap();
        let mut sink = Sink {
            flag: &flag,
            bytes: 0,
        };
        let error = emit(&mut output, &mut sink, &|| flag.load(Ordering::Acquire)).unwrap_err();
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
            plan: &plan,
            filename: true,
            writer: Vec::new(),
            buffer: Vec::new(),
            matched: false,
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
            plan: &plan,
            filename: true,
            writer: Vec::new(),
            buffer: Vec::new(),
            matched: false,
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
}
