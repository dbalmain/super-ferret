//! Interned checkpoint names and counted row postings. Catalog accessors own
//! conversion after semantic validation; same-epoch overlays share this base.
//! The v4 encoder still obtains ordinary name bytes through those accessors.

use std::collections::HashMap;
use std::sync::OnceLock;

use intpack::{bits, pfor128};

use crate::fan_out::fan_out;
use crate::{NameId, NameReader};

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
        self.bytes.capacity()
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
    distinct_names: PackedStrings,
    keys: Keys,
    postings: PackedNameLists,
    distinct: u32,
    rows: u32,
    legacy: OnceLock<Legacy>,
    build_time: std::time::Duration,
}

/// [`ResidentNames::build`]'s sort key: `name`'s first eight bytes as a
/// big-endian number, zero-padded.
fn prefix(name: &[u8]) -> u64 {
    let mut bytes = [0; 8];
    let n = name.len().min(8);
    bytes[..n].copy_from_slice(&name[..n]);
    u64::from_be_bytes(bytes)
}

/// Below this many rows [`ResidentNames::build`] stays on the calling
/// thread: starting threads would cost more than the work.
const PARALLEL_ROWS: u32 = 16_384;
/// The most threads a build uses.
const BUILD_THREADS: usize = 8;

/// `parts - 1` ascending [`prefix`] values cutting a sample of the names
/// into `parts` ranges of about equal size. Equal prefixes always fall in
/// one part, so every name of a part sorts before every name of the next.
fn splitters(names: NameReader<'_>, rows: u32, parts: usize) -> Vec<u64> {
    if parts == 1 || rows == 0 {
        return Vec::new();
    }
    let samples = (rows as usize).min(256 * parts);
    let mut sample: Vec<u64> = (0..samples)
        .map(|i| prefix(names.heap_bytes(NameId((i * rows as usize / samples) as u32))))
        .collect();
    sample.sort_unstable();
    (1..parts).map(|p| sample[p * samples / parts]).collect()
}

/// One name range's share of a build: its distinct names in order and its
/// postings, encoded.
struct Part<'c> {
    names: Vec<&'c [u8]>,
    lists: ListsBuilder,
}

impl<'c> Part<'c> {
    /// Interns the names of `rows` (ascending), fills `posts` with the same
    /// rows grouped by name in name order, and leaves each row's local key
    /// in its place in `rows`.
    fn build(names: NameReader<'c>, rows: &mut [u32], posts: &mut [u32]) -> Self {
        let mut lookup = HashMap::with_capacity(rows.len() / 2);
        let mut keys = Vec::with_capacity(rows.len());
        for &row in rows.iter() {
            let next = lookup.len() as u32;
            keys.push(*lookup.entry(names.heap_bytes(NameId(row))).or_insert(next));
        }
        // A name's first eight bytes, big-endian and zero-padded, order as
        // the name does wherever they differ; comparing them first keeps
        // most comparisons out of `memcmp` and off the name heap.
        let mut distinct: Vec<_> = lookup
            .into_iter()
            .map(|(name, old)| (prefix(name), name, old))
            .collect();
        distinct.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
        let mut remap = vec![0u32; distinct.len()];
        for (key, &(_, _, old)) in distinct.iter().enumerate() {
            remap[old as usize] = key as u32;
        }
        for key in &mut keys {
            *key = remap[*key as usize];
        }
        // One row array and a prefix sum, rather than one allocation per name.
        let mut counts = vec![0usize; distinct.len()];
        for &key in &keys {
            counts[key as usize] += 1;
        }
        let mut next = Vec::with_capacity(counts.len());
        let mut at = 0;
        for &count in &counts {
            next.push(at);
            at += count;
        }
        for (&row, &key) in rows.iter().zip(&keys) {
            posts[next[key as usize]] = row;
            next[key as usize] += 1;
        }
        let mut lists = ListsBuilder::default();
        let mut at = 0;
        for &count in &counts {
            lists.push(&posts[at..at + count]);
            at += count;
        }
        rows.copy_from_slice(&keys);
        Self {
            names: distinct.into_iter().map(|(_, name, _)| name).collect(),
            lists,
        }
    }
}

/// The three packings at the end of a build, and what each makes.
enum Packing {
    Strings,
    Lists(Vec<ListsBuilder>),
    Keys,
}

enum Packed {
    Strings(PackedStrings),
    Lists(PackedNameLists),
    Keys(Keys),
}

struct Legacy {
    heap: Vec<u8>,
    offsets: Vec<usize>,
}

impl ResidentNames {
    pub(crate) fn build(names: NameReader<'_>, rows: u32) -> Self {
        Self::build_with(names, rows, Self::threads(rows))
    }

    /// The threads a build of `rows` uses: one, the calling thread, below
    /// [`PARALLEL_ROWS`].
    pub(crate) fn threads(rows: u32) -> usize {
        match rows < PARALLEL_ROWS {
            true => 1,
            false => crate::fan_out::cores().min(BUILD_THREADS),
        }
    }

    /// Builds on `threads` threads, the calling thread among them
    /// ([`fan_out`]); the result is the same at every count, and with one
    /// nothing is spawned.
    ///
    /// Rows are split by name range, not by row: names are cut at sampled
    /// [`prefix`] splitters, so every part's distinct names sort after the
    /// previous part's and the parts' sorted tables concatenate into the
    /// whole one. Each part interns, sorts and posts its own names; no merge
    /// is needed. Interning still streams rows and sorts only distinct
    /// names, never one borrowed slice per row (D40's scratch); the split
    /// adds a byte and a row number per row.
    pub(crate) fn build_with(names: NameReader<'_>, rows: u32, threads: usize) -> Self {
        let started = std::time::Instant::now();
        let parts = threads.clamp(1, u8::MAX as usize);
        let total = rows as usize;
        let splitters = splitters(names, rows, parts);
        let splitters = &splitters[..];
        let chunk = total.div_ceil(parts).max(1);

        // Pass 1, by row chunk: each row's part, and each chunk's count per
        // part.
        let mut part = vec![0u8; total];
        let counts: Vec<Vec<usize>> = fan_out(
            part.chunks_mut(chunk).enumerate().collect(),
            threads,
            |(c, out)| {
                let mut counts = vec![0; parts];
                let from = NameId((c * chunk) as u32);
                for ((_, name), slot) in names.runs_from(from).zip(out.iter_mut()) {
                    let p = splitters.partition_point(|&s| s <= prefix(name.bytes));
                    *slot = p as u8;
                    counts[p] += 1;
                }
                counts
            },
        );
        let chunks = counts.len();
        let sizes: Vec<usize> = (0..parts)
            .map(|p| counts.iter().map(|c| c[p]).sum())
            .collect();

        // Pass 2, by row chunk: rows grouped by part, ascending within each.
        // Chunk c's rows of part p go after earlier chunks' rows of part p.
        let mut by_part = vec![0u32; total];
        {
            let mut slots: Vec<Vec<&mut [u32]>> = (0..chunks).map(|_| Vec::new()).collect();
            let mut rest = &mut by_part[..];
            for p in 0..parts {
                for (c, counts) in counts.iter().enumerate() {
                    let (here, tail) = rest.split_at_mut(counts[p]);
                    slots[c].push(here);
                    rest = tail;
                }
            }
            let work = slots.into_iter().enumerate().zip(part.chunks(chunk));
            fan_out(work.collect(), threads, |((c, mut slots), part)| {
                let mut next = vec![0; parts];
                for (i, &p) in part.iter().enumerate() {
                    let p = p as usize;
                    slots[p][next[p]] = (c * chunk + i) as u32;
                    next[p] += 1;
                }
            });
        }

        // Pass 3, by part: intern, sort and post. A part's postings take the
        // same span of the row array as its rows, since its keys are
        // contiguous; its local keys then overwrite its rows.
        let mut postings_rows = vec![0u32; total];
        let built: Vec<Part<'_>> = {
            let mut rows_rest = &mut by_part[..];
            let mut posts_rest = &mut postings_rows[..];
            let spans = sizes
                .iter()
                .map(|&size| {
                    let (rows, tail) = std::mem::take(&mut rows_rest).split_at_mut(size);
                    rows_rest = tail;
                    let (posts, tail) = std::mem::take(&mut posts_rest).split_at_mut(size);
                    posts_rest = tail;
                    (rows, posts)
                })
                .collect();
            fan_out(spans, threads, |(rows, posts)| {
                Part::build(names, rows, posts)
            })
        };
        drop(postings_rows);
        let firsts: Vec<u32> = built
            .iter()
            .scan(0u32, |next, part| {
                let first = *next;
                *next += part.names.len() as u32;
                Some(first)
            })
            .collect();

        // Pass 4, by row chunk: each row's global key, read back in row order
        // from its part's local keys (now in `by_part`).
        let mut ids = vec![0u32; total];
        {
            let mut starts = vec![0usize; parts];
            for p in 1..parts {
                starts[p] = starts[p - 1] + sizes[p - 1];
            }
            let (by_part, firsts) = (&by_part[..], &firsts[..]);
            let mut work = Vec::new();
            for ((ids, part), counts) in ids.chunks_mut(chunk).zip(part.chunks(chunk)).zip(&counts)
            {
                work.push((ids, part, starts.clone()));
                for p in 0..parts {
                    starts[p] += counts[p];
                }
            }
            fan_out(work, threads, |(ids, part, mut next)| {
                for (id, &p) in ids.iter_mut().zip(part) {
                    let p = p as usize;
                    *id = firsts[p] + by_part[next[p]];
                    next[p] += 1;
                }
            });
        }
        drop((part, by_part));

        // The three packings are independent; run them side by side.
        let distinct = built.iter().map(|part| part.names.len()).sum::<usize>();
        let (names, lists): (Vec<_>, Vec<_>) = built
            .into_iter()
            .map(|part| (part.names, part.lists))
            .unzip();
        let packings = vec![Packing::Strings, Packing::Lists(lists), Packing::Keys];
        let mut packed = fan_out(packings, threads, |packing| match packing {
            Packing::Strings => Packed::Strings(PackedStrings::new(names.iter().flatten())),
            Packing::Lists(lists) => Packed::Lists(PackedNameLists::concat(lists)),
            Packing::Keys => Packed::Keys(Keys::new(&ids)),
        })
        .into_iter();
        let (
            Some(Packed::Strings(distinct_names)),
            Some(Packed::Lists(postings)),
            Some(Packed::Keys(keys)),
        ) = (packed.next(), packed.next(), packed.next())
        else {
            unreachable!("fan_out keeps the order")
        };
        Self {
            distinct_names,
            keys,
            postings,
            distinct: distinct as u32,
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
        self.distinct_names.get(key as usize)
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
        self.distinct_names.bytes() + self.keys.bytes() + self.postings.bytes()
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
        let mut builder = ListsBuilder::default();
        for list in lists {
            builder.push(list);
        }
        Self::concat([builder])
    }
    /// The lists of each builder in turn, as one.
    fn concat(builders: impl IntoIterator<Item = ListsBuilder>) -> Self {
        let (mut counts, mut offsets, mut bytes) = (Vec::new(), Vec::new(), Vec::new());
        for builder in builders {
            let base = bytes.len() as u64;
            counts.extend_from_slice(&builder.counts);
            offsets.extend(builder.offsets.iter().map(|&offset| base + offset));
            bytes.extend_from_slice(&builder.bytes);
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
        self.counts.bytes() + self.offsets.bytes() + self.bytes.capacity()
    }
}

/// [`PackedNameLists`] being encoded: counts, start offsets and bytes.
#[derive(Default)]
struct ListsBuilder {
    counts: Vec<u32>,
    offsets: Vec<u64>,
    bytes: Vec<u8>,
}

impl ListsBuilder {
    fn push(&mut self, list: &[u32]) {
        assert!(
            list.windows(2).all(|pair| pair[0] < pair[1]),
            "name postings must ascend"
        );
        self.counts
            .push(u32::try_from(list.len()).unwrap_or_else(|_| panic!("name postings exceed u32")));
        self.offsets.push(self.bytes.len() as u64);
        pfor128::encode_sorted(list, &mut self.bytes);
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

#[cfg(test)]
mod build_tests {
    use super::{Keys, PackedNameLists, PackedStrings, ResidentNames};
    use crate::{Catalog, Content, NameId, Stat, Transaction};

    fn stat(ino: u64, mode: u32) -> Stat {
        Stat {
            dev: 7,
            ino,
            size: 1,
            mtime_sec: 1_700_000_000,
            mtime_nsec: 0,
            ctime_sec: 1_700_000_000,
            ctime_nsec: 0,
            mode,
            uid: 1000,
            gid: 100,
            nlink: 1,
        }
    }

    /// Names that stress a range split: one name repeated in every
    /// directory, many sharing their first eight bytes (so one part holds
    /// them all), one-byte names, names that are prefixes of others, bytes
    /// above 0x7f, and a run of distinct numbers like the scan corpus.
    fn names(d: u64) -> Vec<Vec<u8>> {
        let mut names = vec![b"index.js".to_vec(), b"a".to_vec(), vec![0xff, b'z']];
        for i in 0..150u64 {
            names.push(format!("node_modules_{:03}", (i * 7 + d) % 150).into_bytes());
            names.push(format!("{}", d * 1000 + i).into_bytes());
            names.push(format!("n{i}").into_bytes());
        }
        names.push(b"node_modul".to_vec());
        names.push(vec![b'\xe2', b'\x84', b'\xaa', d as u8 + b'a']);
        names
    }

    fn catalog(dir: &std::path::Path) -> Catalog {
        let mut txn = Transaction::begin(dir, 1).unwrap();
        let mut b = txn.batch();
        let root = b.root(b"/w", stat(1, 0o040_755));
        let mut ino = 10;
        for d in 0..12u64 {
            let sub = b.dir(root, format!("d{d}").as_bytes(), stat(2 + d, 0o040_755));
            for name in names(d) {
                ino += 1;
                b.file(sub, &name, stat(ino, 0o100_644), Content::Unindexed);
            }
        }
        txn.add(b);
        txn.commit().unwrap();
        let catalog = Catalog::open(dir).unwrap().unwrap();
        catalog.load_all().unwrap();
        catalog
    }

    fn keys(keys: &Keys) -> (u32, usize, &[u8]) {
        (keys.width, keys.len, &keys.bytes)
    }
    fn strings(strings: &PackedStrings) -> (&[u8], (u32, usize, &[u8])) {
        (&strings.table, keys(&strings.offsets))
    }
    type Lists<'a> = ((u32, usize, &'a [u8]), (u32, usize, &'a [u8]), &'a [u8]);
    fn lists(lists: &PackedNameLists) -> Lists<'_> {
        (keys(&lists.counts), keys(&lists.offsets), &lists.bytes)
    }

    #[test]
    fn a_build_is_the_same_bytes_on_any_number_of_threads() {
        let dir = std::env::temp_dir().join(format!("ferret-resident-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let catalog = catalog(&dir);
        let (reader, rows) = (catalog.name_reader(), catalog.base_name_count());
        assert!(rows > 5000, "{rows} rows");
        let one = ResidentNames::build_with(reader, rows, 1);
        // The serial build against a plain sort: names ascend, every row's
        // key names its own bytes, and postings list exactly those rows.
        let mut expected: Vec<_> = (0..rows)
            .map(|row| reader.heap_bytes(NameId(row)))
            .collect();
        expected.sort();
        expected.dedup();
        assert_eq!(one.distinct_count() as usize, expected.len());
        for (key, name) in expected.iter().enumerate() {
            assert_eq!(one.distinct_name(key as u32), *name);
            let mut postings = Vec::new();
            one.postings(key as u32, &mut postings);
            let rows: Vec<u32> = (0..rows)
                .filter(|&row| reader.heap_bytes(NameId(row)) == *name)
                .collect();
            assert_eq!(postings, rows, "{name:?}");
        }
        for threads in [2, 3, 5, 8, 16, 64, 255] {
            let many = ResidentNames::build_with(reader, rows, threads);
            assert_eq!(many.distinct, one.distinct, "{threads} threads");
            assert_eq!(many.rows, one.rows);
            assert_eq!(strings(&many.distinct_names), strings(&one.distinct_names));
            assert_eq!(keys(&many.keys), keys(&one.keys), "{threads} threads");
            assert_eq!(lists(&many.postings), lists(&one.postings));
        }
        // Helpers that cannot start leave their work to the threads that
        // did: the same bytes, whichever is refused.
        for fail_from in [0, 1, 3] {
            let seams = crate::fan_out::seam::Seams {
                fail_from: Some(fail_from),
                ..Default::default()
            };
            let asked = std::sync::Arc::clone(&seams.asked);
            let _seams = seams.set();
            let many = ResidentNames::build_with(reader, rows, 8);
            assert!(asked.load(std::sync::atomic::Ordering::Relaxed) > 0);
            assert_eq!(keys(&many.keys), keys(&one.keys), "from {fail_from}");
            assert_eq!(strings(&many.distinct_names), strings(&one.distinct_names));
            assert_eq!(lists(&many.postings), lists(&one.postings));
        }
        // One thread is the calling thread alone.
        let seams = crate::fan_out::seam::Seams::default();
        let asked = std::sync::Arc::clone(&seams.asked);
        let _seams = seams.set();
        ResidentNames::build_with(reader, rows, 1);
        assert_eq!(asked.load(std::sync::atomic::Ordering::Relaxed), 0);
        // More threads than rows leaves parts and chunks empty.
        let few = ResidentNames::build_with(reader, 3, 16);
        let serial = ResidentNames::build_with(reader, 3, 1);
        assert_eq!(keys(&few.keys), keys(&serial.keys));
        assert_eq!(
            strings(&few.distinct_names),
            strings(&serial.distinct_names)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
