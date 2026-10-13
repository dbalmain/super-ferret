//! Verifying a content query's Maybe rows: on the calling thread, or on a
//! bounded pool of workers (D76 C).
//!
//! Rows arrive from S1 in name order and leave in name order. The calling
//! thread keeps them in a window, hands each Maybe row whose document is new
//! to the run to a worker, and emits from the window's head as answers come
//! back. For inputs that do not change during the run, and without
//! cancellation, the output is the sequential order's at any thread count:
//!
//! - which rows are emitted, their offsets, and their order;
//! - a `Break` from `emit` (a limit, a closed pipe) stops at the same row.
//!   Workers stop taking jobs, reads in progress stop at their next chunk, and
//!   documents read past that row are not counted anywhere;
//! - the report's counts (`verified`, `changed`, the matching statistics) are
//!   summed at the head, so they count exactly the rows the sequential order
//!   consumed;
//! - a second name of a document already seen in the run is settled on the
//!   calling thread when it reaches the head, by the sequential logic, so the
//!   cache of catalogued answers is in the same state when it is asked.
//!
//! The times differ: `verification_time` and `regex_time` sum the workers'
//! busy time, not wall time.
//!
//! Verification sees a document's bytes when it reads them, and a worker
//! reads up to [`WINDOW_PER_THREAD`] rows per thread ahead of the head. A
//! read may therefore happen before the callbacks for earlier rows run, so
//! a callback that changes a later document (deletes it, rewrites it) may or
//! may not change that document's row, where the sequential order always
//! would. No production caller's callback changes files; one that must see
//! its own effects uses one thread, which keeps the sequential order.
//!
//! Cancellation is checked before a row leaves the window and again after
//! waiting for its worker. A row that has not passed both checks is neither
//! counted nor emitted, and the candidate count rewinds to it; rows that
//! left before are counted in full. On the calling thread a document
//! cancelled between atoms counts the atoms that finished, as the
//! sequential order did. Where cancellation lands is a matter of timing,
//! so the counts of a cancelled run vary with the thread count.
//!
//! Memory: a worker reserves a document's catalogued size (at most
//! [`IN_FLIGHT_BYTES`]) from a shared budget of [`IN_FLIGHT_BYTES`] before
//! reading it, and frees a buffer larger than [`KEPT_BUFFER`] before
//! returning the reservation. The workers' buffers therefore hold at most
//! `IN_FLIGHT_BYTES + threads × KEPT_BUFFER`, plus whatever the files being
//! read grew beyond their catalogued size (each still under the reader's
//! size cap). The calling thread's buffer is separate: it reads one
//! document at a time, as before, and keeps the capacity of the largest it
//! read.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::Scope;
use std::time::{Duration, Instant};

use ferret_catalog::{Catalog, InoId, Kind, NameId};
use ferret_index::Certainty;
use ferret_verify::{MatchStats, TextMatcher};

use crate::expr::{Test, Truth};
use crate::plan::{ReadRequest, ReadVersion, Reader};
use crate::{ContentAtom, Query, Row, RunError, Stats};

/// Makes one [`Reader`] per verifying thread. Each reader is used by one
/// thread only; the factory itself is shared.
pub type Fork<'a> = dyn Fn() -> Box<Reader<'a>> + Sync + 'a;

/// Document bytes the workers may hold at once, by catalogued size.
pub const IN_FLIGHT_BYTES: u64 = 64 << 20;

/// A worker keeps a read buffer up to this capacity between documents.
pub const KEPT_BUFFER: usize = 1 << 20;

/// Rows the window holds per worker before the calling thread waits for
/// its head.
const WINDOW_PER_THREAD: usize = 64;

/// Verifying on `threads` workers, each with a reader from `fork`.
#[derive(Clone, Copy)]
pub struct Workers<'r, 'a> {
    pub fork: &'r Fork<'a>,
    /// One verifies on the calling thread.
    pub threads: usize,
}

/// Where verification reads.
pub(crate) enum Readers<'r, 'a> {
    /// The calling thread, through one reader.
    One(&'r mut Reader<'a>),
    Pool(Workers<'r, 'a>),
}

/// What verifying the rows did, beside the rows.
#[derive(Debug, Default)]
pub(crate) struct Tally {
    pub verified: u64,
    pub changed: u64,
    pub time: Duration,
    pub regex: Duration,
    pub matching: MatchStats,
    pub maybe_docs: u64,
    pub possible_docs: u64,
    pub threads: usize,
}

/// The index's answer for each content atom on each probed document.
pub(crate) struct Probed<'t> {
    /// The names to test, in emission order.
    pub ids: &'t [NameId],
    /// The probed documents, ascending.
    pub docs: &'t [u32],
    /// `table[j * width + i]` is atom `i` on `docs[j]`.
    pub table: &'t [Option<Certainty>],
    pub width: usize,
}

impl Query {
    /// Tests the names `ids` as [`Query::s1_ids`] does, settles each Maybe
    /// row by reading its document, and emits the rows that hold, in the
    /// order of `ids`.
    pub(crate) fn verify_rows(
        &self,
        catalog: &Catalog,
        probed: &Probed<'_>,
        readers: Readers<'_, '_>,
        cancelled: Option<&AtomicBool>,
        emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<(Stats, Tally), RunError> {
        let stopped = || cancelled.is_some_and(|flag| flag.load(Ordering::Acquire));
        match readers {
            Readers::One(read) => {
                let verifier = Verifier::new(self, read, 1);
                self.ordered(catalog, probed, verifier, None, cancelled, emit)
            }
            Readers::Pool(Workers { fork, threads }) if threads <= 1 => {
                let mut read = fork();
                let verifier = Verifier::new(self, &mut *read, 1);
                self.ordered(catalog, probed, verifier, None, cancelled, emit)
            }
            Readers::Pool(Workers { fork, threads }) => {
                let pool = Pool::new();
                std::thread::scope(|scope| {
                    // Stops the workers however this returns, a failed
                    // spawn or a panic included, so the scope can join
                    // them. Installed before the first spawn: a worker
                    // already started waits for jobs until it is stopped.
                    let _stop = StopOnDrop(&pool);
                    // A thread that cannot be created leaves the run with
                    // the workers that were; the output does not depend on
                    // how many there are.
                    let started = (0..threads)
                        .take_while(|&i| {
                            spawn(scope, i, || pool.work(&self.atoms, fork, &stopped)).is_ok()
                        })
                        .count();
                    let mut read = fork();
                    let verifier = Verifier::new(self, &mut *read, started.max(1));
                    let pool = (started > 0).then_some(&pool);
                    self.ordered(catalog, probed, verifier, pool, cancelled, emit)
                })
            }
        }
    }

    fn ordered(
        &self,
        catalog: &Catalog,
        probed: &Probed<'_>,
        verifier: Verifier<'_, '_, '_>,
        pool: Option<&Pool>,
        cancelled: Option<&AtomicBool>,
        emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<(Stats, Tally), RunError> {
        let stopped = || cancelled.is_some_and(|flag| flag.load(Ordering::Acquire));
        let mut window = Window {
            query: self,
            catalog,
            waiting: VecDeque::new(),
            limit: pool.map_or(0, |_| verifier.tally.threads * WINDOW_PER_THREAD),
            pool,
            polled: 0,
            verifier,
            rows: 0,
            emit,
            stopped: &stopped,
            halted_at: None,
            maybe_docs: HashSet::new(),
            possible_docs: HashSet::new(),
        };
        let mut seen = HashSet::new();
        let mut halted = false;
        let Probed {
            ids,
            docs,
            table,
            width,
        } = *probed;
        // A row S1 passed, if its index truth leaves it in the window.
        let mut admit = |row: &Row<'_>, candidates| {
            let doc = catalog.doc(row.inode).map(|d| d.0);
            let at = doc.and_then(|d| docs.binary_search(&d).ok());
            let indexed = |i: usize| match at.and_then(|j| table[j * width + i]) {
                None => Truth::No,
                Some(Certainty::Maybe) => Truth::Maybe,
                Some(Certainty::Yes) => Truth::Yes,
            };
            let truth = self.eval_residual(&mut |test| match test {
                Test::Content(i) => indexed(*i),
                other => self.test_row(catalog, row, other),
            });
            let slot = match (truth, doc) {
                (Truth::No, _) | (Truth::Maybe, None) => return None,
                (Truth::Yes, _) => Slot::Holds,
                (Truth::Maybe, Some(doc)) => match pool {
                    Some(pool) if seen.insert(doc) => {
                        Slot::Job(doc, pool.submit(row.name, catalog.size(row.inode)))
                    }
                    _ => {
                        seen.insert(doc);
                        Slot::Here(doc)
                    }
                },
            };
            Some(Waiting {
                path: row.path.to_vec(),
                name: row.name,
                inode: row.inode,
                kind: row.kind,
                doc,
                candidates,
                slot,
            })
        };
        let mut stats = self.s1_ids(catalog, ids, cancelled, |row, candidates| {
            // A candidate that does not join the window still lets the head
            // advance: otherwise a ready head waits for the next row S1
            // passes, which may be the end of the scan.
            let flow = match row.and_then(|row| admit(row, candidates)) {
                Some(waiting) => {
                    window.waiting.push_back(waiting);
                    window.drain(false)
                }
                None => window.poll(),
            };
            halted = flow.is_break();
            flow
        })?;
        match window.halted_at {
            // S1 ran ahead of the row that stopped the run.
            Some(candidates) => stats.candidates = candidates,
            None if !halted => {
                if let ControlFlow::Break(()) = window.drain(true) {
                    stats.candidates = window.halted_at.unwrap_or(stats.candidates);
                }
            }
            None => {}
        }
        stats.rows = window.rows;
        let mut tally = window.verifier.tally;
        tally.maybe_docs = window.maybe_docs.len() as u64;
        tally.possible_docs = window.possible_docs.len() as u64;
        Ok((stats, tally))
    }
}

// ── the window: rows in name order, waiting for their answers ──

/// A row S1 passed whose content truth is Yes or Maybe, copied out of the
/// run's path buffer.
struct Waiting {
    path: Vec<u8>,
    name: NameId,
    inode: InoId,
    kind: Kind,
    /// Its document, if it has one.
    doc: Option<u32>,
    /// S1's candidate count when it passed.
    candidates: u64,
    slot: Slot,
}

enum Slot {
    /// The index says it holds; no read.
    Holds,
    /// Its document, read by the worker job with this sequence number.
    Job(u32, u64),
    /// Its document, settled on the calling thread at the head.
    Here(u32),
}

struct Window<'w, 'q, 'r, 'a, E> {
    query: &'q Query,
    catalog: &'w Catalog,
    waiting: VecDeque<Waiting>,
    /// Rows held before waiting for the head.
    limit: usize,
    pool: Option<&'w Pool>,
    /// The pool's finished-job count when the window last looked.
    polled: u64,
    verifier: Verifier<'q, 'r, 'a>,
    rows: u64,
    emit: E,
    stopped: &'w dyn Fn() -> bool,
    /// The candidate count at the row that stopped the run, if one did.
    halted_at: Option<u64>,
    /// Documents of rows consumed, by their index truth: Maybe, and not No.
    maybe_docs: HashSet<u32>,
    possible_docs: HashSet<u32>,
}

impl<E: FnMut(&Row<'_>) -> ControlFlow<()>> Window<'_, '_, '_, '_, E> {
    /// Emits from the head while it is settled, or every row when `all`.
    /// A worker's row is waited for only when `all` or the window is full.
    fn drain(&mut self, all: bool) -> ControlFlow<()> {
        while let Some(head) = self.waiting.front() {
            // The accounting boundary (module doc): a cancelled run
            // consumes no further row.
            if (self.stopped)() {
                self.halted_at = Some(head.candidates);
                return ControlFlow::Break(());
            }
            let ready = match head.slot {
                Slot::Holds | Slot::Here(_) => true,
                Slot::Job(_, seq) => {
                    all || self.waiting.len() > self.limit
                        || self.pool.is_some_and(|pool| pool.is_done(seq))
                }
            };
            if !ready {
                break;
            }
            let Some(head) = self.waiting.pop_front() else {
                break;
            };
            let candidates = head.candidates;
            if self.finish(head).is_break() {
                self.halted_at = Some(candidates);
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }

    /// [`Window::drain`] without waiting, between rows: a head job can
    /// only have become ready if a job finished since the last look, which
    /// costs one atomic load per candidate to tell.
    fn poll(&mut self) -> ControlFlow<()> {
        let Some(pool) = self.pool else {
            return ControlFlow::Continue(());
        };
        #[cfg(test)]
        pool.seams.polling(&|| pool.finished());
        let finished = pool.finished();
        if self.waiting.is_empty() || finished == self.polled {
            return ControlFlow::Continue(());
        }
        self.polled = finished;
        self.drain(false)
    }

    /// Counts a consumed row's document, by its index truth.
    fn consumed(&mut self, doc: Option<u32>, maybe: bool) {
        if let Some(doc) = doc {
            self.possible_docs.insert(doc);
            if maybe {
                self.maybe_docs.insert(doc);
            }
        }
    }

    /// Settles one row and emits it if it holds.
    fn finish(&mut self, waiting: Waiting) -> ControlFlow<()> {
        let mut row = Row {
            path: &waiting.path,
            name: waiting.name,
            inode: waiting.inode,
            kind: waiting.kind,
            offset: None,
        };
        let (doc, settled) = match waiting.slot {
            Slot::Holds => {
                self.consumed(waiting.doc, false);
                self.rows += 1;
                return (self.emit)(&row);
            }
            Slot::Here(doc) => {
                self.consumed(waiting.doc, true);
                (doc, self.verifier.here(doc, waiting.name, self.stopped))
            }
            Slot::Job(doc, seq) => {
                let Some(outcome) = self.pool.and_then(|pool| pool.wait(seq)) else {
                    // A worker panicked; the scope re-raises it.
                    return ControlFlow::Break(());
                };
                // Cancelled while waiting: the row is not consumed.
                if (self.stopped)() {
                    return ControlFlow::Break(());
                }
                self.consumed(waiting.doc, true);
                (doc, self.verifier.settle(doc, outcome))
            }
        };
        let answers = match &settled {
            Settled::Unread => return ControlFlow::Continue(()),
            Settled::Stopped => return ControlFlow::Break(()),
            Settled::Cached => &self.verifier.exact[&doc],
            Settled::Fresh(answers) => answers,
        };
        let (query, catalog) = (self.query, self.catalog);
        row.offset = answers
            .iter()
            .zip(&query.atoms)
            .filter_map(|(answer, atom)| match atom {
                ContentAtom::Grep(_) => *answer,
                ContentAtom::Text(_) => None,
            })
            .min();
        let truth = query.eval_residual(&mut |test| match test {
            Test::Content(i) => answers[*i].is_some().into(),
            other => query.test_row(catalog, &row, other),
        });
        if truth != Truth::Yes {
            return ControlFlow::Continue(());
        }
        self.rows += 1;
        (self.emit)(&row)
    }
}

// ── settling a document: the sequential logic ──

/// One document read and matched.
enum Outcome {
    /// The reader failed: vanished, unreadable, refused or cancelled.
    Unread(Duration),
    /// Stopped before the read, or between atoms: the work the atoms that
    /// finished did, which the sequential order counted.
    Stopped(Matched),
    Read {
        version: ReadVersion,
        answers: Vec<Option<usize>>,
        work: Matched,
    },
}

/// The time and matcher work spent on one document.
#[derive(Default)]
struct Matched {
    time: Duration,
    regex: Duration,
    matching: MatchStats,
}

enum Settled {
    Unread,
    Stopped,
    /// The run's cached answers for the document.
    Cached,
    /// Answers for this path alone: its current version differs.
    Fresh(Vec<Option<usize>>),
}

/// The calling thread's side: the cache of catalogued answers, the counts,
/// and a reader for rows settled here.
struct Verifier<'q, 'r, 'a> {
    atoms: &'q [ContentAtom],
    read: &'r mut Reader<'a>,
    matcher: TextMatcher,
    bytes: Vec<u8>,
    /// Only content facts established by a bracketed catalog-version read.
    /// Path failures never enter this map; every path has its own stat.
    exact: HashMap<u32, Vec<Option<usize>>>,
    tally: Tally,
}

impl<'q, 'r, 'a> Verifier<'q, 'r, 'a> {
    fn new(query: &'q Query, read: &'r mut Reader<'a>, threads: usize) -> Self {
        Self {
            atoms: &query.atoms,
            read,
            matcher: TextMatcher::new(),
            bytes: Vec::new(),
            exact: HashMap::new(),
            tally: Tally {
                threads,
                ..Tally::default()
            },
        }
    }

    /// Settles `name`'s row on this thread: reuse the document's cached
    /// answers if a fresh stat says this path is still the catalogued
    /// version, else read it.
    fn here(&mut self, doc: u32, name: NameId, stopped: &dyn Fn() -> bool) -> Settled {
        if self.exact.contains_key(&doc) {
            match (self.read)(name, ReadRequest::Stat) {
                Some(ReadVersion::Catalogued) => return Settled::Cached,
                Some(ReadVersion::Current) => {}
                None => {
                    self.tally.changed += 1;
                    return Settled::Unread;
                }
            }
        }
        let outcome = verify(
            self.atoms,
            &mut *self.read,
            &mut self.matcher,
            &mut self.bytes,
            name,
            stopped,
        );
        self.settle(doc, outcome)
    }

    /// Counts a read, wherever it ran, and caches catalogued answers.
    fn settle(&mut self, doc: u32, outcome: Outcome) -> Settled {
        let tally = &mut self.tally;
        tally.verified += 1;
        let count = |tally: &mut Tally, work: Matched| {
            tally.time += work.time;
            tally.regex += work.regex;
            add(&mut tally.matching, &work.matching);
        };
        match outcome {
            Outcome::Unread(time) => {
                tally.time += time;
                tally.changed += 1;
                Settled::Unread
            }
            Outcome::Stopped(work) => {
                count(tally, work);
                Settled::Stopped
            }
            Outcome::Read {
                version,
                answers,
                work,
            } => {
                count(tally, work);
                match version {
                    ReadVersion::Catalogued => {
                        self.exact.entry(doc).or_insert(answers);
                        Settled::Cached
                    }
                    ReadVersion::Current => {
                        tally.changed += 1;
                        Settled::Fresh(answers)
                    }
                }
            }
        }
    }
}

/// Reads `name`'s document into `bytes` and runs every atom over it,
/// unless `stopped` says to stop first; the reader is given `stopped` too.
fn verify(
    atoms: &[ContentAtom],
    read: &mut Reader<'_>,
    matcher: &mut TextMatcher,
    bytes: &mut Vec<u8>,
    name: NameId,
    stopped: &dyn Fn() -> bool,
) -> Outcome {
    let start = Instant::now();
    if stopped() {
        return Outcome::Stopped(Matched::default());
    }
    let Some(version) = read(name, ReadRequest::Bytes(bytes, stopped)) else {
        return Outcome::Unread(start.elapsed());
    };
    let before = matcher.stats();
    let mut work = Matched::default();
    let mut answers = Vec::with_capacity(atoms.len());
    let mut halted = false;
    for atom in atoms {
        if stopped() {
            halted = true;
            break;
        }
        let engine_start = Instant::now();
        let answer = atom.find(matcher, bytes);
        if matches!(atom, ContentAtom::Grep(_)) {
            work.regex += engine_start.elapsed();
            work.matching.regex_documents += 1;
            work.matching.regex_bytes += bytes.len() as u64;
            work.matching.regex_rejected += u64::from(answer.is_none());
        }
        if stopped() {
            halted = true;
            break;
        }
        answers.push(answer);
    }
    let after = matcher.stats();
    work.matching.documents = after.documents - before.documents;
    work.matching.rejected = after.rejected - before.rejected;
    work.matching.whole = after.whole - before.whole;
    work.matching.tokenized = after.tokenized - before.tokenized;
    work.time = start.elapsed();
    if halted {
        return Outcome::Stopped(work);
    }
    Outcome::Read {
        version,
        answers,
        work,
    }
}

fn add(sum: &mut MatchStats, more: &MatchStats) {
    sum.regex_documents += more.regex_documents;
    sum.regex_bytes += more.regex_bytes;
    sum.regex_rejected += more.regex_rejected;
    sum.documents += more.documents;
    sum.rejected += more.rejected;
    sum.whole += more.whole;
    sum.tokenized += more.tokenized;
}

// ── the pool ──

/// Jobs in submission order, answers by sequence number, and the byte
/// budget the workers share.
struct Pool {
    state: Mutex<PoolState>,
    /// A job was submitted, or the pool stopped.
    work: Condvar,
    /// A job finished, or a worker was lost.
    done: Condvar,
    /// Bytes reserved by workers reading now.
    budget: Mutex<Budget>,
    freed: Condvar,
    stop: AtomicBool,
    /// Jobs finished so far, for [`Window::poll`].
    finished: AtomicU64,
    #[cfg(test)]
    seams: seam::Seams,
}

#[derive(Default)]
struct PoolState {
    /// Sequence number, name, catalogued size.
    jobs: VecDeque<(u64, NameId, u64)>,
    finished: HashMap<u64, Outcome>,
    next: u64,
    /// A worker panicked; its job will never finish.
    lost: bool,
    /// Workers waiting for a job. Wakes are syscalls, so a submit or a
    /// finish signals only a thread that is waiting for it.
    idle: usize,
    /// The job the calling thread is waiting for, if it is.
    awaited: Option<u64>,
}

#[derive(Default)]
struct Budget {
    used: u64,
    /// Workers waiting for bytes to be freed.
    waiting: usize,
}

impl Pool {
    fn new() -> Self {
        Self {
            state: Mutex::default(),
            work: Condvar::new(),
            done: Condvar::new(),
            budget: Mutex::default(),
            freed: Condvar::new(),
            stop: AtomicBool::new(false),
            finished: AtomicU64::new(0),
            #[cfg(test)]
            seams: seam::Seams::current(),
        }
    }

    fn state(&self) -> MutexGuard<'_, PoolState> {
        // A panicking worker holds no lock across user code, so the state
        // is consistent; `lost` reports the panic.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn submit(&self, name: NameId, size: u64) -> u64 {
        let mut state = self.state();
        let seq = state.next;
        state.next += 1;
        state.jobs.push_back((seq, name, size));
        let wake = state.idle > 0;
        drop(state);
        if wake {
            self.work.notify_one();
        }
        seq
    }

    fn finished(&self) -> u64 {
        self.finished.load(Ordering::Acquire)
    }

    fn is_done(&self, seq: u64) -> bool {
        self.state().finished.contains_key(&seq)
    }

    /// The job's outcome, once finished; `None` if a worker was lost.
    fn wait(&self, seq: u64) -> Option<Outcome> {
        let mut state = self.state();
        loop {
            if let Some(outcome) = state.finished.remove(&seq) {
                return Some(outcome);
            }
            if state.lost {
                return None;
            }
            state.awaited = Some(seq);
            state = self
                .done
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
            state.awaited = None;
        }
    }

    fn halt(&self) {
        self.stop.store(true, Ordering::Release);
        // Taking each lock orders the store before any waiter's next check.
        drop(self.state());
        self.work.notify_all();
        drop(self.budget.lock().unwrap_or_else(PoisonError::into_inner));
        self.freed.notify_all();
    }

    fn halted(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// One worker: take jobs in order until the pool stops. Every job taken
    /// finishes, if only as stopped, so the calling thread never waits for
    /// one that will not.
    fn work(&self, atoms: &[ContentAtom], fork: &Fork<'_>, stopped: &(dyn Fn() -> bool + Sync)) {
        let _lost = LostOnPanic(self);
        let mut read = fork();
        let mut matcher = TextMatcher::new();
        let mut bytes = Vec::new();
        let halt = || self.halted() || stopped();
        while let Some((seq, name, size)) = self.next_job() {
            let outcome = match self.reserve(size, &halt) {
                Some(size) => {
                    let held = Held {
                        pool: self,
                        size,
                        bytes: &mut bytes,
                    };
                    verify(
                        atoms,
                        &mut *read,
                        &mut matcher,
                        &mut *held.bytes,
                        name,
                        &halt,
                    )
                }
                None => Outcome::Stopped(Matched::default()),
            };
            let mut state = self.state();
            state.finished.insert(seq, outcome);
            self.finished.fetch_add(1, Ordering::Release);
            let wake = state.awaited == Some(seq);
            drop(state);
            if wake {
                self.done.notify_all();
            }
        }
    }

    fn next_job(&self) -> Option<(u64, NameId, u64)> {
        let mut state = self.state();
        loop {
            if self.halted() {
                return None;
            }
            if let Some(job) = state.jobs.pop_front() {
                return Some(job);
            }
            state.idle += 1;
            state = self
                .work
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
            state.idle -= 1;
        }
    }

    /// Waits until `size` bytes (at most the whole budget) fit, and reserves
    /// them; one reader always proceeds alone. `None` once `halt` says the
    /// pool or the query stopped, waiting or not, so no read starts after.
    fn reserve(&self, size: u64, halt: &dyn Fn() -> bool) -> Option<u64> {
        let size = size.min(IN_FLIGHT_BYTES);
        let mut budget = self.budget.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            // A pool stop wakes this wait. A cancelled query does not, but
            // a reader holding bytes stops at its next chunk and frees
            // them, which does.
            if halt() {
                return None;
            }
            if budget.used == 0 || budget.used + size <= IN_FLIGHT_BYTES {
                break;
            }
            budget.waiting += 1;
            #[cfg(test)]
            self.seams.waiting_for_bytes();
            budget = self
                .freed
                .wait(budget)
                .unwrap_or_else(PoisonError::into_inner);
            budget.waiting -= 1;
        }
        budget.used += size;
        Some(size)
    }

    fn release(&self, size: u64) {
        let mut budget = self.budget.lock().unwrap_or_else(PoisonError::into_inner);
        budget.used -= size;
        let wake = budget.waiting > 0;
        drop(budget);
        if wake {
            self.freed.notify_all();
        }
    }
}

/// Bytes reserved for one read, and the worker's buffer they were charged
/// for. Dropping it frees an oversized buffer first and only then returns
/// the bytes, so a reader that the release wakes cannot allocate against
/// them while the buffer still holds them.
struct Held<'p, 'b> {
    pool: &'p Pool,
    size: u64,
    bytes: &'b mut Vec<u8>,
}

impl Drop for Held<'_, '_> {
    fn drop(&mut self) {
        if self.bytes.capacity() > KEPT_BUFFER {
            *self.bytes = Vec::new();
        }
        self.pool.release(self.size);
    }
}

/// Starts worker `i` on the scope.
fn spawn<'scope>(
    scope: &'scope Scope<'scope, '_>,
    i: usize,
    work: impl FnOnce() + Send + 'scope,
) -> std::io::Result<()> {
    #[cfg(test)]
    if seam::Seams::current().fail_spawn_at == Some(i) {
        return Err(std::io::Error::other("injected spawn failure"));
    }
    #[cfg(not(test))]
    let _ = i;
    std::thread::Builder::new()
        .name("ferret-verify".into())
        .spawn_scoped(scope, work)
        .map(drop)
}

/// Stops the pool when the calling thread leaves the scope, by any path.
struct StopOnDrop<'p>(&'p Pool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.halt();
    }
}

/// Tells the calling thread a worker panicked, so it stops waiting.
struct LostOnPanic<'p>(&'p Pool);

impl Drop for LostOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.state().lost = true;
            self.0.done.notify_all();
        }
    }
}

/// Test-only hooks into the pool, set per test thread: a spawn that fails,
/// and a signal when a worker starts waiting for bytes.
#[cfg(test)]
pub(crate) mod seam {
    use std::cell::RefCell;
    use std::sync::Arc;

    /// Given the pool's finished-job count.
    pub(crate) type Poll = dyn Fn(&dyn Fn() -> u64) + Send + Sync;

    #[derive(Clone, Default)]
    pub(crate) struct Seams {
        /// The worker whose spawn fails.
        pub fail_spawn_at: Option<usize>,
        /// Called, under the budget lock, when a worker waits for bytes.
        pub on_budget_wait: Option<Arc<dyn Fn() + Send + Sync>>,
        /// Called on the calling thread at each candidate that joins no
        /// window, with the pool's finished-job count.
        pub on_poll: Option<Arc<Poll>>,
    }

    thread_local! {
        static SEAMS: RefCell<Seams> = RefCell::default();
    }

    impl Seams {
        /// The calling thread's seams, copied into the pool it builds.
        pub(crate) fn current() -> Self {
            SEAMS.with(|seams| seams.borrow().clone())
        }

        /// Sets the calling thread's seams until the guard drops.
        pub(crate) fn set(self) -> impl Drop {
            struct Reset;
            impl Drop for Reset {
                fn drop(&mut self) {
                    SEAMS.with(|seams| *seams.borrow_mut() = Seams::default());
                }
            }
            SEAMS.with(|seams| *seams.borrow_mut() = self);
            Reset
        }

        pub(super) fn waiting_for_bytes(&self) {
            if let Some(hook) = &self.on_budget_wait {
                hook();
            }
        }

        pub(super) fn polling(&self, finished: &dyn Fn() -> u64) {
            if let Some(hook) = &self.on_poll {
                hook(finished);
            }
        }
    }
}

/// Finding 4 of the round-1 review: a document cancelled between atoms on
/// the calling thread counts the atoms that finished, as the sequential
/// order did. Cancellation is a flag, so it cannot be timed to land between
/// two atoms through the query; this drives [`Verifier::here`], the calling
/// thread's path, with a predicate that says stop after the first atom.
#[cfg(test)]
mod partial {
    use std::cell::Cell;
    use std::time::{Duration, UNIX_EPOCH};

    use super::{Settled, Verifier};
    use crate::{Query, ReadRequest, ReadVersion};

    #[test]
    fn a_document_cancelled_between_atoms_counts_the_finished_atom() {
        let query = Query::parse(
            "grep:needle grep:other",
            UNIX_EPOCH + Duration::from_secs(1),
        )
        .unwrap();
        let mut read = |_, request: ReadRequest<'_>| {
            if let ReadRequest::Bytes(out, _) = request {
                out.clear();
                out.extend_from_slice(b"a needle here");
            }
            Some(ReadVersion::Catalogued)
        };
        let mut verifier = Verifier::new(&query, &mut read, 1);
        // Checked before the read, before the first atom, then after it.
        let checks = Cell::new(0);
        let stopped = || {
            checks.set(checks.get() + 1);
            checks.get() > 2
        };
        let settled = verifier.here(0, ferret_catalog::NameId(0), &stopped);
        assert!(matches!(settled, Settled::Stopped));
        let tally = &verifier.tally;
        assert_eq!(tally.verified, 1);
        assert_eq!(tally.matching.regex_documents, 1);
        assert_eq!(tally.matching.regex_bytes, 13);
        assert_eq!(tally.matching.regex_rejected, 0);
        assert!(verifier.exact.is_empty());
    }
}
