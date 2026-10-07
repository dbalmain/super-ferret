//! Every test drives the real [`IndexWriter`] against a generated corpus,
//! and judges it two ways: against the words the generator put in each
//! document (`truth`), and against a clean rebuild of the same catalog view
//! in a fresh directory. Crashes are injected at [`Point`]s through
//! [`STOP`], which fails the operation there as a crash would stop it.

use std::collections::BTreeSet;

use super::*;
use crate::manifest;

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
        .map(SegmentEntry::file_name)
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
        "the crash left a partial merge"
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
    manifest.tokenizer_version = TOKENIZER_VERSION + 1;
    manifest.publish(&dir.0).unwrap();

    let mut writer = IndexWriter::open(&dir.0, &catalog.view()).unwrap();
    let view = writer.view();
    assert!(view.manifest().segments.is_empty());
    assert_eq!(view.manifest().tokenizer_version, TOKENIZER_VERSION);
    assert_eq!(view.uncovered(&catalog.live).len(), 49);
    assert_eq!(dir.files(), named(&view), "the old segments are removed");
    let asked = follow_all(&mut writer, &catalog, &corpus, &Budget::unbounded()).unwrap();
    assert_eq!(asked.len(), 49);
    assert_eq!(answers(&writer.view(), &catalog), catalog.truth(&corpus));
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
