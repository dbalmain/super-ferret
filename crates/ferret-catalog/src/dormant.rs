//! Revivable identities (D60 A′). The host supplies coverage under its writer
//! gate; this crate stores permissions and never infers coverage from death.

use std::collections::BTreeSet;
use std::io::{self, Write};

use crate::format::{self, Column, Layout, Output, Section, u32_at};
use crate::{DecodeError, DocId, Hash};

pub(crate) const CAP: usize = 1_000_000;

/// Host authorization for covered last-reference deaths. Construct only from
/// the compatible, committed content view while holding the host writer gate.
#[derive(Clone, Debug)]
pub struct Coverage {
    frontier: u32,
    unreadable: Vec<u32>,
}
impl Coverage {
    pub fn new(frontier: u32, mut unreadable: Vec<u32>) -> Self {
        unreadable.sort_unstable();
        unreadable.dedup();
        Self {
            frontier,
            unreadable,
        }
    }

    pub(crate) fn covers(&self, id: DocId) -> bool {
        id.0 < self.frontier && self.unreadable.binary_search(&id.0).is_err()
    }
}

/// Keep the highest id for each hash, then evict the lowest ids on overflow.
/// Temporary sorting uses compact rows rather than a map per retained hash.
pub(crate) fn canonicalize(rows: &mut Vec<(DocId, Hash)>, cap: usize) {
    rows.sort_unstable_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)));
    rows.dedup_by_key(|row| row.1);
    rows.sort_unstable_by_key(|row| row.0);
    if rows.len() > cap {
        rows.drain(..rows.len() - cap);
    }
}

pub(crate) fn write(rows: &[(DocId, Hash)], out: &Output<'_>) -> io::Result<()> {
    let mut section = out.section(Section::Dormant);
    for (id, _) in rows {
        format::put_u32(&mut section, id.0)?;
    }
    for (_, hash) in rows {
        section.write_all(hash)?;
    }
    section.finish()?;
    Ok(())
}

pub(crate) fn rows(bytes: &[u8]) -> impl Iterator<Item = (DocId, Hash)> + '_ {
    let count = bytes.len() / 20;
    (0..count).map(move |row| {
        let mut hash = [0; 16];
        hash.copy_from_slice(&bytes[count * 4 + row * 16..count * 4 + (row + 1) * 16]);
        (DocId(u32_at(bytes, row * 4)), hash)
    })
}

pub(crate) fn check(layout: &Layout, bytes: &[u8], docs: &[u8]) -> Result<(), DecodeError> {
    let mut last = None;
    let mut hashes = Vec::with_capacity(bytes.len() / 20);
    let ids = layout.view(Column::DocId, docs);
    if bytes.len() / 20 > CAP {
        return Err(DecodeError::Corrupt("dormant cap"));
    }
    for (id, hash) in rows(bytes) {
        if id.0 >= layout.next_doc
            || last.is_some_and(|last| id <= last)
            || ids.sequence_row(u64::from(id.0), layout.docs).is_some()
        {
            return Err(DecodeError::Corrupt("dormant documents"));
        }
        last = Some(id);
        hashes.push(hash);
    }
    check_hashes(hashes)
}

pub(crate) fn check_hashes(mut hashes: Vec<Hash>) -> Result<(), DecodeError> {
    if hashes.len() > CAP {
        return Err(DecodeError::Corrupt("dormant cap"));
    }
    hashes.sort_unstable();
    if hashes.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(DecodeError::Corrupt("duplicate dormant hash"));
    }
    Ok(())
}

/// Authorizes only deaths in this transaction's pre-commit live view. A later
/// frontier cannot retroactively grant permission for a forgotten document.
pub(crate) fn prepare<'a>(
    view: &crate::Catalog,
    changes: &'a crate::log::ChangeSet,
    coverage: Option<&Coverage>,
    cap: usize,
) -> std::borrow::Cow<'a, crate::log::ChangeSet> {
    use crate::log::Record;
    let eligible: Vec<_> = changes
        .records
        .iter()
        .filter_map(|record| {
            let Record::DocDelete { id } = record else {
                return None;
            };
            let id = DocId(*id);
            coverage.filter(|coverage| coverage.covers(id))?;
            view.doc_hash(id).map(|hash| (id, hash))
        })
        .collect();
    if eligible.is_empty() {
        return std::borrow::Cow::Borrowed(changes);
    }
    let changed: BTreeSet<_> = changes
        .records
        .iter()
        .filter_map(|record| match record {
            Record::DocPut { id, .. }
            | Record::DocDelete { id }
            | Record::DocDormant { id, .. }
            | Record::DocForget { id } => Some(DocId(*id)),
            _ => None,
        })
        .collect();
    let mut retained: Vec<_> = view
        .dormant_docs()
        .filter(|(id, _)| !changed.contains(id))
        .collect();
    retained.extend(eligible.iter().copied());
    canonicalize(&mut retained, cap.min(CAP));
    let mut prepared = changes.clone();
    for record in &mut prepared.records {
        if let Record::DocDelete { id } = record
            && let Ok(at) = retained.binary_search_by_key(&DocId(*id), |row| row.0)
        {
            *record = Record::DocDormant {
                id: *id,
                hash: retained[at].1,
            };
        }
    }
    prepared.records.extend(
        view.dormant_docs()
            .filter(|(id, _)| {
                !changed.contains(id) && retained.binary_search_by_key(id, |row| row.0).is_err()
            })
            .map(|(id, _)| Record::DocForget { id: id.0 }),
    );
    std::borrow::Cow::Owned(prepared)
}
