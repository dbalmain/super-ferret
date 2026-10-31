//! [`Inverter`]: `(DocId, token)` occurrences in DocId order become
//! `(term, DocIds)` in term order. DocIds only rise, so each term's list is
//! append-only and deduplicated by comparing against its last entry; there
//! is no sort of postings, only of terms, once, when the segment is written.

use std::collections::HashMap;

use super::{WriteError, Writer};

/// Estimated bytes per distinct term beyond its own bytes and postings: the
/// key, a long key's box, the list header and the map's slot.
const PER_TERM: usize = 64;

/// Accumulates one segment's postings in memory, and says when they reach
/// the bound the caller set.
#[derive(Debug)]
pub struct Inverter {
    terms: HashMap<Key, Docs>,
    memory: usize,
    bound: usize,
    last: Option<u32>,
    payload_capacity: usize,
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
            payload_capacity: 0,
        }
    }

    /// Records that `doc` holds `term`. `doc` must not be below any document
    /// added before; repeats of a `(doc, term)` pair are free.
    pub fn add(&mut self, doc: u32, term: &[u8]) -> Result<(), WriteError> {
        if self.last.is_some_and(|last| doc < last) {
            return Err(WriteError::DocOrder);
        }
        self.last = Some(doc);
        let Some(docs) = self.terms.get_mut(term) else {
            let key = Key::new(term);
            self.memory += term.len() + PER_TERM;
            self.payload_capacity += key.heap_bytes();
            self.terms.insert(key, Docs::One(doc));
            return Ok(());
        };
        let grown = docs.push(doc) * size_of::<u32>();
        self.memory += grown;
        self.payload_capacity += grown;
        Ok(())
    }

    /// Estimated bytes held.
    pub fn memory(&self) -> usize {
        self.memory
    }

    /// Capacity charge for keys, posting vectors and the hash table. The
    /// table charge includes empty buckets and control bytes, rounded up to
    /// a power-of-two bucket count; excludes allocator metadata.
    pub fn allocated_bytes(&self) -> usize {
        self.payload_capacity + self.table_bytes()
    }

    fn table_bytes(&self) -> usize {
        if self.terms.capacity() == 0 {
            return 0;
        }
        let buckets = (self.terms.capacity() + 1).next_power_of_two();
        buckets * (size_of::<(Key, Docs)>() + 1) + 16
    }

    /// Capacity charge including the vector of entries needed at drain.
    pub fn flush_bytes(&self) -> usize {
        self.allocated_bytes() + self.terms.len() * size_of::<(Key, Docs)>()
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
        on_entry: impl FnMut(&[u8], &[u32], u64),
    ) -> Result<Writer, WriteError> {
        self.drain(Writer::new(first, last)?, on_entry, |_| {})
    }

    /// Like [`Self::drain_into`], reporting simultaneous capacities of the
    /// table, sorted entries, postings and writer after each encoded entry.
    pub fn drain_into_measured(
        &mut self,
        first: u32,
        last: u32,
        observe: impl FnMut(usize),
    ) -> Result<Writer, WriteError> {
        self.drain(Writer::new(first, last)?, |_, _, _| {}, observe)
    }

    /// Drains through the selected dictionary codec.
    pub fn drain_into_codec(
        &mut self,
        first: u32,
        last: u32,
        codec: u32,
    ) -> Result<Writer, WriteError> {
        self.drain(
            Writer::with_codec(first, last, codec)?,
            |_, _, _| {},
            |_| {},
        )
    }

    fn drain(
        &mut self,
        mut writer: Writer,
        mut on_entry: impl FnMut(&[u8], &[u32], u64),
        mut observe: impl FnMut(usize),
    ) -> Result<Writer, WriteError> {
        let payload = self.payload_capacity;
        let mut terms: Vec<_> = self.terms.drain().collect();
        let allocated = payload + self.table_bytes() + terms.capacity() * size_of::<(Key, Docs)>();
        self.memory = 0;
        self.payload_capacity = 0;
        self.last = None;
        terms.sort_unstable_by(|a, b| a.0.as_slice().cmp(b.0.as_slice()));
        for (term, docs) in &terms {
            let (term, docs) = (term.as_slice(), docs.as_slice());
            let bytes = writer.push(term, docs)?;
            on_entry(term, docs, bytes);
            observe(allocated + writer.allocated_bytes());
        }
        Ok(writer)
    }
}

/// A term's documents. Most terms in a segment occur in one document, so
/// that one lives in the table slot and only a second document allocates.
#[derive(Debug)]
enum Docs {
    One(u32),
    Many(Vec<u32>),
}

impl Docs {
    /// Appends `doc` unless it is already last, returning how many `u32`s of
    /// capacity were allocated.
    fn push(&mut self, doc: u32) -> usize {
        match self {
            Self::One(first) if *first == doc => 0,
            Self::One(first) => {
                // Four, as `Vec`'s first growth of a `u32` list would give.
                let mut docs = Vec::with_capacity(4);
                docs.extend([*first, doc]);
                let capacity = docs.capacity();
                *self = Self::Many(docs);
                capacity
            }
            Self::Many(docs) if docs.last() == Some(&doc) => 0,
            Self::Many(docs) => {
                let capacity = docs.capacity();
                docs.push(doc);
                docs.capacity() - capacity
            }
        }
    }

    fn as_slice(&self) -> &[u32] {
        match self {
            Self::One(doc) => std::slice::from_ref(doc),
            Self::Many(docs) => docs,
        }
    }
}

/// A term's bytes as a table key. Most terms are short, so they live in the
/// slot itself and only a long term allocates.
#[derive(Debug)]
enum Key {
    Inline(u8, [u8; INLINE]),
    Heap(Box<[u8]>),
}

/// Longest term kept in the slot; with the length byte the key is no larger
/// than the boxed form's 16 bytes plus its tag.
const INLINE: usize = 22;

impl Key {
    fn new(term: &[u8]) -> Self {
        match u8::try_from(term.len()) {
            Ok(len) if usize::from(len) <= INLINE => {
                let mut bytes = [0; INLINE];
                bytes[..term.len()].copy_from_slice(term);
                Self::Inline(len, bytes)
            }
            _ => Self::Heap(term.into()),
        }
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Inline(len, bytes) => &bytes[..usize::from(*len)],
            Self::Heap(bytes) => bytes,
        }
    }

    /// Bytes allocated off the table for this key.
    fn heap_bytes(&self) -> usize {
        match self {
            Self::Inline(..) => 0,
            Self::Heap(bytes) => bytes.len(),
        }
    }
}

// `Borrow<[u8]>` requires the key to hash and compare as its bytes do.
impl std::hash::Hash for Key {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for Key {}

impl std::borrow::Borrow<[u8]> for Key {
    fn borrow(&self) -> &[u8] {
        self.as_slice()
    }
}
