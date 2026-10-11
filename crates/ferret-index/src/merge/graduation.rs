//! C2 batches confirmation by document: reading a large file once per term
//! can multiply merge I/O by thousands. Candidate fingerprints come from the
//! input dictionaries, but exact dictionary membership and the indexed file
//! version are both checked before an occurrence can become certain.

use std::collections::{HashMap, HashSet};

use ferret_text::{Scratch, cap, tokenize};

use crate::segment::{DocumentReader, ReadAt, Record, Segment, fingerprint};
use crate::{DocSet, Error};

pub(super) struct Occurrence {
    pub term: Box<[u8]>,
    pub record: Record,
    pub removable: bool,
}

pub(super) fn collect<R: ReadAt>(
    inputs: &[Segment<R>],
    live: &DocSet,
    read: &mut DocumentReader<'_>,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<Occurrence>, Error> {
    let mut known = HashSet::new();
    for input in inputs {
        let mut terms = input.terms();
        while let Some((term, _)) = terms.next_term()? {
            if cancelled() {
                return Err(Error::Cancelled);
            }
            known.insert(fingerprint(term));
        }
    }
    let mut candidates = Vec::new();
    for input in inputs {
        let mut records = input.records();
        while let Some(record) = records.next_positioned_record()? {
            if cancelled() {
                return Err(Error::Cancelled);
            }
            if live.contains(record.doc) && known.contains(&record.fingerprint) {
                candidates.push(record);
            }
        }
    }
    drop(known);
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
        bytes.clear();
        let Some(window) = read(doc, None, &mut bytes) else {
            continue;
        };
        if !window.complete || window.start != 0 {
            continue;
        }
        let mut terms = HashMap::<Box<[u8]>, u32>::new();
        tokenize(&bytes, &mut scratch, |token| {
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
            // The document's bytes supply the spelling, but the merge may
            // recover only a term whose text exists in an input dictionary.
            let mut retained = false;
            for input in inputs {
                if input.entry(&term)?.is_some() {
                    retained = true;
                    break;
                }
            }
            if retained {
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
    }
    out.sort_unstable_by(|a, b| a.term.cmp(&b.term).then(a.record.doc.cmp(&b.record.doc)));
    Ok(out)
}
