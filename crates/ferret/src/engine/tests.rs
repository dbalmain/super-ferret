//! Cancellation of catalog-derived query state after real publications.
#![allow(clippy::unwrap_used)]

use std::cell::Cell;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::time::SystemTime;

use ferret_crawl::{IndexOptions, Refresh, RefreshReason, RefreshRequest, RefreshScope, index};

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
        for i in 0..files {
            let dir = base.join("root").join(format!("d{}", i / 100));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(format!("{i}.txt")), format!("alpha {i}")).unwrap();
        }
        index(
            &base.join("index"),
            &[base.join("root")],
            Refresh::All,
            &IndexOptions::default(),
        )
        .unwrap();
        Self(base)
    }
    fn engine(&self) -> Engine {
        Engine::from_writer(ferret_catalog::WriterSession::open(&self.0.join("index")).unwrap())
    }
    fn publish(&self, engine: &Engine) {
        fs::write(self.0.join("root/new.txt"), b"new publication").unwrap();
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
