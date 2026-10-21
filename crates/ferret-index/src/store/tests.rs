//! Every test drives the real [`IndexWriter`] against a generated corpus,
//! and judges it two ways: against the words the generator put in each
//! document (`truth`), and against a clean rebuild of the same catalog view
//! in a fresh directory. Crashes are injected at [`Point`]s through
//! [`STOP`], which fails the operation there as a crash would stop it.

use std::collections::BTreeSet;

use super::*;
use crate::manifest;
use crate::segment::Section;

const WORDS: [&str; 16] = [
    "amber", "basalt", "cobalt", "dune", "ember", "fjord", "garnet", "heath", "iris", "jade",
    "kelp", "loam", "marl", "nacre", "ochre", "peat",
];

/// Seeded xorshift64*, matching the idiom in `segment/tests.rs`.
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
}

/// A scratch index directory, removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("ferret-index-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        Self(path)
    }

    /// Every file in the directory.
    fn files(&self) -> BTreeSet<String> {
        fs::read_dir(&self.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect()
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Generated documents: `words[doc]` are the vocabulary indexes in it.
struct Corpus {
    words: Vec<Vec<usize>>,
}

impl Corpus {
    fn new(seed: u64, docs: usize) -> Self {
        let mut rng = Rng(seed);
        let words = (0..docs)
            .map(|_| {
                let count = rng.below(7); // some documents are empty
                (0..count).map(|_| rng.below(WORDS.len())).collect()
            })
            .collect();
        Self { words }
    }

    fn text(&self, doc: u32) -> Vec<u8> {
        let words: Vec<&str> = self.words[doc as usize].iter().map(|&w| WORDS[w]).collect();
        words.join(" ").into_bytes()
    }
}

/// One catalog view as the host would describe it.
struct Catalog {
    incarnation: [u8; 16],
    live: DocSet,
    /// Documents the host's reader cannot read.
    unreadable: BTreeSet<u32>,
}

impl Catalog {
    fn new(bound: u32, dead: &[u32]) -> Self {
        Self {
            incarnation: [9; 16],
            live: DocSet::new(bound, (0..bound).filter(|d| !dead.contains(d))),
            unreadable: BTreeSet::new(),
        }
    }

    fn view(&self) -> CatalogView<'_> {
        CatalogView {
            incarnation: self.incarnation,
            live: &self.live,
        }
    }

    /// Per word, the live readable documents that hold it.
    fn truth(&self, corpus: &Corpus) -> Vec<Vec<u32>> {
        (0..WORDS.len())
            .map(|w| {
                self.live
                    .range(0, self.live.bound())
                    .filter(|d| !self.unreadable.contains(d))
                    .filter(|&d| corpus.words[d as usize].contains(&w))
                    .collect()
            })
            .collect()
    }
}

/// A budget of `bytes` per pass, unpaced.
fn budget(bytes: u64) -> Budget<'static> {
    Budget {
        bytes,
        ..Budget::unbounded()
    }
}

/// Follows until covered; returns every document the reader was asked for.
fn follow_all(
    writer: &mut IndexWriter,
    catalog: &Catalog,
    corpus: &Corpus,
    budget: &Budget,
) -> Result<Vec<u32>, Error> {
    let mut asked = Vec::new();
    loop {
        let followed = writer.follow(&catalog.view(), budget, &mut |doc, bytes| {
            asked.push(doc);
            if catalog.unreadable.contains(&doc) {
                return Err(Fault::Unreadable);
            }
            bytes.extend_from_slice(&corpus.text(doc));
            Ok(())
        })?;
        if followed.stopped == Stopped::Covered {
            return Ok(asked);
        }
    }
}

/// Per word, the view's documents, unfiltered: what the segments hold.
fn raw(view: &View) -> Vec<Vec<u32>> {
    WORDS
        .iter()
        .map(|w| view.lookup(w.as_bytes()).unwrap())
        .collect()
}

/// Per word, the view's documents that are live in `catalog`: a query's
/// answer.
fn answers(view: &View, catalog: &Catalog) -> Vec<Vec<u32>> {
    raw(view)
        .into_iter()
        .map(|docs| {
            docs.into_iter()
                .filter(|&d| catalog.live.contains(d))
                .collect()
        })
        .collect()
}

/// The answers of a fresh index over `catalog`, built in one pass.
fn rebuild(name: &str, catalog: &Catalog, corpus: &Corpus) -> Vec<Vec<u32>> {
    let dir = Dir::new(&format!("{name}-clean"));
    let mut writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
    follow_all(&mut writer, catalog, corpus, &Budget::unbounded()).unwrap();
    answers(&writer.view(), catalog)
}

/// The files a view's manifest names, plus the manifest itself.
fn named(view: &View) -> BTreeSet<String> {
    let mut files: BTreeSet<String> = view
        .manifest()
        .segments
        .iter()
        .flat_map(|entry| [entry.file_name(), entry.trigram_file_name()])
        .collect();
    files.insert(manifest::FILE.to_owned());
    files
}

fn stop_at(point: Point) {
    STOP.set(Some(point));
}

#[test]
fn a_clean_build_answers_every_word() {
    let dir = Dir::new("clean");
    let corpus = Corpus::new(1, 300);
    let catalog = Catalog::new(300, &[7, 8, 150, 299]);
    let mut writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
    let asked = follow_all(&mut writer, &catalog, &corpus, &Budget::unbounded()).unwrap();
    let view = writer.view();
    assert_eq!(asked, catalog.live.range(0, 300).collect::<Vec<_>>());
    // Dead documents were never read, so the segments hold only live ones.
    assert_eq!(raw(&view), catalog.truth(&corpus));
    assert!(view.uncovered(&catalog.live).is_empty());
    assert_eq!(view.manifest().segments.len(), 1);
    assert_eq!(
        (view.manifest().frontier, view.manifest().high_water),
        (300, 300)
    );
    assert_eq!(dir.files(), named(&view));
}

/// Covers the first 40 documents, then crashes at `point` while covering
/// the next 40; reopens and checks the state, then finishes and compares
/// with a clean rebuild.
fn crash_while_following(name: &str, point: Point) -> (Manifest, Manifest) {
    let dir = Dir::new(name);
    let corpus = Corpus::new(2, 80);
    let first = Catalog::new(40, &[3]);
    let mut writer = IndexWriter::open(&dir.0, &first.view()).unwrap();
    follow_all(&mut writer, &first, &corpus, &Budget::unbounded()).unwrap();
    let before = writer.view().manifest().clone();

    let second = Catalog::new(80, &[3, 41]);
    stop_at(point);
    assert!(follow_all(&mut writer, &second, &corpus, &Budget::unbounded()).is_err());
    assert!(matches!(
        follow_all(&mut writer, &second, &corpus, &Budget::unbounded()),
        Err(Error::Poisoned)
    ));
    drop(writer);

    let mut writer = IndexWriter::open(&dir.0, &second.view()).unwrap();
    let after = writer.view().manifest().clone();
    assert_eq!(
        dir.files(),
        named(&writer.view()),
        "unnamed files are removed on open"
    );
    follow_all(&mut writer, &second, &corpus, &Budget::unbounded()).unwrap();
    let clean = rebuild(name, &second, &corpus);
    assert_eq!(answers(&writer.view(), &second), clean);
    assert_eq!(clean, second.truth(&corpus));
    assert_trigrams(&writer.view(), &second, &corpus);
    (before, after)
}

#[test]
fn a_crash_after_the_segment_write_keeps_the_old_manifest() {
    let (before, after) = crash_while_following("crash-segment", Point::SegmentWritten);
    assert_eq!(
        after, before,
        "the renamed segment is an orphan, removed on open"
    );
}

#[test]
fn a_crash_after_the_manifest_rename_keeps_the_new_manifest() {
    let (before, after) = crash_while_following("crash-manifest", Point::ManifestRenamed);
    assert_eq!(after.sequence, before.sequence + 1);
    assert_eq!((after.segments.len(), after.frontier), (2, 80));
}

#[test]
fn a_crash_mid_merge_keeps_the_inputs() {
    let dir = Dir::new("crash-merge");
    let corpus = Corpus::new(3, 120);
    let catalog = Catalog::new(120, &[]);
    let mut writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
    follow_all(&mut writer, &catalog, &corpus, &budget(150)).unwrap();
    let before = writer.view().manifest().clone();
    assert!(
        before.segments.len() > 5,
        "{} segments",
        before.segments.len()
    );

    let catalog = Catalog::new(120, &[0, 33, 119]);
    stop_at(Point::MergeMidway);
    assert!(
        writer
            .merge_all(&catalog.view(), &Budget::unbounded())
            .is_err()
    );
    assert!(
        dir.files().iter().any(|f| f.starts_with("tmp-")),
        "a crash left partial output for recovery"
    );
    drop(writer);

    let mut writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
    assert_eq!(writer.view().manifest(), &before);
    assert_eq!(dir.files(), named(&writer.view()));
    let clean = rebuild("crash-merge", &catalog, &corpus);
    assert_eq!(answers(&writer.view(), &catalog), clean);

    writer
        .merge_all(&catalog.view(), &Budget::unbounded())
        .unwrap();
    assert_eq!(writer.view().manifest().segments.len(), 1);
    assert_eq!(raw(&writer.view()), clean, "the merge purged the dead");
    assert_eq!(dir.files(), named(&writer.view()));
}

#[test]
fn a_document_dies_and_another_is_born_between_passes() {
    let dir = Dir::new("churn-pair");
    let corpus = Corpus::new(4, 40);
    let first = Catalog::new(30, &[]);
    let mut writer = IndexWriter::open(&dir.0, &first.view()).unwrap();
    follow_all(&mut writer, &first, &corpus, &Budget::unbounded()).unwrap();

    let second = Catalog::new(40, &[4, 17]);
    let asked = follow_all(&mut writer, &second, &corpus, &Budget::unbounded()).unwrap();
    assert_eq!(
        asked,
        (30..40).collect::<Vec<_>>(),
        "only the born are read"
    );
    let view = writer.view();
    assert_eq!(answers(&view, &second), second.truth(&corpus));
    assert_eq!(
        answers(&view, &second),
        rebuild("churn-pair", &second, &corpus)
    );
    assert!(view.uncovered(&second.live).is_empty());
}

#[test]
fn a_tokenizer_version_change_rebuilds_through_coverage() {
    let dir = Dir::new("tokenizer");
    let corpus = Corpus::new(5, 50);
    let catalog = Catalog::new(50, &[9]);
    let mut writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
    follow_all(&mut writer, &catalog, &corpus, &budget(100)).unwrap();
    drop(writer);

    let mut manifest = Manifest::read(&dir.0).unwrap().unwrap();
    manifest.tokenizer_version = TERM_VERSION + 1;
    manifest.publish(&dir.0).unwrap();

    let mut writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
    let view = writer.view();
    assert!(view.manifest().segments.is_empty());
    assert_eq!(view.manifest().tokenizer_version, TERM_VERSION);
    assert_eq!(view.uncovered(&catalog.live).len(), 49);
    assert_eq!(dir.files(), named(&view), "the old segments are removed");
    let asked = follow_all(&mut writer, &catalog, &corpus, &Budget::unbounded()).unwrap();
    assert_eq!(asked.len(), 49);
    assert_eq!(answers(&writer.view(), &catalog), catalog.truth(&corpus));
}

#[test]
fn an_index_built_before_t1_is_rebuilt() {
    let dir = Dir::new("pre-t1");
    let corpus = Corpus::new(5, 20);
    let catalog = Catalog::new(20, &[]);
    let mut writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
    follow_all(&mut writer, &catalog, &corpus, &budget(100)).unwrap();
    drop(writer);

    // Every index written before T1 records the bare tokenizer's version.
    let mut manifest = Manifest::read(&dir.0).unwrap().unwrap();
    assert_eq!(manifest.tokenizer_version, TERM_VERSION);
    assert_ne!(TERM_VERSION, ferret_text::TOKENIZER_VERSION);
    manifest.tokenizer_version = ferret_text::TOKENIZER_VERSION;
    manifest.publish(&dir.0).unwrap();

    let mut writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
    assert!(writer.view().manifest().segments.is_empty());
    assert_eq!(writer.view().uncovered(&catalog.live).len(), 20);
    follow_all(&mut writer, &catalog, &corpus, &Budget::unbounded()).unwrap();
    assert_eq!(writer.view().manifest().tokenizer_version, TERM_VERSION);
    assert_eq!(answers(&writer.view(), &catalog), catalog.truth(&corpus));
}

#[test]
fn follow_leaves_blob_tokens_out_of_the_term_index() {
    let dir = Dir::new("t1-follow");
    let catalog = Catalog::new(1, &[]);
    let text = b"amber nkzzCi5csBBZh21Dh5Z7n8pP1UaPL+fox+KKcXB0xYeDUnGtBJe3j1LIyw== basalt";
    let mut writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
    writer
        .follow(&catalog.view(), &Budget::unbounded(), &mut |_, bytes| {
            bytes.extend_from_slice(text);
            Ok(())
        })
        .unwrap();
    let view = writer.view();
    assert_eq!(view.lookup(b"amber").unwrap(), [0]);
    assert_eq!(view.lookup(b"basalt").unwrap(), [0]);
    assert!(view.lookup(b"fox").unwrap().is_empty(), "inside the blob");
    assert!(
        view.lookup(b"nkzzci5csbbzh21dh5z7n8pp1uapl")
            .unwrap()
            .is_empty()
    );
}

/// Four segments over 0..10, 10..20, 20..30 and 30..40, one pass each.
fn four_segments(dir: &Dir, corpus: &Corpus) -> IndexWriter {
    let mut writer = IndexWriter::open(&dir.0, &Catalog::new(10, &[]).view()).unwrap();
    for bound in [10, 20, 30, 40] {
        let catalog = Catalog::new(bound, &[]);
        follow_all(&mut writer, &catalog, corpus, &Budget::unbounded()).unwrap();
    }
    let ranges: Vec<_> = writer
        .view()
        .manifest()
        .segments
        .iter()
        .map(|s| (s.first, s.last, s.docs))
        .collect();
    assert_eq!(
        ranges,
        [(0, 9, 10), (10, 19, 10), (20, 29, 10), (30, 39, 10)]
    );
    writer
}

#[test]
fn a_merge_purges_the_dead_and_keeps_range_order() {
    let dir = Dir::new("purge");
    let corpus = Corpus::new(6, 40);
    let mut writer = four_segments(&dir, &corpus);
    // An entirely dead segment, a segment dead at both ends, and dead
    // first and last DocIds of the whole run.
    let mut dead: Vec<u32> = (10..20).collect();
    dead.extend([0, 20, 29, 39]);
    let catalog = Catalog::new(40, &dead);

    let merged = writer
        .merge_all(&catalog.view(), &Budget::unbounded())
        .unwrap()
        .unwrap();
    assert_eq!((merged.inputs, merged.purged), (4, 14));
    let entry = merged.segment.unwrap();
    assert_eq!((entry.first, entry.last, entry.docs), (0, 39, 26));
    let view = writer.view();
    assert_eq!(view.manifest().segments, [entry]);
    // Unfiltered: the segment itself holds exactly the live documents, in
    // order.
    assert_eq!(raw(&view), catalog.truth(&corpus));
    assert_eq!(raw(&view), rebuild("purge", &catalog, &corpus));
    assert_eq!(dir.files(), named(&view));
}

#[test]
fn the_dead_fraction_trigger_drops_a_dead_segment_and_rewrites_a_thinned_one() {
    let dir = Dir::new("dead-fraction");
    let corpus = Corpus::new(7, 40);
    let mut writer = four_segments(&dir, &corpus);

    // Segment 1 entirely dead, segment 3 three-tenths dead (past a
    // quarter), segment 0 one-tenth dead (not).
    let mut dead: Vec<u32> = (10..20).collect();
    dead.extend([5, 30, 35, 39]);
    let catalog = Catalog::new(40, &dead);
    let budget = Budget::unbounded();

    let first = writer
        .merge_if_needed(&catalog.view(), &budget)
        .unwrap()
        .unwrap();
    assert_eq!((first.inputs, first.purged, first.segment), (1, 10, None));
    let second = writer
        .merge_if_needed(&catalog.view(), &budget)
        .unwrap()
        .unwrap();
    let entry = second.segment.unwrap();
    assert_eq!((second.inputs, second.purged), (1, 3));
    assert_eq!((entry.first, entry.last, entry.docs), (30, 39, 7));
    assert!(
        writer
            .merge_if_needed(&catalog.view(), &budget)
            .unwrap()
            .is_none()
    );

    let view = writer.view();
    let ranges: Vec<_> = view
        .manifest()
        .segments
        .iter()
        .map(|s| (s.first, s.last))
        .collect();
    assert_eq!(ranges, [(0, 9), (20, 29), (30, 39)]);
    assert_eq!(answers(&view, &catalog), catalog.truth(&corpus));
    assert!(
        view.uncovered(&catalog.live).is_empty(),
        "the dropped range is not re-covered"
    );
    assert_eq!(dir.files(), named(&view));
}

#[test]
fn ten_small_segments_merge_into_one() {
    let dir = Dir::new("tiers");
    let corpus = Corpus::new(8, 100);
    let mut writer = IndexWriter::open(&dir.0, &Catalog::new(0, &[]).view()).unwrap();
    for bound in (10..=100).step_by(10) {
        let catalog = Catalog::new(bound, &[]);
        follow_all(&mut writer, &catalog, &corpus, &Budget::unbounded()).unwrap();
    }
    let catalog = Catalog::new(100, &[]);
    assert_eq!(writer.view().manifest().segments.len(), 10);
    let merged = writer
        .merge_if_needed(&catalog.view(), &Budget::unbounded())
        .unwrap()
        .unwrap();
    assert_eq!((merged.inputs, merged.purged), (10, 0));
    assert!(
        writer
            .merge_if_needed(&catalog.view(), &Budget::unbounded())
            .unwrap()
            .is_none()
    );
    assert_eq!(raw(&writer.view()), catalog.truth(&corpus));

    // A merge larger than the byte budget waits.
    drop(writer);
    let mut writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
    let bigger = Catalog::new(100, &(0..50).collect::<Vec<_>>());
    assert!(
        writer
            .merge_if_needed(&bigger.view(), &budget(1))
            .unwrap()
            .is_none()
    );
}

#[test]
fn an_unreadable_document_is_never_retried_and_leaves_when_the_catalog_drops_it() {
    let dir = Dir::new("unreadable");
    let corpus = Corpus::new(9, 30);
    let mut first = Catalog::new(20, &[]);
    first.unreadable = BTreeSet::from([3, 12]);
    let mut writer = IndexWriter::open(&dir.0, &first.view()).unwrap();
    let asked = follow_all(&mut writer, &first, &corpus, &Budget::unbounded()).unwrap();
    assert_eq!(asked, (0..20).collect::<Vec<_>>());
    let view = writer.view();
    assert_eq!(view.manifest().unreadable, [3, 12]);
    assert_eq!(view.uncovered(&first.live), [3, 12]);
    assert_eq!(answers(&view, &first), first.truth(&corpus));

    // The reader would now succeed; the index must not ask again.
    let second = Catalog::new(30, &[]);
    let asked = follow_all(&mut writer, &second, &corpus, &Budget::unbounded()).unwrap();
    assert_eq!(asked, (20..30).collect::<Vec<_>>());
    assert_eq!(writer.view().manifest().unreadable, [3, 12]);

    let third = Catalog::new(30, &[3]);
    let followed = writer
        .follow(&third.view(), &Budget::unbounded(), &mut |_, _| {
            panic!("nothing to read")
        })
        .unwrap();
    assert_eq!((followed.pruned, followed.docs), (1, 0));
    assert_eq!(writer.view().manifest().unreadable, [12]);
    // It survives a reopen too.
    let mut writer = IndexWriter::open(&dir.0, &third.view()).unwrap();
    assert_eq!(writer.view().manifest().unreadable, [12]);

    // A merge counts what it keeps without the unreadable document, which
    // sits in the range with no postings: 30 less dead 3 less unreadable
    // 12. Indexed were 18 + 10, and 3 never was, so nothing is purged.
    let merged = writer
        .merge_all(&third.view(), &Budget::unbounded())
        .unwrap()
        .unwrap();
    assert_eq!((merged.segment.unwrap().docs, merged.purged), (28, 0));
    assert_eq!(writer.view().manifest().unreadable, [12]);
    let mut third = third;
    third.unreadable = BTreeSet::from([12]);
    assert_eq!(raw(&writer.view()), third.truth(&corpus));
}

#[test]
fn an_index_past_the_catalogs_next_doc_is_discarded_on_open() {
    let dir = Dir::new("high-water");
    let corpus = Corpus::new(10, 50);
    let built = Catalog::new(50, &[]);
    let mut writer = IndexWriter::open(&dir.0, &built.view()).unwrap();
    follow_all(&mut writer, &built, &corpus, &Budget::unbounded()).unwrap();
    drop(writer);

    // The same incarnation at its own high water keeps the index...
    let writer = IndexWriter::open(&dir.0, &built.view()).unwrap();
    assert_eq!(writer.view().manifest().segments.len(), 1);
    drop(writer);

    // ...one whose next_doc is lower does not.
    let behind = Catalog::new(40, &[]);
    let mut writer = IndexWriter::open(&dir.0, &behind.view()).unwrap();
    let view = writer.view();
    assert!(view.manifest().segments.is_empty());
    assert_eq!(
        (view.manifest().frontier, view.manifest().high_water),
        (0, 0)
    );
    assert_eq!(dir.files(), named(&view));
    follow_all(&mut writer, &behind, &corpus, &Budget::unbounded()).unwrap();
    assert_eq!(raw(&writer.view()), behind.truth(&corpus));

    // So does another incarnation, mid-life.
    let mut other = Catalog::new(40, &[]);
    other.incarnation = [1; 16];
    let followed = writer
        .follow(&other.view(), &Budget::unbounded(), &mut |doc, bytes| {
            bytes.extend_from_slice(&corpus.text(doc));
            Ok(())
        })
        .unwrap();
    assert_eq!(followed.docs, 40);
    assert_eq!(writer.view().manifest().incarnation, [1; 16]);
}

#[test]
fn a_stop_request_publishes_what_the_pass_covered() {
    let dir = Dir::new("stop");
    let corpus = Corpus::new(11, 30);
    let catalog = Catalog::new(30, &[]);
    let mut writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
    let followed = writer
        .follow(&catalog.view(), &Budget::unbounded(), &mut |doc, bytes| {
            if doc == 12 {
                return Err(Fault::Stop);
            }
            bytes.extend_from_slice(&corpus.text(doc));
            Ok(())
        })
        .unwrap();
    assert_eq!(
        (followed.stopped, followed.docs, followed.remaining),
        (Stopped::Asked, 12, 18)
    );
    assert_eq!(writer.view().manifest().frontier, 12);
    let asked = follow_all(&mut writer, &catalog, &corpus, &Budget::unbounded()).unwrap();
    assert_eq!(asked, (12..30).collect::<Vec<_>>());
    assert_eq!(raw(&writer.view()), catalog.truth(&corpus));
}

/// Many rounds of deaths, births, unreadable documents, small follow
/// passes and merges; after each round the answers equal the truth.
fn churn(rounds: u32, seed: u64) {
    let dir = Dir::new(&format!("churn-{seed}"));
    let corpus = Corpus::new(seed, (rounds * 40) as usize);
    let mut rng = Rng(seed ^ 0x9e37);
    let (mut dead, mut unreadable) = (Vec::new(), BTreeSet::new());
    let mut writer = IndexWriter::open(&dir.0, &Catalog::new(0, &[]).view()).unwrap();
    for round in 1..=rounds {
        let bound = round * 40;
        for _ in 0..rng.below(25) {
            dead.push(rng.below(bound as usize) as u32);
        }
        for _ in 0..rng.below(2) {
            unreadable.insert(bound - 40 + rng.below(40) as u32);
        }
        let mut catalog = Catalog::new(bound, &dead);
        catalog.unreadable = unreadable.clone();
        follow_all(
            &mut writer,
            &catalog,
            &corpus,
            &budget(200 + rng.below(800) as u64),
        )
        .unwrap();
        while writer
            .merge_if_needed(&catalog.view(), &Budget::unbounded())
            .unwrap()
            .is_some()
        {}
        let view = writer.view();
        assert_eq!(
            answers(&view, &catalog),
            catalog.truth(&corpus),
            "round {round}"
        );
        let ranges = &view.manifest().segments;
        assert!(ranges.windows(2).all(|p| p[0].last < p[1].first));
        assert!(
            ranges.len() < 3 * merge::MERGE_FACTOR,
            "{} segments",
            ranges.len()
        );
    }
}

#[test]
fn a_short_churn_stays_correct() {
    churn(12, 12);
}

#[test]
#[ignore = "long churn; run with --ignored"]
fn a_long_churn_stays_correct() {
    for seed in 100..104 {
        churn(150, seed);
    }
}

#[test]
fn a_cooperatively_cancelled_merge_can_retry_without_reopening() {
    let dir = Dir::new("cooperative-retry");
    let catalog = Catalog::new(20, &[]);
    let corpus = Corpus::new(42, 20);
    let mut writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
    follow_all(&mut writer, &catalog, &corpus, &Budget::unbounded()).unwrap();
    let before = writer.view();
    let paused = std::cell::Cell::new(false);
    let pace = |_: usize| {
        paused.set(true);
        Ok(())
    };
    let cancelled = || paused.get();
    let budget = Budget {
        pace: &pace,
        cancelled: &cancelled,
        ..Budget::unbounded()
    };
    assert!(matches!(
        writer.merge_all(&catalog.view(), &budget),
        Err(Error::Cancelled)
    ));
    assert!(Arc::ptr_eq(&writer.view(), &before));
    assert_eq!(dir.files(), named(&before));
    assert!(
        writer
            .merge_all(&catalog.view(), &Budget::unbounded())
            .unwrap()
            .is_some()
    );
    assert_eq!(
        answers(&writer.view(), &catalog),
        rebuild("cooperative-retry", &catalog, &corpus)
    );
}

#[test]
fn an_interrupted_io_error_is_not_a_cooperative_pause() {
    let dir = Dir::new("interrupted-io");
    let catalog = Catalog::new(20, &[]);
    let corpus = Corpus::new(42, 20);
    let mut writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
    follow_all(&mut writer, &catalog, &corpus, &Budget::unbounded()).unwrap();
    let pace = |_: usize| {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "real I/O failure",
        ))
    };
    let budget = Budget {
        pace: &pace,
        ..Budget::unbounded()
    };
    assert!(
        matches!(writer.merge_all(&catalog.view(), &budget), Err(Error::Segment(ReadError::Io(error))) if error.kind() == io::ErrorKind::Interrupted)
    );
    assert!(matches!(
        writer.merge_all(&catalog.view(), &Budget::unbounded()),
        Err(Error::Poisoned)
    ));
}

/// A follow can make a tiny newly appended segment 100% dead. That new policy
/// preference must not displace the inputs of a cancelled large merge.
#[test]
fn cancelled_merge_selection_survives_unrelated_follow() {
    let dir = Dir::new("retry-selection");
    let corpus = Corpus::new(0x5eed, 1001);
    let mut catalog = Catalog::new(1000, &[]);
    let mut writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
    follow_all(&mut writer, &catalog, &corpus, &Budget::unbounded()).unwrap();
    catalog.live = DocSet::new(1000, 0..500);
    let resources = writer.view().merge_resources(&catalog.live).unwrap();
    let cancelled = Budget {
        cancelled: &|| true,
        ..Budget::unbounded()
    };
    assert!(matches!(
        writer.merge_if_needed(&catalog.view(), &cancelled),
        Err(Error::Cancelled)
    ));
    catalog.live = DocSet::new(1001, (0..500).chain([1000]));
    follow_all(&mut writer, &catalog, &corpus, &Budget::unbounded()).unwrap();
    catalog.live = DocSet::new(1001, 0..500);
    assert_eq!(
        writer.view().merge_resources(&catalog.live).unwrap(),
        resources
    );
    let merged = writer
        .merge_if_needed(&catalog.view(), &Budget::unbounded())
        .unwrap()
        .unwrap();
    assert_eq!(merged.purged, 500, "retry the large half-dead input first");
}

#[test]
fn pair_crashes_recover_the_previous_manifest_and_sweep_both_files() {
    for point in [Point::TermSynced, Point::TermRenamed, Point::PairRenamed] {
        let (before, after) = crash_while_following(&format!("pair-{point:?}"), point);
        assert_eq!(before, after);
    }
}

#[test]
fn cancellation_after_the_first_output_removes_outputs_and_can_retry() {
    let dir = Dir::new("pair-cancel");
    let catalog = Catalog::new(40, &[]);
    let corpus = Corpus::new(901, 40);
    let mut writer = four_segments(&dir, &corpus);
    let before = writer.view();
    CANCELLED.set(false);
    CANCEL_AT.set(Some(Point::TermSynced));
    let budget = Budget {
        cancelled: &|| CANCELLED.get(),
        ..Budget::unbounded()
    };
    assert!(matches!(
        writer.merge_all(&catalog.view(), &budget),
        Err(Error::Cancelled)
    ));
    CANCELLED.set(false);
    assert!(Arc::ptr_eq(&before, &writer.view()));
    assert_eq!(dir.files(), named(&before));
    writer
        .merge_all(&catalog.view(), &Budget::unbounded())
        .unwrap();
    assert_eq!(raw(&writer.view()), catalog.truth(&corpus));
}

#[test]
fn publication_failure_keeps_outputs_and_reopen_selects_the_new_pair() {
    let dir = Dir::new("pair-publication");
    let catalog = Catalog::new(40, &[]);
    let corpus = Corpus::new(902, 40);
    let mut writer = four_segments(&dir, &corpus);
    let before = writer.view();
    stop_at(Point::ManifestRenamed);
    assert!(
        writer
            .merge_all(&catalog.view(), &Budget::unbounded())
            .is_err()
    );
    let published = Manifest::read(&dir.0).unwrap().unwrap();
    assert_eq!(published.segments.len(), 1);
    let output = published.segments[0];
    assert!(dir.0.join(output.file_name()).exists());
    assert!(dir.0.join(output.trigram_file_name()).exists());
    assert!(dir.files().is_superset(&named(&before)));
    drop(writer);
    let writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
    assert_eq!(writer.view().manifest(), &published);
    assert_eq!(dir.files(), named(&writer.view()));
    assert_eq!(raw(&writer.view()), catalog.truth(&corpus));
    assert_trigrams(&writer.view(), &catalog, &corpus);
}

fn assert_trigrams(view: &View, catalog: &Catalog, corpus: &Corpus) {
    use std::collections::BTreeMap;
    let mut truth = BTreeMap::<Vec<u8>, Vec<u32>>::new();
    for doc in catalog.live.range(0, catalog.live.bound()) {
        if catalog.unreadable.contains(&doc) {
            continue;
        }
        let keys: BTreeSet<_> = corpus
            .text(doc)
            .windows(3)
            .map(|w| w.iter().map(u8::to_ascii_lowercase).collect::<Vec<_>>())
            .collect();
        for key in keys {
            truth.entry(key).or_default().push(doc);
        }
    }
    let mut actual = BTreeMap::<Vec<u8>, Vec<u32>>::new();
    for segment in view.trigrams() {
        let mut cursor = segment.terms();
        while let Some((key, entry)) = cursor.next_term().unwrap() {
            let docs = actual.entry(key.to_vec()).or_default();
            match segment.read(&entry).unwrap() {
                Hit::Single(doc) => docs.push(doc),
                Hit::Postings(list) => list.decode(docs),
            }
        }
    }
    assert_eq!(actual, truth);
}

#[test]
fn pair_merge_purges_both_files_and_old_pins_remain_readable() {
    let dir = Dir::new("pair-purge");
    let corpus = Corpus::new(903, 40);
    let mut writer = four_segments(&dir, &corpus);
    let old = writer.view();
    let catalog = Catalog::new(40, &[0, 7, 10, 19, 20, 29, 39]);
    writer
        .merge_all(&catalog.view(), &Budget::unbounded())
        .unwrap();
    assert_eq!(raw(&writer.view()), catalog.truth(&corpus));
    assert_trigrams(&writer.view(), &catalog, &corpus);
    assert_trigrams(&old, &Catalog::new(40, &[]), &corpus);
    assert_eq!(dir.files(), named(&writer.view()));
}

#[test]
fn legacy_v2_and_changed_trigram_version_rebuild_both_structures() {
    for legacy in [true, false] {
        let dir = Dir::new(if legacy { "pair-v2" } else { "pair-version" });
        let corpus = Corpus::new(904, 40);
        let catalog = Catalog::new(40, &[]);
        let writer = four_segments(&dir, &corpus);
        let mut manifest = writer.view().manifest().clone();
        drop(writer);
        if legacy {
            // A genuine v2 layout: 80-byte head and 64-byte term entries,
            // without the v3 extractor field or per-entry trigram facts.
            let current = manifest.encode();
            let end = 88 + manifest.segments.len() * 104;
            let mut bytes = current[..80].to_vec();
            bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
            for entry in current[88..end].chunks_exact(104) {
                bytes.extend_from_slice(&entry[..64]);
            }
            bytes.extend_from_slice(&current[end..current.len() - 16]);
            let digest = blake3::hash(&bytes);
            bytes.extend_from_slice(&digest.as_bytes()[..16]);
            fs::write(dir.0.join(manifest::FILE), bytes).unwrap();
        } else {
            manifest.trigram_version = TRIGRAM_VERSION + 1;
            manifest.publish(&dir.0).unwrap();
        }
        assert!(View::open(&dir.0, &catalog.view()).unwrap().is_none());
        let mut writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
        assert!(writer.view().segments().is_empty());
        assert!(writer.view().trigrams().is_empty());
        assert_eq!(writer.view().uncovered(&catalog.live).len(), 40);
        assert_eq!(dir.files(), named(&writer.view()));
        follow_all(&mut writer, &catalog, &corpus, &Budget::unbounded()).unwrap();
        assert_trigrams(&writer.view(), &catalog, &corpus);
    }
}

#[test]
fn every_head_fact_is_checked_for_each_file() {
    for tri in [false, true] {
        for field in 0..7 {
            let dir = Dir::new(&format!("pair-head-{tri}-{field}"));
            let corpus = Corpus::new(905, 40);
            let writer = four_segments(&dir, &corpus);
            let catalog = Catalog::new(40, &[]);
            let mut manifest = writer.view().manifest().clone();
            drop(writer);
            let entry = &mut manifest.segments[1];
            match field {
                2 if tri => entry.trigram_keys += 1,
                2 => entry.terms += 1,
                3 if tri => entry.trigram_pairs += 1,
                3 => entry.pairs += 1,
                4 if tri => entry.trigram_bytes += 1,
                4 => entry.bytes += 1,
                5 if tri => entry.trigram_digest[0] ^= 1,
                5 => entry.digest[0] ^= 1,
                0 | 1 | 6 => {
                    // Rewrite a valid head with the wrong extractor version,
                    // preserving its digest in the manifest to isolate version.
                    let name = if tri {
                        entry.trigram_file_name()
                    } else {
                        entry.file_name()
                    };
                    let version = if tri { TRIGRAM_VERSION } else { TERM_VERSION };
                    let mut out = File::create(dir.0.join(&name)).unwrap();
                    let first = entry.first + u32::from(field == 0);
                    let last = entry.last - u32::from(field == 1);
                    let sizes = Writer::keyed(first, last, version + u32::from(field == 6))
                        .unwrap()
                        .finish(&mut out)
                        .unwrap();
                    let segment = Segment::open(File::open(dir.0.join(name)).unwrap()).unwrap();
                    if tri {
                        entry.trigram_keys = 0;
                        entry.trigram_pairs = 0;
                        entry.trigram_bytes = sizes.total();
                        entry.trigram_digest = segment.digest();
                    } else {
                        entry.terms = 0;
                        entry.pairs = 0;
                        entry.bytes = sizes.total();
                        entry.digest = segment.digest();
                    }
                }
                _ => unreachable!(),
            }
            // Direct pair opening also isolates a bad version from fits().
            assert!(matches!(
                open_pairs(&dir.0, &manifest, 4),
                Err(Error::Segment(ReadError::Head))
            ));
            {
                manifest.publish(&dir.0).unwrap();
                assert!(matches!(
                    View::open(&dir.0, &catalog.view()),
                    Err(Error::Segment(ReadError::Head))
                ));
            }
        }
    }
}

#[test]
fn revision_five_charge_and_exact_admission_make_progress() {
    let mut terms = Inverter::new(BUFFER);
    let mut trigrams = TrigramInverter::with_capacity(PairBuffer::CAPACITY);
    assert!(!PairBuffer::full(&terms, &trigrams, BUFFER));
    assert!(PairBuffer::fits(trigrams.len(), 1));
    terms.add(0, b"word").unwrap();
    trigrams.add(0, b"abcabc").unwrap();
    assert!(PairBuffer::full(
        &terms,
        &trigrams,
        terms.memory() + 16 * trigrams.len()
    ));
    assert!(!PairBuffer::full(
        &terms,
        &trigrams,
        terms.memory() + 16 * trigrams.len() + 1
    ));

    let dir = Dir::new("pair-progress");
    let catalog = Catalog::new(2, &[]);
    let mut writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
    let bytes = vec![b'a'; PairBuffer::CAPACITY + 3];
    let budget = Budget {
        bytes: u64::MAX,
        ..Budget::unbounded()
    };
    let followed = writer
        .follow(&catalog.view(), &budget, &mut |doc, out| {
            out.extend_from_slice(if doc == 0 { &bytes[..] } else { b"abc" });
            Ok(())
        })
        .unwrap();
    assert_eq!((followed.docs, followed.remaining), (2, 0));
    assert_eq!(followed.segment.unwrap().docs, 2);
    assert_eq!(writer.trigrams.len(), 0);
    assert!(writer.trigrams.allocated_bytes() >= PairBuffer::CAPACITY * 8 + (2 << 20));

    let dir = Dir::new("pair-distinct-oversized");
    let catalog = Catalog::new(2, &[]);
    let mut writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
    let mut seed = 0x4b17_9a2d_03e5_c681u64;
    let bytes: Vec<_> = (0..(4 << 20))
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 32) as u8
        })
        .collect();
    let distinct = writer.trigrams.distinct_count(&bytes);
    assert!(distinct > PairBuffer::CAPACITY, "distinct keys: {distinct}");
    let budget = Budget {
        buffer: 0,
        bytes: 0,
        ..Budget::unbounded()
    };
    let first = writer
        .follow(&catalog.view(), &budget, &mut |doc, out| {
            out.extend_from_slice(if doc == 0 { &bytes[..] } else { b"abc" });
            Ok(())
        })
        .unwrap();
    assert_eq!((first.docs, first.remaining), (1, 1));
    let next = writer
        .follow(&catalog.view(), &budget, &mut |_, out| {
            out.extend_from_slice(b"abc");
            Ok(())
        })
        .unwrap();
    assert_eq!(next.docs, 1);
    assert_eq!(next.remaining, 0);
}

#[test]
fn trigram_sources_estimate_read_and_share_coverage() {
    use crate::candidate::{Atom, Pinned, Source};
    let dir = Dir::new("pair-source");
    let corpus = Corpus::new(906, 40);
    let writer = four_segments(&dir, &corpus);
    let view = writer.view();
    let live = DocSet::new(42, 0..42);
    let pinned = Pinned::new(Some(&view), &live);
    assert_eq!(pinned.sources().count(), 2);
    assert_eq!(
        pinned.uncovered().range(0, 42).collect::<Vec<_>>(),
        [40, 41]
    );
    assert_eq!(Pinned::new(None, &live).sources().count(), 0);
    let source = Source::Trigrams(&view);
    assert!(source.read(&Atom::Term(b"amber")).unwrap().is_none());
    assert!(source.estimate(&Atom::Term(b"amber")).unwrap().is_none());
    for key in [*b"amb", *b"ber", [0xff; 3]] {
        let atom = Atom::Trigram(key);
        let expected: Vec<_> = (0..40)
            .filter(|&doc| corpus.text(doc).windows(3).any(|w| w == key))
            .collect();
        let estimate = source.estimate(&atom).unwrap().unwrap();
        assert_eq!(estimate.docs, expected.len() as u64);
        let candidates = source.read(&atom).unwrap().unwrap();
        assert_eq!(candidates.estimate().docs, estimate.docs);
        let mut cursor = candidates.cursor();
        let mut actual = Vec::new();
        let mut target = 0;
        while let Some((doc, _)) = cursor.next_geq(target) {
            actual.push(doc);
            target = doc + 1;
        }
        assert_eq!(actual, expected);
        assert!(matches!(
            source.estimate_until(&atom, &|| true),
            Err(ReadError::Cancelled)
        ));
        assert!(matches!(
            source.read_until(&atom, &|| true),
            Err(ReadError::Cancelled)
        ));
    }
}

#[test]
fn publication_error_before_rename_keeps_outputs_until_recovery() {
    let dir = Dir::new("pair-publication-old");
    let corpus = Corpus::new(907, 40);
    let mut writer = four_segments(&dir, &corpus);
    let catalog = Catalog::new(40, &[]);
    let before = writer.view();
    stop_at(Point::ManifestSynced);
    assert!(
        writer
            .merge_all(&catalog.view(), &Budget::unbounded())
            .is_err()
    );
    assert_eq!(Manifest::read(&dir.0).unwrap().unwrap(), *before.manifest());
    assert_eq!(dir.files().len(), named(&before).len() + 3);
    drop(writer);
    let writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
    assert_eq!(writer.view().manifest(), before.manifest());
    assert_eq!(dir.files(), named(&before));
    assert_trigrams(&writer.view(), &catalog, &corpus);
}

#[test]
fn follow_pause_commits_the_completed_pair() {
    let dir = Dir::new("pair-follow-pause");
    let live = DocSet::new(2, 0..2);
    let catalog = CatalogView {
        incarnation: [9; 16],
        live: &live,
    };
    let mut writer = IndexWriter::open(&dir.0, &catalog).unwrap();
    let paused = std::cell::Cell::new(false);
    let budget = Budget {
        cancelled: &|| paused.get(),
        ..Budget::unbounded()
    };
    let followed = writer
        .follow(&catalog, &budget, &mut |_, bytes| {
            bytes.extend_from_slice(b"amber");
            paused.set(true);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        (followed.docs, followed.remaining, followed.stopped),
        (1, 1, Stopped::Asked)
    );
    assert_eq!(writer.view().manifest().frontier, 1);
    assert_eq!(writer.view().segments().len(), 1);
    assert_eq!(writer.view().trigrams().len(), 1);
    assert_eq!(dir.files(), named(&writer.view()));
}

#[test]
fn error_after_successful_reconcile_cleans_unpublished_outputs() {
    let dir = Dir::new("pair-reconcile-error");
    let corpus = Corpus::new(908, 40);
    let mut writer = four_segments(&dir, &corpus);
    let mut catalog = Catalog::new(40, &[]);
    catalog.incarnation = [8; 16];
    let budget = Budget {
        pace: &|_| Err(io::Error::other("injected output failure")),
        ..Budget::unbounded()
    };
    assert!(
        writer
            .follow(&catalog.view(), &budget, &mut |doc, out| {
                out.extend_from_slice(&corpus.text(doc));
                Ok(())
            })
            .is_err()
    );
    assert!(writer.view().segments().is_empty());
    assert_eq!(dir.files(), named(&writer.view()));
}

/// Review of P7: segments open on the host's thread count, capped by the
/// segment count, and a refused spawn opens the rest on the calling thread,
/// in manifest order and with the first error a serial open returns.
#[test]
fn segment_open_follows_the_thread_count_and_survives_a_refused_spawn() {
    let dir = Dir::new("open-threads");
    let corpus = Corpus::new(1049, 40);
    let writer = four_segments(&dir, &corpus);
    let catalog = Catalog::new(40, &[]);
    let manifest = writer.view().manifest().clone();
    let entries = &manifest.segments;
    let expected: Vec<_> = entries
        .iter()
        .map(|e| e.digest)
        .chain(entries.iter().map(|e| e.trigram_digest))
        .collect();
    let digests = |segments: &[Arc<Segment<File>>], trigrams: &[Arc<Segment<File>>]| {
        segments
            .iter()
            .chain(trigrams)
            .map(|s| s.digest())
            .collect::<Vec<_>>()
    };
    for (threads, spawned) in [(0, 0), (1, 0), (2, 2), (3, 2), (4, 4), (64, 4)] {
        OPEN_SPAWNS.set(0);
        let view = View::open_with_threads(&dir.0, &catalog.view(), threads)
            .unwrap()
            .unwrap();
        let opened = digests(view.segments(), view.trigrams());
        assert_eq!((OPEN_SPAWNS.get(), opened), (spawned, expected.clone()));
    }
    for fail_at in [0, 1, 3] {
        FAIL_OPEN_SPAWN_AT.set(Some(fail_at));
        OPEN_SPAWNS.set(0);
        let (segments, trigrams) = open_pairs(&dir.0, &manifest, 4).unwrap();
        let opened = digests(&segments, &trigrams);
        assert_eq!((OPEN_SPAWNS.get(), opened), (fail_at, expected.clone()));
    }
    // A bad head in the last chunk is reported, whoever opens that chunk.
    let mut broken = manifest.clone();
    broken.segments[3].bytes += 1;
    let refusals = [None, Some(0), Some(1), Some(3)];
    for (threads, fail_at) in [2, 4].into_iter().flat_map(|t| refusals.map(|f| (t, f))) {
        FAIL_OPEN_SPAWN_AT.set(fail_at);
        let error = open_pairs(&dir.0, &broken, threads).err();
        assert!(
            matches!(error, Some(Error::Segment(ReadError::Head))),
            "{threads} threads, refused at {fail_at:?}: {error:?}"
        );
    }
    // An earlier NotFound wins over that later head error, as serially: the
    // NotFound is what makes View::open retry from a newer manifest.
    fs::remove_file(dir.0.join(entries[1].file_name())).unwrap();
    for (threads, fail_at) in [2, 4].into_iter().flat_map(|t| refusals.map(|f| (t, f))) {
        FAIL_OPEN_SPAWN_AT.set(fail_at);
        let error = open_pairs(&dir.0, &broken, threads).err();
        assert!(
            matches!(&error, Some(Error::Io(e)) if e.kind() == io::ErrorKind::NotFound),
            "{threads} threads, refused at {fail_at:?}: {error:?}"
        );
    }
    FAIL_OPEN_SPAWN_AT.set(None);
}

/// A corpus of short postings lists: each document holds 25 words of 3,000,
/// so a typical list is a few documents and a page holds hundreds of them.
fn wide_text(doc: u32) -> Vec<u8> {
    let mut rng = Rng(u64::from(doc) + 1);
    let words: Vec<String> = (0..25).map(|_| format!("w{}", rng.below(3000))).collect();
    words.join(" ").into_bytes()
}

/// An index over [`wide_text`] in several segments of about `bytes` of
/// text each.
fn wide_index(dir: &Dir, catalog: &Catalog, bytes: u64) -> IndexWriter {
    let mut writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
    loop {
        let followed = writer
            .follow(&catalog.view(), &budget(bytes), &mut |doc, text| {
                text.extend_from_slice(&wide_text(doc));
                Ok(())
            })
            .unwrap();
        if followed.stopped == Stopped::Covered {
            break;
        }
    }
    let segments = writer.view().manifest().segments.len();
    assert!(segments >= 4, "{segments} segments");
    writer
}

/// Bytes of the distinct file pages `reads` touch, counted from the spans
/// alone.
fn touched_bytes(reads: &[input_trace::Read]) -> u64 {
    let page = CHUNK as u64;
    let pages: BTreeSet<(usize, u64, u64)> = reads
        .iter()
        .flat_map(|r| {
            let end = r.offset + r.len as u64;
            (r.offset / page..end.div_ceil(page)).map(move |p| (r.file, r.size, p))
        })
        .collect();
    pages
        .iter()
        .map(|&(_, size, p)| size.min((p + 1) * page) - p * page)
        .sum()
}

#[test]
fn a_merge_charges_the_pacer_for_input_pages_it_reads() {
    let dir = Dir::new("charge-once");
    let catalog = Catalog::new(2400, &[]);
    let mut writer = wide_index(&dir, &catalog, 80_000);
    let (input, other) = (std::cell::Cell::new(0), std::cell::Cell::new(0));
    let pace = |bytes: usize| {
        let counter = if input_trace::charging() {
            &input
        } else {
            &other
        };
        counter.set(counter.get() + bytes as u64);
        Ok(())
    };
    let budget = Budget {
        pace: &pace,
        ..Budget::unbounded()
    };
    let (merged, reads) = input_trace::during(|| writer.merge_all(&catalog.view(), &budget));
    let merged = merged.unwrap().unwrap();
    let page = CHUNK as u64;
    assert!(
        reads.iter().any(|r| r.offset % page != 0),
        "no unaligned read"
    );
    assert!(
        reads
            .iter()
            .any(|r| r.size % page != 0 && r.offset + r.len as u64 > r.size / page * page),
        "no read of a final partial page"
    );
    // Every page read is charged at least once. Repeat reads of the pages
    // nearby are not charged again, so the inputs, some larger than the
    // window, are charged within an eighth of their distinct pages (3% over
    // when written); charging every read was 240 times them.
    let touched = touched_bytes(&reads);
    let (input, other) = (input.get(), other.get());
    assert!(
        touched <= input && input <= touched + touched / 8,
        "input paced {input} for {touched} touched"
    );
    // The writer charges its output, and at most twice more for a postings
    // section spilled to scratch and copied back.
    assert!(
        other <= 3 * merged.written,
        "output paced {other} for {} written",
        merged.written
    );
}

#[test]
fn a_merge_input_charges_a_page_again_once_it_leaves_the_window() {
    let dir = Dir::new("charge-window");
    fs::create_dir_all(&dir.0).unwrap();
    let page = CHUNK as u64;
    let pages = RECENT_PAGES as u64 + 2;
    let size = pages * page + 100;
    let path = dir.0.join("input");
    fs::write(&path, vec![7; size as usize]).unwrap();
    let file = File::open(&path).unwrap();
    let paced = RefCell::new(Vec::new());
    let pace = |bytes: usize| {
        paced.borrow_mut().push(bytes as u64);
        Ok(())
    };
    let input = PacedInput::new(&file, &pace).unwrap();
    let charge = |offset: u64, len: usize| {
        let mut buf = vec![0; len];
        input.read_exact_at(&mut buf, offset).unwrap();
        assert_eq!(buf, vec![7; len]);
        paced.borrow_mut().drain(..).collect::<Vec<_>>()
    };
    // An unaligned span charges both whole pages it touches; a repeat is
    // free but still calls the pacer.
    assert_eq!(charge(100, CHUNK), [2 * page]);
    assert_eq!(charge(page - 1, 2), [0]);
    // The final partial page charges only the bytes the file has.
    assert_eq!(charge(size - 10, 10), [100]);
    assert_eq!(charge(pages * page, 1), [0]);
    // Touching a window's worth of other pages pushes pages 0 and 1 and the
    // final page out.
    for p in 2..pages {
        assert_eq!(charge(p * page, CHUNK), [page], "page {p}");
    }
    assert_eq!(charge(pages * page + 1, 1), [100]);
    assert_eq!(charge(0, 1), [page]);
    // The last page the loop read is still among the most recent.
    assert_eq!(charge((pages - 1) * page + 5, 5), [0]);
}

#[test]
fn a_merge_still_checks_each_input_chunk() {
    let dir = Dir::new("charge-once-corrupt");
    let catalog = Catalog::new(600, &[]);
    let mut writer = wide_index(&dir, &catalog, 20_000);
    let path = dir.0.join(writer.view().manifest().segments[1].file_name());
    let segment = Segment::open(File::open(&path).unwrap()).unwrap();
    let (blocks, postings) = (
        segment.section_bytes(Section::Blocks),
        segment.section_bytes(Section::Postings),
    );
    let sections: u64 = [
        Section::Blocks,
        Section::Postings,
        Section::Index,
        Section::Sums,
        Section::Fingerprints,
        Section::FingerprintIndex,
    ]
    .map(|s| segment.section_bytes(s))
    .iter()
    .sum();
    let head = fs::metadata(&path).unwrap().len() - sections;
    let at = head + blocks + postings / 2;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut byte = [0];
    file.read_exact_at(&mut byte, at).unwrap();
    std::os::unix::fs::FileExt::write_all_at(&file, &[byte[0] ^ 0xff], at).unwrap();

    let error = writer
        .merge_all(&catalog.view(), &Budget::unbounded())
        .err();
    assert!(
        matches!(
            error,
            Some(Error::Segment(ReadError::Corrupt(Section::Postings)))
        ),
        "{error:?}"
    );
}
