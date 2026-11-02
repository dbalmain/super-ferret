//! The calling thread's side of a verified scan: consuming positions in
//! order, settling later names of a document, and emitting rows.

use super::*;
use crate::verify::settle::{Settled, Verifier};

/// How a consumed position settled.
enum Verdict {
    /// Whether the row holds, and its offset if so.
    Holds(Option<Option<usize>>),
    /// The row holds by the run's cached answers for this document.
    Cached(u32),
    /// Nothing to emit; the run goes on.
    Skip,
    /// The run stops at this position.
    Stop,
}

/// Consumes positions in order: counts them, settles later names of a
/// document, and emits.
pub(super) struct Ledger<'s, 'q, 'c, 'p, 'r, 'a, E> {
    scan: &'s Scan<'q, 'c, 'p>,
    pub(super) verifier: Verifier<'q, 'r, 'a>,
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
    pub(super) fn new(
        scan: &'s Scan<'q, 'c, 'p>,
        read: &'r mut Reader<'a>,
        threads: usize,
        emit: E,
    ) -> Self {
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
    pub(super) fn halted(mut self, at: usize) -> (Stats, Tally) {
        self.halted_at = Some(at as u64 + 1);
        self.finished()
    }

    /// [`Ledger::halted`] on a pool, stopping its workers now.
    pub(super) fn halt(&mut self, pool: &Pool, at: usize) {
        self.halted_at = Some(at as u64 + 1);
        pool.halt();
    }

    pub(super) fn finished(self) -> (Stats, Tally) {
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

    /// Settles how a worker left `row`, whose document is `doc`, counting the
    /// document.
    fn verdict(&mut self, how: How, doc: Option<u32>, row: &Row<'_>) -> Verdict {
        let scan = self.scan;
        let known = |verifier: &Verifier<'_, '_, '_>, doc: u32| verifier.exact.contains_key(&doc);
        match how {
            How::Holds => {
                self.consumed(doc, false);
                Verdict::Holds(Some(None))
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
                    Settled::Unread => Verdict::Skip,
                    Settled::Stopped => Verdict::Stop,
                    Settled::Cached | Settled::Fresh(_) => Verdict::Holds(holds),
                }
            }
            How::Cached { doc: read, holds } if known(&self.verifier, read) => {
                self.consumed(doc, true);
                Verdict::Holds(holds)
            }
            How::Gone(read) if known(&self.verifier, read) => {
                self.consumed(doc, true);
                self.verifier.tally.changed += 1;
                Verdict::Skip
            }
            How::Again {
                doc: read,
                outcome,
                holds,
            } if known(&self.verifier, read) => {
                self.consumed(doc, true);
                match self.verifier.settle(read, outcome, true) {
                    Settled::Unread => Verdict::Skip,
                    Settled::Stopped => Verdict::Stop,
                    Settled::Cached => Verdict::Cached(read),
                    Settled::Fresh(_) => Verdict::Holds(holds),
                }
            }
            How::Read { doc: read, .. }
            | How::Cached { doc: read, .. }
            | How::Gone(read)
            | How::Again { doc: read, .. }
            | How::Here(read) => {
                self.consumed(doc, true);
                let stopped = || scan.stopped();
                match self.verifier.here(read, row.name, &stopped) {
                    Settled::Unread => Verdict::Skip,
                    Settled::Stopped => Verdict::Stop,
                    Settled::Cached => Verdict::Cached(read),
                    Settled::Fresh(answers) => {
                        Verdict::Holds(holds(scan.query, scan.catalog, row, &answers))
                    }
                }
            }
        }
    }

    /// Consumes one position; `Break` stops the run there.
    pub(super) fn consume(
        &mut self,
        item: Item,
        paths: &[u8],
    ) -> Result<ControlFlow<()>, RunError> {
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
        let offset = match self.verdict(pending.how, pending.doc, &row) {
            Verdict::Holds(holds) => holds,
            Verdict::Cached(read) => {
                holds(scan.query, scan.catalog, &row, &self.verifier.exact[&read])
            }
            Verdict::Skip => return Ok(ControlFlow::Continue(())),
            Verdict::Stop => return Ok(ControlFlow::Break(())),
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

// ── the pool ──
