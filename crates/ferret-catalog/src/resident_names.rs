//! Interned checkpoint names and counted row postings. Catalog accessors own
//! conversion after semantic validation; same-epoch overlays share this base.
//! The v4 encoder still obtains ordinary name bytes through those accessors.

use std::collections::HashMap;
use std::sync::OnceLock;

use intpack::{bits, pfor128};

use crate::{NameId, NameReader};

/// An owned fixed-width array; its codec is intpack, not a second decoder.
pub(crate) struct Keys {
    width: u32,
    bytes: Vec<u8>,
}

impl Keys {
    pub(crate) fn new(values: &[u32]) -> Self {
        let width = u32::BITS - values.iter().copied().max().unwrap_or(0).leading_zeros();
        let mut bytes = Vec::with_capacity((values.len() * width as usize).div_ceil(8) + 8);
        let mut writer = bits::Writer::new(&mut bytes);
        for &value in values {
            writer.put(u64::from(value), width);
        }
        bytes.extend_from_slice(&[0; 8]);
        Self { width, bytes }
    }
    pub(crate) fn get(&self, index: usize) -> u32 {
        bits::Reader::new(&self.bytes).get(index * self.width as usize, self.width) as u32
    }
    pub(crate) fn bytes(&self) -> usize {
        self.bytes.len()
    }
}

/// Immutable base dictionary, packed row keys and PFor row postings. Keys are
/// local to this object, never durable ids or handles across an epoch.
pub struct ResidentNames {
    table: Vec<u8>,
    offsets: Keys,
    keys: Keys,
    postings: PackedNameLists,
    distinct: u32,
    rows: u32,
    legacy: OnceLock<Legacy>,
}

struct Legacy {
    heap: Vec<u8>,
    offsets: Vec<usize>,
}

impl ResidentNames {
    pub(crate) fn build(names: NameReader<'_>, rows: u32) -> Self {
        // Borrow conversion scratch from the checked source; it is released
        // before that source is replaced, rather than kept beside the table.
        let row_names: Vec<_> = names.runs_from(NameId(0)).map(|(_, n)| n.bytes).collect();
        debug_assert_eq!(row_names.len(), rows as usize);
        let mut distinct = row_names.clone();
        distinct.sort_unstable();
        distinct.dedup();
        let lookup: HashMap<_, _> = distinct
            .iter()
            .enumerate()
            .map(|(i, &name)| (name, i as u32))
            .collect();
        let ids: Vec<_> = row_names.iter().map(|name| lookup[name]).collect();
        let mut table = Vec::new();
        let mut offsets = Vec::with_capacity(distinct.len() + 1);
        for name in &distinct {
            offsets.push(
                u32::try_from(table.len())
                    .unwrap_or_else(|_| panic!("resident name table exceeds u32")),
            );
            table.extend_from_slice(name);
            table.push(0);
        }
        offsets.push(
            u32::try_from(table.len())
                .unwrap_or_else(|_| panic!("resident name table exceeds u32")),
        );
        // One row array and a prefix sum, rather than one allocation per name.
        let mut counts = vec![0u32; distinct.len()];
        for &key in &ids {
            counts[key as usize] += 1;
        }
        let mut starts = vec![0usize; counts.len() + 1];
        for (i, &count) in counts.iter().enumerate() {
            starts[i + 1] = starts[i] + count as usize;
        }
        let mut next = starts.clone();
        let mut postings_rows = vec![0u32; rows as usize];
        for (row, &key) in ids.iter().enumerate() {
            postings_rows[next[key as usize]] = row as u32;
            next[key as usize] += 1;
        }
        let postings = PackedNameLists::new(
            (0..counts.len()).map(|i| &postings_rows[starts[i]..starts[i + 1]]),
        );
        Self {
            table,
            offsets: Keys::new(&offsets),
            keys: Keys::new(&ids),
            postings,
            distinct: distinct.len() as u32,
            rows,
            legacy: OnceLock::new(),
        }
    }

    pub fn distinct_count(&self) -> u32 {
        self.distinct
    }
    pub fn row_count(&self) -> u32 {
        self.rows
    }
    pub fn distinct_name(&self, key: u32) -> &[u8] {
        assert!(key < self.distinct);
        let start = self.offsets.get(key as usize) as usize;
        let end = self.offsets.get(key as usize + 1) as usize;
        &self.table[start..end - 1]
    }
    pub fn key(&self, row: NameId) -> u32 {
        assert!(row.0 < self.rows);
        self.keys.get(row.0 as usize)
    }
    pub(crate) fn name(&self, row: NameId) -> &[u8] {
        self.distinct_name(self.key(row))
    }
    pub fn count(&self, key: u32) -> u32 {
        assert!(key < self.distinct);
        self.postings.count(key)
    }
    /// Appends ascending base row ids; effective callers must filter deaths and
    /// replacements against their pinned catalog before returning candidates.
    pub fn postings(&self, key: u32, out: &mut Vec<u32>) {
        assert!(key < self.distinct);
        self.postings.get(key, out);
    }
    pub fn bytes(&self) -> usize {
        self.table.len() + self.offsets.bytes() + self.keys.bytes() + self.postings.bytes()
    }
    // Compatibility for explicitly requested raw-heap scans. Resident engine
    // execution uses dictionary/posting APIs and never creates this copy.
    fn legacy(&self) -> &Legacy {
        self.legacy.get_or_init(|| {
            let mut heap = Vec::new();
            let mut offsets = Vec::with_capacity(self.rows as usize);
            for row in 0..self.rows {
                offsets.push(heap.len());
                heap.extend_from_slice(self.name(NameId(row)));
                heap.push(0);
            }
            Legacy { heap, offsets }
        })
    }
    pub(crate) fn legacy_heap(&self) -> &[u8] {
        &self.legacy().heap
    }
    pub(crate) fn legacy_start(&self, row: NameId) -> usize {
        self.legacy().offsets[row.0 as usize]
    }
    pub(crate) fn legacy_at(&self, offset: usize) -> Option<NameId> {
        let legacy = self.legacy();
        (offset < legacy.heap.len())
            .then(|| NameId(legacy.offsets.partition_point(|&start| start <= offset) as u32 - 1))
    }
}

/// Counted packed lists of name keys or rows. Query's term dictionary uses the
/// same opaque codec as catalog's row postings; no codec bytes cross its API.
pub struct PackedNameLists {
    counts: Keys,
    offsets: Keys,
    bytes: Vec<u8>,
}
impl PackedNameLists {
    pub fn new<'a>(lists: impl Iterator<Item = &'a [u32]>) -> Self {
        let (mut counts, mut offsets, mut bytes) = (Vec::new(), Vec::new(), Vec::new());
        for list in lists {
            assert!(
                list.windows(2).all(|pair| pair[0] < pair[1]),
                "name postings must ascend"
            );
            counts.push(
                u32::try_from(list.len()).unwrap_or_else(|_| panic!("name postings exceed u32")),
            );
            offsets.push(
                u32::try_from(bytes.len())
                    .unwrap_or_else(|_| panic!("packed name postings exceed u32")),
            );
            pfor128::encode_sorted(list, &mut bytes);
        }
        offsets.push(
            u32::try_from(bytes.len())
                .unwrap_or_else(|_| panic!("packed name postings exceed u32")),
        );
        Self {
            counts: Keys::new(&counts),
            offsets: Keys::new(&offsets),
            bytes,
        }
    }
    pub fn count(&self, list: u32) -> u32 {
        self.counts.get(list as usize)
    }
    pub fn get(&self, list: u32, out: &mut Vec<u32>) {
        let start = self.offsets.get(list as usize) as usize;
        let end = self.offsets.get(list as usize + 1) as usize;
        pfor128::decode_sorted(self.count(list) as usize, &self.bytes[start..end], out);
    }
    pub fn bytes(&self) -> usize {
        self.counts.bytes() + self.offsets.bytes() + self.bytes.len()
    }
}
