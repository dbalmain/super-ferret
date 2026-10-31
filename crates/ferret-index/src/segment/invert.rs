//! [`Inverter`]: `(DocId, token)` occurrences in DocId order become
//! `(term, DocIds)` in term order. DocIds only rise, so each term's list is
//! append-only and deduplicated by comparing against its last entry; there
//! is no sort of postings, only of terms, once, when the segment is written.

use std::collections::HashMap;
use std::hash::{BuildHasher, BuildHasherDefault, Hash, Hasher, RandomState};
use std::sync::LazyLock;

use super::{WriteError, Writer};

/// Estimated bytes per distinct term beyond its own bytes and postings: the
/// key, a long key's box, the list header and the map's slot.
const PER_TERM: usize = 64;

/// Accumulates one segment's postings in memory, and says when they reach
/// the bound the caller set.
#[derive(Debug)]
pub struct Inverter {
    terms: HashMap<Key, Docs, BuildHasherDefault<Passed>>,
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
            terms: HashMap::default(),
            memory: 0,
            bound,
            last: None,
            payload_capacity: 0,
        }
    }

    /// Records that `doc` holds `term`. `doc` must not be below any document
    /// added before; repeats of a `(doc, term)` pair are free.
    pub fn add(&mut self, doc: u32, term: &[u8]) -> Result<(), WriteError> {
        self.add_hashed(doc, term_hash(term), term)
    }

    /// [`Inverter::add`] with `term`'s [`term_hash`], computed by the caller,
    /// so that a reader thread can take the hashing off the writer's.
    pub(crate) fn add_hashed(
        &mut self,
        doc: u32,
        hash: u64,
        term: &[u8],
    ) -> Result<(), WriteError> {
        debug_assert_eq!(hash, term_hash(term));
        if self.last.is_some_and(|last| doc < last) {
            return Err(WriteError::DocOrder);
        }
        self.last = Some(doc);
        let probe: &dyn Lookup = &(hash, term);
        let Some(docs) = self.terms.get_mut(probe) else {
            let key = Key::new(hash, term);
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
        // Most terms differ in their first eight bytes, which compare as one
        // integer; only a tie compares the rest.
        terms.sort_unstable_by(|a, b| {
            a.0.prefix()
                .cmp(&b.0.prefix())
                .then_with(|| a.0.as_slice().cmp(b.0.as_slice()))
        });
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

/// The hash an [`Inverter`]'s table files `term` under. The seed is random
/// per process, as std's maps' are, and shared by every thread.
pub(crate) fn term_hash(term: &[u8]) -> u64 {
    static SEED: LazyLock<RandomState> = LazyLock::new(RandomState::new);
    SEED.hash_one(term)
}

/// A term as a table key: its bytes and its [`term_hash`], which the table
/// uses as given rather than hashing the bytes again.
#[derive(Debug)]
struct Key {
    hash: u64,
    bytes: Bytes,
}

/// Most terms are short, so they live in the slot itself and only a long
/// term allocates.
#[derive(Debug)]
enum Bytes {
    Inline(u8, [u8; INLINE]),
    Heap(Box<[u8]>),
}

/// Longest term kept in the slot; with the length byte the key is no larger
/// than the boxed form's 16 bytes plus its tag.
const INLINE: usize = 22;

impl Key {
    fn new(hash: u64, term: &[u8]) -> Self {
        let bytes = match u8::try_from(term.len()) {
            Ok(len) if usize::from(len) <= INLINE => {
                let mut bytes = [0; INLINE];
                bytes[..term.len()].copy_from_slice(term);
                Bytes::Inline(len, bytes)
            }
            _ => Bytes::Heap(term.into()),
        };
        Self { hash, bytes }
    }

    fn as_slice(&self) -> &[u8] {
        match &self.bytes {
            Bytes::Inline(len, bytes) => &bytes[..usize::from(*len)],
            Bytes::Heap(bytes) => bytes,
        }
    }

    /// The first eight bytes, zero-padded, as a big-endian integer: ordered
    /// as the bytes are, up to ties.
    fn prefix(&self) -> u64 {
        let mut prefix = [0; 8];
        match &self.bytes {
            // Past its length an inline key is zeros.
            Bytes::Inline(_, bytes) => prefix.copy_from_slice(&bytes[..8]),
            // A boxed key is longer than an inline one can be.
            Bytes::Heap(bytes) => prefix.copy_from_slice(&bytes[..8]),
        }
        u64::from_be_bytes(prefix)
    }

    /// Bytes allocated off the table for this key.
    fn heap_bytes(&self) -> usize {
        match &self.bytes {
            Bytes::Inline(..) => 0,
            Bytes::Heap(bytes) => bytes.len(),
        }
    }
}

/// What the table compares: a stored [`Key`], or a `(hash, term)` probe,
/// which looks a term up without building a key. Both hash as their stored
/// hash and compare as their bytes, as `Borrow` requires.
trait Lookup {
    fn hash(&self) -> u64;
    fn bytes(&self) -> &[u8];
}

impl Lookup for Key {
    fn hash(&self) -> u64 {
        self.hash
    }

    fn bytes(&self) -> &[u8] {
        self.as_slice()
    }
}

impl Lookup for (u64, &[u8]) {
    fn hash(&self) -> u64 {
        self.0
    }

    fn bytes(&self) -> &[u8] {
        self.1
    }
}

impl<'a> std::borrow::Borrow<dyn Lookup + 'a> for Key {
    fn borrow(&self) -> &(dyn Lookup + 'a) {
        self
    }
}

impl Hash for dyn Lookup + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(Lookup::hash(self));
    }
}

impl PartialEq for dyn Lookup + '_ {
    fn eq(&self, other: &Self) -> bool {
        Lookup::hash(self) == Lookup::hash(other) && self.bytes() == other.bytes()
    }
}

impl Eq for dyn Lookup + '_ {}

impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash);
    }
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash && self.as_slice() == other.as_slice()
    }
}

impl Eq for Key {}

/// Hands the table a [`term_hash`] unchanged: it is already SipHash.
#[derive(Default)]
struct Passed(u64);

impl Hasher for Passed {
    fn write(&mut self, _: &[u8]) {
        unreachable!("an inverter key hashes as its stored u64");
    }

    fn write_u64(&mut self, hash: u64) {
        self.0 = hash;
    }

    fn finish(&self) -> u64 {
        self.0
    }
}
