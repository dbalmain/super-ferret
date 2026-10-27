//! Verifying a content query's Maybe rows: on the calling thread, or on a
//! bounded pool of workers (D76 C).
//!
//! The candidate names arrive as one array in name order. Each position is
//! handled whole by one thread: S1's tests, the index truth, reading and
//! matching the document, and the residual, which leaves a finished row or
//! nothing. Workers claim positions in batches, in order, from an atomic
//! counter, and post each finished position into its batch's slot of a
//! bounded ring before starting the next, so a finished position never
//! waits for a slower one after it in its batch. The calling thread takes
//! the slots in order and emits their rows, so it does a few additions and
//! the callback per row, and sleeps only when the next position is not
//! finished. A worker more than the ring's length of batches ahead of the
//! calling thread waits for it.
//!
//! For inputs that do not change during the run, and without cancellation,
//! the output is the sequential order's at any thread count:
//!
//! - which rows are emitted, their offsets, and their order;
//! - a `Break` from `emit` (a limit, a closed pipe) stops at the same row.
//!   Workers claim no batch and start no read once the calling thread sees it,
//!   reads in progress stop at their next chunk, and positions finished past
//!   that row are not counted anywhere;
//! - the report's counts (`verified`, `changed`, the matching statistics) are
//!   summed by the calling thread, so they count exactly the positions the
//!   sequential order consumed;
//! - a document is read through the first of its names in the array. A later
//!   name of it is settled as the sequential logic settles it: on a worker if
//!   that read has finished and found the catalogued version (a stat, and a
//!   read only if the stat finds another version), else on the calling thread
//!   when it is consumed. The calling thread checks each worker's assumption
//!   about the cache of catalogued answers when it consumes the row, and
//!   settles the row itself when the assumption is wrong, so the cache is in
//!   the same state when it is asked as in the sequential order.
//!
//! The times differ: `verification_time` and `regex_time` sum the workers'
//! busy time, not wall time.
//!
//! Verification sees a document's bytes when it reads them, and a worker
//! reads up to the ring's length of batches ahead of the calling thread. A
//! read may therefore happen before the callbacks for earlier rows run, so
//! a callback that changes a later document (deletes it, rewrites it) may or
//! may not change that document's row, where the sequential order always
//! would. No production caller's callback changes files; one that must see
//! its own effects uses one thread, which keeps the sequential order.
//!
//! Cancellation is checked before each position is consumed. A position
//! that has not passed the check is neither counted nor emitted, and the
//! candidate count rewinds to it; positions consumed before are counted in
//! full. On the calling thread a document cancelled between atoms counts
//! the atoms that finished, as the sequential order did, streamed or read
//! whole, and a read cut short by cancellation is not a change. Where
//! cancellation lands is a matter of timing, so the counts of a cancelled
//! run vary with the thread count.
//!
//! Memory: a worker reserves a document's catalogued size (at most
//! [`IN_FLIGHT_BYTES`]) from a shared budget of [`IN_FLIGHT_BYTES`] before
//! reading it, and frees a buffer larger than [`KEPT_BUFFER`] before
//! returning the reservation. The workers' buffers therefore hold at most
//! `IN_FLIGHT_BYTES + threads × KEPT_BUFFER`, plus whatever the files being
//! read grew beyond their catalogued size (each still under the reader's
//! size cap). The calling thread's buffer is separate: it reads one
//! document at a time, as before, and keeps the capacity of the largest it
//! read. The ring holds finished rows, not documents: a path and a few
//! words per position.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read};
use std::ops::{ControlFlow, Range};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{Scope, ScopedJoinHandle};
use std::time::{Duration, Instant};

use ferret_catalog::{Catalog, InoId, Kind, NameId, Target};
use ferret_index::Certainty;
use ferret_verify::{MatchStats, Search, TextMatcher};

use crate::expr::{Test, Truth};
use crate::plan::{ReadRequest, ReadVersion, Reader};
use crate::run::NameTester;
use crate::{ContentAtom, DocNames, Query, Row, RunError, Stats};

/// Makes one [`Reader`] per verifying thread. Each reader is used by one
/// thread only; the factory itself is shared.
pub type Fork<'a> = dyn Fn() -> Box<Reader<'a>> + Sync + 'a;

/// Document bytes the workers may hold at once, by catalogued size.
pub const IN_FLIGHT_BYTES: u64 = 64 << 20;

/// A worker keeps a read buffer up to this capacity between documents.
pub const KEPT_BUFFER: usize = 1 << 20;

/// Batches each worker gets on average: enough that one slow batch (a large
/// document) leaves the others work to balance with, few enough that the
/// calling thread wakes a few hundred times per run, not per document.
const BATCHES_PER_THREAD: usize = 16;

/// The most positions in a batch.
const MAX_BATCH: usize = 256;

/// Finished batches per worker that the ring holds ahead of the calling
/// thread. Enough that workers rarely wait on a slow batch at the head:
/// at 4, workers at 16 threads spent about 30 ms each waiting for room in a
/// 160 ms scan; at 16, almost none (docs/SCAN.md, step 2).
const RING_PER_THREAD: usize = 16;

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
    /// Reads that failed or found a new version; not a read a stop ended
    /// ([`unread`]).
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
    /// Each document's live names.
    pub names: &'t DocNames,
    /// The probed documents, ascending.
    pub docs: &'t [u32],
    /// `table[j * width + i]` is atom `i` on `docs[j]`.
    pub table: &'t [Option<Certainty>],
    pub width: usize,
}

impl Query {
    /// Tests the names `ids` against S1, settles each Maybe row by reading
    /// its document, and emits the rows that hold, in the order of `ids`.
    pub(crate) fn verify_rows(
        &self,
        catalog: &Catalog,
        probed: &Probed<'_>,
        readers: Readers<'_, '_>,
        cancelled: Option<&AtomicBool>,
        emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<(Stats, Tally), RunError> {
        NameTester::load(catalog)?;
        let scan = Scan::new(self, catalog, probed, cancelled);
        match readers {
            Readers::One(read) => scan.alone(read, emit),
            Readers::Pool(Workers { fork, threads }) if threads <= 1 => {
                scan.alone(&mut *fork(), emit)
            }
            Readers::Pool(Workers { fork, threads }) => {
                let stopped = || scan.stopped();
                let pool = Pool::new(scan.ids.len(), threads);
                std::thread::scope(|scope| {
                    // Stops the workers however this returns, a failed
                    // spawn or a panic included, so the scope can join
                    // them. Installed before the first spawn: a worker
                    // already started waits for room until it is stopped.
                    let _stop = StopOnDrop(&pool);
                    // A thread that cannot be created leaves the run with
                    // the workers that were; the output does not depend on
                    // how many there are.
                    let started = (0..threads)
                        .take_while(|&i| {
                            spawn(scope, i, || pool.work(&scan, fork, &stopped)).is_ok()
                        })
                        .count();
                    let mut read = fork();
                    if started == 0 {
                        return scan.alone(&mut *read, emit);
                    }
                    scan.pooled(&pool, started, &mut *read, emit)
                })
            }
        }
    }
}

// ── the scan: what every thread shares ──

/// The run's names and what each position needs, shared by every thread.
struct Scan<'q, 'c, 'p> {
    query: &'q Query,
    catalog: &'c Catalog,
    ids: &'p [NameId],
    names: &'p DocNames,
    docs: &'p [u32],
    table: &'p [Option<Certainty>],
    width: usize,
    cancelled: Option<&'q AtomicBool>,
    /// Who reads each document, when `ids` is not ascending: `first[doc]`
    /// is the first position of a name of `doc`, and `repeated` has a bit
    /// for each document with more. Ascending, a thread asks
    /// [`Scan::names`] instead, which costs the calling thread nothing.
    first: Option<(Vec<u32>, Vec<u64>)>,
    /// What each repeated document's reader found, for its later names:
    /// the answers if the read was of the catalogued version, else `None`.
    /// Sharded by document, so threads rarely meet on a lock.
    found: Vec<Mutex<Found>>,
    /// Ascending, for a document with many names before a position's: its
    /// first name in the run, found once ([`Scan::owns`]). Sharded as
    /// `found` is.
    owners: Vec<Mutex<HashMap<u32, NameId>>>,
    /// Membership checks [`Scan::owns`] made, for a test.
    #[cfg(test)]
    owner_checks: Option<std::sync::Arc<AtomicU64>>,
}

/// Names before a position's that [`Scan::owns`] checks one by one; past
/// this, it finds the document's first name in the run once and keeps it.
const OWNER_SCAN: usize = 16;

/// One shard of [`Scan::found`]: a document's answers if its read found
/// the catalogued version, else `None`.
type Found = HashMap<u32, Option<Vec<Option<usize>>>>;

/// Shards of [`Scan::found`].
const FOUND_SHARDS: usize = 64;

impl<'q, 'c, 'p> Scan<'q, 'c, 'p> {
    fn new(
        query: &'q Query,
        catalog: &'c Catalog,
        probed: &Probed<'p>,
        cancelled: Option<&'q AtomicBool>,
    ) -> Self {
        let first = (!probed.ids.is_sorted()).then(|| {
            let docs = catalog.next_doc().0 as usize;
            let mut first = vec![u32::MAX; docs];
            let mut repeated = vec![0; docs.div_ceil(64)];
            for (at, &id) in probed.ids.iter().enumerate() {
                let Some(doc) = doc_of(catalog, id) else {
                    continue;
                };
                match first.get_mut(doc as usize) {
                    Some(slot) if *slot == u32::MAX => *slot = at as u32,
                    Some(_) => set(&mut repeated, doc),
                    None => {}
                }
            }
            (first, repeated)
        });
        Self {
            query,
            catalog,
            ids: probed.ids,
            names: probed.names,
            docs: probed.docs,
            table: probed.table,
            width: probed.width,
            cancelled,
            first,
            found: (0..FOUND_SHARDS).map(|_| Mutex::default()).collect(),
            owners: (0..FOUND_SHARDS).map(|_| Mutex::default()).collect(),
            #[cfg(test)]
            owner_checks: seam::Seams::current().owner_checks,
        }
    }

    /// Whether position `at` holds the first name of `doc` in the run: the
    /// one that reads it.
    fn owns(&self, at: usize, doc: u32) -> bool {
        if let Some((first, _)) = &self.first {
            return first.get(doc as usize) == Some(&(at as u32));
        }
        // Ascending: an earlier position holds a smaller name. Usually the
        // name just before is in the run, or there are few; a name filter
        // can leave many out, and checking them again for each later name
        // would be quadratic in the document's names.
        let id = self.ids[at];
        let names = self.names.names(doc);
        let before = names.partition_point(|&name| name < id);
        let in_run = |name: &NameId| {
            #[cfg(test)]
            if let Some(checks) = &self.owner_checks {
                checks.fetch_add(1, Ordering::Relaxed);
            }
            self.ids.binary_search(name).is_ok()
        };
        if before <= OWNER_SCAN {
            return !names[..before].iter().any(in_run);
        }
        let shard = &self.owners[doc as usize % FOUND_SHARDS];
        if let Some(&first) = lock(shard).get(&doc) {
            return first == id;
        }
        // `id` itself is in the run, so the search ends by it.
        let first = names[..=before].iter().copied().find(in_run).unwrap_or(id);
        lock(shard).insert(doc, first);
        first == id
    }

    /// Whether `doc` may be asked for again in this run: only then are its
    /// answers worth keeping.
    fn is_repeated(&self, doc: u32) -> bool {
        match &self.first {
            Some((_, repeated)) => repeated
                .get(doc as usize / 64)
                .is_some_and(|word| word & (1 << (doc % 64)) != 0),
            None => self.names.names(doc).len() > 1,
        }
    }

    fn shard(&self, doc: u32) -> &Mutex<Found> {
        &self.found[doc as usize % FOUND_SHARDS]
    }

    fn stopped(&self) -> bool {
        self.cancelled
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }

    /// Every position on the calling thread, each consumed as soon as it is
    /// handled: the sequential order.
    fn alone(
        &self,
        read: &mut Reader<'_>,
        emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<(Stats, Tally), RunError> {
        let mut ledger = Ledger::new(self, read, 1, emit);
        let mut names = NameTester::new(self.query, self.catalog, self.cancelled);
        let stopped = || self.stopped();
        let mut batch = Batch::<Item>::default();
        for at in 0..self.ids.len() {
            if stopped() {
                return Ok(ledger.halted(at));
            }
            batch.clear();
            let item = self.item(
                at,
                &mut names,
                &mut ledger.verifier.hand,
                None,
                &stopped,
                &mut batch.paths,
            );
            if ledger.consume(item, &batch.paths)?.is_break() {
                return Ok(ledger.halted(at));
            }
        }
        Ok(ledger.finished())
    }

    /// The calling thread's side of a pool: takes the batches in order and
    /// consumes their positions.
    fn pooled(
        &self,
        pool: &Pool,
        threads: usize,
        read: &mut Reader<'_>,
        emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<(Stats, Tally), RunError> {
        let mut ledger = Ledger::new(self, read, threads, emit);
        let result = self.drain(pool, &mut ledger);
        // Stop the workers before the run's state is dropped, which scales
        // with the documents verified: a worker that found room meanwhile
        // would start another read.
        pool.halt();
        #[cfg(test)]
        pool.seams.tearing_down(&|| pool.finished());
        result?;
        Ok(ledger.finished())
    }

    /// Consumes every position in order, until one stops the run.
    fn drain<E: FnMut(&Row<'_>) -> ControlFlow<()>>(
        &self,
        pool: &Pool,
        ledger: &mut Ledger<'_, 'q, 'c, 'p, '_, '_, E>,
    ) -> Result<(), RunError> {
        let mut part = Batch::default();
        let mut items = Vec::new();
        for index in 0..pool.batches {
            loop {
                part.clear();
                let Some(complete) = pool.take(index, &mut part) else {
                    // A worker panicked; the scope re-raises it.
                    return Ok(());
                };
                std::mem::swap(&mut items, &mut part.items);
                for (offset, item) in items.drain(..).enumerate() {
                    let at = part.start + offset;
                    // The accounting boundary (module doc): a cancelled run
                    // consumes no further position.
                    if self.stopped() || ledger.consume(item, &part.paths)?.is_break() {
                        ledger.halt(pool, at);
                        return Ok(());
                    }
                }
                std::mem::swap(&mut items, &mut part.items);
                if complete {
                    break;
                }
            }
        }
        Ok(())
    }

    /// Handles position `at` whole: S1's tests, the index truth, the read
    /// if this thread owns it, and the residual. A row's path goes into
    /// `paths`.
    fn item(
        &self,
        at: usize,
        names: &mut NameTester<'q, 'c>,
        hand: &mut Hand<'_, '_>,
        pool: Option<&Pool>,
        stop: &dyn Fn() -> bool,
        paths: &mut Vec<u8>,
    ) -> Item {
        let mut item = Item::Skip;
        let flow = names.test(self.ids[at], &mut |row| {
            item = self.admit(at, row, hand, pool, stop, paths);
            ControlFlow::Continue(())
        });
        match flow {
            Err(error) => Item::Failed(error),
            Ok(ControlFlow::Break(())) => Item::Cancelled,
            Ok(ControlFlow::Continue(())) => item,
        }
    }

    /// A row S1 passed: whether its index truth keeps it, and if its
    /// document is this position's to read, the read and the residual.
    fn admit(
        &self,
        at: usize,
        row: &Row<'_>,
        hand: &mut Hand<'_, '_>,
        pool: Option<&Pool>,
        stop: &dyn Fn() -> bool,
        paths: &mut Vec<u8>,
    ) -> Item {
        let (query, catalog) = (self.query, self.catalog);
        let doc = catalog.doc(row.inode).map(|d| d.0);
        let probed = doc.and_then(|d| self.docs.binary_search(&d).ok());
        let indexed = |i: usize| match probed.and_then(|j| self.table[j * self.width + i]) {
            None => Truth::No,
            Some(Certainty::Maybe) => Truth::Maybe,
            Some(Certainty::Yes) => Truth::Yes,
        };
        let truth = query.eval_residual(&mut |test| match test {
            Test::Content(i) => indexed(*i),
            other => query.test_row(catalog, row, other),
        });
        let how = match (truth, doc) {
            (Truth::No, _) | (Truth::Maybe, None) => return Item::Skip,
            (Truth::Yes, _) => How::Holds,
            (Truth::Maybe, Some(doc)) if self.owns(at, doc) => {
                let outcome = self.read(row, hand, pool, stop);
                if pool.is_some() && self.is_repeated(doc) {
                    let found = match &outcome {
                        Outcome::Read {
                            version: ReadVersion::Catalogued,
                            answers,
                            ..
                        } => Some(answers.clone()),
                        _ => None,
                    };
                    lock(self.shard(doc)).insert(doc, found);
                }
                let holds = match &outcome {
                    Outcome::Read { answers, .. } => holds(query, catalog, row, answers),
                    _ => None,
                };
                How::Read {
                    doc,
                    outcome,
                    holds,
                }
            }
            // A later name on a worker, whose reader found the catalogued
            // version: what the calling thread would do on reaching it,
            // done here. A stat; a read only if the path changed.
            (Truth::Maybe, Some(doc)) if pool.is_some() => {
                let found = lock(self.shard(doc)).get(&doc).cloned();
                match found {
                    Some(Some(answers)) => match (hand.read)(row.name, ReadRequest::Stat) {
                        Some(ReadVersion::Catalogued) => How::Cached {
                            doc,
                            holds: holds(query, catalog, row, &answers),
                        },
                        Some(ReadVersion::Current) => {
                            let outcome = self.read(row, hand, pool, stop);
                            let holds = match &outcome {
                                Outcome::Read { answers, .. } => {
                                    holds(query, catalog, row, answers)
                                }
                                _ => None,
                            };
                            How::Again {
                                doc,
                                outcome,
                                holds,
                            }
                        }
                        None => How::Gone(doc),
                    },
                    // Not read yet, or not cacheable: left to the calling
                    // thread.
                    _ => How::Here(doc),
                }
            }
            (Truth::Maybe, Some(doc)) => How::Here(doc),
        };
        let start = paths.len();
        paths.extend_from_slice(row.path);
        Item::Row(Pending {
            path: start..paths.len(),
            name: row.name,
            inode: row.inode,
            kind: row.kind,
            doc,
            how,
        })
    }

    /// Reads and matches `row`'s document on this thread.
    fn read(
        &self,
        row: &Row<'_>,
        hand: &mut Hand<'_, '_>,
        pool: Option<&Pool>,
        stop: &dyn Fn() -> bool,
    ) -> Outcome {
        match pool {
            Some(pool) => pool.read(
                self.query,
                hand,
                row.name,
                self.catalog.size(row.inode),
                stop,
            ),
            None => hand.verify(&self.query.atoms, row.name, stop),
        }
    }
}

/// The live document `id` names, if any.
fn doc_of(catalog: &Catalog, id: NameId) -> Option<u32> {
    match catalog.name(id).target() {
        Target::Inode(inode) if catalog.is_live_inode(inode) => catalog.doc(inode).map(|d| d.0),
        _ => None,
    }
}

fn set(bits: &mut [u64], at: u32) {
    if let Some(word) = bits.get_mut(at as usize / 64) {
        *word |= 1 << (at % 64);
    }
}

/// Whether a row holds given its document's answers, and if so its offset:
/// the first match of a `grep:` atom.
fn holds(
    query: &Query,
    catalog: &Catalog,
    row: &Row<'_>,
    answers: &[Option<usize>],
) -> Option<Option<usize>> {
    let offset = answers
        .iter()
        .zip(&query.atoms)
        .filter_map(|(answer, atom)| match atom {
            ContentAtom::Grep(_) => *answer,
            ContentAtom::Text(_) => None,
        })
        .min();
    let truth = query.eval_residual(&mut |test| match test {
        Test::Content(i) => answers[*i].is_some().into(),
        other => query.test_row(catalog, row, other),
    });
    (truth == Truth::Yes).then_some(offset)
}

// ── a position's result ──

/// What one position left for the calling thread.
enum Item {
    /// No row: S1 or the index truth rejected the name.
    Skip,
    /// S1 saw the query cancelled.
    Cancelled,
    Failed(RunError),
    Row(Pending),
}

trait Placed {
    fn place(&mut self, path: Range<usize>);
}

impl Placed for Item {
    fn place(&mut self, path: Range<usize>) {
        if let Self::Row(pending) = self {
            pending.path = path;
        }
    }
}

/// A row S1 passed whose index truth is Yes or Maybe.
struct Pending {
    /// Its path, in its batch's `paths`.
    path: Range<usize>,
    name: NameId,
    inode: InoId,
    kind: Kind,
    /// Its document, if it has one.
    doc: Option<u32>,
    how: How,
}

enum How {
    /// The index says it holds; no read.
    Holds,
    /// Read by the first of its document's names: the outcome, and the
    /// row's offset if it holds.
    Read {
        doc: u32,
        outcome: Outcome,
        holds: Option<Option<usize>>,
    },
    /// A later name of a document whose reader found the catalogued
    /// version, and whose own stat says it is still that version: the
    /// cached answers hold, or not.
    Cached {
        doc: u32,
        holds: Option<Option<usize>>,
    },
    /// A later name whose stat failed.
    Gone(u32),
    /// A later name whose stat found another version, read for itself.
    Again {
        doc: u32,
        outcome: Outcome,
        holds: Option<Option<usize>>,
    },
    /// A later name of a document: settled by the calling thread when
    /// consumed.
    Here(u32),
}

/// Consecutive positions, finished.
struct Batch<T = Item> {
    /// The first position.
    start: usize,
    items: Vec<T>,
    /// The rows' paths.
    paths: Vec<u8>,
}

impl<T> Default for Batch<T> {
    fn default() -> Self {
        Self {
            start: 0,
            items: Vec::new(),
            paths: Vec::new(),
        }
    }
}

impl<T: Placed> Batch<T> {
    fn clear(&mut self) {
        self.items.clear();
        self.paths.clear();
    }

    /// Appends `item`, whose path is all of `paths`.
    fn push(&mut self, mut item: T, paths: &[u8]) {
        let start = self.paths.len();
        item.place(start..start + paths.len());
        self.paths.extend_from_slice(paths);
        self.items.push(item);
    }
}

/// A slot of the ring: one batch's finished positions not yet taken.
struct Slot<T = Item> {
    /// The batch, plus one (0: none).
    index: usize,
    part: Batch<T>,
    /// Every position of the batch has been posted.
    complete: bool,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self {
            index: 0,
            part: Batch::default(),
            complete: false,
        }
    }
}

// ── the calling thread's side ──

/// Consumes positions in order: counts them, settles later names of a
/// document, and emits.
struct Ledger<'s, 'q, 'c, 'p, 'r, 'a, E> {
    scan: &'s Scan<'q, 'c, 'p>,
    verifier: Verifier<'q, 'r, 'a>,
    rows: u64,
    emit: E,
    /// Documents of rows consumed, by their index truth: Maybe, and not No.
    maybe_docs: Vec<u64>,
    possible_docs: Vec<u64>,
    /// The candidate count at the position that stopped the run, if one did.
    halted_at: Option<u64>,
}

impl<'s, 'q, 'c, 'p, 'r, 'a, E: FnMut(&Row<'_>) -> ControlFlow<()>>
    Ledger<'s, 'q, 'c, 'p, 'r, 'a, E>
{
    fn new(scan: &'s Scan<'q, 'c, 'p>, read: &'r mut Reader<'a>, threads: usize, emit: E) -> Self {
        let words = (scan.catalog.next_doc().0 as usize).div_ceil(64);
        Self {
            scan,
            verifier: Verifier::new(scan.query, read, threads),
            rows: 0,
            emit,
            maybe_docs: vec![0; words],
            possible_docs: vec![0; words],
            halted_at: None,
        }
    }

    /// Stops the run at position `at`, which is not consumed.
    fn halted(mut self, at: usize) -> (Stats, Tally) {
        self.halted_at = Some(at as u64 + 1);
        self.finished()
    }

    /// [`Ledger::halted`] on a pool, stopping its workers now.
    fn halt(&mut self, pool: &Pool, at: usize) {
        self.halted_at = Some(at as u64 + 1);
        pool.halt();
    }

    fn finished(self) -> (Stats, Tally) {
        let stats = Stats {
            candidates: self.halted_at.unwrap_or(self.scan.ids.len() as u64),
            rows: self.rows,
            ..Stats::default()
        };
        let count = |bits: &[u64]| bits.iter().map(|w| u64::from(w.count_ones())).sum();
        let mut tally = self.verifier.tally;
        tally.maybe_docs = count(&self.maybe_docs);
        tally.possible_docs = count(&self.possible_docs);
        (stats, tally)
    }

    /// Counts a consumed row's document, by its index truth.
    fn consumed(&mut self, doc: Option<u32>, maybe: bool) {
        if let Some(doc) = doc {
            set(&mut self.possible_docs, doc);
            if maybe {
                set(&mut self.maybe_docs, doc);
            }
        }
    }

    /// Consumes one position; `Break` stops the run there.
    fn consume(&mut self, item: Item, paths: &[u8]) -> Result<ControlFlow<()>, RunError> {
        let pending = match item {
            Item::Skip => return Ok(ControlFlow::Continue(())),
            Item::Cancelled => return Ok(ControlFlow::Break(())),
            Item::Failed(error) => return Err(error),
            Item::Row(pending) => pending,
        };
        let mut row = Row {
            path: &paths[pending.path],
            name: pending.name,
            inode: pending.inode,
            kind: pending.kind,
            offset: None,
        };
        let scan = self.scan;
        let doc = pending.doc;
        let known = |verifier: &Verifier<'_, '_, '_>, doc: u32| verifier.exact.contains_key(&doc);
        let settled = match pending.how {
            How::Holds => {
                self.consumed(doc, false);
                Ok(Some(None))
            }
            // A worker took the cache's state for granted: a first read
            // found no answers cached, a later name found them. Both hold
            // whenever the run is as the sequential order saw it; when not,
            // this thread settles the row as that order would.
            How::Read {
                doc: read,
                outcome,
                holds,
            } if !known(&self.verifier, read) => {
                self.consumed(doc, true);
                let keep = scan.is_repeated(read);
                match self.verifier.settle(read, outcome, keep) {
                    Settled::Unread => return Ok(ControlFlow::Continue(())),
                    Settled::Stopped => return Ok(ControlFlow::Break(())),
                    Settled::Cached | Settled::Fresh(_) => Ok(holds),
                }
            }
            How::Cached { doc: read, holds } if known(&self.verifier, read) => {
                self.consumed(doc, true);
                Ok(holds)
            }
            How::Gone(read) if known(&self.verifier, read) => {
                self.consumed(doc, true);
                self.verifier.tally.changed += 1;
                return Ok(ControlFlow::Continue(()));
            }
            How::Again {
                doc: read,
                outcome,
                holds,
            } if known(&self.verifier, read) => {
                self.consumed(doc, true);
                match self.verifier.settle(read, outcome, true) {
                    Settled::Unread => return Ok(ControlFlow::Continue(())),
                    Settled::Stopped => return Ok(ControlFlow::Break(())),
                    Settled::Cached => Err(read),
                    Settled::Fresh(_) => Ok(holds),
                }
            }
            How::Read { doc: read, .. }
            | How::Cached { doc: read, .. }
            | How::Gone(read)
            | How::Again { doc: read, .. }
            | How::Here(read) => {
                self.consumed(doc, true);
                let stopped = || scan.stopped();
                match self.verifier.here(read, pending.name, &stopped) {
                    Settled::Unread => return Ok(ControlFlow::Continue(())),
                    Settled::Stopped => return Ok(ControlFlow::Break(())),
                    Settled::Cached => Err(read),
                    Settled::Fresh(answers) => Ok(holds(scan.query, scan.catalog, &row, &answers)),
                }
            }
        };
        // `Err`: the row holds by the run's cached answers for the document.
        let offset = match settled {
            Ok(holds) => holds,
            Err(read) => holds(scan.query, scan.catalog, &row, &self.verifier.exact[&read]),
        };
        let Some(offset) = offset else {
            return Ok(ControlFlow::Continue(()));
        };
        // The emission guard: a stop that came while this position was
        // settled (a host's checks after a successful read, a cached
        // answer's stat) ends the run here, as one before it would have.
        if scan.stopped() {
            return Ok(ControlFlow::Break(()));
        }
        row.offset = offset;
        self.rows += 1;
        Ok((self.emit)(&row))
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

/// A thread's reading tools: its reader, matcher and buffer.
struct Hand<'r, 'a> {
    read: &'r mut Reader<'a>,
    matcher: TextMatcher,
    /// A whole document, for atoms that need one.
    bytes: Vec<u8>,
    /// A run of lines at a time, for line-local patterns ([`stream`]).
    lines: Vec<u8>,
    /// This thread's own copy of each grep atom's search, by atom: a regex
    /// shared between threads hands each a match cache from a pool, which
    /// many threads at once contend on.
    regexes: Vec<Option<Search<'static>>>,
}

impl<'r, 'a> Hand<'r, 'a> {
    fn new(read: &'r mut Reader<'a>, atoms: &[ContentAtom]) -> Self {
        let regexes = atoms
            .iter()
            .map(|atom| match atom {
                ContentAtom::Grep(grep) => Some(grep.search()),
                ContentAtom::Text(_) => None,
            })
            .collect();
        Self {
            read,
            matcher: TextMatcher::new(),
            bytes: Vec::new(),
            lines: Vec::new(),
            regexes,
        }
    }

    /// Reads `name`'s document and runs every atom over it, unless `stopped`
    /// says to stop first; the reader is given `stopped` too.
    fn verify(
        &mut self,
        atoms: &[ContentAtom],
        name: NameId,
        stopped: &dyn Fn() -> bool,
    ) -> Outcome {
        if streams(atoms) {
            return stream(
                &self.regexes,
                &mut *self.read,
                &mut self.lines,
                name,
                stopped,
            );
        }
        verify(
            atoms,
            &self.regexes,
            &mut *self.read,
            &mut self.matcher,
            &mut self.bytes,
            name,
            stopped,
        )
    }
}

/// The calling thread's side: the cache of catalogued answers, the counts,
/// and its reading tools, for later names of a document.
struct Verifier<'q, 'r, 'a> {
    atoms: &'q [ContentAtom],
    hand: Hand<'r, 'a>,
    /// Only content facts established by a bracketed catalog-version read.
    /// Path failures never enter this map; every path has its own stat.
    exact: HashMap<u32, Vec<Option<usize>>>,
    tally: Tally,
}

impl<'q, 'r, 'a> Verifier<'q, 'r, 'a> {
    fn new(query: &'q Query, read: &'r mut Reader<'a>, threads: usize) -> Self {
        Self {
            atoms: &query.atoms,
            hand: Hand::new(read, &query.atoms),
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
            match (self.hand.read)(name, ReadRequest::Stat) {
                Some(ReadVersion::Catalogued) => return Settled::Cached,
                Some(ReadVersion::Current) => {}
                None => {
                    self.tally.changed += 1;
                    return Settled::Unread;
                }
            }
        }
        let outcome = self.hand.verify(self.atoms, name, stopped);
        self.settle(doc, outcome, true)
    }

    /// Counts a read, wherever it ran, and caches catalogued answers if
    /// `keep`: only a document with another name in the run is asked again.
    fn settle(&mut self, doc: u32, outcome: Outcome, keep: bool) -> Settled {
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
                        if keep {
                            self.exact.entry(doc).or_insert(answers);
                        }
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
    regexes: &[Option<Search<'_>>],
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
        return unread(start, stopped);
    };
    let before = matcher.stats();
    let mut work = Matched::default();
    let mut answers = Vec::with_capacity(atoms.len());
    let mut halted = false;
    for (atom, regex) in atoms.iter().zip(regexes) {
        if stopped() {
            halted = true;
            break;
        }
        let engine_start = Instant::now();
        let answer = match regex {
            Some(search) => search.find(bytes),
            None => atom.find(matcher, bytes),
        };
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

/// A read that failed: stopped, if the run was, which is not a change.
fn unread(start: Instant, stopped: &dyn Fn() -> bool) -> Outcome {
    match stopped() {
        true => Outcome::Stopped(Matched {
            time: start.elapsed(),
            ..Matched::default()
        }),
        false => Outcome::Unread(start.elapsed()),
    }
}

/// Whether every atom is a pattern [`stream`] can search a run of lines at
/// a time: then documents are streamed, not read whole.
fn streams(atoms: &[ContentAtom]) -> bool {
    !atoms.is_empty()
        && atoms
            .iter()
            .all(|atom| matches!(atom, ContentAtom::Grep(grep) if grep.is_line_local()))
}

/// [`verify`] for line-local patterns ([`ferret_verify::Grep::is_line_local`]):
/// the document streams through `buffer`, and each read is searched up to
/// its last `\n`, not including it, with the partial line after it carried
/// to the next. A line longer than the buffer grows it, by doubling, up to
/// the rest of the document's declared length: a document's buffer is never
/// larger than 64 KiB or its length. The run stops early once every atom
/// has matched.
///
/// The answers are the whole-document search's. The first match of a
/// line-local pattern in a document is the first match in the first run of
/// whole lines, cut short of a `\n`, that has one, and its offset is the
/// run's offset plus the match's. The counts are too: each atom counts the
/// document's whole size, matched or not.
///
/// `stopped` is asked before each read and each atom's search, before the
/// answers are accepted, and again once the host's read returns, as
/// [`verify`] asks it between atoms. A stopped document, whatever the host
/// returned, counts the atoms that had finished: those that matched, and
/// those the last run was searched for without a match.
fn stream(
    regexes: &[Option<Search<'_>>],
    read: &mut Reader<'_>,
    buffer: &mut Vec<u8>,
    name: NameId,
    stopped: &dyn Fn() -> bool,
) -> Outcome {
    let start = Instant::now();
    if stopped() {
        return Outcome::Stopped(Matched::default());
    }
    // Sized before the read, in case the host never calls `each`.
    let mut answers = vec![None; regexes.len()];
    // Which atoms' answers are final: matched, or searched to the end.
    let mut done = vec![false; regexes.len()];
    let mut regex = Duration::ZERO;
    let mut size = 0;
    // Set when `stopped` ends the read.
    let mut halted = false;
    let mut each = |source: &mut dyn Read, len: u64| {
        size = len;
        regex = Duration::ZERO;
        answers.clear();
        answers.resize(regexes.len(), None);
        done.clear();
        done.resize(regexes.len(), false);
        let mut open = regexes.len();
        let mut halt = || {
            halted = true;
            Err(io::Error::other("stopped"))
        };
        crate::scan::runs(
            source,
            len,
            buffer,
            stopped,
            None,
            |run, base, terminated| {
                let engine = Instant::now();
                for ((search, answer), done) in regexes.iter().zip(&mut answers).zip(&mut done) {
                    let (false, Some(search)) = (*done, search) else {
                        continue;
                    };
                    if stopped() {
                        regex += engine.elapsed();
                        return halt();
                    }
                    if let Some(found) = search.find(run) {
                        *answer = Some(base + found);
                        open -= 1;
                    }
                    *done = answer.is_some() || !terminated;
                }
                regex += engine.elapsed();
                if (open == 0 || !terminated) && stopped() {
                    return halt();
                }
                Ok(if open == 0 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                })
            },
        )
    };
    let version = read(name, ReadRequest::Stream(&mut each, stopped));
    // The atoms that finished, matched or not: every one after a full read.
    let finished = || {
        let settled = || answers.iter().zip(&done).filter(|(_, done)| **done);
        let counted = settled().count() as u64;
        let mut work = Matched {
            regex,
            ..Matched::default()
        };
        work.matching.regex_documents = counted;
        work.matching.regex_bytes = counted * size;
        work.matching.regex_rejected = settled().filter(|(a, _)| a.is_none()).count() as u64;
        work.time = start.elapsed();
        work
    };
    // A stop after `each` returned, in the host's own checks, is a stop
    // too: the host may have succeeded or given up because of it.
    if halted || stopped() {
        return Outcome::Stopped(finished());
    }
    match version {
        None => Outcome::Unread(start.elapsed()),
        Some(version) => Outcome::Read {
            version,
            work: finished(),
            answers,
        },
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

/// The batch counter, the ring of finished batches, and the byte budget
/// the workers share.
///
/// Every wait here pairs a sequentially consistent store of the condition
/// with a sequentially consistent load of a "someone is waiting" word on
/// the other side, and the notifier takes the waiter's lock before it
/// signals: so either the waiter sees the condition before it sleeps, or
/// the notifier sees the waiter and wakes it. Nobody signals a thread that
/// is not waiting, which keeps wakes, and their syscalls, per batch.
struct Pool<T: Placed = Item> {
    /// Positions in the run.
    positions: usize,
    /// Positions per batch, and the batch count.
    batch: usize,
    batches: usize,
    /// The next batch to claim.
    next: AtomicUsize,
    /// Batches the calling thread has taken.
    head: AtomicUsize,
    /// Each batch's finished positions not yet taken, at
    /// `index % ring.len()`.
    ring: Vec<Mutex<Slot<T>>>,
    /// Positions posted to each slot, ever: the calling thread sleeps until
    /// its slot's count moves.
    posted: Vec<AtomicUsize>,
    /// The batch the calling thread sleeps for, plus one (0: none).
    awaited: AtomicUsize,
    ready: Mutex<()>,
    ready_signal: Condvar,
    /// Workers sleeping for room in the ring.
    roomless: AtomicUsize,
    room: Mutex<()>,
    room_signal: Condvar,
    budget: Budget,
    stop: AtomicBool,
    /// A worker panicked; its batch will never finish.
    lost: AtomicBool,
    /// Positions finished so far, for the test seams.
    #[cfg(test)]
    finished: AtomicU64,
    #[cfg(test)]
    seams: seam::Seams,
}

/// Bytes reserved by workers reading now.
#[derive(Default)]
struct Budget {
    used: AtomicU64,
    /// Workers sleeping for bytes to be freed.
    waiting: AtomicUsize,
    lock: Mutex<()>,
    freed: Condvar,
}

impl<T: Placed> Pool<T> {
    fn schedule(positions: usize, threads: usize) -> Self {
        let batch = (positions / (threads * BATCHES_PER_THREAD)).clamp(1, MAX_BATCH);
        let slots = threads * RING_PER_THREAD;
        Self {
            positions,
            batch,
            batches: positions.div_ceil(batch),
            next: AtomicUsize::new(0),
            head: AtomicUsize::new(0),
            ring: (0..slots).map(|_| Mutex::default()).collect(),
            posted: (0..slots).map(|_| AtomicUsize::new(0)).collect(),
            awaited: AtomicUsize::new(0),
            ready: Mutex::new(()),
            ready_signal: Condvar::new(),
            roomless: AtomicUsize::new(0),
            room: Mutex::new(()),
            room_signal: Condvar::new(),
            budget: Budget::default(),
            stop: AtomicBool::new(false),
            lost: AtomicBool::new(false),
            #[cfg(test)]
            finished: AtomicU64::new(0),
            #[cfg(test)]
            seams: seam::Seams::current(),
        }
    }

    #[cfg(test)]
    fn finished(&self) -> u64 {
        self.finished.load(Ordering::Acquire)
    }

    fn halt(&self) {
        self.stop.store(true, Ordering::SeqCst);
        // Taking each lock orders the store before any sleeper's next check.
        drop(lock(&self.room));
        self.room_signal.notify_all();
        drop(lock(&self.budget.lock));
        self.budget.freed.notify_all();
    }

    fn halted(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Waits until batch `index` has a slot in the ring: the calling thread
    /// has taken the batch that held it. False once the pool stops.
    fn room_for(&self, index: usize) -> bool {
        let fits = || index < self.head.load(Ordering::SeqCst) + self.ring.len();
        if fits() {
            return !self.halted();
        }
        self.roomless.fetch_add(1, Ordering::SeqCst);
        let mut guard = lock(&self.room);
        while !fits() && !self.halted() {
            guard = self
                .room_signal
                .wait(guard)
                .unwrap_or_else(PoisonError::into_inner);
        }
        drop(guard);
        self.roomless.fetch_sub(1, Ordering::SeqCst);
        !self.halted()
    }

    /// Posts position `at` of batch `index`, whose row's path is `paths`;
    /// `last` if it is the batch's last.
    fn post(&self, index: usize, at: usize, item: T, paths: &[u8], last: bool) {
        let slot = index % self.ring.len();
        {
            let mut held = lock(&self.ring[slot]);
            if held.index != index + 1 {
                held.index = index + 1;
                held.part.clear();
            }
            if held.part.items.is_empty() {
                held.part.start = at;
            }
            held.part.push(item, paths);
            held.complete = last;
        }
        #[cfg(test)]
        self.finished.fetch_add(1, Ordering::AcqRel);
        self.posted[slot].fetch_add(1, Ordering::SeqCst);
        if self.awaited.load(Ordering::SeqCst) == index + 1 {
            drop(lock(&self.ready));
            self.ready_signal.notify_one();
        }
        #[cfg(test)]
        self.seams.published(&|| self.finished());
    }

    /// Batch `index`'s next finished positions, into `out` (empty): waits
    /// until there is at least one, or the batch is complete. True once the
    /// batch is complete, which frees its slot; `None` if a worker was lost.
    fn take(&self, index: usize, out: &mut Batch<T>) -> Option<bool> {
        let slot = index % self.ring.len();
        let posted = &self.posted[slot];
        loop {
            let seen = posted.load(Ordering::SeqCst);
            {
                let mut held = lock(&self.ring[slot]);
                if held.index == index + 1 && (!held.part.items.is_empty() || held.complete) {
                    std::mem::swap(&mut held.part, out);
                    let complete = held.complete;
                    if complete {
                        held.index = 0;
                        held.complete = false;
                    }
                    drop(held);
                    if complete {
                        self.head.store(index + 1, Ordering::SeqCst);
                        if self.roomless.load(Ordering::SeqCst) > 0 {
                            drop(lock(&self.room));
                            self.room_signal.notify_all();
                        }
                    }
                    return Some(complete);
                }
            }
            let mut guard = lock(&self.ready);
            self.awaited.store(index + 1, Ordering::SeqCst);
            let moved = || posted.load(Ordering::SeqCst) != seen;
            while !moved() && !self.lost.load(Ordering::SeqCst) {
                guard = self
                    .ready_signal
                    .wait(guard)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            self.awaited.store(0, Ordering::SeqCst);
            drop(guard);
            if !moved() {
                return None;
            }
        }
    }

    /// Waits until `size` bytes (at most the whole budget) fit, and reserves
    /// them; one reader always proceeds alone. `None` once `halt` says the
    /// pool or the query stopped, waiting or not, so no read starts after.
    fn reserve(&self, size: u64, halt: &dyn Fn() -> bool) -> Option<u64> {
        let size = size.min(IN_FLIGHT_BYTES);
        let budget = &self.budget;
        let fits = |used: u64| used == 0 || used + size <= IN_FLIGHT_BYTES;
        loop {
            // A pool stop wakes this wait. A cancelled query does not, but
            // a reader holding bytes stops at its next chunk and frees
            // them, which does.
            if halt() {
                return None;
            }
            let used = budget.used.load(Ordering::SeqCst);
            if fits(used) {
                if budget
                    .used
                    .compare_exchange(used, used + size, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    return Some(size);
                }
                continue;
            }
            let guard = lock(&budget.lock);
            budget.waiting.fetch_add(1, Ordering::SeqCst);
            if !halt() && !fits(budget.used.load(Ordering::SeqCst)) {
                #[cfg(test)]
                self.seams.waiting_for_bytes();
                drop(
                    budget
                        .freed
                        .wait(guard)
                        .unwrap_or_else(PoisonError::into_inner),
                );
            }
            budget.waiting.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn release(&self, size: u64) {
        let budget = &self.budget;
        budget.used.fetch_sub(size, Ordering::SeqCst);
        if budget.waiting.load(Ordering::SeqCst) > 0 {
            drop(lock(&budget.lock));
            budget.freed.notify_all();
        }
    }
}

impl Pool {
    fn new(positions: usize, threads: usize) -> Self {
        Self::schedule(positions, threads)
    }

    /// One worker: claim batches in order and post each position as it
    /// finishes, until none is left or the pool stops.
    fn work(&self, scan: &Scan<'_, '_, '_>, fork: &Fork<'_>, stopped: &(dyn Fn() -> bool + Sync)) {
        let _lost = LostOnPanic(self);
        let mut read = fork();
        let mut hand = Hand::new(&mut *read, &scan.query.atoms);
        let mut names = NameTester::new(scan.query, scan.catalog, scan.cancelled);
        let halt = || self.halted() || stopped();
        let mut paths = Vec::new();
        loop {
            if self.halted() {
                return;
            }
            let index = self.next.fetch_add(1, Ordering::Relaxed);
            if index >= self.batches || !self.room_for(index) {
                return;
            }
            let start = index * self.batch;
            let end = (start + self.batch).min(self.positions);
            for at in start..end {
                paths.clear();
                let item = scan.item(at, &mut names, &mut hand, Some(self), &halt, &mut paths);
                self.post(index, at, item, &paths, at + 1 == end);
            }
        }
    }

    /// Reads and matches one document on a worker, within the byte budget.
    fn read(
        &self,
        query: &Query,
        hand: &mut Hand<'_, '_>,
        name: NameId,
        size: u64,
        halt: &dyn Fn() -> bool,
    ) -> Outcome {
        match self.reserve(size, halt) {
            Some(size) => {
                let outcome = hand.verify(&query.atoms, name, halt);
                if hand.bytes.capacity() > KEPT_BUFFER {
                    hand.bytes = Vec::new();
                }
                if hand.lines.capacity() > KEPT_BUFFER {
                    hand.lines = Vec::new();
                }
                // Freed only after an oversized buffer is: a reader that the
                // release wakes cannot allocate against bytes still held.
                self.release(size);
                outcome
            }
            None => Outcome::Stopped(Matched::default()),
        }
    }
}

/// A pool lock; a panicking worker holds none across user code, so what it
/// guards is consistent.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Starts worker `i` on the scope.
fn spawn<'scope>(
    scope: &'scope Scope<'scope, '_>,
    i: usize,
    work: impl FnOnce() + Send + 'scope,
) -> std::io::Result<()> {
    spawn_named(scope, i, "ferret-verify", work).map(drop)
}

/// Starts thread `i` on the scope, named so profiles can tell it apart. The
/// process's first spawn reserves the descriptor table its workers share.
pub(crate) fn spawn_named<'scope, R: Send + 'scope>(
    scope: &'scope Scope<'scope, '_>,
    i: usize,
    name: &str,
    work: impl FnOnce() -> R + Send + 'scope,
) -> std::io::Result<ScopedJoinHandle<'scope, R>> {
    #[cfg(test)]
    if seam::Seams::current().fail_spawn_at == Some(i) {
        return Err(std::io::Error::other("injected spawn failure"));
    }
    #[cfg(not(test))]
    let _ = i;
    #[cfg(debug_assertions)]
    if refused_for_tests(name) {
        return Err(rustix::io::Errno::AGAIN.into());
    }
    reserve_descriptor_table();
    std::thread::Builder::new()
        .name(name.into())
        .spawn_scoped(scope, work)
}

/// Whether `FERRET_TEST_REFUSE_THREADS=NAME=K` refuses this spawn, as
/// `pthread_create` does under `RLIMIT_NPROC`: the first `K` threads named
/// `NAME` start, and later ones are refused. Debug builds only, so the CLI's
/// tests can refuse one kind of thread at a time.
#[cfg(debug_assertions)]
fn refused_for_tests(name: &str) -> bool {
    static LIMIT: std::sync::OnceLock<Option<(String, usize)>> = std::sync::OnceLock::new();
    static STARTED: AtomicUsize = AtomicUsize::new(0);
    let limit = LIMIT.get_or_init(|| {
        let value = std::env::var("FERRET_TEST_REFUSE_THREADS").ok()?;
        let (name, count) = value.rsplit_once('=')?;
        Some((name.to_owned(), count.parse().ok()?))
    });
    limit.as_ref().is_some_and(|(refused, count)| {
        refused == name && STARTED.fetch_add(1, Ordering::Relaxed) >= *count
    })
}

/// Search threads the system refused to start: the run went on with
/// `running` of the `asked` threads.
#[derive(Debug)]
pub struct Shortfall {
    /// Threads searching, the calling thread included when it searches.
    pub running: usize,
    /// Threads the run asked for.
    pub asked: usize,
    /// The first refusal.
    pub cause: std::io::Error,
}

/// Grows the process's descriptor table to 1024 slots, once, before the first
/// worker shares it. Growing a table that several threads share waits out an
/// RCU grace period while every other opener waits on the resize, and a deep
/// tree's held directory chains (D21) reach a few hundred descriptors, so the
/// table used to grow several times mid-search: 46–49 ms of a 520 ms index
/// grep at 32 threads on a 300k-file tree (kdiag, 2026-10-20). Growing it is
/// cheap only while the process has one thread: a parallel `--files`, which
/// spawned its writer's thread first, waited 20 ms here. A host calls this
/// before its first thread; every worker spawn calls it too. Best effort:
/// under a low open-file limit, or with no descriptor 2 to duplicate, the
/// table grows as it did before.
pub fn reserve_descriptor_table() {
    static RESERVED: std::sync::Once = std::sync::Once::new();
    RESERVED.call_once(grow_descriptor_table);
}

fn grow_descriptor_table() {
    use std::os::fd::AsFd;
    let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile)
        .current
        .unwrap_or(u64::MAX);
    let Ok(top) = i32::try_from(limit.saturating_sub(1).min(1023)) else {
        return;
    };
    // Dropped at once: the table keeps its size after the slot is freed.
    let _ = rustix::io::fcntl_dupfd_cloexec(std::io::stderr().as_fd(), top);
}

/// Stops the pool when the calling thread leaves the scope, by any path.
struct StopOnDrop<'p, T: Placed = Item>(&'p Pool<T>);

impl<T: Placed> Drop for StopOnDrop<'_, T> {
    fn drop(&mut self) {
        self.0.halt();
    }
}

/// Tells the calling thread a worker panicked, so it stops waiting.
struct LostOnPanic<'p, T: Placed = Item>(&'p Pool<T>);

impl<T: Placed> Drop for LostOnPanic<'_, T> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.lost.store(true, Ordering::SeqCst);
            drop(lock(&self.0.ready));
            self.0.ready_signal.notify_all();
        }
    }
}

// A panicking file worker may still own a byte reservation. Stop and wake
// its peers before the scope joins, rather than waiting on those leaked bytes.
struct StopOnPanic<'p>(&'p Pool<()>);

impl Drop for StopOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.halt();
        }
    }
}

/// Tasks the [`completion_groups`] producer may run ahead of the workers, per
/// worker. A few per worker keep the queue from emptying between the
/// producer's wakes; the backlog is paths, not captures (D93 bounds those).
const FEED_PER_THREAD: usize = 4;

/// At most this many consecutive indexed paths, and this many input bytes,
/// belong to a worker's run. A larger file travels alone.
const RUN_TASKS: usize = 16;
const RUN_BYTES: u64 = 64 << 10;

/// Tasks produced on a separate thread. Indexed paths are taken in runs for
/// checked-directory reuse; their pending tails remain accessible even while
/// the owner is blocked in a read. Streaming producers keep immediate wakes:
/// they may block after producing a single task whose execution releases them.
struct Feed<Task> {
    state: Mutex<FeedState<Task>>,
    runs: Vec<Mutex<VecDeque<(u64, Task)>>>,
    ready: Condvar,
    room: Condvar,
    capacity: usize,
    indexed: bool,
}

struct FeedState<Task> {
    tasks: VecDeque<(u64, Task)>,
    /// No more tasks will arrive; pending runs can still be stolen.
    closed: bool,
    sleeping: usize,
    /// The producer is asleep on `room` and has not been woken.
    full: bool,
}

impl<Task> Feed<Task> {
    fn new(threads: usize, indexed: bool) -> Self {
        // D107's wake threshold must fit even with only two workers.
        let capacity = (threads * FEED_PER_THREAD).max(if indexed { RUN_TASKS } else { 1 });
        Self {
            state: Mutex::new(FeedState {
                tasks: VecDeque::with_capacity(capacity),
                closed: false,
                sleeping: 0,
                full: false,
            }),
            runs: (0..threads).map(|_| Mutex::default()).collect(),
            ready: Condvar::new(),
            room: Condvar::new(),
            capacity,
            indexed,
        }
    }

    /// Runs `next` without holding a feed or run lock.
    fn produce(
        &self,
        next: &mut impl FnMut(&dyn Fn() -> bool) -> Option<(u64, Task)>,
        stopped: &dyn Fn() -> bool,
    ) {
        loop {
            {
                let mut state = lock(&self.state);
                while state.tasks.len() >= self.capacity && !state.closed {
                    state.full = true;
                    state = wait(&self.room, state);
                }
                state.full = false;
                if state.closed {
                    return;
                }
            }
            if stopped() {
                return;
            }
            let Some(task) = next(stopped) else { return };
            let mut state = lock(&self.state);
            if state.closed {
                return;
            }
            state.tasks.push_back(task);
            let wake = if self.indexed {
                state.sleeping > 0 && state.tasks.len().is_multiple_of(RUN_TASKS)
            } else {
                state.sleeping >= state.tasks.len()
            };
            drop(state);
            if wake {
                self.ready.notify_one();
            }
        }
    }

    /// Takes the near end of this worker's run, a new run, or the far end of
    /// another worker's run (half its pending tail). The feed lock orders
    /// assignment and stealing against going to sleep, closing the
    /// lost-wakeup window. Owners take only their run lock, and release it
    /// before any read or publication.
    fn take(&self, worker: usize) -> Option<(u64, Task)> {
        if self.indexed
            && let Some(task) = lock(&self.runs[worker]).pop_front()
        {
            return Some(task);
        }
        let mut state = lock(&self.state);
        loop {
            if let Some(task) = state.tasks.pop_front() {
                let lend = self.indexed
                    && self.fill_run(worker, &mut state.tasks, task.0)
                    && state.sleeping > 0;
                let wake = state.full && state.tasks.len() <= self.capacity / 2;
                if wake {
                    state.full = false;
                }
                drop(state);
                if wake {
                    self.room.notify_one();
                }
                if lend {
                    self.ready.notify_one();
                }
                return Some(task);
            }
            if self.indexed {
                for offset in 1..self.runs.len() {
                    let mut run = lock(&self.runs[(worker + offset) % self.runs.len()]);
                    if !run.is_empty() {
                        // Keep locality in the stolen tail too; repeated
                        // single-file steals scatter the same directory
                        // across every idle worker.
                        let half = run.len() / 2;
                        let mut own = lock(&self.runs[worker]);
                        own.extend(run.drain(half..));
                        let task = own.pop_front();
                        let lend = (!run.is_empty() || !own.is_empty()) && state.sleeping > 0;
                        drop(own);
                        drop(run);
                        drop(state);
                        if lend {
                            self.ready.notify_one();
                        }
                        return task;
                    }
                }
            }
            if state.closed {
                return None;
            }
            state.sleeping += 1;
            state = wait(&self.ready, state);
            state.sleeping -= 1;
        }
    }

    /// Leaves the current file on the caller and its consecutive tail in
    /// the shared run. The caller holds the feed lock; the run is empty.
    fn fill_run(&self, worker: usize, tasks: &mut VecDeque<(u64, Task)>, mut bytes: u64) -> bool {
        let mut run = lock(&self.runs[worker]);
        while bytes <= RUN_BYTES && run.len() + 1 < RUN_TASKS {
            let Some((size, _)) = tasks.front() else {
                break;
            };
            if *size > RUN_BYTES - bytes {
                break;
            }
            bytes += *size;
            if let Some(next) = tasks.pop_front() {
                run.push_back(next);
            }
        }
        !run.is_empty()
    }

    fn close(&self) {
        lock(&self.state).closed = true;
        self.ready.notify_all();
        self.room.notify_all();
    }
}

fn wait<'a, T>(signal: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    signal.wait(guard).unwrap_or_else(PoisonError::into_inner)
}

/// Closes the feed when its holder leaves, by any path: the producer's end
/// wakes the sleeping workers, and a worker's exit stops the producer.
struct CloseOnDrop<'f, Task>(&'f Feed<Task>);

impl<Task> Drop for CloseOnDrop<'_, Task> {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// A group whose capture holds this many bytes or more is handed to the caller
/// alone; smaller groups travel together up to this many bytes in all. Keeps
/// D93's capture bound to within one batch per worker.
const HANDOFF_BYTES: u64 = 64 << 10;
/// At most this many groups travel in one handoff.
const HANDOFF_GROUPS: usize = 16;

/// A worker's completed groups not yet handed to the caller: at most
/// [`HANDOFF_BYTES`] and [`HANDOFF_GROUPS`] of them.
struct Held<G> {
    groups: Vec<G>,
    bytes: u64,
}

impl<G> Default for Held<G> {
    fn default() -> Self {
        Self {
            groups: Vec::new(),
            bytes: 0,
        }
    }
}

impl<G> Held<G> {
    /// Holds a completed group of `weight` bytes and returns the batches to
    /// hand off now, in completion order: first the held batch, when this
    /// group would push it past [`HANDOFF_BYTES`] or reaches that size alone;
    /// then the large group alone, or the batch this group filled to
    /// [`HANDOFF_GROUPS`].
    fn add(&mut self, group: G, weight: u64) -> [Option<Vec<G>>; 2] {
        if weight >= HANDOFF_BYTES {
            return [self.take(), Some(vec![group])];
        }
        let crossed = if self.bytes + weight > HANDOFF_BYTES {
            self.take()
        } else {
            None
        };
        self.bytes += weight;
        self.groups.push(group);
        let full = if self.groups.len() >= HANDOFF_GROUPS {
            self.take()
        } else {
            None
        };
        [crossed, full]
    }

    /// The held batch, if any, leaving the slot empty.
    fn take(&mut self) -> Option<Vec<G>> {
        self.bytes = 0;
        (!self.groups.is_empty()).then(|| std::mem::take(&mut self.groups))
    }
}

/// Input scheduling for completed file groups, with the worker count.
pub enum GroupSchedule {
    /// A producer that may block: wake a worker for each available task.
    Streaming(usize),
    /// Catalog enumeration: take bounded runs with stealing and wake at 16
    /// queued tasks, or when enumeration ends.
    Indexed(usize),
}

/// Prepares checked file groups on workers and emits them on the caller in
/// completion order. One thread keeps source order. The producer runs on its
/// own thread, a few tasks per worker ahead of them, and no worker ever calls
/// it; if that thread cannot start, the caller works alone. Returns the
/// [`Shortfall`] if the system refused a worker or the producer.
///
/// Workers hand groups over in batches through a bounded queue of `threads`
/// handoffs: a group whose `weight` (its capture's allocation) reaches
/// [`HANDOFF_BYTES`] goes alone, smaller ones together up to that many bytes.
/// So the queue holds at most `threads` large groups; each worker owns one in
/// progress and a held batch of at most [`HANDOFF_BYTES`]; the caller owns one
/// handoff.
///
/// Workers hold groups only while the caller is busy. Before it sleeps, the
/// caller takes every held batch; a worker that then completes a group finds
/// it idle and hands off at once. So a held group waits only while the caller
/// has other groups to publish, never on its worker's next file, nor on a
/// worker asleep waiting for the producer.
///
/// Each task carries the same input-byte reservation as document
/// verification; callers discard oversized input buffers before returning
/// bounded or spilled groups. A stop wakes the feed and the budget and reaches
/// active reads. The producer receives the same stop predicate and must poll
/// it during filtering or traversal before returning its next task.
pub fn completion_groups<Task: Send, State, Group: Send>(
    schedule: GroupSchedule,
    cancelled: &AtomicBool,
    next: impl FnMut(&dyn Fn() -> bool) -> Option<(u64, Task)> + Send,
    make: impl Fn() -> State + Sync,
    work: impl Fn(&mut State, Task, &dyn Fn() -> bool) -> Group + Sync,
    weight: impl Fn(&Group) -> u64 + Sync,
    emit: impl FnMut(Group) -> ControlFlow<()>,
) -> Option<Shortfall> {
    completion_groups_with_handoff(
        schedule,
        cancelled,
        next,
        make,
        |state, task, stopped, _, _| work(state, task, stopped),
        weight,
        emit,
    )
}

/// Like [`completion_groups`], with handoffs during an active task. A handoff
/// flushes that worker's held batch first and uses the same bounded queue.
/// The final boolean tells `work` whether publication runs concurrently. On
/// the single-thread and spawn-fallback paths handoffs publish synchronously;
/// callers must not wait for that same task to supply more data in that case.
/// A failed handoff means the publisher stopped and the task must unwind.
pub fn completion_groups_with_handoff<Task: Send, State, Group: Send>(
    schedule: GroupSchedule,
    cancelled: &AtomicBool,
    next: impl FnMut(&dyn Fn() -> bool) -> Option<(u64, Task)> + Send,
    make: impl Fn() -> State + Sync,
    work: impl Fn(&mut State, Task, &dyn Fn() -> bool, &dyn Fn(Group) -> bool, bool) -> Group + Sync,
    weight: impl Fn(&Group) -> u64 + Sync,
    mut emit: impl FnMut(Group) -> ControlFlow<()>,
) -> Option<Shortfall> {
    let (threads, indexed) = match schedule {
        GroupSchedule::Streaming(threads) => (threads, false),
        GroupSchedule::Indexed(threads) => (threads, true),
    };
    if threads <= 1 {
        groups_alone_with_handoff(next, cancelled, &make, &work, &mut emit);
        return None;
    }
    let pool = Pool::<()>::schedule(0, threads);
    let source = Mutex::new(next);
    let feed = Feed::new(threads, indexed);
    let held: Vec<Mutex<Held<Group>>> = (0..threads).map(|_| Mutex::default()).collect();
    // Set by the caller before its last look at the slots ahead of sleeping;
    // cleared by the first worker to hand it work, or by the caller on waking.
    let idle = AtomicBool::new(false);
    let (sender, receiver) = std::sync::mpsc::sync_channel::<Vec<Group>>(threads);
    std::thread::scope(|scope| {
        let _stop = StopOnDrop(&pool);
        let _close = CloseOnDrop(&feed);
        let make = &make;
        let work = &work;
        let feed = &feed;
        let pool = &pool;
        let stopped = move || pool.halted() || cancelled.load(Ordering::Acquire);
        let weight = &weight;
        let held = &held;
        let idle = &idle;
        let mut refused = None;
        let started = (0..threads)
            .take_while(|&i| {
                let sender = sender.clone();
                let started = spawn(scope, i, move || {
                    let _panic = StopOnPanic(pool);
                    let _close = CloseOnDrop(feed);
                    let mut state = make();
                    let lane = Lane {
                        slot: &held[i],
                        idle,
                        sender,
                        pool,
                    };
                    while !stopped() {
                        let Some((size, task)) = feed.take(i) else {
                            break;
                        };
                        let Some(reservation) = lane.reserve(size, &stopped) else {
                            break;
                        };
                        let handoff = |group| lane.handoff(group, &stopped);
                        let group = work(&mut state, task, &stopped, &handoff, true);
                        drop(reservation);
                        if stopped() {
                            break;
                        }
                        let bytes = weight(&group);
                        if lane.hand(group, bytes).is_break() {
                            break;
                        }
                    }
                    lane.flush(stopped());
                });
                started.map_err(|error| refused = Some(error)).is_ok()
            })
            .count();
        drop(sender);
        let source = &source;
        let fed = started > 0
            && spawn_named(scope, threads, "ferret-producer", move || {
                let _panic = StopOnPanic(pool);
                let _close = CloseOnDrop(feed);
                feed.produce(&mut *lock(source), &stopped);
            })
            .map_err(|error| refused.get_or_insert(error))
            .is_ok();
        let shortfall = refused.map(|cause| Shortfall {
            running: if fed { started } else { 1 },
            asked: threads,
            cause,
        });
        if !fed {
            // Any started workers find the feed closed and empty, and leave.
            feed.close();
            groups_alone_with_handoff(&mut *lock(source), cancelled, make, work, &mut emit);
            return shortfall;
        }
        collect(&receiver, held, idle, &stopped, &mut emit);
        // Dropping the receiver unblocks senders before scope join. The stop
        // guard also wakes input-budget waiters.
        drop(receiver);
        shortfall
    })
}

/// A worker's way to the caller: its slot of held groups, and the bounded
/// queue of handoffs. Shared by [`completion_groups`] and [`lane_groups`].
pub struct Lane<'g, Group> {
    slot: &'g Mutex<Held<Group>>,
    idle: &'g AtomicBool,
    sender: std::sync::mpsc::SyncSender<Vec<Group>>,
    pool: &'g Pool<()>,
}

impl<'g, Group> Lane<'g, Group> {
    /// Reserves the input bytes for a task of `size` (at least one read
    /// buffer) until the reservation drops; `None` once the run stops.
    pub fn reserve(&self, size: u64, stopped: &dyn Fn() -> bool) -> Option<Reservation<'g>> {
        let size = self
            .pool
            .reserve(size.max(crate::scan::BUFFER as u64), stopped)?;
        Some(Reservation {
            pool: self.pool,
            size,
        })
    }

    /// Stops the run and wakes every lane waiting for bytes. For a walker
    /// that panics: its siblings poll the stop, not the walker.
    pub fn stop(&self) {
        self.pool.halt();
    }

    /// Hands over an active task's group after flushing its held batch.
    /// Sends outside the slot lock: the publisher may consume this task until
    /// it finishes, so it must never wait on a lock held by the sender.
    /// False means the publisher stopped and the task must unwind.
    pub fn handoff(&self, group: Group, stopped: &dyn Fn() -> bool) -> bool {
        let batch = lock(self.slot).take();
        if let Some(batch) = batch
            && self.sender.send(batch).is_err()
        {
            return false;
        }
        self.idle.store(false, Ordering::SeqCst);
        !stopped() && self.sender.send(vec![group]).is_ok()
    }

    /// Holds a completed group of `bytes`, or hands batches over now (see
    /// [`completion_groups`]). Breaks once the caller has gone.
    pub fn hand(&self, group: Group, bytes: u64) -> ControlFlow<()> {
        let handoffs = {
            let mut slot = lock(self.slot);
            let mut handoffs = slot.add(group, bytes);
            // Read under the slot lock, which orders it after the caller's
            // last look at this slot. Any send wakes the caller, who looks at
            // every slot again before sleeping, so only one worker need flush.
            if handoffs.iter().all(Option::is_none)
                && self.idle.load(Ordering::SeqCst)
                && self.idle.swap(false, Ordering::SeqCst)
            {
                handoffs[1] = slot.take();
            }
            handoffs
        };
        // Send outside the slot lock: the caller takes that lock to collect,
        // and a full queue waits on the caller.
        for batch in handoffs.into_iter().flatten() {
            if self.sender.send(batch).is_err() {
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }

    /// Out of work, hands over what is held. A stopped run drops it, as the
    /// caller drops queued groups.
    pub fn flush(&self, stopped: bool) {
        // Take first: a guard in the condition would live through a send
        // that may wait on the collecting caller.
        let batch = lock(self.slot).take();
        if !stopped && let Some(batch) = batch {
            let _ = self.sender.send(batch);
        }
    }
}

/// Input bytes held by a [`Lane`]'s task, released when dropped, unwinding
/// included, so a panicking reader cannot strand its siblings.
pub struct Reservation<'g> {
    pool: &'g Pool<()>,
    size: u64,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.pool.release(self.size);
    }
}

/// Collects handoffs on the caller until every lane has gone, the run stops
/// or `emit` breaks. Before sleeping, takes every held batch.
fn collect<Group>(
    receiver: &std::sync::mpsc::Receiver<Vec<Group>>,
    held: &[Mutex<Held<Group>>],
    idle: &AtomicBool,
    stopped: &dyn Fn() -> bool,
    emit: &mut impl FnMut(Group) -> ControlFlow<()>,
) {
    // Slots seen empty in a row since `idle` was last set, and the next
    // slot to look at. Sleeping needs a full clean pass after setting it.
    let mut clean = 0;
    let mut at = 0;
    loop {
        let batch = match receiver.try_recv() {
            Ok(batch) => Some(batch),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            Err(std::sync::mpsc::TryRecvError::Empty) if clean < held.len() => {
                if clean == 0 {
                    idle.store(true, Ordering::SeqCst);
                }
                let batch = lock(&held[at]).take();
                at = (at + 1) % held.len();
                clean += 1;
                batch
            }
            // Every slot was empty after `idle` was set: a group completed
            // from here on is sent, which wakes this receive.
            Err(std::sync::mpsc::TryRecvError::Empty) => match receiver.recv() {
                Ok(batch) => Some(batch),
                Err(_) => break,
            },
        };
        if let Some(batch) = batch {
            idle.store(false, Ordering::SeqCst);
            clean = 0;
            if publish(batch, stopped, emit).is_break() {
                break;
            }
        }
    }
}

/// Like [`completion_groups`], for workers that find their own tasks: `run`
/// gets one [`Lane`] per worker and the stop predicate, starts its workers,
/// and returns once they have all finished. The caller emits groups in
/// completion order meanwhile, under the same handoff and input bounds.
///
/// `run` gets its own thread, as the caller is busy emitting; if that thread
/// cannot start, nothing runs and the spawn's error is returned. A worker
/// whose walk panics should [`Lane::stop`] the run before unwinding further.
pub fn lane_groups<'g, Group: Send + 'g>(
    threads: usize,
    cancelled: &AtomicBool,
    run: impl FnOnce(Vec<Lane<'_, Group>>, &(dyn Fn() -> bool + Sync)) + Send,
    mut emit: impl FnMut(Group) -> ControlFlow<()>,
) -> io::Result<()> {
    let threads = threads.max(1);
    let pool = Pool::<()>::schedule(0, threads);
    let held: Vec<Mutex<Held<Group>>> = (0..threads).map(|_| Mutex::default()).collect();
    let idle = AtomicBool::new(false);
    let (sender, receiver) = std::sync::mpsc::sync_channel::<Vec<Group>>(threads);
    std::thread::scope(|scope| {
        let _stop = StopOnDrop(&pool);
        let pool = &pool;
        let held = &held;
        let idle = &idle;
        let stopped = move || pool.halted() || cancelled.load(Ordering::Acquire);
        let lanes = (0..threads)
            .map(|i| Lane {
                slot: &held[i],
                idle,
                sender: sender.clone(),
                pool,
            })
            .collect();
        drop(sender);
        let started = spawn_named(scope, threads, "ferret-walk", move || {
            let _panic = StopOnPanic(pool);
            run(lanes, &stopped);
        })
        .map(drop);
        if started.is_ok() {
            collect(&receiver, held, idle, &stopped, &mut emit);
        }
        drop(receiver);
        started
    })
}

/// Emits a handoff's groups in order until the run stops or `emit` breaks.
fn publish<Group>(
    batch: Vec<Group>,
    halted: &dyn Fn() -> bool,
    emit: &mut impl FnMut(Group) -> ControlFlow<()>,
) -> ControlFlow<()> {
    for group in batch {
        if halted() {
            return ControlFlow::Break(());
        }
        emit(group)?;
    }
    ControlFlow::Continue(())
}

fn groups_alone_with_handoff<Task, State, Group>(
    mut next: impl FnMut(&dyn Fn() -> bool) -> Option<(u64, Task)>,
    cancelled: &AtomicBool,
    make: &impl Fn() -> State,
    work: &impl Fn(&mut State, Task, &dyn Fn() -> bool, &dyn Fn(Group) -> bool, bool) -> Group,
    emit: &mut impl FnMut(Group) -> ControlFlow<()>,
) {
    let stopped = || cancelled.load(Ordering::Acquire);
    let emit = std::cell::RefCell::new(emit);
    let handoff = |group| {
        if stopped() || emit.borrow_mut()(group).is_break() {
            cancelled.store(true, Ordering::Release);
            false
        } else {
            true
        }
    };
    let mut state = make();
    while !stopped() {
        let Some((_, task)) = next(&stopped) else {
            break;
        };
        let item = work(&mut state, task, &stopped, &handoff, false);
        if stopped() || emit.borrow_mut()(item).is_break() {
            break;
        }
    }
}

// File groups use the pool only for its input budget and stop flag.
impl Placed for () {
    fn place(&mut self, _: Range<usize>) {}
}

/// Test-only hooks into the pool, set per test thread: a spawn that fails,
/// and a signal when a worker starts waiting for bytes.
#[cfg(test)]
pub(crate) mod seam {
    use std::cell::RefCell;
    use std::sync::Arc;

    /// Given the pool's finished-position count.
    pub(crate) type Poll = dyn Fn(&dyn Fn() -> u64) + Send + Sync;

    #[derive(Clone, Default)]
    pub(crate) struct Seams {
        /// The thread whose spawn fails: worker `i`, or `threads` for the
        /// completion producer.
        pub fail_spawn_at: Option<usize>,
        /// Called, under the budget lock, when a worker sleeps for bytes.
        pub on_budget_wait: Option<Arc<dyn Fn() + Send + Sync>>,
        /// Called on the calling thread when the run is over and its state
        /// is about to be dropped, with the pool's finished-position count.
        pub on_teardown: Option<Arc<Poll>>,
        /// Called on a worker after it posts a position, with the pool's
        /// finished-position count.
        pub on_publish: Option<Arc<Poll>>,
        /// Counts the run-membership checks of owner selection.
        pub owner_checks: Option<Arc<std::sync::atomic::AtomicU64>>,
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

        pub(super) fn published(&self, finished: &dyn Fn() -> u64) {
            if let Some(hook) = &self.on_publish {
                hook(finished);
            }
        }

        pub(super) fn tearing_down(&self, finished: &dyn Fn() -> u64) {
            if let Some(hook) = &self.on_teardown {
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
            // `(?s)` lets `.` match a newline, so the document is read
            // whole: the path that checks between atoms.
            "grep:needle grep:(?s)oth.r",
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

    /// Hands out `chunks` one per read; `stop` is set as the read after
    /// the last of `before` chunks is made.
    struct Chunks<'a> {
        chunks: &'a [&'a [u8]],
        before: usize,
        stop: &'a Cell<bool>,
    }

    impl std::io::Read for Chunks<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.before == 0 {
                self.stop.set(true);
            }
            self.before = self.before.saturating_sub(1);
            let Some((first, rest)) = self.chunks.split_first() else {
                return Ok(0);
            };
            buf[..first.len()].copy_from_slice(first);
            self.chunks = rest;
            Ok(first.len())
        }
    }

    /// A streamed document whose run is stopped once `before` reads have
    /// been made, through the calling thread's [`Verifier::here`].
    fn stream_stopped(chunks: &[&[u8]], before: usize) -> (Settled, super::Tally) {
        let query = Query::parse(
            "grep:needle grep:other",
            UNIX_EPOCH + Duration::from_secs(1),
        )
        .unwrap();
        assert!(super::streams(&query.atoms));
        let stop = Cell::new(false);
        let size = chunks.iter().map(|c| c.len() as u64).sum();
        let mut read = |_, request: ReadRequest<'_>| {
            let ReadRequest::Stream(each, _) = request else {
                panic!("line-local reads stream");
            };
            let mut source = Chunks {
                chunks,
                before,
                stop: &stop,
            };
            each(&mut source, size).ok()?;
            Some(ReadVersion::Catalogued)
        };
        let mut verifier = Verifier::new(&query, &mut read, 1);
        let settled = verifier.here(0, ferret_catalog::NameId(0), &|| stop.get());
        (settled, std::mem::take(&mut verifier.tally))
    }

    /// Stopped as the last chunk is read: the document is not accepted, and
    /// `needle`, which matched in the first, counts; `other` had not
    /// finished. Not a change.
    #[test]
    fn a_stream_stopped_on_its_final_chunk_is_not_accepted() {
        let (settled, tally) = stream_stopped(&[b"a needle\n", b"tail\n"], 1);
        assert!(matches!(settled, Settled::Stopped));
        assert_eq!(tally.verified, 1);
        assert_eq!(tally.changed, 0);
        assert_eq!(tally.matching.regex_documents, 1);
        assert_eq!(tally.matching.regex_bytes, 14);
        assert_eq!(tally.matching.regex_rejected, 0);
    }

    /// Stopped after the end of the document was read and searched: both
    /// atoms finished, so both count, and the document is not accepted.
    /// The stop is the seventh check: one before the read, one per read
    /// (two: the text, then its end), one per unanswered atom's search
    /// (three), then the one before the answers are accepted.
    #[test]
    fn a_stream_stopped_after_its_end_counts_every_atom() {
        let query = Query::parse(
            "grep:needle grep:other",
            UNIX_EPOCH + Duration::from_secs(1),
        )
        .unwrap();
        let mut read = |_, request: ReadRequest<'_>| {
            let ReadRequest::Stream(each, _) = request else {
                panic!("line-local reads stream");
            };
            each(&mut &b"a needle\ntail\n"[..], 14).ok()?;
            Some(ReadVersion::Catalogued)
        };
        let mut verifier = Verifier::new(&query, &mut read, 1);
        let checks = Cell::new(0);
        let stopped = || {
            checks.set(checks.get() + 1);
            checks.get() >= 7
        };
        let settled = verifier.here(0, ferret_catalog::NameId(0), &stopped);
        assert!(matches!(settled, Settled::Stopped));
        let tally = &verifier.tally;
        assert_eq!(tally.changed, 0);
        assert_eq!(tally.matching.regex_documents, 2);
        assert_eq!(tally.matching.regex_bytes, 28);
        assert_eq!(tally.matching.regex_rejected, 1);
    }

    /// Stopped while the second chunk is wanted, after `needle` finished in
    /// the first: the finished atom counts, and cancellation is no change.
    #[test]
    fn a_stream_stopped_after_one_atom_counts_it() {
        let (settled, tally) = stream_stopped(&[b"a needle\n", b"more\n", b"tail\n"], 1);
        assert!(matches!(settled, Settled::Stopped));
        assert_eq!(tally.verified, 1);
        assert_eq!(tally.changed, 0);
        assert_eq!(tally.matching.regex_documents, 1);
        assert_eq!(tally.matching.regex_bytes, 19);
    }

    /// Sol round 2: an atom that finished without a match counts when a
    /// stop ends the next. The document is empty, so its end is the first
    /// run: `needle` is searched and rejected, then the stop comes before
    /// `other`, at the fourth check (before the read, before the first
    /// read of the source, before each atom). The whole-document path
    /// counts the same rejection.
    #[test]
    fn a_stream_stopped_after_a_rejection_at_its_end_counts_it() {
        let query = Query::parse(
            "grep:needle grep:other",
            UNIX_EPOCH + Duration::from_secs(1),
        )
        .unwrap();
        let mut read = |_, request: ReadRequest<'_>| {
            let ReadRequest::Stream(each, _) = request else {
                panic!("line-local reads stream");
            };
            each(&mut &b""[..], 0).ok()?;
            Some(ReadVersion::Catalogued)
        };
        let mut verifier = Verifier::new(&query, &mut read, 1);
        let checks = Cell::new(0);
        let stopped = || {
            checks.set(checks.get() + 1);
            checks.get() >= 4
        };
        let settled = verifier.here(0, ferret_catalog::NameId(0), &stopped);
        assert!(matches!(settled, Settled::Stopped));
        let tally = &verifier.tally;
        assert_eq!(tally.changed, 0);
        assert_eq!(tally.matching.regex_documents, 1);
        assert_eq!(tally.matching.regex_rejected, 1);
    }

    /// Sol round 2: the stream finishes, then the stop comes in the host's
    /// own checks after it (its bracketing `fstat`, say). Whether the host
    /// still returns a version or gives up, the document is stopped, not
    /// accepted and not a change, and both finished atoms count.
    #[test]
    fn a_stream_stopped_after_it_finished_counts_its_work() {
        for host_returns in [Some(ReadVersion::Catalogued), None] {
            let query = Query::parse(
                "grep:needle grep:other",
                UNIX_EPOCH + Duration::from_secs(1),
            )
            .unwrap();
            let stop = Cell::new(false);
            let mut read = |_, request: ReadRequest<'_>| {
                let ReadRequest::Stream(each, _) = request else {
                    panic!("line-local reads stream");
                };
                each(&mut &b"a needle\nother\n"[..], 15).ok()?;
                stop.set(true);
                host_returns
            };
            let mut verifier = Verifier::new(&query, &mut read, 1);
            let settled = verifier.here(0, ferret_catalog::NameId(0), &|| stop.get());
            assert!(matches!(settled, Settled::Stopped), "{host_returns:?}");
            let tally = &verifier.tally;
            assert_eq!(tally.changed, 0, "{host_returns:?}");
            assert_eq!(tally.matching.regex_documents, 2, "{host_returns:?}");
            assert_eq!(tally.matching.regex_bytes, 30, "{host_returns:?}");
            assert!(verifier.exact.is_empty());
        }
    }

    /// Sol round 2: a whole-document read that fails as the run is
    /// cancelled is stopped, not a change.
    #[test]
    fn a_whole_read_failing_as_the_run_stops_is_not_a_change() {
        let query = Query::parse("grep:(?s)need.e", UNIX_EPOCH + Duration::from_secs(1)).unwrap();
        assert!(!super::streams(&query.atoms));
        let stop = Cell::new(false);
        let mut read = |_, request: ReadRequest<'_>| {
            let ReadRequest::Bytes(..) = request else {
                panic!("a `(?s)` pattern reads whole");
            };
            stop.set(true);
            None
        };
        let mut verifier = Verifier::new(&query, &mut read, 1);
        let settled = verifier.here(0, ferret_catalog::NameId(0), &|| stop.get());
        assert!(matches!(settled, Settled::Stopped));
        assert_eq!(verifier.tally.verified, 1);
        assert_eq!(verifier.tally.changed, 0);
    }
}

/// [`stream`] against [`verify`], the whole-document search, on the same
/// bytes: every answer and count agree for line-local patterns, with the
/// bytes arriving a few at a time, so runs end at every kind of place, and
/// with a line longer than the buffer.
#[cfg(test)]
mod streamed {
    use std::io::Read;
    use std::time::{Duration, UNIX_EPOCH};

    use ferret_catalog::NameId;
    use ferret_verify::TextMatcher;

    use super::{Hand, Outcome, stream, streams, verify};
    use crate::{Query, ReadRequest, ReadVersion};

    /// Hands out `bytes` 1 to `most` at a time.
    struct Dribble<'a> {
        bytes: &'a [u8],
        most: usize,
        reads: usize,
    }

    impl Read for Dribble<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            let n = buf
                .len()
                .min(1 + self.reads % self.most)
                .min(self.bytes.len());
            buf[..n].copy_from_slice(&self.bytes[..n]);
            self.bytes = &self.bytes[n..];
            Ok(n)
        }
    }

    /// Random documents from pieces that include CRLF, NUL, two- and
    /// three-byte characters (which the dribbled reads split), and
    /// malformed UTF-8: a lone continuation byte, a truncated lead, and
    /// 0xff.
    fn documents() -> Vec<Vec<u8>> {
        let alphabet: [&[u8]; 16] = [
            b"a",
            b"o",
            b" ",
            b"f",
            b"\n",
            b"k",
            b"1",
            b"_",
            b"\n",
            b"\r\n",
            b"\0",
            "\u{e9}".as_bytes(),
            "\u{65e5}".as_bytes(),
            b"\xa9",
            b"\xe6\x97",
            b"\xff",
        ];
        let mut seed = 0x2545_f491_u64;
        let mut docs: Vec<Vec<u8>> = (0..300)
            .map(|_| {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                let len = (seed >> 33) as usize % 300;
                (0..len)
                    .flat_map(|_| {
                        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                        alphabet[(seed >> 33) as usize % alphabet.len()]
                    })
                    .copied()
                    .collect()
            })
            .collect();
        let mut long = vec![b'a'; 70_000];
        long.extend_from_slice(b"o foo_\n\nk1 a");
        docs.push(long);
        docs.extend([
            b"".to_vec(),
            b"\n".to_vec(),
            b"foo".to_vec(),
            b"a\n\n".to_vec(),
        ]);
        docs
    }

    fn answers(outcome: Outcome) -> (Vec<Option<usize>>, ferret_verify::MatchStats) {
        match outcome {
            Outcome::Read { answers, work, .. } => (answers, work.matching),
            _ => panic!("not read"),
        }
    }

    #[test]
    fn streamed_answers_are_the_whole_documents() {
        let patterns = [
            "foo",
            "^foo",
            "foo$",
            "^$",
            r"\bfo",
            r"o\b",
            r"\Bo",
            "^a*$",
            "k1",
            "(?i)K",
            "[a-z]+1",
            "f.o",
            "^",
            "$",
            "a{3}",
            "_$",
            r"^\b",
            r"\b$",
            "(?-u)o_",
            r"\r$",
            r"\x00",
            r"a\x00",
            "\u{e9}",
            "(?i)\u{c9}",
            "\u{65e5}",
            r"\w+$",
            "^.$",
            "f.$",
            r"[^a\r\n]+$",
            r"(?-u)\xff",
            r"(?-u)[\x80-\xff]{2}",
            r"(?-u)^[^\x00\n]*$",
        ];
        let docs = documents();
        let never = || false;
        for (i, pattern) in patterns.into_iter().enumerate() {
            // Every other query negates its second atom: the answers are
            // the atoms', whatever the expression does with them.
            let not = if i % 2 == 0 { "" } else { "NOT " };
            let text = format!("grep:{pattern} {not}grep:a");
            let query = Query::parse(&text, UNIX_EPOCH + Duration::from_secs(1)).unwrap();
            assert!(streams(&query.atoms), "{pattern} is line-local");
            for doc in &docs {
                let mut whole = |_, request: ReadRequest<'_>| {
                    let ReadRequest::Bytes(out, _) = request else {
                        panic!("whole reads are bytes");
                    };
                    out.clear();
                    out.extend_from_slice(doc);
                    Some(ReadVersion::Catalogued)
                };
                let mut bytes = Vec::new();
                let mut matcher = TextMatcher::new();
                let regexes = Hand::new(&mut whole, &query.atoms).regexes;
                let expected = answers(verify(
                    &query.atoms,
                    &regexes,
                    &mut whole,
                    &mut matcher,
                    &mut bytes,
                    NameId(0),
                    &never,
                ));
                let mut dribbled = |_, request: ReadRequest<'_>| {
                    let ReadRequest::Stream(each, _) = request else {
                        panic!("line-local reads stream");
                    };
                    // A few bytes at a time; the long line in larger
                    // pieces, which still end mid-line.
                    let mut source = Dribble {
                        bytes: doc,
                        most: if doc.len() > 1000 { 4093 } else { 7 },
                        reads: 0,
                    };
                    each(&mut source, doc.len() as u64).ok()?;
                    Some(ReadVersion::Catalogued)
                };
                let mut buffer = Vec::new();
                let got = answers(stream(
                    &regexes,
                    &mut dribbled,
                    &mut buffer,
                    NameId(0),
                    &never,
                ));
                let shown = String::from_utf8_lossy(&doc[..doc.len().min(80)]);
                assert_eq!(got, expected, "{pattern} over {shown:?}");
            }
        }
    }

    /// Sol round 1, P6: a line as long as the document grows the buffer to
    /// the document's length and no further, at the doubling boundaries
    /// and at 8 MiB, the size cap, and the answers stay the whole search's.
    #[test]
    fn a_long_line_grows_the_buffer_to_the_documents_length_at_most() {
        use crate::scan::BUFFER as LINES;
        let query = Query::parse("grep:a$ grep:ab", UNIX_EPOCH + Duration::from_secs(1)).unwrap();
        let never = || false;
        let regexes = Hand::new(&mut |_, _| None, &query.atoms).regexes;
        for size in [
            LINES - 1,
            LINES,
            LINES + 1,
            2 * LINES - 1,
            2 * LINES,
            3 * LINES,
            8 << 20,
        ] {
            for end in [b'a', b'\n'] {
                let mut doc = vec![b'a'; size];
                doc[size - 1] = end;
                let mut read = |_, request: ReadRequest<'_>| {
                    let ReadRequest::Stream(each, _) = request else {
                        panic!("line-local reads stream");
                    };
                    each(&mut doc.as_slice(), doc.len() as u64).ok()?;
                    Some(ReadVersion::Catalogued)
                };
                let mut buffer = Vec::new();
                let got = answers(stream(&regexes, &mut read, &mut buffer, NameId(0), &never));
                let at = size - 1 - usize::from(end == b'\n');
                assert_eq!(got.0, [Some(at), None], "{size} ending {end}");
                assert!(
                    buffer.capacity() <= size.max(LINES),
                    "{size} ending {end}: a {} byte buffer",
                    buffer.capacity()
                );
            }
        }
    }

    #[test]
    fn patterns_that_see_a_newline_or_a_text_end_are_read_whole() {
        for pattern in [
            r"a\nb", r"\s", "[^a]", "(?s).", r"\Aa", r"a\z", "(?R)^a", "(?R)a$",
        ] {
            let text = format!("grep:{pattern}");
            let query = Query::parse(&text, UNIX_EPOCH + Duration::from_secs(1)).unwrap();
            assert!(!streams(&query.atoms), "{pattern} is not line-local");
        }
    }
}

#[cfg(test)]
mod group_tests {
    use super::*;
    use std::sync::Barrier;

    fn completion_child(name: &str) -> bool {
        const MARKER: &str = "FERRET_TEST_COMPLETION_CHILD";
        if std::env::var_os(MARKER).is_some() {
            return false;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("verify::group_tests::{name}")])
            .env(MARKER, name)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                return true;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("completion worker left a waiter asleep");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // D106 B: the owner cannot step or lend while task zero is blocked.
    // Every other task in its run must still reach the real publisher.
    #[test]
    fn indexed_run_is_stolen_while_its_head_is_blocked() {
        let published = AtomicUsize::new(0);
        let mut tasks = (0..RUN_TASKS).map(|task| (1, task));
        completion_groups(
            GroupSchedule::Indexed(2),
            &AtomicBool::new(false),
            |_| tasks.next(),
            || (),
            |_, task, _| {
                if task == 0 {
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while published.load(Ordering::Acquire) < RUN_TASKS - 1 {
                        assert!(Instant::now() < deadline, "run stranded behind its head");
                        std::thread::yield_now();
                    }
                }
                task
            },
            |_| 0,
            |task| {
                if task != 0 {
                    published.fetch_add(1, Ordering::Release);
                }
                ControlFlow::Continue(())
            },
        );
        assert_eq!(published.load(Ordering::Acquire), RUN_TASKS - 1);
    }

    // A full queue must bound captures, and dropping its receiver on Break
    // must unblock senders before scope join. Isolate a regression's deadlock.
    // Groups exactly at the handoff threshold go alone, so the bound stays
    // one group per queue slot, per worker and on the caller.
    #[test]
    fn completion_queue_bounds_captures_and_releases_blocked_senders() {
        if completion_child("completion_queue_bounds_captures_and_releases_blocked_senders") {
            return;
        }
        let threads = 4;
        let prepared = AtomicUsize::new(0);
        let mut tasks = (0..256).map(|task| (1, task));
        let mut emitted = 0;
        completion_groups(
            GroupSchedule::Streaming(threads),
            &AtomicBool::new(false),
            |_| tasks.next(),
            || (),
            |_, task, _| {
                prepared.fetch_add(1, Ordering::Release);
                task
            },
            |_| HANDOFF_BYTES,
            |_| {
                emitted += 1;
                let deadline = Instant::now() + Duration::from_secs(2);
                while prepared.load(Ordering::Acquire) < 2 * threads + 1
                    && Instant::now() < deadline
                {
                    std::thread::yield_now();
                }
                // Once saturated, another prepare cannot finish until the
                // caller consumes a group, which it deliberately does not.
                std::thread::sleep(Duration::from_millis(10));
                assert_eq!(prepared.load(Ordering::Acquire), 2 * threads + 1);
                ControlFlow::Break(())
            },
        );
        assert_eq!(emitted, 1);
    }

    /// Runs `body`, catching its panic, and returns the message of every panic
    /// on any thread, in order. The hook is process-wide: call this only in a
    /// [`completion_child`], which runs one test per process.
    fn panics(body: impl FnOnce()) -> Vec<String> {
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
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
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        std::panic::set_hook(previous);
        std::mem::take(&mut *lock(&seen))
    }

    /// Asserts the only panic was `intended`, re-raised by the scopes it
    /// unwound through.
    fn assert_only_panic(seen: &[String], intended: &str) {
        assert!(
            seen.first().is_some_and(|first| first == intended)
                && seen[1..]
                    .iter()
                    .all(|rest| rest == "a scoped thread panicked"),
            "expected only {intended:?}, saw {seen:?}"
        );
    }

    // A panicking worker must stop the run before the scope joins: a sibling
    // waiting for bytes that the panic's own release cannot satisfy, and a
    // reader polling the stop, would otherwise sleep forever. The poller
    // keeps all but one buffer of the budget, so only the stop wakes the
    // waiter.
    #[test]
    fn completion_worker_panic_wakes_input_budget_waiters() {
        if completion_child("completion_worker_panic_wakes_input_budget_waiters") {
            return;
        }
        #[derive(Clone, Copy, Debug)]
        enum Task {
            Poll,
            Panic,
            Wait,
        }
        const INJECTED: &str = "injected worker panic with a sibling waiting";
        let buffer = crate::scan::BUFFER as u64;
        let waiting = std::sync::Arc::new(AtomicBool::new(false));
        let _seams = seam::Seams {
            on_budget_wait: Some({
                let waiting = waiting.clone();
                std::sync::Arc::new(move || waiting.store(true, Ordering::SeqCst))
            }),
            ..Default::default()
        }
        .set();
        let polling = AtomicBool::new(false);
        let serial = AtomicBool::new(false);
        let mut tasks = [
            (IN_FLIGHT_BYTES - buffer, Task::Poll),
            (buffer, Task::Panic),
            (2 * buffer, Task::Wait),
        ]
        .into_iter();
        let until = |flag: &AtomicBool, stopped: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !flag.load(Ordering::SeqCst) && !stopped() && Instant::now() < deadline {
                std::thread::yield_now();
            }
            flag.load(Ordering::SeqCst)
        };
        let seen = panics(|| {
            completion_groups_with_handoff(
                GroupSchedule::Streaming(3),
                &AtomicBool::new(false),
                |stopped| {
                    // The poller reserves first, so the panicking reader's
                    // buffer fits beside it and the waiter's two do not.
                    let next = tasks.next()?;
                    if !matches!(next.1, Task::Poll) {
                        until(&polling, stopped);
                    }
                    Some(next)
                },
                || (),
                |_, task, stopped, _, concurrent| {
                    if !concurrent {
                        serial.store(true, Ordering::SeqCst);
                        polling.store(true, Ordering::SeqCst);
                        return;
                    }
                    match task {
                        Task::Poll => {
                            polling.store(true, Ordering::SeqCst);
                            while !stopped() {
                                std::thread::yield_now();
                            }
                        }
                        Task::Panic if until(&waiting, &|| false) => panic!("{INJECTED}"),
                        Task::Panic => panic!("setup: no sibling waited for bytes"),
                        Task::Wait => {}
                    }
                },
                |_| 0,
                |()| ControlFlow::Continue(()),
            );
        });
        assert!(
            !serial.load(Ordering::SeqCst),
            "no worker threads: {seen:?}"
        );
        assert!(
            polling.load(Ordering::SeqCst),
            "the poller never ran: {seen:?}"
        );
        assert!(
            waiting.load(Ordering::SeqCst),
            "no sibling waited: {seen:?}"
        );
        assert_only_panic(&seen, INJECTED);
    }

    // The first task is a quiet hit and the producer then blocks until that
    // hit cancels the run, as grep's filtering producer can. A worker that
    // fetched the hit and called the producer again before working it would
    // wait forever (grep-why P2); isolate a regression's deadlock.
    #[test]
    fn a_first_task_hit_reaches_a_worker_while_the_producer_blocks() {
        if completion_child("a_first_task_hit_reaches_a_worker_while_the_producer_blocks") {
            return;
        }
        let cancelled = AtomicBool::new(false);
        let mut calls = 0;
        completion_groups(
            GroupSchedule::Streaming(2),
            &cancelled,
            |stopped| {
                calls += 1;
                if calls == 1 {
                    return Some((1, ()));
                }
                while !stopped() {
                    std::thread::yield_now();
                }
                None
            },
            || (),
            |_, (), _| cancelled.store(true, Ordering::Release),
            |_| 0,
            |()| ControlFlow::Continue(()),
        );
        assert!(cancelled.load(Ordering::Acquire));
    }

    // Small groups travel in batches, so more are prepared before the queue
    // stalls than one per slot, but never more than the batch rules allow:
    // with quarter-threshold groups, four per batch, each worker may hold a
    // full batch it is sending plus one group, the queue a full batch per
    // slot and the caller one batch.
    #[test]
    fn completion_queue_bounds_small_groups_by_batch() {
        if completion_child("completion_queue_bounds_small_groups_by_batch") {
            return;
        }
        let threads = 4;
        let prepared = AtomicUsize::new(0);
        let mut tasks = (0..1024).map(|task| (1, task));
        completion_groups(
            GroupSchedule::Streaming(threads),
            &AtomicBool::new(false),
            |_| tasks.next(),
            || (),
            |_, task, _| {
                prepared.fetch_add(1, Ordering::Release);
                task
            },
            |_| HANDOFF_BYTES / 4,
            |_| {
                // Wait for preparation to stall, then never consume more.
                let mut seen = prepared.load(Ordering::Acquire);
                loop {
                    std::thread::sleep(Duration::from_millis(50));
                    let now = prepared.load(Ordering::Acquire);
                    if now == seen {
                        break;
                    }
                    seen = now;
                }
                assert!(seen > 2 * threads + 1, "groups were not batched: {seen}");
                assert!(
                    seen <= 9 * threads + 4,
                    "batches exceeded the bound: {seen}"
                );
                ControlFlow::Break(())
            },
        );
    }

    // A worker that completes a group while the caller is idle must hand it
    // off before taking its next task, which here is slow.
    #[test]
    fn group_completed_while_the_caller_is_idle_publishes_before_a_slow_task() {
        let published = AtomicBool::new(false);
        let waited_out = AtomicBool::new(false);
        let mut tasks = (0..3).map(|task| (1, task));
        let mut emitted = Vec::new();
        completion_groups(
            GroupSchedule::Streaming(2),
            &AtomicBool::new(false),
            |_| tasks.next(),
            || (),
            |_, task, _| {
                // Tasks 0 and 2 finish only once something is published;
                // task 1 is the fast one its worker holds.
                if task != 1 {
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while !published.load(Ordering::Acquire) {
                        if Instant::now() >= deadline {
                            waited_out.store(true, Ordering::Release);
                            break;
                        }
                        std::thread::yield_now();
                    }
                }
                task
            },
            |_| 0,
            |task| {
                emitted.push(task);
                published.store(true, Ordering::Release);
                ControlFlow::Continue(())
            },
        );
        assert!(!waited_out.load(Ordering::Acquire));
        assert_eq!(emitted[0], 1);
        emitted.sort_unstable();
        assert_eq!(emitted, [0, 1, 2]);
    }

    // A worker that completes a group while the caller is busy holds it, then
    // blocks on a slow task. When the caller runs out of queued groups it
    // must collect the held one rather than sleep: both other tasks finish
    // only once that group is published.
    #[test]
    fn group_held_while_the_caller_is_busy_publishes_before_a_slow_task() {
        let emitting = AtomicBool::new(false);
        let held_done = AtomicBool::new(false);
        let published = AtomicBool::new(false);
        let waited_out = AtomicBool::new(false);
        let wait_for = |flag: &AtomicBool| {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !flag.load(Ordering::Acquire) {
                if Instant::now() >= deadline {
                    waited_out.store(true, Ordering::Release);
                    return;
                }
                std::thread::yield_now();
            }
        };
        let mut tasks = (0..4).map(|task| (1, task));
        let mut emitted = Vec::new();
        completion_groups(
            GroupSchedule::Streaming(2),
            &AtomicBool::new(false),
            |_| tasks.next(),
            || (),
            |_, task, _| {
                match task {
                    // Task 0 keeps the caller busy, below.
                    0 => {}
                    // Task 1 completes while the caller publishes task 0.
                    1 => {
                        wait_for(&emitting);
                        held_done.store(true, Ordering::Release);
                    }
                    // Whichever worker takes 2 and 3 is stuck until task 1
                    // is published, so neither can hand it off.
                    _ => wait_for(&published),
                }
                task
            },
            |_| 0,
            |task| {
                if task == 0 {
                    emitting.store(true, Ordering::Release);
                    wait_for(&held_done);
                    // Let task 1's worker hold it and start its next task.
                    std::thread::sleep(Duration::from_millis(50));
                }
                if task == 1 {
                    published.store(true, Ordering::Release);
                }
                emitted.push(task);
                ControlFlow::Continue(())
            },
        );
        assert!(!waited_out.load(Ordering::Acquire));
        assert_eq!(emitted[..2], [0, 1]);
        emitted.sort_unstable();
        assert_eq!(emitted, [0, 1, 2, 3]);
    }

    // The batch rules, as the workers apply them: the threshold is inclusive,
    // a group that would cross it flushes the held batch first, a large group
    // follows the batch it overtakes, and a batch is capped in count.
    #[test]
    fn held_batches_follow_the_handoff_rules() {
        let mut held = Held::<u32>::default();
        // Just under the threshold is held; exactly at it goes alone.
        assert_eq!(held.add(0, HANDOFF_BYTES - 1), [None, None]);
        assert_eq!(held.add(1, HANDOFF_BYTES), [Some(vec![0]), Some(vec![1])]);
        assert_eq!(held.take(), None);
        // Just over goes alone too, with nothing held before it.
        assert_eq!(held.add(2, HANDOFF_BYTES + 1), [None, Some(vec![2])]);
        // Filling the batch to exactly the threshold holds it; one more byte
        // flushes it and starts the next batch.
        assert_eq!(held.add(3, HANDOFF_BYTES / 2), [None, None]);
        assert_eq!(held.add(4, HANDOFF_BYTES / 2), [None, None]);
        assert_eq!(held.add(5, 1), [Some(vec![3, 4]), None]);
        assert_eq!(held.bytes, 1);
        // A batch that would cross by its last group flushes before it.
        assert_eq!(held.add(6, HANDOFF_BYTES - 2), [None, None]);
        assert_eq!(held.add(7, 2), [Some(vec![5, 6]), None]);
        assert_eq!(held.take(), Some(vec![7]));
        // Count cap: the sixteenth small group hands the batch off.
        for group in 0..HANDOFF_GROUPS as u32 - 1 {
            assert_eq!(held.add(group, 0), [None, None]);
        }
        let full = held.add(99, 0);
        assert_eq!(full[0], None);
        assert_eq!(full[1].as_ref().map(Vec::len), Some(HANDOFF_GROUPS));
        assert_eq!(held.take(), None);
    }

    // Four workers each hold their first task at a barrier, so the groups
    // are prepared in parallel; each payload arrives whole, once, and small
    // ones travel through the held batches.
    #[test]
    fn groups_use_parallel_workers_and_preserve_each_payload() {
        if completion_child("groups_use_parallel_workers_and_preserve_each_payload") {
            return;
        }
        let barrier = Barrier::new(4);
        let mut emitted = Vec::new();
        let mut tasks = (0..80).map(|at| (1, at));
        completion_groups(
            GroupSchedule::Streaming(4),
            &AtomicBool::new(false),
            move |_| tasks.next(),
            || false,
            |started, at, _| {
                if !*started {
                    *started = true;
                    barrier.wait();
                }
                (at, vec![at as u8; 1000])
            },
            |(_, payload)| payload.len() as u64,
            |item| {
                emitted.push(item);
                ControlFlow::Continue(())
            },
        );
        emitted.sort();
        assert_eq!(
            emitted,
            (0..80)
                .map(|at| (at, vec![at as u8; 1000]))
                .collect::<Vec<_>>()
        );
    }

    // Every task but the first waits for the stop. The caller's Break must
    // reach those workers and the producer, blocked on a full feed, before
    // the scope joins; isolate a regression's deadlock.
    #[test]
    fn a_group_emission_break_cancels_in_progress_work() {
        if completion_child("a_group_emission_break_cancels_in_progress_work") {
            return;
        }
        let mut emitted = Vec::new();
        let mut tasks = (0..80).map(|at| (1, at));
        completion_groups(
            GroupSchedule::Streaming(4),
            &AtomicBool::new(false),
            move |_| tasks.next(),
            || (),
            |_, at, stopped| {
                if at != 0 {
                    while !stopped() {
                        std::thread::yield_now();
                    }
                }
                at
            },
            |_| 0,
            |at| {
                emitted.push(at);
                ControlFlow::Break(())
            },
        );
        assert_eq!(emitted, [0]);
    }

    // A refused spawn still emits every group: with no worker or no producer
    // the caller works alone, in source order; with some workers started,
    // they share the work in completion order.
    #[test]
    fn spawn_failure_still_emits_every_group() {
        if completion_child("spawn_failure_still_emits_every_group") {
            return;
        }
        let threads = 4;
        // (thread whose spawn fails, whether the caller works alone)
        for (fail, alone) in [(0, true), (1, false), (3, false), (threads, true)] {
            let _seams = seam::Seams {
                fail_spawn_at: Some(fail),
                ..Default::default()
            }
            .set();
            let caller = std::thread::current().id();
            let mut tasks = (0..32).map(|task| (1, task));
            let mut emitted = Vec::new();
            let on_caller = AtomicUsize::new(0);
            completion_groups(
                GroupSchedule::Streaming(threads),
                &AtomicBool::new(false),
                |_| tasks.next(),
                || (),
                |_, task, _| {
                    if std::thread::current().id() == caller {
                        on_caller.fetch_add(1, Ordering::Relaxed);
                    }
                    task
                },
                |_| 0,
                |task| {
                    emitted.push(task);
                    ControlFlow::Continue(())
                },
            );
            let on_caller = on_caller.into_inner();
            if alone {
                assert_eq!(emitted, (0..32).collect::<Vec<_>>(), "fail at {fail}");
                assert_eq!(on_caller, 32, "fail at {fail}");
            } else {
                emitted.sort_unstable();
                assert_eq!(emitted, (0..32).collect::<Vec<_>>(), "fail at {fail}");
                assert_eq!(on_caller, 0, "fail at {fail}");
            }
        }
    }

    // A refused walk-thread spawn must not pass for an empty search: the
    // caller either runs the search or hears why it could not.
    #[test]
    fn a_refused_walk_spawn_is_reported() {
        let threads = 2;
        let _seams = seam::Seams {
            fail_spawn_at: Some(threads),
            ..Default::default()
        }
        .set();
        let ran = AtomicBool::new(false);
        let result = lane_groups(
            threads,
            &AtomicBool::new(false),
            |_, _| ran.store(true, Ordering::Relaxed),
            |(): ()| ControlFlow::Continue(()),
        );
        assert!(!ran.load(Ordering::Relaxed));
        let error = result.expect_err("the search never ran, silently");
        assert_eq!(error.to_string(), "injected spawn failure");
    }

    // A reservation taken by a task that panics is released as it unwinds,
    // so a later reader is not left waiting on leaked bytes.
    #[test]
    fn a_panicking_reader_releases_its_reservation() {
        if completion_child("a_panicking_reader_releases_its_reservation") {
            return;
        }
        lane_groups(
            1,
            &AtomicBool::new(false),
            |lanes, stopped| {
                let lane = &lanes[0];
                let held = lane.reserve(u64::MAX, stopped);
                assert!(held.is_some(), "the reader never held the budget");
                let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _held = held;
                    panic!("reader fails mid-read");
                }));
                let payload = unwound.expect_err("the reader did not panic");
                assert_eq!(
                    payload.downcast_ref::<&str>(),
                    Some(&"reader fails mid-read")
                );
                assert!(lane.reserve(u64::MAX, stopped).is_some());
            },
            |(): ()| ControlFlow::Continue(()),
        )
        .unwrap();
    }

    /// A walk of [`Job`]s for the find scheduler. Its first step lends every
    /// job but the next, so each job after it runs as its own task.
    struct Jobs(Vec<Job>);

    #[derive(Clone, Copy)]
    enum Job {
        /// Does nothing, so the walk splits.
        Lend,
        /// Reserves the whole input budget and panics once a sibling waits
        /// for bytes.
        Hog,
        /// Waits for the hog's reservation, then reserves.
        Wait,
        /// Panics once a sibling polls.
        Panic,
        /// Holds a reservation until the run stops, as a slow read does.
        Poll,
    }

    impl crate::find::schedule::Split for Jobs {
        fn split(
            &mut self,
            _: bool,
            _: crate::find::schedule::Share,
        ) -> Option<(Self, Option<std::sync::Arc<AtomicUsize>>)> {
            (self.0.len() > 1).then(|| (Jobs(self.0.split_off(1)), None))
        }
        fn waiting(&self) -> bool {
            false
        }
        fn suspended(&self) -> bool {
            false
        }
    }

    /// Runs [`Job`]s against its lane, as grep's walk workers search files.
    struct Jobber<'l, 'g> {
        lane: &'l Lane<'g, ()>,
        stopped: &'l (dyn Fn() -> bool + Sync),
        /// Both jobs of a run are on their own workers before either acts.
        met: &'l Barrier,
        hogged: &'l AtomicBool,
        waiting: &'l AtomicBool,
        polling: &'l AtomicBool,
    }

    impl crate::find::schedule::Visitor<Jobs, ()> for Jobber<'_, '_> {
        fn step(&mut self, walk: &mut Jobs, (): &mut ()) -> crate::find::schedule::Step {
            use crate::find::schedule::Step;
            if walk.0.is_empty() {
                return Step::Done;
            }
            let job = walk.0.remove(0);
            if !matches!(job, Job::Lend) {
                self.met.wait();
            }
            let until = |flag: &AtomicBool| {
                while !flag.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            };
            match job {
                Job::Lend => {}
                Job::Hog => {
                    let _held = self.lane.reserve(u64::MAX, self.stopped);
                    self.hogged.store(true, Ordering::SeqCst);
                    until(self.waiting);
                    panic!("hog fails holding the budget");
                }
                Job::Wait => {
                    until(self.hogged);
                    let _held = self.lane.reserve(1, self.stopped);
                }
                Job::Panic => {
                    until(self.polling);
                    panic!("walker fails")
                }
                Job::Poll => {
                    let _held = self.lane.reserve(1, self.stopped);
                    self.polling.store(true, Ordering::SeqCst);
                    while !(self.stopped)() {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
            Step::Next
        }
        fn lend(&mut self, (): &mut ()) {}
        fn end(&mut self, (): &mut ()) -> u64 {
            0
        }
    }

    /// Walks `jobs` with two scheduler workers inside [`lane_groups`], as
    /// grep's live walk does, and returns its [`panics`].
    fn walk_jobs(jobs: Vec<Job>) -> Vec<String> {
        let waiting = std::sync::Arc::new(AtomicBool::new(false));
        let _seams = seam::Seams {
            on_budget_wait: Some({
                let waiting = waiting.clone();
                std::sync::Arc::new(move || waiting.store(true, Ordering::SeqCst))
            }),
            ..Default::default()
        }
        .set();
        let met = Barrier::new(2);
        let hogged = AtomicBool::new(false);
        let polling = AtomicBool::new(false);
        panics(|| {
            let _ = lane_groups(
                2,
                &AtomicBool::new(false),
                |lanes, stopped| {
                    let quit = AtomicBool::new(false);
                    let stop = || lanes[0].stop();
                    let schedule = crate::find::schedule::Schedule {
                        workers: 2,
                        starts: false,
                        quit: &quit,
                        on_panic: Some(&stop),
                        on_refused: None,
                    };
                    let jobber = |i: usize| Jobber {
                        lane: &lanes[i],
                        stopped,
                        met: &met,
                        hogged: &hogged,
                        waiting: &waiting,
                        polling: &polling,
                    };
                    crate::find::schedule::run(
                        &schedule,
                        Jobs(jobs),
                        (),
                        &mut jobber(0),
                        |_| true,
                        jobber,
                    );
                },
                |(): ()| ControlFlow::Continue(()),
            );
        })
    }

    // P2 of the D105 review: a walker panicking inside the scheduler stops
    // the lane pool, so a sibling waiting for bytes, or reading until the
    // run stops, wakes and the panic reaches the caller.
    #[test]
    fn a_walker_panic_stops_sibling_walkers() {
        if completion_child("a_walker_panic_stops_sibling_walkers") {
            return;
        }
        // Each panic happens only after its sibling is waiting or polling.
        let seen = walk_jobs(vec![Job::Lend, Job::Hog, Job::Wait]);
        assert_only_panic(&seen, "hog fails holding the budget");
        let seen = walk_jobs(vec![Job::Lend, Job::Panic, Job::Poll]);
        assert_only_panic(&seen, "walker fails");
    }

    // Streaming keeps the input reservation while the publisher consumes an
    // active task. A sibling waiting for that budget must not strand either
    // thread, including when publication stops or the stream owner panics.
    #[test]
    fn active_lane_streams_release_budget_waiters_and_channel_senders() {
        if completion_child("active_lane_streams_release_budget_waiters_and_channel_senders") {
            return;
        }
        enum Handoff {
            Held,
            Stream(std::sync::mpsc::Receiver<usize>),
        }
        for mode in 0..3 {
            let waiting = std::sync::Arc::new(AtomicBool::new(false));
            let _seams = seam::Seams {
                on_budget_wait: Some({
                    let waiting = waiting.clone();
                    std::sync::Arc::new(move || waiting.store(true, Ordering::SeqCst))
                }),
                ..Default::default()
            }
            .set();
            let mut held = false;
            let mut received = 0;
            let seen = panics(|| {
                lane_groups(
                    2,
                    &AtomicBool::new(false),
                    |lanes, stopped| {
                        std::thread::scope(|scope| {
                            let _reservation = lanes[0].reserve(u64::MAX, stopped).unwrap();
                            let sibling = &lanes[1];
                            scope.spawn(move || {
                                let _reservation = sibling.reserve(1, stopped);
                            });
                            while !waiting.load(Ordering::SeqCst) {
                                std::thread::yield_now();
                            }
                            assert!(lanes[0].hand(Handoff::Held, 0).is_continue());
                            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
                            assert!(lanes[0].handoff(Handoff::Stream(receiver), stopped));
                            for chunk in 0..32 {
                                if sender.send(chunk).is_err() {
                                    break;
                                }
                                assert!(mode != 2, "stream owner panics after its first chunk");
                            }
                        });
                    },
                    |handoff| match handoff {
                        Handoff::Held => {
                            held = true;
                            ControlFlow::Continue(())
                        }
                        Handoff::Stream(receiver) => {
                            assert!(held, "active handoff did not flush the held batch");
                            for chunk in receiver {
                                assert_eq!(chunk, received);
                                received += 1;
                                if mode == 1 {
                                    return ControlFlow::Break(());
                                }
                            }
                            ControlFlow::Continue(())
                        }
                    },
                )
                .unwrap();
            });
            if mode == 2 {
                assert_only_panic(&seen, "stream owner panics after its first chunk");
            } else {
                assert_eq!(seen, Vec::<String>::new());
            }
            assert_eq!(received, if mode == 0 { 32 } else { 1 });
        }
    }

    // An active task cannot wait for its own synchronous publisher. Both
    // worker-start and producer-start fallback must expose that same mode.
    #[test]
    fn active_handoffs_publish_synchronously_on_serial_and_spawn_fallback_paths() {
        if completion_child(
            "active_handoffs_publish_synchronously_on_serial_and_spawn_fallback_paths",
        ) {
            return;
        }
        for (threads, fail_spawn_at) in [(1, None), (2, Some(0)), (2, Some(2))] {
            let _seams = seam::Seams {
                fail_spawn_at,
                ..Default::default()
            }
            .set();
            let mut tasks = (0..4).map(|task| (1, task));
            let mut emitted = Vec::new();
            completion_groups_with_handoff(
                GroupSchedule::Streaming(threads),
                &AtomicBool::new(false),
                |_| tasks.next(),
                || (),
                |_, task, _, handoff, concurrent| {
                    assert!(!concurrent);
                    assert!(handoff(task * 2));
                    task * 2 + 1
                },
                |_| 0,
                |item| {
                    emitted.push(item);
                    ControlFlow::Continue(())
                },
            );
            assert_eq!(emitted, (0..8).collect::<Vec<_>>());
        }
    }
}
