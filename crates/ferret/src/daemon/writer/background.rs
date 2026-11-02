//! The writer loop's background schedule: timers, refreshes and their log.

use std::collections::BTreeSet;

use super::*;

/// The writer loop's background schedule: the backstop, poll, retry and
/// content timers, and the refreshes they start.
pub(super) struct Background<'h> {
    host: &'h Arc<Host>,
    engine: &'h Arc<Engine>,
    scheduler: &'h Arc<crate::scheduler::Scheduler>,
    watch: &'h Option<Arc<Watch>>,
    context: crate::cli::Context,
    pub(super) options: IndexOptions,
    hourly: Duration,
    polling: Duration,
    full_due: Instant,
    poll_due: Instant,
    /// Whether a poll with roots to walk was issued that no refresh has
    /// taken yet.
    poll_issued: bool,
    refresh_log: RefreshLog,
    retry_due: Option<Duration>,
    pub(super) initial: bool,
    pub(super) content_due: Instant,
    content_state: content::State,
}

impl<'h> Background<'h> {
    pub(super) fn new(
        host: &'h Arc<Host>,
        engine: &'h Arc<Engine>,
        scheduler: &'h Arc<crate::scheduler::Scheduler>,
        watch: &'h Option<Arc<Watch>>,
        context: crate::cli::Context,
    ) -> Self {
        let hourly = duration("FERRET_BACKSTOP_MS", 60 * 60 * 1000).max(Duration::from_millis(1));
        let polling = duration("FERRET_POLL_MS", 5 * 60 * 1000).max(Duration::from_millis(1));
        let initial = watch.is_none();
        lock(&host.writer_status).fallback_backstop = initial;
        Self {
            host,
            engine,
            scheduler,
            watch,
            context,
            options: IndexOptions {
                watch: watch.clone(),
                bulk: Some(scheduler.clone()),
                ..IndexOptions::default()
            },
            hourly,
            polling,
            full_due: Instant::now() + hourly,
            poll_due: Instant::now() + polling,
            poll_issued: false,
            refresh_log: RefreshLog::default(),
            retry_due: None,
            initial,
            content_due: Instant::now(),
            content_state: content::State::default(),
        }
    }

    /// When the loop must next wake without a message.
    pub(super) fn deadline(&self, now: Instant) -> Instant {
        let mut deadline = self.full_due.min(self.poll_due);
        // A catalog retry owns this boundary. An expired content deadline
        // must not wake repeatedly while that retry is cooling down.
        if self.retry_due.is_none() {
            deadline = deadline.min(self.content_due);
        }
        if let Some(retry) = self.retry_due {
            deadline = deadline.min(now + retry.saturating_sub(self.scheduler.now()));
        }
        if self.retry_due.is_none()
            && let Some(due) = self.watch.as_ref().and_then(|w| w.next_due())
        {
            deadline = deadline.min(due);
        }
        deadline
    }

    pub(super) fn backstop_if_due(&mut self) {
        if Instant::now() >= self.full_due {
            if let Some(w) = self.watch {
                w.backstop(RefreshReason::Backstop);
            } else {
                self.initial = true;
                lock(&self.host.writer_status).fallback_backstop = true;
            }
            self.full_due = Instant::now() + self.hourly;
        }
    }

    pub(super) fn poll_if_due(&mut self) {
        if Instant::now() >= self.poll_due {
            if let Some(w) = self.watch {
                let pin = self.engine.pin();
                let scopes = w
                    .polling_roots(pin.catalog())
                    .into_iter()
                    .chain(retained_roots(pin.catalog()))
                    .collect::<BTreeSet<_>>();
                // A poll with nothing to walk issues no refresh. Marking it
                // issued would make the next inotify burst count as a poll.
                self.poll_issued |= !scopes.is_empty();
                w.scoped_roots(scopes.into_iter().collect());
            } else {
                self.initial = true;
                self.poll_issued = true;
                lock(&self.host.writer_status).fallback_backstop = true;
            }
            self.poll_due = Instant::now() + self.polling;
        }
    }

    /// One background turn: wait out a retry or a pause, take a content
    /// turn, or refresh.
    pub(super) fn turn(&mut self) -> io::Result<()> {
        let scheduler = self.scheduler;
        if self.retry_due.is_some_and(|retry| scheduler.now() < retry) {
            return Ok(());
        }
        // An expired retry is spent here. Left set, it would keep hiding the
        // watch's next due time from the deadline, and a later `continue`
        // would spin on the past deadline.
        self.retry_due = None;
        self.options.workers = scheduler.status().workers;
        let paused = scheduler.status().paused;
        let burst = self
            .watch
            .as_ref()
            .and_then(|w| w.take_admitted(paused.is_none()));
        if paused.is_some() && burst.is_none() {
            self.content_due = Instant::now() + Duration::from_secs(1);
            if self.initial
                || self
                    .watch
                    .as_ref()
                    .is_some_and(|w| w.status().backstop.is_some())
            {
                self.retry_due = Some(scheduler.now() + Duration::from_secs(1));
            }
            return Ok(());
        }
        if burst.is_none() && !self.initial {
            if Instant::now() >= self.content_due {
                self.content_due = Instant::now()
                    + content::turn(self.host, self.engine, scheduler, &mut self.content_state)?;
            }
            return Ok(());
        }
        if paused.is_none() {
            lock(&self.host.writer_status).blocked = None;
        }
        self.refresh(burst);
        Ok(())
    }

    /// Refreshes for a burst, or the whole catalog, and schedules what
    /// follows from the outcome.
    fn refresh(&mut self, burst: Option<ferret_crawl::watch::Burst>) {
        let started = Instant::now();
        // The burst taken above holds any poll issued before it.
        let polled = std::mem::take(&mut self.poll_issued);
        let _operation = Operation::start(self.host, "refresh");
        #[cfg(debug_assertions)]
        std::thread::sleep(duration("FERRET_WATCH_TEST_REFRESH_DELAY_MS", 0));
        global_inputs(self.watch.as_ref(), self.engine, &self.context);
        let (result, reason) = self.run_refresh(burst.as_ref());
        let elapsed = started.elapsed();
        self.space_polls(elapsed, polled, reason);
        if elapsed >= Duration::from_secs(1) {
            release_freed_memory();
        }
        self.refresh_log
            .record(&self.context, reason, polled, elapsed, &result);
        let deferred = result
            .as_ref()
            .ok()
            .and_then(|report| match report.outcome {
                ferret_crawl::RefreshOutcome::DeferredBulk(reason) => Some(reason),
                _ => None,
            });
        let retry_current = result.as_ref().is_ok_and(|report| {
            matches!(
                report.outcome,
                ferret_crawl::RefreshOutcome::RetryFromCurrent(_)
            )
        });
        self.settle(&result, burst.as_ref(), reason, retry_current, deferred);
        if let Some(burst) = burst
            && let Some(watch) = self.watch
        {
            if deferred.is_some() {
                watch.defer(burst);
                // Its retry is still that poll, for the refresh log.
                self.poll_issued |= polled;
            } else {
                watch.finish(burst, result.is_ok() && !retry_current);
            }
        }
    }

    /// Refreshes for `burst`, or the whole catalog, under the current global
    /// ignore rules. Returns the result and the reason the refresh ran for.
    fn run_refresh(
        &mut self,
        burst: Option<&ferret_crawl::watch::Burst>,
    ) -> (io::Result<ferret_crawl::RefreshReport>, RefreshReason) {
        let mut reason = RefreshReason::Backstop;
        let result = crate::index::global_ignore(&self.context)
            .map_err(io::Error::other)
            .and_then(|global| {
                self.options.global = Some(global);
                let pin = self.engine.pin();
                let request = burst.map_or_else(
                    || ferret_crawl::RefreshRequest {
                        expected_generation: pin.generation(),
                        scopes: vec![],
                        rename_hints: vec![],
                        reason: RefreshReason::Backstop,
                    },
                    |b| b.request(pin.catalog()),
                );
                reason = request.reason;
                self.engine
                    .refresh(request, &self.options)
                    .map_err(io::Error::other)
            });
        (result, reason)
    }

    /// Past the watch cap a poll rewalks a whole root, which can outlast the
    /// poll interval and would then run back to back (D116). A long refresh
    /// spaces the next poll by four times its duration, so polling holds the
    /// writer at most a fifth of the time. Only a refresh that walked the
    /// polled roots, a poll's or a full one, spaces them: a slow scoped burst
    /// elsewhere must not starve them.
    fn space_polls(&mut self, elapsed: Duration, polled: bool, reason: RefreshReason) {
        let spacing = elapsed.saturating_mul(4);
        if (polled || reason != RefreshReason::Burst) && spacing > self.polling {
            self.poll_due = self.poll_due.max(Instant::now() + spacing);
        }
    }

    /// Records a refresh's outcome in the writer status and schedules what
    /// follows: a retry, or, after a success, the watches and content.
    fn settle(
        &mut self,
        result: &io::Result<ferret_crawl::RefreshReport>,
        burst: Option<&ferret_crawl::watch::Burst>,
        reason: RefreshReason,
        retry_current: bool,
        deferred: Option<ferret_catalog::bulk::Blocked>,
    ) {
        match result {
            Ok(_) if retry_current => {
                // A budget checkpoint may keep sequence unchanged. It did not
                // observe the queued work; resolve the complete marker again.
                self.initial = true;
                lock(&self.host.writer_status).fallback_backstop = self.watch.is_none();
                self.retry_due = Some(self.scheduler.now());
            }
            Ok(_) if deferred.is_some() => {
                lock(&self.host.writer_status).blocked = deferred;
                if self.watch.is_none() {
                    self.initial = true;
                    lock(&self.host.writer_status).fallback_backstop = true;
                }
                self.retry_due = Some(self.scheduler.now() + Duration::from_secs(1));
            }
            Ok(report) => {
                global_inputs(self.watch.as_ref(), self.engine, &self.context);
                let complete = burst.is_none_or(|b| b.reason() != RefreshReason::Burst)
                    && report.report.coverage_faults.is_empty();
                successful(self.host, &report.report, complete);
                lock(&self.host.writer_status).last_reason = Some(reason);
                if let Some(w) = self.watch {
                    w.adopt_aliases(&report.view);
                    if burst.is_none_or(|b| b.reconcile_watches()) {
                        w.reconcile(&report.view);
                    }
                }
                self.initial = false;
                self.retry_due = None;
                self.content_due = Instant::now();
            }
            Err(error) => {
                lock(&self.host.writer_status).error = Some(error.to_string());
                self.retry_due = Some(self.scheduler.now() + Duration::from_secs(1));
            }
        }
    }
}

/// Roots whose effective catalog still holds a retained directory (D26's
/// S1+ checked retention), which polling retries until a refresh repairs them.
/// The catalog is the authority: a report names only the faults in the scopes
/// it observed, so it can neither add nor clear another root's protection, and
/// the markers survive a daemon restart. One pass over the directories, taken
/// once per poll interval rather than per burst.
pub(super) fn retained_roots(catalog: &ferret_catalog::Catalog) -> BTreeSet<PathBuf> {
    let names = catalog.name_reader();
    let mut tops = BTreeSet::new();
    for dir in catalog.dir_ids() {
        if catalog.retained_at(dir).is_none() {
            continue;
        }
        let mut at = dir;
        while let Some(name) = catalog.dir_name(at) {
            at = names.edge(name).0;
        }
        tops.insert(at);
    }
    catalog
        .roots()
        .filter(|(id, _)| tops.contains(id))
        .map(|(_, path)| PathBuf::from(OsStr::from_bytes(path)))
        .collect()
}

/// The query log's record of what the daemon's own refreshes cost, so the
/// hourly backstop's price (D121) can be read from real use: one `refresh`
/// line per backstop, overflow or poll refresh, with its kind, outcome,
/// elapsed time and the walk's counts. An inotify burst is not logged, since
/// one can follow every save. A refresh that is deferred or fails is retried
/// every second while the cause lasts, so only the first line of such a run
/// is written; the next line is the kind's next different outcome.
#[derive(Default)]
struct RefreshLog {
    /// The kind and outcome of the last line, when it was one a retry would
    /// repeat.
    repeating: Option<(&'static str, &'static str)>,
}
impl RefreshLog {
    /// Logs a finished refresh. `polled` says it took a poll's roots. Without
    /// a watch every refresh is a full walk, so a poll is logged as a
    /// backstop. A line that cannot be written is a warning on the daemon's
    /// stderr, as a command's is on the client's.
    fn record(
        &mut self,
        context: &crate::cli::Context,
        reason: RefreshReason,
        polled: bool,
        elapsed: Duration,
        result: &io::Result<ferret_crawl::RefreshReport>,
    ) {
        use ferret_crawl::RefreshOutcome;
        let kind = match reason {
            RefreshReason::Backstop => "backstop",
            RefreshReason::Overflow => "overflow",
            RefreshReason::PolicyChange => "policy-change",
            RefreshReason::Burst if polled => "poll",
            RefreshReason::Burst => return,
        };
        let outcome = match result.as_ref().map(|r| &r.outcome) {
            Ok(RefreshOutcome::Unchanged) => "unchanged",
            Ok(RefreshOutcome::Committed { .. } | RefreshOutcome::Checkpointed) => "published",
            Ok(RefreshOutcome::DeferredBulk(_)) => "deferred",
            Ok(RefreshOutcome::RetryFromCurrent(_)) => "retry",
            Err(_) => "error",
        };
        let repeats = matches!(outcome, "deferred" | "retry" | "error");
        if repeats && self.repeating == Some((kind, outcome)) {
            return;
        }
        self.repeating = repeats.then_some((kind, outcome));
        let began = SystemTime::now().checked_sub(elapsed).unwrap_or(UNIX_EPOCH);
        let mut line = Vec::new();
        let mut object = crate::log::line(&mut line, "refresh", began);
        object
            .str("kind", kind)
            .str("outcome", outcome)
            .int("total_us", elapsed.as_micros() as i128);
        if let Ok(refreshed) = result {
            crate::index::log_report(&mut object, &refreshed.report);
        }
        object.end();
        context.log(&line);
    }
}
