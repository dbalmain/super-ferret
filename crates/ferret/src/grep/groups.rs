//! File transactions, bounded record caching and worker-owned line rendering.
//! The parent host supplies operands/policy; query's existing scheduler owns
//! parallelism. A bounded completion queue feeds the calling publisher.

use super::*;
use ferret_query::find::LiveWalk;
use ferret_query::find::schedule::{self, Schedule, Step, Visitor};
use ferret_query::{GroupSchedule, Lane};
use ferret_query::{ReadOperation, ReadVersion};
use std::collections::HashMap;
use std::io::{BufRead, BufWriter, IoSlice};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

pub(super) const GROUP_BYTES: usize = 4 << 20;
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
    offset: u64,
    source_lines: Option<(u64, u64)>,
    terminated: bool,
    number: u64,
    bytes: Vec<u8>,
    kind: line_grep::LineKind,
    fragment: bool,
    highlights: Option<Vec<std::ops::Range<usize>>>,
}

fn record_size(bytes: &[u8], highlights: Option<&[std::ops::Range<usize>]>) -> usize {
    bytes.len() + std::mem::size_of::<Record>() + highlights.map_or(0, std::mem::size_of_val)
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
            .saturating_add(record_size(line.bytes, line.highlights));
        if self.bytes > ENTRY_BYTES {
            self.records = None;
        } else if let Some(records) = &mut self.records {
            records.push(Record {
                offset: line.offset,
                source_lines: line.source_lines,
                terminated: line.terminated,
                number: line.number,
                bytes: line.bytes.to_vec(),
                kind: line.kind,
                fragment: line.fragment,
                highlights: line.highlights.map(<[_]>::to_vec),
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

/// Where a file's rendered output stands. A heading belongs to the first
/// printed record. An empty `-o` fragment prints nothing but keeps its line in
/// the window, so `--` marks only lines no window covered.
#[derive(Default)]
struct Cursor {
    line: Option<u64>,
    printed: bool,
    gap: bool,
}

pub(super) enum Observed {
    Ready(Group),
    Skipped,
    Failed(ReadFault),
    Spool(io::Error),
    Streamed(InputResult, Option<String>),
}

/// Rendered captures never spill in production: crossing the threshold hands
/// the captured prefix to the single publisher, then keeps writing there.
struct OutputCapture<'a> {
    buffer: OutputBuffer,
    target: &'a mut dyn Write,
    streamed: bool,
}
impl<'a> OutputCapture<'a> {
    fn new(target: &'a mut dyn Write) -> Self {
        Self {
            buffer: OutputBuffer::default().with_memory_limit(GROUP_BYTES),
            target,
            streamed: false,
        }
    }
}
impl Write for OutputCapture<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !self.streamed && self.buffer.len() + bytes.len() as u64 > GROUP_BYTES as u64 {
            self.streamed = true;
            self.buffer.write_to(self.target)?;
            self.buffer = OutputBuffer::default();
        }
        if self.streamed {
            self.target.write(bytes)
        } else {
            self.buffer.write(bytes)
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.streamed {
            self.target.flush()
        } else {
            self.buffer.flush()
        }
    }
}

impl Render<'_> {
    fn line(
        &self,
        output: &mut dyn Write,
        line: line_grep::Line<'_>,
        cursor: &mut Cursor,
    ) -> io::Result<()> {
        let mut output = Cancellable {
            inner: output,
            stopped: self.stopped,
        };
        if self.plan.json {
            if self.plan.quiet {
                return Ok(());
            }
            return super::json::line(
                &mut output,
                self.prefix.unwrap_or(b"<stdin>"),
                self.plan.line_number,
                line,
            );
        }
        let gap = cursor.line.is_some_and(|number| {
            line.source_lines.map_or(line.number, |(start, _)| start) > number.saturating_add(1)
        });
        cursor.line = Some(line.source_lines.map_or(line.number, |(_, end)| end));
        cursor.gap |= gap;
        if line.fragment && line.bytes.is_empty() {
            return Ok(());
        }
        if !cursor.printed
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
        if (self.plan.before > 0 || self.plan.after > 0) && cursor.printed && cursor.gap {
            output.write_all(b"--\n")?;
        }
        cursor.printed = true;
        cursor.gap = false;
        let delimiter: &[u8] = if line.kind == line_grep::LineKind::Context {
            b"-"
        } else {
            b":"
        };
        if !self.plan.presentation.color.enabled() && !self.plan.line_number {
            let prefix = self
                .prefix
                .filter(|_| self.plan.presentation.heading != Some(true));
            let separator = prefix.map_or(b"".as_slice(), |_| {
                if self.plan.presentation.null {
                    b"\0"
                } else {
                    delimiter
                }
            });
            return write_parts(
                &mut output,
                &mut [
                    IoSlice::new(prefix.unwrap_or_default()),
                    IoSlice::new(separator),
                    IoSlice::new(line.bytes),
                    IoSlice::new(b"\n"),
                ],
            );
        }
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
        output: &mut OutputCapture<'_>,
        capture: &mut Capture,
    ) -> Result<InputResult, (InputResult, ScanError)> {
        #[cfg(test)]
        super::seam::BEFORE_SCAN.with_borrow_mut(|hook| {
            if let Some(hook) = hook {
                hook();
            }
        });
        #[cfg(any(test, debug_assertions))]
        let mut observed = super::seam::Observed::new(input);
        #[cfg(any(test, debug_assertions))]
        let input: &mut dyn Read = &mut observed;
        let mut cursor = Cursor::default();
        let result =
            line_grep::scan_lines_progress(self.plan, input, size, buffer, self.stopped, |line| {
                capture.push(line);
                self.line(output, line, &mut cursor)
            });
        let partial = match &result {
            Ok(result) | Err((result, _)) => *result,
        };
        output
            .flush()
            .map_err(|error| (partial, ScanError::Output(error)))?;
        result
    }
}

/// Test scans still use the production threshold, with a discard destination.
#[cfg(test)]
pub(super) fn observe(
    render: &Render<'_>,
    buffer: &mut Vec<u8>,
    read: &mut dyn FnMut(ferret_query::Consumer<'_>) -> ReadOutcome,
    cache: bool,
) -> io::Result<Observed> {
    observe_streaming(render, buffer, read, cache, &mut io::sink())
}

/// Retry an unpublished changed attempt once. Once streaming has started,
/// retain its prefix and report file faults without retrying. Only actual
/// destination failures escape as errors that stop the whole run.
pub(super) fn observe_streaming(
    render: &Render<'_>,
    buffer: &mut Vec<u8>,
    read: &mut dyn FnMut(ferret_query::Consumer<'_>) -> ReadOutcome,
    cache: bool,
    target: &mut dyn Write,
) -> io::Result<Observed> {
    let mut output = OutputCapture::new(target);
    let mut result = InputResult::default();
    let mut capture = Capture::new(cache);
    let mut output_failure = None;
    let mut changed_after_publication = false;
    let outcome = ferret_query::retry_read(
        || {
            output.buffer = OutputBuffer::default().with_memory_limit(GROUP_BYTES);
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
                Err((partial, error)) => {
                    result = partial;
                    match error {
                        ScanError::Input(error) => Err(error),
                        ScanError::Output(error) => {
                            output_failure = Some(error);
                            Err(io::Error::other("grep rendering failed"))
                        }
                    }
                }
            };
            let outcome = read(&mut each);
            if output_failure.is_some() {
                ReadOutcome::Cancelled
            } else if output.streamed && matches!(outcome, ReadOutcome::RetryChanged) {
                changed_after_publication = true;
                // Do not let retry_read repeat an irreversible observation.
                ReadOutcome::Failed(ReadFault::Unstable)
            } else {
                outcome
            }
        },
        render.stopped,
    );
    let spool_failure = if let Some(error) = output_failure {
        if ferret_query::find::spill_path(&error).is_none() {
            return Err(error);
        }
        Some(error)
    } else {
        None
    };
    if output.streamed {
        let error = if let Some(error) = spool_failure {
            Some(error.to_string())
        } else if changed_after_publication {
            Some("changed during read after output began; output is partial (not retried)".into())
        } else {
            match outcome {
                ReadOutcome::Failed(error) => Some(error.to_string()),
                ReadOutcome::Vanished => {
                    Some("vanished after output began; output is partial".into())
                }
                _ => None,
            }
        };
        return Ok(Observed::Streamed(result, error));
    }
    if let Some(error) = spool_failure {
        return Ok(Observed::Spool(error));
    }
    Ok(match outcome {
        ReadOutcome::Stable(version) => Observed::Ready(Group {
            result,
            output: output.buffer,
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
    plan: &Plan,
    each: ferret_query::Consumer<'_>,
    stopped: &dyn Fn() -> bool,
) -> ReadOutcome {
    let cap = (recursive && !plan.traversal.unrestricted)
        .then(|| ferret_policy::Config::default().size_cap);
    engine::reader::direct(
        path,
        cap,
        &mut |input, size| {
            if recursive && !plan.text {
                // Unrestricted recursion follows rg's initial scan-window
                // binary gate, including early quiet/name modes. Policy walks
                // retain D13's indexing sniffer; explicit files bypass both.
                let probe = if plan.traversal.unrestricted {
                    CHUNK
                } else {
                    ferret_policy::SNIFF_LEN
                };
                let mut buffered = io::BufReader::with_capacity(probe, input);
                let head = buffered.fill_buf()?;
                let binary = if plan.traversal.unrestricted {
                    head.contains(&0)
                } else {
                    ferret_policy::sniff(head) == ferret_policy::Content::Binary
                };
                if binary {
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
    fn write_vectored(&mut self, bytes: &[IoSlice<'_>]) -> io::Result<usize> {
        if (self.stopped)() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "grep cancelled",
            ));
        }
        // Cap both bytes and stack space. Partial vector writes preserve the
        // same between-chunk cancellation bound as ordinary writes.
        let mut parts = [IoSlice::new(&[]); 4];
        let mut left = CHUNK;
        let mut used = 0;
        for part in bytes
            .iter()
            .filter(|part| !part.is_empty())
            .take(parts.len())
        {
            let count = part.len().min(left);
            parts[used] = IoSlice::new(&part[..count]);
            used += 1;
            left -= count;
            if left == 0 {
                break;
            }
        }
        self.inner.write_vectored(&parts[..used])
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn write_parts(mut output: impl Write, mut parts: &mut [IoSlice<'_>]) -> io::Result<()> {
    IoSlice::advance_slices(&mut parts, 0);
    while !parts.is_empty() {
        match output.write_vectored(parts) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ));
            }
            Ok(written) => IoSlice::advance_slices(&mut parts, written),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
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
            .map(|r| record_size(&r.bytes, r.highlights.as_deref()))
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
        destination: (&dyn Fn() -> bool, &mut dyn Write),
    ) -> io::Result<Observed> {
        prepare(
            &self.plan,
            &mut self.buffer,
            self.read.as_deref_mut(),
            job,
            filename,
            cache,
            eligible,
            destination,
        )
    }
}

/// Searches one job's bytes into a group, or replays a cached duplicate's.
#[allow(clippy::too_many_arguments)]
fn prepare(
    plan: &Plan,
    buffer: &mut Vec<u8>,
    read: Option<&mut CheckedReader<'_>>,
    job: &Job,
    filename: bool,
    cache: &Mutex<Cache>,
    eligible: bool,
    destination: (&dyn Fn() -> bool, &mut dyn Write),
) -> io::Result<Observed> {
    let (stopped, target) = destination;
    let render = Render {
        plan,
        prefix: (filename || plan.json).then(|| label(job.path(), plan.paths.is_empty())),
        stopped,
    };
    let observed = match job {
        Job::Checked(_, name, doc) => {
            let Some(read) = read else {
                return Err(io::Error::other("catalogued input has no checked reader"));
            };
            let cached = eligible.then(|| lock(cache).get(*doc)).flatten();
            if let Some(cached) = cached {
                match read(*name, ReadRequest::Stat) {
                    ReadOutcome::Stable(ReadVersion::Catalogued) => {
                        let mut output = OutputCapture::new(target);
                        let mut cursor = Cursor::default();
                        for record in &cached.records {
                            render.line(
                                &mut output,
                                line_grep::Line {
                                    offset: record.offset,
                                    source_lines: record.source_lines,
                                    terminated: record.terminated,
                                    number: record.number,
                                    bytes: &record.bytes,
                                    kind: record.kind,
                                    fragment: record.fragment,
                                    highlights: record.highlights.as_deref(),
                                },
                                &mut cursor,
                            )?;
                        }
                        output.flush()?;
                        if output.streamed {
                            return Ok(Observed::Streamed(cached.result, None));
                        }
                        return Ok(Observed::Ready(Group {
                            result: cached.result,
                            output: output.buffer,
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
            let observed = observe_streaming(
                &render,
                buffer,
                &mut |each| read(*name, ReadRequest::Stream(each, stopped)),
                eligible,
                target,
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
        Job::Direct(path, recursive) => observe_streaming(
            &render,
            buffer,
            &mut |each| read_file(path, *recursive, plan, each, stopped),
            false,
            target,
        )?,
        Job::Absent(_) => Observed::Ready(Group {
            result: InputResult::empty(plan),
            output: OutputBuffer::default().with_memory_limit(GROUP_BYTES),
            version: ReadVersion::Catalogued,
            records: None,
        }),
        Job::Failed(_, _) => Observed::Skipped,
    };
    Ok(observed)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

type Fork<'a> = dyn Fn() -> Option<Box<CheckedReader<'a>>> + Sync + 'a;

enum Message {
    Begin,
    Chunk(Vec<u8>),
    Finish(InputResult, Option<String>),
}

enum Handoff {
    Complete(Job, Observed),
    Stream(PathBuf, Receiver<Message>),
    // Single-thread and spawn fallback publish on the calling thread.
    Direct(PathBuf, Message),
}

// A closed relay is an aftermath, not the stdout fault itself. Receiver drop
// can wake a worker before the publisher records BrokenPipe (D94); that worker
// must not replace the original error with ConnectionAborted and exit 2.
#[derive(Debug)]
struct PublisherStopped;
impl std::fmt::Display for PublisherStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("grep publisher stopped")
    }
}
impl std::error::Error for PublisherStopped {}

fn publisher_stopped() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, PublisherStopped)
}

struct Relay<'a> {
    path: &'a Path,
    handoff: &'a dyn Fn(Handoff) -> bool,
    concurrent: bool,
    stopped: &'a dyn Fn() -> bool,
    started: bool,
    sender: Option<SyncSender<Message>>,
}
impl Relay<'_> {
    fn send(&mut self, message: Message) -> io::Result<()> {
        let accepted = if self.concurrent {
            if !self.started {
                let (sender, receiver) = mpsc::sync_channel(1);
                self.sender = Some(sender);
                self.started = true;
                if !(self.handoff)(Handoff::Stream(self.path.to_owned(), receiver)) {
                    return Err(publisher_stopped());
                }
            }
            self.sender
                .as_ref()
                .is_some_and(|sender| sender.send(message).is_ok())
        } else {
            if !self.started {
                self.started = true;
                if !(self.handoff)(Handoff::Direct(self.path.to_owned(), Message::Begin)) {
                    return Err(publisher_stopped());
                }
            }
            (self.handoff)(Handoff::Direct(self.path.to_owned(), message))
        };
        if accepted {
            Ok(())
        } else {
            Err(publisher_stopped())
        }
    }
}
impl Write for Relay<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if (self.stopped)() {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        let count = bytes.len().min(CHUNK);
        self.send(Message::Chunk(bytes[..count].to_vec()))?;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn publish_handoff<W: Write>(
    host: &mut Host<'_, W>,
    current: &mut Option<u64>,
    handoff: Handoff,
) -> io::Result<()> {
    match handoff {
        Handoff::Complete(job, observed) => {
            if let Job::Failed(_, message) = &job {
                host.error(job.path(), message);
                return Ok(());
            }
            match observed {
                Observed::Ready(_) if host.plan.quiet && host.plan.json => Ok(()),
                Observed::Ready(group) => host.commit(job.path(), group.result, group.output),
                Observed::Skipped => Ok(()),
                Observed::Failed(error) => {
                    host.error(job.path(), &error);
                    Ok(())
                }
                Observed::Spool(error) => {
                    host.error(job.path(), &error);
                    Ok(())
                }
                Observed::Streamed(_, _) => unreachable!("streamed results finish their handoff"),
            }
        }
        Handoff::Stream(path, receiver) => {
            let mut printed = host.start_stream(&path)?;
            for message in receiver {
                match message {
                    Message::Chunk(bytes) => {
                        host.writer.write_all(&bytes)?;
                        printed += bytes.len() as u64;
                    }
                    Message::Finish(result, error) => {
                        return host.finish_stream(&path, result, printed, error);
                    }
                    Message::Begin => unreachable!("channel streams begin at handoff"),
                }
            }
            // A panicking worker drops its sender; never leave the writer
            // waiting on a completion it can no longer supply.
            host.finish_stream(
                &path,
                InputResult::default(),
                printed,
                Some("worker stopped after output began; output is partial".into()),
            )
        }
        Handoff::Direct(path, message) => {
            match message {
                Message::Begin => *current = Some(host.start_stream(&path)?),
                Message::Chunk(bytes) => {
                    host.writer.write_all(&bytes)?;
                    if let Some(printed) = current {
                        *printed += bytes.len() as u64;
                    }
                }
                Message::Finish(result, error) => {
                    let printed = current.take().unwrap_or(0);
                    host.finish_stream(&path, result, printed, error)?;
                }
            }
            Ok(())
        }
    }
}

pub(super) fn run<'a, W: Write>(
    host: &mut Host<'_, W>,
    mut next: impl FnMut(&dyn Fn() -> bool) -> Option<(u64, Job)> + Send,
    fork: &Fork<'a>,
    names: Option<&ferret_query::DocNames>,
    threads: usize,
    cancelled: &AtomicBool,
) -> io::Result<()> {
    let search = Search::new(host, cancelled);
    let mut current = None;
    ferret_query::completion_groups_with_handoff(
        if names.is_some() {
            GroupSchedule::Indexed(threads)
        } else {
            GroupSchedule::Streaming(threads)
        },
        cancelled,
        |stopped| {
            let _publication = search
                .plan
                .traversal
                .live
                .then(|| lock(&search.publication));
            next(stopped)
        },
        || Worker {
            plan: search.plan.clone(),
            buffer: Vec::new(),
            read: fork(),
        },
        |worker, job, stopped, handoff, concurrent| {
            let eligible = matches!(&job, Job::Checked(_,_,doc) if names.is_some_and(|names| names.names(*doc).len() > 1));
            let group = search.work(job, stopped, handoff, concurrent, |job, target| {
                worker.prepare(
                    job,
                    search.filename,
                    &search.cache,
                    eligible,
                    (stopped, target),
                )
            });
            if worker.buffer.capacity() > ferret_query::KEPT_BUFFER {
                worker.buffer = Vec::new();
            }
            group
        },
        weight,
        |group| search.publish(host, &mut current, group),
    );
    search.finish(host)
}

fn weight(handoff: &Handoff) -> u64 {
    match handoff {
        Handoff::Complete(_, Observed::Ready(group)) => group.output.memory_bytes() as u64,
        _ => 0,
    }
}

/// What the workers of one segment share: the duplicate cache, quiet and
/// fatal outcomes, and the publication lock that orders live policy
/// warnings against groups.
struct Search<'s> {
    plan: &'s Plan,
    filename: bool,
    cancelled: &'s AtomicBool,
    cache: Mutex<Cache>,
    hit: AtomicBool,
    quiet_stats: Mutex<json::Stats>,
    fatal: Mutex<Option<io::Error>>,
    publication: Mutex<()>,
}

impl<'s> Search<'s> {
    fn new<W>(host: &Host<'s, W>, cancelled: &'s AtomicBool) -> Self {
        Self {
            plan: host.plan,
            filename: host.filename,
            cancelled,
            cache: Mutex::new(Cache::default()),
            hit: AtomicBool::new(false),
            quiet_stats: Mutex::new(json::Stats::default()),
            fatal: Mutex::new(None),
            publication: Mutex::new(()),
        }
    }

    /// Records the first fatal error and stops the run.
    fn fail(&self, error: io::Error) {
        let mut fatal = lock(&self.fatal);
        if fatal.is_none() {
            *fatal = Some(error);
        }
        self.cancelled.store(true, Ordering::Release);
    }

    /// Renders a file through the same relay for feed and live walk workers.
    fn work(
        &self,
        job: Job,
        stopped: &dyn Fn() -> bool,
        handoff: &dyn Fn(Handoff) -> bool,
        concurrent: bool,
        prepare: impl FnOnce(&Job, &mut dyn Write) -> io::Result<Observed>,
    ) -> Handoff {
        let relay = Relay {
            path: job.path(),
            handoff,
            concurrent,
            stopped,
            started: false,
            sender: None,
        };
        let mut stream = BufWriter::with_capacity(CHUNK, relay);
        let result = prepare(&job, &mut stream);
        // Never retry pending writes implicitly after a destination fault.
        let (mut relay, _) = stream.into_parts();
        let result = match result {
            Ok(Observed::Streamed(result, error)) => relay
                .send(Message::Finish(result, error))
                .map(|()| Observed::Skipped),
            other => other,
        };
        self.observed(job, result, stopped)
    }

    /// A worker's outcome for `job`: `-q` stops at the first match, and an
    /// error not caused by stopping is fatal.
    fn observed(
        &self,
        job: Job,
        result: io::Result<Observed>,
        stopped: &dyn Fn() -> bool,
    ) -> Handoff {
        match result {
            Ok(observed) => {
                if self.plan.quiet
                    && self.plan.json
                    && let Observed::Ready(group) = &observed
                {
                    lock(&self.quiet_stats).add(group.result, 0);
                }
                if self.plan.quiet
                    && matches!(&observed, Observed::Ready(group) if group.result.matched)
                {
                    self.hit.store(true, Ordering::Release);
                    self.cancelled.store(true, Ordering::Release);
                }
                Handoff::Complete(job, observed)
            }
            Err(error) => {
                if !stopped()
                    && !error
                        .get_ref()
                        .is_some_and(|cause| cause.is::<PublisherStopped>())
                {
                    self.fail(error);
                }
                Handoff::Complete(job, Observed::Skipped)
            }
        }
    }

    /// Publishes one group on the output thread.
    fn publish<W: Write>(
        &self,
        host: &mut Host<'_, W>,
        current: &mut Option<u64>,
        handoff: Handoff,
    ) -> ControlFlow<()> {
        let _publication = lock(&self.publication);
        let result = publish_handoff(host, current, handoff);
        if let Err(error) = result {
            self.fail(error);
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }

    fn finish<W>(self, host: &mut Host<'_, W>) -> io::Result<()> {
        host.matched |= self.hit.load(Ordering::Acquire);
        host.json_stats.merge(
            self.quiet_stats
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
        );
        match self
            .fatal
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
        {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// Searches `source`'s operands with the walk itself spread over `threads`
/// workers (D105): find's walk schedule lends subtrees of each live
/// directory operand between workers, each searching the files it reaches
/// and handing groups to this thread in completion order, under the same
/// handoff and input bounds as [`run`]. File operands are searched in order
/// by the first worker. Sets `searched` if any walk selected a file (D99).
/// Fails with [`Unstarted`], having searched nothing, if the walk's own
/// thread cannot start.
pub(super) fn walk<W: Write>(
    host: &mut Host<'_, W>,
    source: Source<'_>,
    threads: usize,
    cancelled: &AtomicBool,
    searched: &mut bool,
) -> io::Result<()> {
    let search = Search::new(host, cancelled);
    let found = AtomicBool::new(false);
    let mut current = None;
    let Source {
        inputs,
        filter,
        live,
        ..
    } = source;
    let started = ferret_query::lane_groups(
        threads,
        cancelled,
        |lanes, stopped| {
            let walker = |i: usize| Walker {
                search: &search,
                filter: &filter,
                lane: &lanes[i],
                stopped,
                found: &found,
                live: live.is_some(),
                plan: search.plan.clone(),
                buffer: Vec::new(),
            };
            let mut first = walker(0);
            // A walker's panic stops the lanes too: a sibling may be waiting
            // for input bytes, or reading until the run stops.
            let stop = || lanes[0].stop();
            for input in inputs {
                if stopped() {
                    break;
                }
                let go = match input {
                    Input::File(path, size) => first.search(size, Job::Direct(path, false)),
                    Input::Failed(path, error) => {
                        first.search(0, Job::Failed(path, error.to_string()))
                    }
                    Input::Directory(path) => {
                        let policy = {
                            // The root's rules may warn, as in the serial
                            // source.
                            let _publication = live.is_some().then(|| lock(&search.publication));
                            live.map(|anchors| anchors.start(&path, stopped))
                                .transpose()
                        };
                        match policy {
                            Ok(policy) => {
                                let quit = AtomicBool::new(false);
                                let schedule = Schedule {
                                    workers: threads,
                                    starts: false,
                                    quit: &quit,
                                    on_panic: Some(&stop),
                                };
                                schedule::run(
                                    &schedule,
                                    sources::live(&path, &filter.cwd),
                                    Walked {
                                        descend: true,
                                        policy: policy.map(live::Policy::deferring),
                                    },
                                    &mut first,
                                    |_| true,
                                    walker,
                                );
                                !quit.load(Ordering::Acquire)
                            }
                            Err(error) => first.search(0, Job::Failed(path, error.to_string())),
                        }
                    }
                    // The parent splits operands at stdin barriers.
                    Input::Stdin => false,
                };
                if !go {
                    break;
                }
            }
            for lane in &lanes {
                lane.flush(stopped());
            }
        },
        |group| search.publish(host, &mut current, group),
    );
    if let Err(error) = started {
        return Err(io::Error::other(Unstarted(error)));
    }
    *searched |= found.load(Ordering::Acquire);
    search.finish(host)
}

/// A search thread the system refused to start, so that nothing of the
/// operands was searched.
#[derive(Debug)]
pub(super) struct Unstarted(io::Error);

impl std::fmt::Display for Unstarted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "starting a search thread: {}", self.0)
    }
}

impl std::error::Error for Unstarted {}

/// The refused spawn an error from [`walk`] carries, if it is one.
pub(super) fn unstarted(error: &io::Error) -> Option<&Unstarted> {
    error.get_ref()?.downcast_ref()
}

/// A walk task's own state: whether to enter the directory it last
/// yielded, and its copy of the live policy stack (D13), cloned when the
/// task lends part of its walk.
struct Walked {
    descend: bool,
    policy: Option<live::Policy>,
}

/// One worker of [`walk`]: filters each entry its task reaches and searches
/// the selected files.
struct Walker<'w, 's> {
    search: &'w Search<'s>,
    filter: &'w Filter<'w>,
    lane: &'w Lane<'w, Handoff>,
    stopped: &'w (dyn Fn() -> bool + Sync),
    found: &'w AtomicBool,
    live: bool,
    plan: Plan,
    buffer: Vec<u8>,
}

impl Walker<'_, '_> {
    /// Searches one job and hands its group over; false once the run stops.
    fn search(&mut self, size: u64, job: Job) -> bool {
        if matches!(job, Job::Failed(..)) {
            return self
                .lane
                .hand(Handoff::Complete(job, Observed::Skipped), 0)
                .is_continue();
        }
        let Some(reservation) = self.lane.reserve(size, self.stopped) else {
            return false;
        };
        let handoff = |group| self.lane.handoff(group, self.stopped);
        let group = self
            .search
            .work(job, self.stopped, &handoff, true, |job, target| {
                prepare(
                    &self.plan,
                    &mut self.buffer,
                    None,
                    job,
                    self.search.filename,
                    &self.search.cache,
                    false,
                    (self.stopped, target),
                )
            });
        if self.buffer.capacity() > ferret_query::KEPT_BUFFER {
            self.buffer = Vec::new();
        }
        drop(reservation);
        if (self.stopped)() {
            return false;
        }
        let bytes = weight(&group);
        self.lane.hand(group, bytes).is_continue()
    }
}

impl Visitor<LiveWalk, Walked> for Walker<'_, '_> {
    fn step(&mut self, walk: &mut LiveWalk, task: &mut Walked) -> Step {
        let stopped = self.stopped;
        if stopped() {
            return Step::Quit;
        }
        let Some(entry) = walk.next_with_cancel(task.descend, &mut || Ok(()), &mut || stopped())
        else {
            return if stopped() { Step::Quit } else { Step::Done };
        };
        task.descend = true;
        let job = match entry {
            Ok(entry) => {
                let mut selected = false;
                let job = self.filter.entry(
                    entry,
                    &mut task.descend,
                    task.policy.as_mut(),
                    &mut selected,
                    self.live.then_some(&self.search.publication),
                    stopped,
                );
                if selected && !self.found.load(Ordering::Relaxed) {
                    self.found.store(true, Ordering::Relaxed);
                }
                job
            }
            Err(error) => Some((0, Job::Failed(error.path, error.error.to_string()))),
        };
        match job {
            Some((size, job)) => {
                if self.search(size, job) {
                    Step::Next
                } else {
                    Step::Quit
                }
            }
            None => Step::Next,
        }
    }

    // The lent levels are below a directory on the lender's stack, which the
    // borrower's first entry unwinds to.
    fn lend(&mut self, parent: &mut Walked) -> Walked {
        Walked {
            descend: true,
            policy: parent.policy.clone(),
        }
    }

    // Errors are published as groups.
    fn end(&mut self, _task: &mut Walked) -> u64 {
        0
    }
}

/// The per-entry selection every walk applies: depth, glob, hidden and
/// ignore filters, live policy, then the catalog's view of the file.
pub(super) struct Filter<'a> {
    catalog: Option<&'a ferret_catalog::Catalog>,
    candidates: Option<&'a ferret_index::DocSet>,
    emit_absent: bool,
    traversal: line_grep::Traversal,
    text: bool,
    selection: &'a GrepSelection,
    cwd: PathBuf,
}

impl Filter<'_> {
    /// The job a walked entry yields, if any. Sets `descend` for a directory
    /// and `searched` for a selected file (D99). `publication`, when given,
    /// is held while printing a deferred policy's warnings.
    fn entry(
        &self,
        entry: &ferret_query::find::Entry,
        descend: &mut bool,
        policy: Option<&mut live::Policy>,
        searched: &mut bool,
        publication: Option<&Mutex<()>>,
        stopped: &dyn Fn() -> bool,
    ) -> Option<(u64, Job)> {
        let kind = match entry.kind() {
            Ok(kind) => kind,
            Err(error) => {
                return Some((0, Job::Failed(entry.path().to_owned(), error.to_string())));
            }
        };
        let relative = entry.path().strip_prefix(&self.cwd).unwrap_or(entry.path());
        if self
            .traversal
            .max_depth
            .is_some_and(|limit| entry.depth() > limit)
        {
            *descend = false;
            return None;
        }
        if kind == FileKind::Directory {
            *descend = self
                .traversal
                .max_depth
                .is_none_or(|limit| entry.depth() < limit)
                && self
                    .selection
                    .descend(relative.as_os_str().as_bytes(), entry.depth());
            if *descend && let Some(policy) = policy {
                let accepted = policy.accepts(entry, true, stopped);
                let warnings = policy.take_warnings();
                if !warnings.is_empty() {
                    // Not inside another file's group.
                    let _publication = publication.map(lock);
                    for warning in warnings {
                        eprintln!("{warning}");
                    }
                }
                match accepted {
                    Ok(allowed) => *descend = allowed,
                    Err(error) => {
                        *descend = false;
                        return Some((0, Job::Failed(entry.path().to_owned(), error.to_string())));
                    }
                }
            }
            return None;
        }
        if kind != FileKind::File
            || !self
                .selection
                .allows(relative.as_os_str().as_bytes(), entry.depth())
        {
            return None;
        }
        if let Some(policy) = policy {
            match policy.accepts(entry, true, stopped) {
                Ok(true) => {}
                Ok(false) => return None,
                Err(error) => {
                    return Some((0, Job::Failed(entry.path().to_owned(), error.to_string())));
                }
            }
        }
        *searched = true;
        if let (Some(catalog), Some(name)) = (self.catalog, entry.catalog_name()) {
            let inode = catalog.name(name).child;
            let Some(doc) = catalog.doc(inode) else {
                if self.text && catalog.state(inode) == ferret_catalog::ContentState::Binary {
                    return Some((
                        catalog.inode(inode).stat.size,
                        Job::Direct(entry.path().to_owned(), true),
                    ));
                }
                return None;
            };
            if self
                .candidates
                .is_some_and(|candidates| !candidates.contains(doc.0))
            {
                if self.emit_absent {
                    return Some((0, Job::Absent(entry.path().to_owned())));
                }
                return None;
            }
            return Some((
                catalog.inode(inode).stat.size,
                Job::Checked(entry.path().to_owned(), name, doc.0),
            ));
        }
        Some((0, Job::Direct(entry.path().to_owned(), true)))
    }
}

#[cfg(test)]
type OnEntry = dyn FnMut(&Path) + Send;

/// Workers advance the source before reserving input bytes for their next file.
/// It retains find's prune feedback and live fallback, without a path backlog.
pub(super) struct Source<'a> {
    inputs: std::vec::IntoIter<Input>,
    source: Option<sources::Directory>,
    descend: bool,
    filter: Filter<'a>,
    live: Option<&'a live::Anchors>,
    policy: Option<live::Policy>,
    searched: bool,
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
            filter: Filter {
                catalog,
                candidates,
                emit_absent: false,
                traversal: Default::default(),
                text: false,
                selection,
                cwd,
            },
            live: None,
            policy: None,
            searched: false,
            #[cfg(test)]
            on_entry: None,
            #[cfg(test)]
            on_listing_poll: None,
        }
    }

    pub(super) fn with_live(mut self, live: Option<&'a live::Anchors>) -> Self {
        self.live = live;
        self
    }

    pub(super) fn with_traversal(mut self, traversal: line_grep::Traversal, text: bool) -> Self {
        self.filter.traversal = traversal;
        self.filter.text = text;
        self
    }

    pub(super) fn with_absence(mut self, emit: bool) -> Self {
        self.filter.emit_absent = emit;
        self
    }

    /// Whether a directory operand walks the live tree, which [`walk`] can
    /// spread over workers.
    pub(super) fn walks_live(&self) -> bool {
        (self.filter.traversal.unrestricted || self.live.is_some())
            && self
                .inputs
                .as_slice()
                .iter()
                .any(|input| matches!(input, Input::Directory(_)))
    }

    /// Whether the walk selected any file: it passed depth, glob, hidden and
    /// ignore filters. Index candidate pruning and binary or oversized
    /// documents still count, as rg searches them (D99).
    pub(super) fn searched(&self) -> bool {
        self.searched
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
                    if let Some(job) = self.filter.entry(
                        entry,
                        &mut self.descend,
                        self.policy.as_mut(),
                        &mut self.searched,
                        None,
                        stopped,
                    ) {
                        return Some(job);
                    }
                }
                self.source = None;
            }
            match self.inputs.next()? {
                Input::File(path, size) => return Some((size, Job::Direct(path, false))),
                Input::Failed(path, error) => {
                    return Some((0, Job::Failed(path, error.to_string())));
                }
                Input::Directory(path) => {
                    self.policy = match self
                        .live
                        .map(|anchors| anchors.start(&path, stopped))
                        .transpose()
                    {
                        Ok(policy) => policy,
                        Err(error) => return Some((0, Job::Failed(path, error.to_string()))),
                    };
                    match sources::Directory::new(
                        &path,
                        self.filter.catalog,
                        &self.filter.cwd,
                        self.filter.traversal.unrestricted || self.live.is_some(),
                    ) {
                        Ok(source) => self.source = Some(source),
                        Err(error) => return Some((0, Job::Failed(path, error.to_string()))),
                    }
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

    fn host<W: Write>(plan: &Plan, writer: W) -> Host<'_, W> {
        Host {
            plan,
            filename: true,
            writer,
            buffer: Vec::new(),
            matched: false,
            emitted_group: false,
            json_stats: json::Stats::default(),
            errors: false,
            cancelled: None,
        }
    }

    // Drive the real handoff with groups just over the capture threshold.
    // All groups must stream, stay whole, and bypass completed-group replay.
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
        let mut jobs =
            (0..FILES).map(|n| Job::Checked(format!("file-{n:02}").into(), NameId(n), n));
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
            spilled.borrow().is_empty(),
            "large groups streamed instead of committing a capture"
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
                    while !published.load(Ordering::Acquire) && std::time::Instant::now() < deadline
                    {
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
        assert_eq!(lines[0], "fast:red");
        lines.sort_unstable();
        assert_eq!(lines, ["fast:red", "later:red", "slow:red"]);
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
        let line_grep::Command::Search(plan) =
            line_grep::parse(&["-B1".into(), "red".into()]).unwrap()
        else {
            panic!("search")
        };
        let render = Render {
            plan: &plan,
            prefix: None,
            stopped: &|| false,
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
                    (&|| false, &mut io::sink()),
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
                    (&|| false, &mut io::sink()),
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
        let mut worker = Worker {
            plan: plan(&["-Un", "--color=always", "red\\nblue"]),
            buffer: Vec::new(),
            read: Some(Box::new(read)),
        };
        let cache = Mutex::new(Cache::default());
        for (name, path) in [(0, "copy-a"), (1, "copy-b")] {
            let Observed::Ready(mut group) = worker
                .prepare(
                    &Job::Checked(path.into(), NameId(name), 0),
                    false,
                    &cache,
                    true,
                    (&|| false, &mut io::sink()),
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
        let mut worker = Worker {
            plan: plan(&["-UnC1", "--color=always", "-r", "<$2/$1>", "(red)\\n(blue)"]),
            buffer: Vec::new(),
            read: Some(Box::new(read)),
        };
        let cache = Mutex::new(Cache::default());
        for name in [0, 1] {
            let Observed::Ready(mut group) = worker
                .prepare(
                    &Job::Checked("copy".into(), NameId(name), 0),
                    false,
                    &cache,
                    true,
                    (&|| false, &mut io::sink()),
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
                plan: &plan,
                filename: true,
                writer: Vec::new(),
                buffer: Vec::new(),
                matched: false,
                emitted_group: false,
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
            plan: &plan,
            filename: true,
            writer: Vec::new(),
            buffer: Vec::new(),
            matched: false,
            emitted_group: false,
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
            plan: &plan,
            filename: true,
            writer: Vec::new(),
            buffer: Vec::new(),
            matched: false,
            emitted_group: false,
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
        let mut worker = Worker {
            plan: plan(&["--json", "-B1", "red"]),
            buffer: Vec::new(),
            read: Some(Box::new(read)),
        };
        let cache = Mutex::new(Cache::default());
        for (name, path) in [(0, "first"), (1, "second")] {
            let Observed::Ready(mut group) = worker
                .prepare(
                    &Job::Checked(path.into(), NameId(name), 0),
                    false,
                    &cache,
                    true,
                    (&|| false, &mut io::sink()),
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
    #[test]
    fn panicking_stream_worker_releases_the_publisher() {
        let plan = plan(&["red"]);
        let mut bytes = b"red ".to_vec();
        bytes.resize(GROUP_BYTES + CHUNK, b'x');
        bytes.push(b'\n');
        let fork = || -> Option<Box<CheckedReader<'_>>> {
            Some(Box::new(|_, request| {
                let ReadRequest::Stream(each, _) = request else {
                    panic!("stream")
                };
                each(&mut &bytes[..], bytes.len() as u64).unwrap();
                panic!("injected panic after streaming")
            }))
        };
        let mut host = host(&plan, Vec::new());
        let mut jobs = [Job::Checked("panic-file".into(), NameId(0), 0)].into_iter();
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
        assert!(result.is_err());
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
        let mut worker = Worker {
            plan: plan(&["--json", "red"]),
            buffer: Vec::new(),
            read: Some(Box::new(read)),
        };
        let cache = Mutex::new(Cache::default());
        let first = worker
            .prepare(
                &Job::Checked("short".into(), NameId(0), 0),
                true,
                &cache,
                true,
                (&|| false, &mut io::sink()),
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
                (&|| false, &mut output),
            )
            .unwrap();
        assert!(matches!(second, Observed::Streamed(_, None)));
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
}
