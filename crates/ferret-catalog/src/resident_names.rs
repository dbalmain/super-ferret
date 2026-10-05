//! Interned checkpoint names and counted row postings. Catalog accessors own
//! conversion after semantic validation; same-epoch overlays share this base.
//! The v4 encoder still obtains ordinary name bytes through those accessors.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::OnceLock;

use intpack::{bits, pfor128};

use crate::{NameId, NameReader};

#[derive(Default)]
struct NameHasher(u64);

impl Hasher for NameHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.0 = (self.0.rotate_left(5) ^ u64::from_le_bytes(word))
                .wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }
}

/// An owned fixed-width array; its codec is intpack, not a second decoder.
pub(crate) struct Keys {
    width: u32,
    len: usize,
    bytes: Vec<u8>,
}

impl Keys {
    pub(crate) fn new(values: &[u32]) -> Self {
        Self::from_values(values.iter().copied().map(u64::from))
    }
    fn from_values(values: impl ExactSizeIterator<Item = u64> + Clone) -> Self {
        let len = values.len();
        let width = u64::BITS - values.clone().max().unwrap_or(0).leading_zeros();
        assert!(
            width <= 57,
            "resident byte offset exceeds addressable codec width"
        );
        let mut bytes = Vec::with_capacity((values.len() * width as usize).div_ceil(8) + 8);
        let mut writer = bits::Writer::new(&mut bytes);
        for value in values {
            writer.put(value, width);
        }
        bytes.extend_from_slice(&[0; 8]);
        Self { width, len, bytes }
    }
    fn get_offset(&self, index: usize) -> usize {
        assert!(index < self.len, "resident packed key out of range");
        bits::Reader::new(&self.bytes).get(index * self.width as usize, self.width) as usize
    }
    pub(crate) fn get(&self, index: usize) -> u32 {
        self.get_offset(index) as u32
    }
    pub(crate) fn bytes(&self) -> usize {
        self.bytes.len()
    }
}

/// Sorted byte strings stored in one byte table with packed offsets.
/// Strings are byte values: empty and non-UTF-8 entries are valid.
pub struct PackedStrings {
    table: Vec<u8>,
    offsets: Keys,
}

impl PackedStrings {
    /// Packs strings in the supplied order. The caller keeps them sorted when
    /// binary search is required.
    pub fn new(strings: impl IntoIterator<Item = impl AsRef<[u8]>>) -> Self {
        let strings: Vec<_> = strings.into_iter().collect();
        let mut table = Vec::new();
        let mut offsets = Vec::with_capacity(strings.len() + 1);
        for string in strings {
            offsets.push(table.len() as u64);
            table.extend_from_slice(string.as_ref());
        }
        offsets.push(table.len() as u64);
        Self {
            table,
            offsets: Keys::from_values(offsets.into_iter()),
        }
    }

    pub fn len(&self) -> usize {
        self.offsets.len - 1
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn get(&self, index: usize) -> &[u8] {
        let start = self.offsets.get_offset(index);
        let end = self.offsets.get_offset(index + 1);
        &self.table[start..end]
    }
    pub fn binary_search(&self, needle: &[u8]) -> Result<usize, usize> {
        let mut low = 0;
        let mut high = self.len();
        while low < high {
            let mid = low + (high - low) / 2;
            match self.get(mid).cmp(needle) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return Ok(mid),
            }
        }
        Err(low)
    }
    pub fn bytes(&self) -> usize {
        self.table.capacity() + self.offsets.bytes()
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
    build_time: std::time::Duration,
}

struct Legacy {
    heap: Vec<u8>,
    offsets: Vec<usize>,
}

impl ResidentNames {
    pub(crate) fn build(names: NameReader<'_>, rows: u32) -> Self {
        let started = std::time::Instant::now();
        // Intern while streaming rows. Sort only distinct names, never one
        // borrowed slice per row; repeated names must not multiply build
        // scratch.
        let mut lookup: HashMap<_, _, BuildHasherDefault<NameHasher>> = HashMap::default();
        let mut ids = Vec::with_capacity(rows as usize);
        for (_, name) in names.runs_from(NameId(0)) {
            let next = lookup.len() as u32;
            ids.push(*lookup.entry(name.bytes).or_insert(next));
        }
        debug_assert_eq!(ids.len(), rows as usize);
        let mut distinct: Vec<_> = lookup.into_iter().collect();
        distinct.sort_unstable_by_key(|&(name, _)| name);
        let mut remap = vec![0u32; distinct.len()];
        for (key, &(_, old)) in distinct.iter().enumerate() {
            remap[old as usize] = key as u32;
        }
        for key in &mut ids {
            *key = remap[*key as usize];
        }
        let mut table = Vec::new();
        let mut offsets = Vec::with_capacity(distinct.len() + 1);
        for &(name, _) in &distinct {
            offsets.push(table.len() as u64);
            table.extend_from_slice(name);
            table.push(0);
        }
        offsets.push(table.len() as u64);
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
        let offsets = Keys::from_values(offsets.iter().copied());
        let keys = Keys::new(&ids);
        Self {
            table,
            offsets,
            keys,
            postings,
            distinct: distinct.len() as u32,
            rows,
            legacy: OnceLock::new(),
            build_time: started.elapsed(),
        }
    }

    /// Dictionary and postings construction, excluding checked-source release.
    pub fn build_time(&self) -> std::time::Duration {
        self.build_time
    }
    /// Extra storage only if a caller explicitly requested the legacy heap API.
    pub fn raw_scan_bytes(&self) -> usize {
        self.legacy.get().map_or(0, |legacy| {
            legacy.heap.len() + legacy.offsets.len() * std::mem::size_of::<usize>()
        })
    }

    pub fn distinct_count(&self) -> u32 {
        self.distinct
    }
    pub fn row_count(&self) -> u32 {
        self.rows
    }
    pub fn distinct_name(&self, key: u32) -> &[u8] {
        assert!(key < self.distinct);
        let start = self.offsets.get_offset(key as usize);
        let end = self.offsets.get_offset(key as usize + 1);
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
            offsets.push(bytes.len() as u64);
            pfor128::encode_sorted(list, &mut bytes);
        }
        offsets.push(bytes.len() as u64);
        Self {
            counts: Keys::new(&counts),
            offsets: Keys::from_values(offsets.iter().copied()),
            bytes,
        }
    }
    pub fn count(&self, list: u32) -> u32 {
        self.counts.get(list as usize)
    }
    pub fn get(&self, list: u32, out: &mut Vec<u32>) {
        let start = self.offsets.get_offset(list as usize);
        let end = self.offsets.get_offset(list as usize + 1);
        pfor128::decode_sorted(self.count(list) as usize, &self.bytes[start..end], out);
    }
    pub fn bytes(&self) -> usize {
        self.counts.bytes() + self.offsets.bytes() + self.bytes.len()
    }
}

#[cfg(test)]
mod packed_strings_tests {
    use super::PackedStrings;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
    }

    #[test]
    fn packed_strings_round_trip_seeded_bytes() {
        let mut rng = Rng(0x51b0_5eed);
        let mut strings = vec![Vec::new(), vec![0xff, 0, 0x80], Vec::new()];
        for _ in 0..512 {
            let len = (rng.next() % 20) as usize;
            strings.push((0..len).map(|_| rng.next() as u8).collect());
        }
        strings.sort();
        strings.dedup();
        let packed = PackedStrings::new(strings.iter());
        assert_eq!(packed.len(), strings.len());
        for (i, expected) in strings.iter().enumerate() {
            assert_eq!(packed.get(i), expected);
            assert_eq!(packed.binary_search(expected), Ok(i));
        }
        assert_eq!(packed.get(0), b"");
        assert!(strings.iter().any(|string| !string.is_ascii()));
    }
}
