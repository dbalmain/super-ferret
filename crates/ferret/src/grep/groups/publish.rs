//! What one search's workers share, the relay that hands their groups to the
//! publishing thread, and [`run`], the parallel feed.

use super::job::{Cache, Job, Observed, Worker};
use super::*;
use ferret_query::GroupSchedule;
use std::io::BufWriter;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};

type Fork<'a> = dyn Fn() -> Option<Box<CheckedReader<'a>>> + Sync + 'a;

pub(super) enum Message {
    Begin,
    Chunk(Vec<u8>),
    Finish(InputResult, Option<String>, Option<capture::Held>),
}

pub(super) enum Handoff {
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

pub(super) fn publisher_stopped() -> io::Error {
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
                host.error(job.path(), message)?;
                return Ok(());
            }
            match observed {
                Observed::Ready(_) if host.plan.quiet && host.plan.json => Ok(()),
                Observed::Ready(group) => host.commit(job.path(), group.result, group.output),
                Observed::Skipped => Ok(()),
                Observed::Failed(error) => {
                    host.error(job.path(), &error)?;
                    Ok(())
                }
                Observed::Spool(error) => {
                    host.error(job.path(), &error)?;
                    Ok(())
                }
                Observed::Streamed(..) => unreachable!("streamed results finish their handoff"),
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
                    Message::Finish(result, error, _capture) => {
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
                Message::Finish(result, error, _capture) => {
                    let printed = current.take().unwrap_or(0);
                    host.finish_stream(&path, result, printed, error)?;
                }
            }
            Ok(())
        }
    }
}

pub(in super::super) fn run<'a, W: Write>(
    host: &mut Host<'_, W>,
    mut next: impl FnMut(&dyn Fn() -> bool) -> Option<(u64, Job)> + Send,
    fork: &Fork<'a>,
    names: Option<&ferret_query::DocNames>,
    threads: usize,
    cancelled: &AtomicBool,
) -> io::Result<()> {
    let search = Search::new(host, cancelled, threads);
    let mut current = None;
    let shortfall = ferret_query::completion_groups_with_handoff(
        if names.is_some() {
            GroupSchedule::Indexed(threads)
        } else {
            GroupSchedule::Streaming(threads)
        },
        cancelled,
        |stopped| {
            let _descriptors = search.descriptors.acquire(stopped)?;
            let _publication = search
                .plan
                .traversal
                .live
                .then(|| lock(&search.publication));
            next(stopped)
        },
        || Worker::new(search.plan.clone(), fork()),
        |worker, job, stopped, handoff, concurrent| {
            let eligible = matches!(&job, Job::Checked(_,_,doc) if names.is_some_and(|names| names.names(*doc).len() > 1));
            search.work(job, stopped, handoff, concurrent, |job, target| {
                worker.prepare(
                    job,
                    search.filename,
                    &search.cache,
                    eligible,
                    (stopped, target, Some(&search.capture)),
                )
            })
        },
        weight,
        |group| search.publish(host, &mut current, group),
    );
    if let Some(shortfall) = shortfall {
        host.writer.flush()?;
        host.report_shortfall(&shortfall)?;
    }
    search.finish(host)
}

pub(super) fn weight(handoff: &Handoff) -> u64 {
    match handoff {
        Handoff::Complete(_, Observed::Ready(group)) => group.output.memory_bytes() as u64,
        _ => 0,
    }
}

/// What the workers of one segment share: the duplicate cache, quiet and
/// fatal outcomes, and the publication lock that orders live policy
/// warnings against groups.
pub(super) struct Search<'s> {
    pub(super) plan: &'s Plan,
    pub(super) filename: bool,
    cancelled: &'s AtomicBool,
    pub(super) cache: Mutex<Cache>,
    hit: AtomicBool,
    quiet_stats: Mutex<json::Stats>,
    fatal: Mutex<Option<io::Error>>,
    pub(super) publication: Mutex<()>,
    pub(super) descriptors: Arc<line_grep::IoBudget>,
    pub(super) tty: bool,
    pub(super) runtime: Option<&'s Runtime<'s>>,
    pub(super) capture: Arc<capture::Budget>,
}

impl<'s> Search<'s> {
    pub(super) fn new<W: Write>(
        host: &Host<'s, W>,
        cancelled: &'s AtomicBool,
        threads: usize,
    ) -> Self {
        Self {
            plan: host.plan,
            filename: host.filename,
            cancelled,
            cache: Mutex::new(Cache::default()),
            hit: AtomicBool::new(false),
            quiet_stats: Mutex::new(json::Stats::default()),
            fatal: Mutex::new(None),
            publication: Mutex::new(()),
            descriptors: host
                .runtime
                .and_then(|runtime| runtime.descriptors.as_ref())
                .map_or_else(|| Arc::new(line_grep::IoBudget::new(threads)), Arc::clone),
            tty: host.tty(),
            runtime: host.runtime,
            capture: capture::Budget::new(threads),
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
    pub(super) fn work(
        &self,
        job: Job,
        stopped: &dyn Fn() -> bool,
        handoff: &dyn Fn(Handoff) -> bool,
        concurrent: bool,
        prepare: impl FnOnce(&Job, &mut dyn Write) -> io::Result<Observed>,
    ) -> Handoff {
        let Some(_descriptors) = self.descriptors.acquire(stopped) else {
            return Handoff::Complete(job.finished(), Observed::Skipped);
        };
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
            Ok(Observed::Streamed(result, error, capture)) => relay
                .send(Message::Finish(result, error, capture))
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
                Handoff::Complete(job.finished(), observed)
            }
            Err(error) => {
                if !stopped()
                    && !error
                        .get_ref()
                        .is_some_and(|cause| cause.is::<PublisherStopped>())
                {
                    self.fail(error);
                }
                Handoff::Complete(job.finished(), Observed::Skipped)
            }
        }
    }

    /// Publishes one group on the output thread.
    pub(super) fn publish<W: Write>(
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

    pub(super) fn finish<W>(self, host: &mut Host<'_, W>) -> io::Result<()> {
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
