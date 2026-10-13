//! Verifying a content query's Maybe rows: on the calling thread, or on a
//! bounded pool of workers (D76 C).
//!
//! Rows arrive from S1 in name order and leave in name order. The calling
//! thread keeps them in a window, hands each Maybe row whose document is new
//! to the run to a worker, and emits from the window's head as answers come
//! back. Everything the sequential order decided is decided the same way:
//!
//! - which rows are emitted, their offsets, and their order;
//! - a `Break` from `emit` (a limit, a closed pipe) stops at the same row.
//!   Workers stop taking jobs; documents they read past that row are not
//!   counted anywhere;
//! - the report's counts (`verified`, `changed`, the matching statistics) are
//!   summed at the head, so they count exactly the rows the sequential order
//!   consumed;
//! - a second name of a document already seen in the run is settled on the
//!   calling thread when it reaches the head, by the sequential logic, so the
//!   cache of catalogued answers is in the same state when it is asked.
//!
//! Only times differ: `verification_time` and `regex_time` sum the workers'
//! busy time, not wall time.
//!
//! Memory: a worker reserves a document's catalogued size from a shared
//! budget of [`IN_FLIGHT_BYTES`] before reading it and returns it after
//! matching, and drops a buffer larger than [`KEPT_BUFFER`] afterwards. Bytes
//! held at once are therefore at most `IN_FLIGHT_BYTES + threads ×
//! KEPT_BUFFER`, plus whatever files grew since they were catalogued (each
//! still under the reader's size cap). The calling thread's own reads are
//! one document at a time, as before.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
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
                let pool = Pool::new(threads);
                std::thread::scope(|scope| {
                    for _ in 0..threads {
                        scope.spawn(|| pool.work(&self.atoms, fork, &stopped));
                    }
                    // Stops the workers however this returns, so the scope
                    // can join them.
                    let _stop = StopOnDrop(&pool);
                    let mut read = fork();
                    let verifier = Verifier::new(self, &mut *read, threads);
                    self.ordered(catalog, probed, verifier, Some(&pool), cancelled, emit)
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
            limit: pool.map_or(0, |pool| pool.threads * WINDOW_PER_THREAD),
            pool,
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
        let mut stats = self.s1_ids(catalog, ids, cancelled, |row, candidates| {
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
                (Truth::No, _) | (Truth::Maybe, None) => return ControlFlow::Continue(()),
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
            window.waiting.push_back(Waiting {
                path: row.path.to_vec(),
                name: row.name,
                inode: row.inode,
                kind: row.kind,
                doc,
                candidates,
                slot,
            });
            let flow = window.drain(false);
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

    /// Settles one row and emits it if it holds.
    fn finish(&mut self, waiting: Waiting) -> ControlFlow<()> {
        let mut row = Row {
            path: &waiting.path,
            name: waiting.name,
            inode: waiting.inode,
            kind: waiting.kind,
            offset: None,
        };
        if let Some(doc) = waiting.doc {
            self.possible_docs.insert(doc);
            if !matches!(waiting.slot, Slot::Holds) {
                self.maybe_docs.insert(doc);
            }
        }
        let (doc, settled) = match waiting.slot {
            Slot::Holds => {
                self.rows += 1;
                return (self.emit)(&row);
            }
            Slot::Here(doc) => (doc, self.verifier.here(doc, waiting.name, self.stopped)),
            Slot::Job(doc, seq) => {
                let Some(outcome) = self.pool.and_then(|pool| pool.wait(seq)) else {
                    // A worker panicked; the scope re-raises it.
                    return ControlFlow::Break(());
                };
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
    /// Cancelled between atoms.
    Stopped(Duration),
    Read {
        version: ReadVersion,
        answers: Vec<Option<usize>>,
        time: Duration,
        regex: Duration,
        matching: MatchStats,
    },
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
        match outcome {
            Outcome::Unread(time) => {
                tally.time += time;
                tally.changed += 1;
                Settled::Unread
            }
            Outcome::Stopped(time) => {
                tally.time += time;
                Settled::Stopped
            }
            Outcome::Read {
                version,
                answers,
                time,
                regex,
                matching,
            } => {
                tally.time += time;
                tally.regex += regex;
                add(&mut tally.matching, &matching);
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

/// Reads `name`'s document into `bytes` and runs every atom over it.
fn verify(
    atoms: &[ContentAtom],
    read: &mut Reader<'_>,
    matcher: &mut TextMatcher,
    bytes: &mut Vec<u8>,
    name: NameId,
    stopped: &dyn Fn() -> bool,
) -> Outcome {
    let start = Instant::now();
    let Some(version) = read(name, ReadRequest::Bytes(bytes)) else {
        return Outcome::Unread(start.elapsed());
    };
    let before = matcher.stats();
    let mut regex = Duration::ZERO;
    let mut matching = MatchStats::default();
    let mut answers = Vec::with_capacity(atoms.len());
    for atom in atoms {
        if stopped() {
            return Outcome::Stopped(start.elapsed());
        }
        let engine_start = Instant::now();
        let answer = atom.find(matcher, bytes);
        if matches!(atom, ContentAtom::Grep(_)) {
            regex += engine_start.elapsed();
            matching.regex_documents += 1;
            matching.regex_bytes += bytes.len() as u64;
            matching.regex_rejected += u64::from(answer.is_none());
        }
        if stopped() {
            return Outcome::Stopped(start.elapsed());
        }
        answers.push(answer);
    }
    let after = matcher.stats();
    matching.documents = after.documents - before.documents;
    matching.rejected = after.rejected - before.rejected;
    matching.whole = after.whole - before.whole;
    matching.tokenized = after.tokenized - before.tokenized;
    Outcome::Read {
        version,
        answers,
        time: start.elapsed(),
        regex,
        matching,
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
    threads: usize,
    state: Mutex<PoolState>,
    /// A job was submitted, or the pool stopped.
    work: Condvar,
    /// A job finished, or a worker was lost.
    done: Condvar,
    /// Bytes reserved by workers reading now.
    budget: Mutex<Budget>,
    freed: Condvar,
    stop: AtomicBool,
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
    fn new(threads: usize) -> Self {
        Self {
            threads,
            state: Mutex::default(),
            work: Condvar::new(),
            done: Condvar::new(),
            budget: Mutex::default(),
            freed: Condvar::new(),
            stop: AtomicBool::new(false),
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

    /// One worker: take jobs in order until the pool stops.
    fn work(&self, atoms: &[ContentAtom], fork: &Fork<'_>, stopped: &(dyn Fn() -> bool + Sync)) {
        let _lost = LostOnPanic(self);
        let mut read = fork();
        let mut matcher = TextMatcher::new();
        let mut bytes = Vec::new();
        let halt = || self.halted() || stopped();
        while let Some((seq, name, size)) = self.next_job() {
            let held = self.reserve(size);
            let outcome = verify(atoms, &mut *read, &mut matcher, &mut bytes, name, &halt);
            self.release(held);
            if bytes.capacity() > KEPT_BUFFER {
                bytes = Vec::new();
            }
            let mut state = self.state();
            state.finished.insert(seq, outcome);
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

    /// Waits until `size` bytes (at most the whole budget) fit, or the pool
    /// stops; one reader always proceeds alone.
    fn reserve(&self, size: u64) -> u64 {
        let size = size.min(IN_FLIGHT_BYTES);
        let mut budget = self.budget.lock().unwrap_or_else(PoisonError::into_inner);
        while budget.used > 0 && budget.used + size > IN_FLIGHT_BYTES && !self.halted() {
            budget.waiting += 1;
            budget = self
                .freed
                .wait(budget)
                .unwrap_or_else(PoisonError::into_inner);
            budget.waiting -= 1;
        }
        budget.used += size;
        size
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
