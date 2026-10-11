//! Graduation batches candidates by document. Input dictionary spellings
//! are collected once; a fingerprint alone never licenses a certain hit.
//! Codec 5 checks offset windows and reuses a full fallback for that document,
//! rather than rereading a large document once per matching term.

use std::collections::HashMap;

use ferret_text::{Scratch, cap, tokenize};

use crate::segment::{DocumentReader, ReadAt, Record, Segment, fingerprint, verify_record};
use crate::{DocSet, Error};

pub(super) struct Occurrence {
    pub term: Box<[u8]>,
    pub record: Record,
    pub removable: bool,
}

type Known = HashMap<u32, Vec<Box<[u8]>>>;

pub(super) fn collect<R: ReadAt>(
    inputs: &[Segment<R>],
    live: &DocSet,
    offsets: bool,
    read: &mut DocumentReader<'_>,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<Occurrence>, Error> {
    let mut known = Known::new();
    for input in inputs {
        let mut terms = input.terms();
        while let Some((term, _)) = terms.next_term()? {
            if cancelled() {
                return Err(Error::Cancelled);
            }
            let spellings = known.entry(fingerprint(term)).or_default();
            if !spellings.iter().any(|t| t.as_ref() == term) {
                spellings.push(term.into());
            }
        }
    }
    let mut candidates = Vec::new();
    for input in inputs {
        let mut records = input.records();
        while let Some(record) = records.next_positioned_record()? {
            if cancelled() {
                return Err(Error::Cancelled);
            }
            if live.contains(record.doc) && known.contains_key(&record.fingerprint) {
                candidates.push(record);
            }
        }
    }
    candidates.sort_unstable_by_key(|r| (r.doc, r.fingerprint));
    let (mut bytes, mut scratch, mut out) = (Vec::new(), Scratch::default(), Vec::new());
    let mut at = 0;
    while at < candidates.len() {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        let doc = candidates[at].doc;
        let end = at + candidates[at..].partition_point(|r| r.doc == doc);
        let records = &candidates[at..end];
        at = end;
        if !offsets {
            bytes.clear();
            if let Some(window) = read(doc, None, &mut bytes)
                && window.complete
                && window.start == 0
            {
                full(
                    doc,
                    records,
                    &known,
                    &bytes,
                    &mut scratch,
                    &mut out,
                    cancelled,
                )?;
            }
            continue;
        }
        let mut window_hits = Vec::new();
        'records: for record in records {
            for term in &known[&record.fingerprint] {
                if cancelled() {
                    return Err(Error::Cancelled);
                }
                let mut complete = false;
                let mut reader = |doc, offset, bytes: &mut Vec<u8>| {
                    let window = read(doc, offset, bytes)?;
                    complete = window.complete && window.start == 0;
                    Some(window)
                };
                let verified = verify_record(
                    doc,
                    term,
                    record.offset,
                    record.offset.is_some(),
                    &mut reader,
                    &mut bytes,
                );
                if complete {
                    // Recheck all candidates from these same stable full
                    // bytes. This also replaces earlier window decisions
                    // with the full-document collision check.
                    window_hits.clear();
                    full(
                        doc,
                        records,
                        &known,
                        &bytes,
                        &mut scratch,
                        &mut out,
                        cancelled,
                    )?;
                    break 'records;
                }
                if let Some(verified) = verified {
                    window_hits.push(Occurrence {
                        term: term.clone(),
                        record: Record {
                            offset: Some(verified.offset),
                            ..*record
                        },
                        removable: verified.removable,
                    });
                }
            }
        }
        out.extend(window_hits);
    }
    out.sort_unstable_by(|a, b| a.term.cmp(&b.term).then(a.record.doc.cmp(&b.record.doc)));
    Ok(out)
}

fn full(
    doc: u32,
    records: &[Record],
    known: &Known,
    bytes: &[u8],
    scratch: &mut Scratch,
    out: &mut Vec<Occurrence>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), Error> {
    let mut terms = HashMap::<Box<[u8]>, u32>::new();
    tokenize(bytes, scratch, |token| {
        let term = cap(token.bytes);
        let fp = fingerprint(term);
        if records.binary_search_by_key(&fp, |r| r.fingerprint).is_ok()
            && let Ok(offset) = u32::try_from(token.run.start)
        {
            terms.entry(term.into()).or_insert(offset);
        }
    });
    let mut counts = HashMap::<u32, usize>::new();
    for term in terms.keys() {
        *counts.entry(fingerprint(term)).or_default() += 1;
    }
    for (term, offset) in terms {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        let fp = fingerprint(&term);
        // These spellings came from real input dictionary cursors, not
        // from fingerprint equality or unverified document bytes.
        if known[&fp].contains(&term) {
            out.push(Occurrence {
                term,
                record: Record {
                    fingerprint: fp,
                    doc,
                    offset: Some(offset),
                },
                removable: counts[&fp] == 1,
            });
        }
    }
    Ok(())
}
