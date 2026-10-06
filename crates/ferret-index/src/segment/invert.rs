//! [`Inverter`]: `(DocId, token)` occurrences in DocId order become
//! `(term, DocIds)` in term order. DocIds only rise, so each term's list is
//! append-only and deduplicated by comparing against its last entry; there
//! is no sort of postings, only of terms, once, when the segment is written.

use std::collections::HashMap;

use super::{WriteError, Writer};

/// Estimated bytes per distinct term beyond its own bytes and postings: the
/// boxed key and list headers and the map's slot.
const PER_TERM: usize = 64;

/// Accumulates one segment's postings in memory, and says when they reach
/// the bound the caller set.
#[derive(Debug)]
pub struct Inverter {
    terms: HashMap<Box<[u8]>, Vec<u32>>,
    memory: usize,
    bound: usize,
    last: Option<u32>,
}

impl Inverter {
    /// An empty inverter that reports [`Inverter::is_full`] once its
    /// estimated memory reaches `bound` bytes.
    pub fn new(bound: usize) -> Self {
        Self {
            terms: HashMap::new(),
            memory: 0,
            bound,
            last: None,
        }
    }

    /// Records that `doc` holds `term`. `doc` must not be below any document
    /// added before; repeats of a `(doc, term)` pair are free.
    pub fn add(&mut self, doc: u32, term: &[u8]) -> Result<(), WriteError> {
        if self.last.is_some_and(|last| doc < last) {
            return Err(WriteError::DocOrder);
        }
        self.last = Some(doc);
        let docs = match self.terms.get_mut(term) {
            Some(docs) => docs,
            None => {
                self.memory += term.len() + PER_TERM;
                self.terms.entry(term.into()).or_default()
            }
        };
        if docs.last() != Some(&doc) {
            let capacity = docs.capacity();
            docs.push(doc);
            self.memory += (docs.capacity() - capacity) * size_of::<u32>();
        }
        Ok(())
    }

    /// Estimated bytes held.
    pub fn memory(&self) -> usize {
        self.memory
    }

    /// Whether the estimate has reached the bound.
    pub fn is_full(&self) -> bool {
        self.memory >= self.bound
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Drains everything added into a [`Writer`] over `[first, last]`, which
    /// must contain every document added, and leaves the inverter empty.
    pub fn drain_into(&mut self, first: u32, last: u32) -> Result<Writer, WriteError> {
        self.drain_into_with(first, last, |_, _, _| {})
    }

    /// Same as [`Inverter::drain_into`], calling `on_entry` with each term,
    /// its documents and the entry's real encoded size (`Writer::push`'s
    /// return value) as it is written, in term order.
    pub fn drain_into_with(
        &mut self,
        first: u32,
        last: u32,
        mut on_entry: impl FnMut(&[u8], &[u32], u64),
    ) -> Result<Writer, WriteError> {
        let mut writer = Writer::new(first, last)?;
        let mut terms: Vec<_> = self.terms.drain().collect();
        self.memory = 0;
        self.last = None;
        terms.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for (term, docs) in &terms {
            let bytes = writer.push(term, docs)?;
            on_entry(term, docs, bytes);
        }
        Ok(writer)
    }
}
