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
//! [`IN_FLIGHT_BYTES`](crate::IN_FLIGHT_BYTES)) from a shared [`Budget`] of
//! that many bytes before reading it, and frees a buffer larger than
//! [`KEPT_BUFFER`] before returning the reservation. The workers' buffers
//! therefore hold at most `IN_FLIGHT_BYTES + threads × KEPT_BUFFER`, plus
//! whatever the files being read grew beyond their catalogued size (each still
//! under the reader's size cap). The calling thread's buffer is separate: it
//! reads one document at a time, as before, and keeps the capacity of the
//! largest it read. The ring holds finished rows, not documents: a path and a
//! few words per position.

use std::collections::HashMap;
use std::io::{self, Read};
use std::ops::{ControlFlow, Range};
#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::thread::Scope;
use std::time::{Duration, Instant};

use ferret_catalog::{Catalog, InoId, Kind, NameId, Target};
use ferret_index::Certainty;
use ferret_verify::{MatchStats, Search, TextMatcher};

use crate::budget::Budget;
use crate::expr::{Test, Truth};
use crate::plan::{ReadRequest, ReadVersion, Reader};
use crate::run::NameTester;
use crate::threads::{lock, spawn_named};
use crate::{ContentAtom, DocNames, Query, Row, RunError, Stats};

mod ring;

use ring::{Batch, LostOnPanic, Placed, Pool, StopOnDrop};

/// Makes one [`Reader`] per verifying thread. Each reader is used by one
/// thread only; the factory itself is shared.
pub type Fork<'a> = dyn Fn() -> Box<Reader<'a>> + Sync + 'a;

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

/// Starts worker `i` on the scope.
fn spawn<'scope>(
    scope: &'scope Scope<'scope, '_>,
    i: usize,
    work: impl FnOnce() + Send + 'scope,
) -> std::io::Result<()> {
    spawn_named(scope, i, "ferret-verify", work).map(drop)
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

        pub(crate) fn waiting_for_bytes(&self) {
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
mod partial;

/// [`stream`] against [`verify`], the whole-document search, on the same
/// bytes: every answer and count agree for line-local patterns, with the
/// bytes arriving a few at a time, so runs end at every kind of place, and
/// with a line longer than the buffer.
#[cfg(test)]
mod streamed;
