//! Every test drives the real [`Writer`], [`Inverter`] and [`Segment`]:
//! round trips against the input, a lookup at every block boundary, and
//! mutation fuzz that truncates and flips the written bytes.

use std::collections::BTreeMap;

use super::*;

/// Seeded xorshift64*, matching the idiom in
/// `crates/ferret-verify/src/tests.rs`.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn bytes(&mut self, min: usize, max: usize) -> Vec<u8> {
        let len = min + self.below(max - min + 1);
        (0..len).map(|_| self.next() as u8).collect()
    }

    /// `count` distinct documents from `first..=last`, ascending.
    fn docs(&mut self, first: u32, last: u32, count: usize) -> Vec<u32> {
        let span = (last - first) as usize + 1;
        if count * 2 > span {
            let mut all: Vec<u32> = (first..=last).collect();
            while all.len() > count {
                let i = self.below(all.len());
                all.remove(i);
            }
            return all;
        }
        let mut set = std::collections::BTreeSet::new();
        while set.len() < count {
            set.insert(first + self.below(span) as u32);
        }
        set.into_iter().collect()
    }
}

type Terms = BTreeMap<Vec<u8>, Vec<u32>>;

fn write(first: u32, last: u32, terms: &Terms) -> (Vec<u8>, Sizes) {
    let mut writer = Writer::new(first, last).unwrap();
    for (term, docs) in terms {
        writer.push(term, docs).unwrap();
    }
    let mut bytes = Vec::new();
    let sizes = writer.finish(&mut bytes).unwrap();
    assert_eq!(sizes.total(), bytes.len() as u64);
    (bytes, sizes)
}

fn documents(hit: Hit) -> Vec<u32> {
    match hit {
        Hit::Single(doc) => vec![doc],
        Hit::Postings(postings) => {
            let mut out = Vec::new();
            postings.decode(&mut out);
            assert_eq!(out.len(), postings.len() as usize);
            let mut cursor = postings.cursor();
            let walked: Vec<u32> = std::iter::from_fn(|| cursor.next()).collect();
            assert_eq!(walked, out);
            out
        }
    }
}

/// Reads `bytes` back and checks it holds exactly `terms`: in order through
/// [`Segment::terms`], by [`Segment::lookup`], and through `next_geq`.
fn check(bytes: &[u8], first: u32, last: u32, terms: &Terms, rng: &mut Rng) {
    let segment = Segment::open(bytes).unwrap();
    let info = segment.info();
    assert_eq!((info.first, info.last), (first, last));
    assert_eq!(info.terms, terms.len() as u64);
    assert_eq!(info.pairs, terms.values().map(|d| d.len() as u64).sum());
    assert_eq!(info.tokenizer_version, ferret_text::TOKENIZER_VERSION);

    let mut iter = segment.terms();
    let mut expected = terms.iter();
    while let Some((term, entry)) = iter.next_term().unwrap() {
        let (want_term, want_docs) = expected.next().unwrap();
        assert_eq!(term, want_term.as_slice());
        assert_eq!(entry.df as usize, want_docs.len());
        assert_eq!(&documents(segment.read(&entry).unwrap()), want_docs);
    }
    assert!(expected.next().is_none());

    for (term, docs) in terms {
        let hit = segment.lookup(term).unwrap().unwrap();
        assert_eq!(matches!(hit, Hit::Single(_)), docs.len() == 1);
        if let Hit::Postings(postings) = &hit {
            let mut cursor = postings.cursor();
            let mut target = first.saturating_sub(3);
            loop {
                let want = docs.get(docs.partition_point(|&d| d < target)).copied();
                assert_eq!(cursor.next_geq(target), want, "next_geq({target})");
                let Some(at) = want else { break };
                let Some(next) = at.checked_add(rng.below(40) as u32) else {
                    break;
                };
                target = next;
            }
        }
        assert_eq!(&documents(hit), docs);
        // A term just past this one, absent unless it is the next term.
        let mut after = term.clone();
        after.push(0);
        if !terms.contains_key(&after) {
            assert!(segment.lookup(&after).unwrap().is_none());
        }
    }
    for _ in 0..50 {
        let probe = rng.bytes(0, 12);
        if !terms.contains_key(&probe) {
            assert!(segment.lookup(&probe).unwrap().is_none());
        }
    }
}

/// Random terms in two families, long shared prefixes and none, with
/// documents from `first..=last` at the frequencies the brief names.
fn random_terms(rng: &mut Rng, first: u32, last: u32, count: usize) -> Terms {
    const DFS: [usize; 7] = [1, 2, 127, 128, 129, 130, 256];
    let docs = (last - first) as usize + 1;
    let mut terms = Terms::new();
    terms.insert(b"everywhere".to_vec(), (first..=last).collect());
    while terms.len() < count {
        let term = if rng.below(2) == 0 {
            let mut term = b"a_long_shared_prefix_of_forty_bytes_____".to_vec();
            term.extend(rng.bytes(0, 6));
            term
        } else {
            rng.bytes(1, 64)
        };
        let df = match rng.below(4) {
            0 => DFS[rng.below(DFS.len())],
            1 => 1 + rng.below(8),
            _ => 1,
        };
        let list = rng.docs(first, last, df.min(docs));
        terms.insert(term, list);
    }
    terms
}

// ── round trips ──

#[test]
fn random_segments_round_trip() {
    for seed in 1..=8u64 {
        let mut rng = Rng(0x5e9_0000 ^ seed);
        let first = rng.below(1 << 20) as u32;
        let last = first + 200 + rng.below(800) as u32;
        let count = 1 + rng.below(600);
        let terms = random_terms(&mut rng, first, last, count);
        let (bytes, sizes) = write(first, last, &terms);
        assert_eq!(
            sizes.singletons,
            terms.values().filter(|d| d.len() == 1).count() as u64
        );
        check(&bytes, first, last, &terms, &mut rng);
    }
}

#[test]
fn document_frequencies_around_a_pfor_block_round_trip() {
    let mut rng = Rng(0xdf);
    let (first, last) = (10, 10 + 999);
    let mut terms = Terms::new();
    for df in [1, 2, 127, 128, 129, 130, 1000] {
        terms.insert(format!("df{df:04}").into_bytes(), rng.docs(first, last, df));
    }
    // 130 consecutive documents: a run block then a tail.
    terms.insert(b"run".to_vec(), (first..first + 130).collect());
    let (bytes, _) = write(first, last, &terms);
    // Identical bytes need no second read-back; the in-memory path's round
    // trips are checked above. Open it once to be sure it is a segment.
    assert_eq!(
        Segment::open(bytes.as_slice()).unwrap().info().terms,
        terms.len() as u64
    );
}

#[test]
fn empty_segment_round_trips() {
    let terms = Terms::new();
    let (bytes, sizes) = write(7, 9, &terms);
    assert_eq!(sizes.total(), HEAD as u64);
    check(&bytes, 7, 9, &terms, &mut Rng(1));
}

#[test]
fn single_term_segments_round_trip() {
    for docs in [vec![u32::MAX], vec![3, u32::MAX]] {
        let terms = Terms::from([(b"only".to_vec(), docs)]);
        let (bytes, _) = write(3, u32::MAX, &terms);
        check(&bytes, 3, u32::MAX, &terms, &mut Rng(2));
    }
    let terms = Terms::from([(Vec::new(), vec![0])]);
    let (bytes, _) = write(0, 0, &terms);
    check(&bytes, 0, 0, &terms, &mut Rng(3));
}

// ── lookups at block boundaries ──

/// Regression guard for an off-by-one in the block search: with `<` for
/// `<=`, a term that is a block's first term is searched for in the block
/// before it and reported absent.
#[test]
fn lookup_finds_terms_at_every_block_edge() {
    let terms: Terms = (0..3 * BLOCK + 5)
        .map(|i| (format!("t{i:03}").into_bytes(), vec![i as u32]))
        .collect();
    let (bytes, _) = write(0, 200, &terms);
    let segment = Segment::open(bytes.as_slice()).unwrap();
    let doc = |term: &str| match segment.lookup(term.as_bytes()).unwrap() {
        Some(Hit::Single(doc)) => Some(doc),
        Some(Hit::Postings(_)) => panic!("{term} has one document"),
        None => None,
    };
    for b in 0..4 {
        let first = b * BLOCK;
        assert_eq!(
            doc(&format!("t{first:03}")),
            Some(first as u32),
            "first of block {b}"
        );
        if b < 3 {
            let last = first + BLOCK - 1;
            assert_eq!(
                doc(&format!("t{last:03}")),
                Some(last as u32),
                "last of block {b}"
            );
            // Between this block's last term and the next block's first.
            assert_eq!(doc(&format!("t{last:03}~")), None, "after block {b}");
        }
    }
    assert_eq!(doc("t"), None, "before the first term");
    assert_eq!(doc("t0405"), None, "inside a block, absent");
    assert_eq!(doc(&format!("t{:03}", 3 * BLOCK + 4)), Some(100));
    assert_eq!(doc("u"), None, "after the last term");
}

// ── inverter and writer contract ──

#[test]
fn inverter_matches_a_brute_force_inversion() {
    let mut rng = Rng(0x1_17e7);
    let mut inverter = Inverter::new(usize::MAX);
    let mut expected = Terms::new();
    let vocabulary: Vec<Vec<u8>> = (0..200).map(|_| rng.bytes(1, 10)).collect();
    for doc in 40..140u32 {
        for _ in 0..rng.below(30) {
            let term = &vocabulary[rng.below(vocabulary.len())];
            inverter.add(doc, term).unwrap();
            let docs = expected.entry(term.clone()).or_default();
            if docs.last() != Some(&doc) {
                docs.push(doc);
            }
        }
    }
    assert!(inverter.memory() > 0);
    let writer = inverter.drain_into(40, 139).unwrap();
    assert!(inverter.is_empty());
    assert_eq!(inverter.memory(), 0);
    let mut bytes = Vec::new();
    writer.finish(&mut bytes).unwrap();
    check(&bytes, 40, 139, &expected, &mut rng);
}

#[test]
fn inverter_reports_full_at_its_bound_and_refuses_falling_documents() {
    let mut inverter = Inverter::new(200);
    inverter.add(5, b"x").unwrap();
    assert!(!inverter.is_full());
    for i in 0..10u8 {
        inverter.add(5, &[b'a', i]).unwrap();
    }
    assert!(inverter.is_full());
    assert_eq!(inverter.add(4, b"x"), Err(WriteError::DocOrder));
    assert_eq!(
        inverter.drain_into(6, 9).err(),
        Some(WriteError::DocRange(5))
    );
}

/// A spilling writer, spilled after every push, writes the very bytes the
/// in-memory writer does, with sections past the spill threshold so whole
/// chunks leave memory, a partial chunk stays behind, and postings travel
/// through the scratch file.
#[test]
fn a_spilling_writer_writes_the_same_bytes() {
    let mut rng = Rng(0x0005_9111);
    let (first, last) = (1_000, 10_001_000);
    let mut terms = Terms::new();
    while terms.len() < 60_000 {
        let df = if rng.below(4) == 0 {
            2 + rng.below(6)
        } else {
            1
        };
        terms.insert(rng.bytes(4, 40), rng.docs(first, last, df));
    }
    for i in 0..60u32 {
        // Strided with jitter: wide gaps, so the lists are long in bytes.
        let list = (0..20_000)
            .map(|d| first + d * 499 + rng.below(400) as u32)
            .collect();
        terms.insert(format!("zz-long-{i:02}").into_bytes(), list);
    }
    let (memory, sizes) = write(first, last, &terms);
    assert!(
        sizes.blocks > super::write::SPILL as u64 && sizes.postings > super::write::SPILL as u64
    );

    let dir = std::env::temp_dir().join(format!("ferret-index-spill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let open = |name: &str| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.join(name))
            .unwrap()
    };
    let (out, scratch) = (open("out"), open("postings"));
    let paced = std::cell::Cell::new(0);
    let pace = |n: usize| {
        paced.set(paced.get() + n);
        Ok(())
    };
    let mut writer = Writer::spilling(first, last, out.try_clone().unwrap(), scratch).unwrap();
    for (term, docs) in &terms {
        writer.push(term, docs).unwrap();
        writer.spill(&pace).unwrap();
    }
    let spilled = writer.finish_spilled(&pace).unwrap();
    let bytes = std::fs::read(dir.join("out")).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();

    assert_eq!(spilled, sizes);
    assert!(
        bytes == memory,
        "spilled segment differs from the in-memory one"
    );
    // Every byte but the head is paced once, and spilled postings once more
    // on their way to scratch.
    let once = sizes.total() - sizes.head;
    assert!(paced.get() as u64 > once && paced.get() as u64 <= once + 2 * sizes.postings);
    // Identical bytes need no second read-back; the in-memory path's round
    // trips are checked above. Open it once to be sure it is a segment.
    assert_eq!(
        Segment::open(bytes.as_slice()).unwrap().info().terms,
        terms.len() as u64
    );
}

#[test]
fn writer_rejects_input_that_breaks_its_contract() {
    assert_eq!(
        Writer::new(5, 4).err(),
        Some(WriteError::Range { first: 5, last: 4 })
    );
    let mut writer = Writer::new(10, 20).unwrap();
    writer.push(b"b", &[10]).unwrap();
    assert_eq!(writer.push(b"b", &[11]), Err(WriteError::TermOrder));
    assert_eq!(writer.push(b"a", &[11]), Err(WriteError::TermOrder));
    assert_eq!(writer.push(b"c", &[]), Err(WriteError::NoDocuments));
    assert_eq!(writer.push(b"c", &[12, 12]), Err(WriteError::DocOrder));
    assert_eq!(writer.push(b"c", &[9, 12]), Err(WriteError::DocRange(9)));
    assert_eq!(writer.push(b"c", &[12, 21]), Err(WriteError::DocRange(21)));
    writer.push(b"c", &[12, 20]).unwrap();
    let mut bytes = Vec::new();
    writer.finish(&mut bytes).unwrap();
    let terms = Terms::from([(b"b".to_vec(), vec![10]), (b"c".to_vec(), vec![12, 20])]);
    check(&bytes, 10, 20, &terms, &mut Rng(4));
}

// ── mutation fuzz ──

/// Opens `bytes` and reads everything: every term, every list, every
/// lookup. A clean segment returns `Ok`; a damaged one must return the
/// error, never panic.
fn exercise(bytes: &[u8]) -> Result<(), ReadError> {
    let segment = Segment::open(bytes)?;
    let mut terms = Vec::new();
    let mut iter = segment.terms();
    while let Some((term, entry)) = iter.next_term()? {
        terms.push(term.to_vec());
        documents(segment.read(&entry)?);
    }
    for term in &terms {
        segment.lookup(term)?;
    }
    Ok(())
}

/// A segment with several chunks in both blocks and postings, and its
/// boundaries: head fields, then each section's end.
fn fuzz_segment() -> (Vec<u8>, Vec<usize>) {
    let mut rng = Rng(0xf022);
    let terms = random_terms(&mut rng, 100, 3099, 900);
    let (bytes, sizes) = write(100, 3099, &terms);
    assert!(sizes.blocks > 2 * CHUNK as u64 && sizes.postings > 2 * CHUNK as u64);
    let mut edges = vec![0, 8, 12, FIELDS, HEAD - 16, HEAD];
    let mut at = HEAD as u64;
    for len in [sizes.blocks, sizes.postings, sizes.index, sizes.sums] {
        at += len;
        edges.push(at as usize);
    }
    (bytes, edges)
}

fn mutation_fuzz(iterations: u64) {
    let (bytes, edges) = fuzz_segment();
    exercise(&bytes).unwrap();
    for &edge in &edges[..edges.len() - 1] {
        for cut in [edge.saturating_sub(1), edge, edge + 1] {
            assert!(exercise(&bytes[..cut]).is_err(), "truncated at {cut}");
        }
    }
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(exercise(&longer).is_err(), "extended by a byte");

    let mut rng = Rng(0x0dba_11ca_fef0_0d42);
    // The head, then each section.
    let sections: Vec<(usize, usize)> = edges[4..].windows(2).map(|w| (w[0], w[1])).collect();
    for i in 0..iterations {
        let cut = rng.below(bytes.len());
        assert!(exercise(&bytes[..cut]).is_err(), "truncated at {cut}");

        let (start, end) = sections[i as usize % sections.len()];
        let mut flipped = bytes.clone();
        for _ in 0..1 + rng.below(3) {
            let at = start + rng.below(end - start);
            flipped[at] ^= 1 << rng.below(8);
        }
        if flipped != bytes {
            assert!(exercise(&flipped).is_err(), "flip in {start}..{end}");
        }
    }
}

#[test]
fn mutation_fuzz_never_panics() {
    mutation_fuzz(300);
}

/// Longer mutation fuzz run, for ad hoc use:
/// `FERRET_SEGMENT_FUZZ_ITERS=20000 cargo test -p ferret-index -- --ignored
/// mutation_fuzz_long`.
#[test]
#[ignore]
fn mutation_fuzz_long() {
    let iterations = std::env::var("FERRET_SEGMENT_FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000);
    mutation_fuzz(iterations);
}
