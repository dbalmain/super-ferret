//! Cancellation of catalog-derived query state after real publications.
#![allow(clippy::unwrap_used)]

use std::cell::Cell;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::time::SystemTime;

use ferret_crawl::{IndexOptions, Refresh, RefreshReason, RefreshRequest, RefreshScope, index};

use ferret_catalog::NameId;

use super::*;

struct Tree(PathBuf);
impl Tree {
    /// An indexed root of `files` small documents, each holding `alpha`,
    /// a hundred to a directory.
    fn with_files(files: usize) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "ferret-derived-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(base.join("root")).unwrap();
        let tree = Self(base);
        tree.add(0, files);
        let base = &tree.0;
        index(
            &base.join("index"),
            &[base.join("root")],
            Refresh::All,
            &IndexOptions::default(),
        )
        .unwrap();
        tree
    }
    fn engine(&self) -> Engine {
        Engine::from_writer(ferret_catalog::WriterSession::open(&self.0.join("index")).unwrap())
    }
    fn publish(&self, engine: &Engine) {
        fs::write(self.0.join("root/new.txt"), b"new publication").unwrap();
        self.refresh(engine);
    }
    /// Writes `files` more documents, a hundred to a directory, numbered
    /// on from `first`.
    fn add(&self, first: usize, files: usize) {
        for i in first..first + files {
            let dir = self.0.join("root").join(format!("d{}", i / 100));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(format!("{i}.txt")), format!("alpha {i}")).unwrap();
        }
    }
    /// Deletes every document but `d0`'s, the first hundred. The
    /// directories stay: a vanished directory leaves its subtree
    /// unobserved, which the crawl answers with a fresh checkpoint rather
    /// than overlay deletions.
    fn delete_all_but_the_first_hundred(&self) {
        for dir in fs::read_dir(self.0.join("root")).unwrap() {
            let dir = dir.unwrap().path();
            if dir.file_name().unwrap() != "d0" {
                for file in fs::read_dir(&dir).unwrap() {
                    fs::remove_file(file.unwrap().path()).unwrap();
                }
            }
        }
    }
    fn refresh(&self, engine: &Engine) {
        engine
            .refresh(
                RefreshRequest {
                    expected_generation: engine.pin().generation(),
                    scopes: vec![RefreshScope::Root(self.0.join("root"))],
                    rename_hints: Vec::new(),
                    reason: RefreshReason::Burst,
                },
                &IndexOptions::default(),
            )
            .unwrap();
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The warmed old generation does not excuse doing cold preparation before
/// checking an already-cancelled query on the newly published generation.
#[test]
fn cancelled_cold_query_after_publication_does_not_publish_derived_caches() {
    let tree = Tree::with_files(128);
    let engine = tree.engine();
    let query = Query::from_args([b"text:alpha".as_slice()], SystemTime::now()).unwrap();
    let old = engine.pin();
    old.search_content(&query, None, None, |_| ControlFlow::Continue(()))
        .unwrap();
    assert!(old.derived.docs.get().is_some());
    assert!(old.derived.live.get().is_some());
    tree.publish(&engine);
    let cold = engine.pin();
    assert_ne!(old.generation(), cold.generation());
    assert!(cold.derived.docs.get().is_none());
    assert!(cold.derived.live.get().is_none());
    let cancelled = AtomicBool::new(true);
    cold.search_content(&query, None, Some(&cancelled), |_| panic!("cancelled row"))
        .unwrap();
    assert!(
        cold.derived.docs.get().is_none(),
        "cancelled cold DocNames build was published"
    );
    assert!(
        cold.derived.live.get().is_none(),
        "cancelled cold live build was published"
    );
    assert!(
        cold.search_content(&query, None, None, |_| ControlFlow::Continue(()))
            .unwrap()
            .rows
            > 0
    );
}

/// Each derived-state builder over a catalog of many thousand documents
/// stops at the checkpoint where the flag is first seen, partway through
/// its scan, and leaves the view's cache empty for the next query.
#[test]
fn cancelling_partway_through_a_derived_build_leaves_the_cache_empty() {
    const FILES: usize = 4 * DocSet::CHECK_EVERY;
    let tree = Tree::with_files(FILES);
    let engine = tree.engine();
    let cold = engine.pin();
    assert!(cold.catalog().docs().count() >= FILES);

    // DocNames asks at entry, after loading, then every CHECK_EVERY names:
    // the fourth ask is the second in its first pass, with more to come.
    let asks = Cell::new(0);
    let fourth = || {
        asks.set(asks.get() + 1);
        asks.get() == 4
    };
    assert!(cold.doc_names_until(fourth).unwrap().is_none());
    assert_eq!(asks.get(), 4, "DocNames did not stop at the flag");
    assert!(
        cold.derived.docs.get().is_none(),
        "cancelled DocNames build was published"
    );

    // The live set asks at entry, then every CHECK_EVERY documents.
    asks.set(0);
    let third = || {
        asks.set(asks.get() + 1);
        asks.get() == 3
    };
    assert!(cold.live_until(third).is_none());
    assert_eq!(asks.get(), 3, "the live build did not stop at the flag");
    assert!(
        cold.derived.live.get().is_none(),
        "cancelled live build was published"
    );

    // The next, uncancelled query builds and publishes both in full.
    let query = Query::from_args([b"text:alpha".as_slice()], SystemTime::now()).unwrap();
    let stats = cold
        .search_content(&query, None, None, |_| ControlFlow::Continue(()))
        .unwrap();
    assert_eq!(stats.rows, FILES as u64);
    assert_eq!(cold.derived.live.get().unwrap().len() as usize, FILES);
    assert!(cold.derived.docs.get().is_some());
}

/// A predicate that answers true from its `n`th ask on, as a host flag
/// set partway through stays set, counting every ask.
fn from_ask(asks: &Cell<usize>, n: usize) -> impl Fn() -> bool + '_ {
    move || {
        asks.set(asks.get() + 1);
        asks.get() >= n
    }
}

/// A catalog whose base holds 16,384 documents, all but the first hundred
/// since deleted in its overlay: `next_doc` stays past them, and the dead
/// rows stay in the base. The default limits would checkpoint at 5% dead;
/// raised limits stand for a log written under larger host limits, or
/// for any dead fraction below the trigger at ten million documents.
fn mostly_deleted() -> (Tree, Engine) {
    let tree = Tree::with_files(4 * DocSet::CHECK_EVERY);
    let mut writer = ferret_catalog::WriterSession::open(&tree.0.join("index")).unwrap();
    writer.set_compaction_limits(ferret_catalog::CompactionLimits {
        log_bytes: u64::MAX,
        records: u64::MAX,
        dirty_percent: u32::MAX,
        dead_percent: u32::MAX,
    });
    let engine = Engine::from_writer(writer);
    let checkpoint = engine.pin().generation().checkpoint;
    tree.delete_all_but_the_first_hundred();
    tree.refresh(&engine);
    assert_eq!(
        engine.pin().generation().checkpoint,
        checkpoint,
        "compacted"
    );
    assert_eq!(engine.pin().catalog().docs().count(), 100);
    (tree, engine)
}

/// The live build asks after every CHECK_EVERY rows it scans, dead ones
/// included, not after every CHECK_EVERY live documents it finds: with a
/// hundred live documents among 16,384 rows, the second ask is inside the
/// scan of dead base rows, and the sixth inside the overlay's deletions.
#[test]
fn the_live_build_stops_among_deleted_rows() {
    let (_tree, engine) = mostly_deleted();
    let cold = engine.pin();
    let asks = Cell::new(0);
    assert!(cold.live_until(from_ask(&asks, 2)).is_none());
    assert_eq!(asks.get(), 2, "the live build did not stop at the flag");
    assert!(cold.derived.live.get().is_none());
    // Entry and four asks cover the 16,384 base rows; the sixth comes
    // inside the overlay's 16,284 deletions.
    asks.set(0);
    assert!(cold.live_until(from_ask(&asks, 6)).is_none());
    assert_eq!(asks.get(), 6, "the overlay scan did not stop at the flag");
    assert!(cold.derived.live.get().is_none());
    assert_eq!(cold.live().len(), 100);
}

/// DocNames' prefix pass covers every id below `next_doc`, live or not, so
/// it asks too: the flag set at its second ask stops it there.
#[test]
fn doc_names_stop_in_the_prefix_pass_over_a_sparse_bound() {
    let (_tree, engine) = mostly_deleted();
    let cold = engine.pin();
    assert!(cold.catalog().next_doc().0 as usize >= 4 * DocSet::CHECK_EVERY);
    // Entry, after loading, and each name pass's asks, then the prefix
    // pass's asks at ids 0 and CHECK_EVERY.
    let names = cold.catalog().name_reader().runs_from(NameId(0)).count();
    let prefix = 2 + names / DocSet::CHECK_EVERY + 2;
    let asks = Cell::new(0);
    assert!(
        cold.doc_names_until(from_ask(&asks, prefix))
            .unwrap()
            .is_none()
    );
    assert_eq!(asks.get(), prefix, "DocNames did not stop at the flag");
    assert!(cold.derived.docs.get().is_none());
}

/// The uncovered build asks after every CHECK_EVERY ids of bitmap it
/// copies, empty words included: with every document past the frontier
/// deleted, the second ask is inside that run of empty words.
#[test]
fn the_uncovered_build_stops_in_an_empty_bitmap() {
    let tree = Tree::with_files(100);
    let engine = tree.engine();
    engine.attach_content(&tree.0.join("index")).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    tree.add(100, 4 * DocSet::CHECK_EVERY);
    tree.refresh(&engine);
    tree.delete_all_but_the_first_hundred();
    tree.refresh(&engine);
    let cold = engine.pin();
    let view = cold.content().unwrap();
    assert!(view.uncovered_set(cold.live()).is_empty());
    assert!(cold.catalog().next_doc().0 as usize > 4 * DocSet::CHECK_EVERY);
    let asks = Cell::new(0);
    assert!(Pinned::new_until(Some(view), cold.live(), from_ask(&asks, 2)).is_none());
    assert_eq!(
        asks.get(),
        2,
        "the uncovered build did not stop at the flag"
    );
}
