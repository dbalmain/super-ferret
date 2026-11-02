//! Handling one position whole: S1's tests, the index truth, the read if
//! this thread owns the document, and the residual; and settling a later
//! name of a document already read.

use super::*;

impl<'q, 'c, 'p> Scan<'q, 'c, 'p> {
    /// Handles position `at` whole: S1's tests, the index truth, the read
    /// if this thread owns it, and the residual. A row's path goes into
    /// `paths`.
    pub(super) fn item(
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
        let doc = self.catalog.doc(row.inode).map(|d| d.0);
        let how = match (self.truth(row, doc), doc) {
            (Truth::No, _) | (Truth::Maybe, None) => return Item::Skip,
            (Truth::Yes, _) => How::Holds,
            (Truth::Maybe, Some(doc)) if self.owns(at, doc) => {
                self.read_owned(row, doc, hand, pool, stop)
            }
            (Truth::Maybe, Some(doc)) if pool.is_some() => self.revisit(row, doc, hand, pool, stop),
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

    /// `row`'s residual, with content tests answered by the index for its
    /// document `doc`.
    fn truth(&self, row: &Row<'_>, doc: Option<u32>) -> Truth {
        let probed = doc.and_then(|d| self.docs.binary_search(&d).ok());
        let indexed = |i: usize| match probed.and_then(|j| self.table[j * self.width + i]) {
            None => Truth::No,
            Some(Certainty::Maybe) => Truth::Maybe,
            Some(Certainty::Yes) => Truth::Yes,
        };
        self.query.eval_residual(&mut |test| match test {
            Test::Content(i) => indexed(*i),
            other => self.query.test_row(self.catalog, row, other),
        })
    }

    /// Reads `doc`, this position's to read, sharing a catalogued read's
    /// answers with the document's later names on a pool.
    fn read_owned(
        &self,
        row: &Row<'_>,
        doc: u32,
        hand: &mut Hand<'_, '_>,
        pool: Option<&Pool>,
        stop: &dyn Fn() -> bool,
    ) -> How {
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
        let holds = self.read_holds(row, &outcome);
        How::Read {
            doc,
            outcome,
            holds,
        }
    }

    /// A later name on a worker, whose reader found the catalogued version:
    /// what the calling thread would do on reaching it, done here. A stat; a
    /// read only if the path changed.
    fn revisit(
        &self,
        row: &Row<'_>,
        doc: u32,
        hand: &mut Hand<'_, '_>,
        pool: Option<&Pool>,
        stop: &dyn Fn() -> bool,
    ) -> How {
        let found = lock(self.shard(doc)).get(&doc).cloned();
        // Not read yet, or not cacheable: left to the calling thread.
        let Some(Some(answers)) = found else {
            return How::Here(doc);
        };
        match (hand.read)(row.name, ReadRequest::Stat) {
            Some(ReadVersion::Catalogued) => How::Cached {
                doc,
                holds: holds(self.query, self.catalog, row, &answers),
            },
            Some(ReadVersion::Current) => {
                let outcome = self.read(row, hand, pool, stop);
                let holds = self.read_holds(row, &outcome);
                How::Again {
                    doc,
                    outcome,
                    holds,
                }
            }
            None => How::Gone(doc),
        }
    }

    /// Whether a read's answers make `row` hold, and its offset if so.
    fn read_holds(&self, row: &Row<'_>, outcome: &Outcome) -> Option<Option<usize>> {
        match outcome {
            Outcome::Read { answers, .. } => holds(self.query, self.catalog, row, answers),
            _ => None,
        }
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
