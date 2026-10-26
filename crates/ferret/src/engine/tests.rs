//! Cancellation of catalog-derived query state after real publications.
#![allow(clippy::unwrap_used)]

use std::cell::Cell;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::time::SystemTime;

use ferret_crawl::{IndexOptions, Refresh, RefreshReason, RefreshRequest, RefreshScope, index};

use ferret_catalog::{NameId, Target};

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
        tree.reindex();
        tree
    }
    /// Indexes the root again from scratch: a new checkpoint holding
    /// whatever the test wrote since.
    fn reindex(&self) {
        index(
            &self.0.join("index"),
            &[self.0.join("root")],
            Refresh::All,
            &IndexOptions::default(),
        )
        .unwrap();
    }
    /// An engine over everything written so far, all of it in the
    /// checkpoint rather than an overlay, whose refreshes stay in that
    /// epoch: the default limits compact once 1% of a small tree changes.
    fn compacted_engine(&self) -> Engine {
        self.reindex();
        let mut writer = ferret_catalog::WriterSession::open(&self.0.join("index")).unwrap();
        writer.set_compaction_limits(ferret_catalog::CompactionLimits {
            dirty_percent: 100,
            dead_percent: 100,
            ..ferret_catalog::CompactionLimits::default()
        });
        let engine = Engine::from_writer(writer);
        engine.compact().unwrap();
        assert_eq!(engine.pin().catalog().changed_names().count(), 0);
        engine
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

/// A predecessor whose caches were never built does not trigger eager work
/// during publication, and cancellation still leaves the new view cold.
#[test]
fn cancelled_cold_query_after_publication_does_not_publish_derived_caches() {
    let tree = Tree::with_files(128);
    let engine = tree.engine();
    let query = Query::from_args([b"text:alpha".as_slice()], SystemTime::now()).unwrap();
    let old = engine.pin();
    assert!(old.derived.docs.get().is_none());
    assert!(old.derived.live.get().is_none());
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

/// A generation that adds, removes, renames and edits documents, and gives
/// one document a second name while taking one from another, carries the
/// previous view's built sets forward rather than leaving them for the next
/// query, and what it carries equals a fresh build. The removed document's
/// id is the discriminating case: a set that only gains survives it.
#[test]
fn adopted_derived_sets_match_a_fresh_build_after_document_and_link_changes() {
    let tree = Tree::with_files(300);
    let dir = tree.0.join("root/d0");
    fs::hard_link(dir.join("3.txt"), dir.join("3.alias")).unwrap();
    fs::write(dir.join("5.copy"), b"alpha 5").unwrap();
    let engine = tree.compacted_engine();
    let previous = engine.pin();
    warm(&previous);
    let removed = document_named(previous.catalog(), b"1.txt");
    let gained = document_named(previous.catalog(), b"2.txt");
    let lost = document_named(previous.catalog(), b"3.txt");
    let edited = document_named(previous.catalog(), b"4.txt");
    let shared = document_named(previous.catalog(), b"5.txt");
    assert_eq!(document_named(previous.catalog(), b"5.copy"), shared);
    assert_eq!(previous.doc_names().unwrap().names(lost.0).len(), 2);
    assert_eq!(previous.doc_names().unwrap().names(shared.0).len(), 2);

    fs::rename(dir.join("0.txt"), dir.join("renamed.txt")).unwrap();
    fs::remove_file(dir.join("1.txt")).unwrap();
    fs::hard_link(dir.join("2.txt"), dir.join("2.alias")).unwrap();
    fs::remove_file(dir.join("3.alias")).unwrap();
    fs::write(dir.join("4.txt"), b"alpha edited in place").unwrap();
    fs::write(dir.join("5.copy"), b"alpha no longer a copy").unwrap();
    fs::write(dir.join("added.txt"), b"alpha added").unwrap();
    tree.refresh(&engine);

    let adopted = engine.pin();
    assert_adopted_and_fresh(&adopted);
    assert!(
        !adopted.live().contains(removed.0),
        "removed DocId survived adoption"
    );
    assert!(!adopted.live().contains(edited.0));
    let names = adopted.doc_names().unwrap();
    assert_eq!(names.names(gained.0).len(), 2);
    assert_eq!(names.names(lost.0).len(), 1);
    assert_eq!(names.names(shared.0).len(), 1);
    for added in [&b"renamed.txt"[..], b"added.txt", b"4.txt", b"5.copy"] {
        let doc = document_named(adopted.catalog(), added);
        assert!(adopted.live().contains(doc.0));
        assert_eq!(names.names(doc.0).len(), 1);
    }

    // A second generation adopts from an adopted view.
    fs::remove_file(dir.join("2.alias")).unwrap();
    tree.refresh(&engine);
    let second = engine.pin();
    assert_adopted_and_fresh(&second);
    assert_eq!(second.doc_names().unwrap().names(gained.0).len(), 1);
}

/// An inode that had no document and gains one under a name the overlay
/// never rewrote: no previous row holds that name, so DocNames is left for
/// the first query to build, while the live set is still carried.
#[test]
fn a_kept_name_gaining_a_document_leaves_doc_names_to_the_full_build() {
    let tree = Tree::with_files(300);
    let file = tree.0.join("root/d0/binary.dat");
    fs::write(&file, b"alpha\0binary").unwrap();
    let engine = tree.compacted_engine();
    let previous = engine.pin();
    warm(&previous);
    assert!(
        previous
            .catalog()
            .doc(inode_named(previous.catalog(), b"binary.dat"))
            .is_none()
    );

    fs::write(&file, b"alpha now text").unwrap();
    tree.refresh(&engine);

    let next = engine.pin();
    let doc = next
        .catalog()
        .doc(inode_named(next.catalog(), b"binary.dat"))
        .unwrap();
    assert!(next.derived.live.get().is_some(), "live set not adopted");
    assert!(
        next.derived.docs.get().is_none(),
        "DocNames adopted without the kept name"
    );
    assert_eq!(next.live(), &live_documents(next.catalog()));
    assert!(next.live().contains(doc.0));
    assert_eq!(next.doc_names().unwrap().names(doc.0).len(), 1);
}

/// Distinct inodes with identical content share one document, so renaming
/// one of many copies touches a row as long as the catalog's copies. The
/// overlay holds one changed name; rebuilding that row would cost every
/// copy's name before publication, and the cumulative overlay would
/// re-seed it on every later refresh of the epoch. DocNames is left for
/// the first query to build instead, which builds it correctly; the live
/// set is still carried.
#[test]
fn renaming_one_of_many_identical_files_leaves_doc_names_to_the_full_build() {
    let tree = Tree::with_files(0);
    for i in 0..300 {
        let dir = tree.0.join("root").join(format!("d{}", i / 100));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{i}.txt")), b"alpha identical").unwrap();
    }
    let engine = tree.compacted_engine();
    let previous = engine.pin();
    warm(&previous);
    let shared = document_named(previous.catalog(), b"0.txt");
    assert_eq!(previous.doc_names().unwrap().names(shared.0).len(), 300);

    fs::rename(
        tree.0.join("root/d0/0.txt"),
        tree.0.join("root/d2/moved.txt"),
    )
    .unwrap();
    tree.refresh(&engine);

    let next = engine.pin();
    assert_eq!(
        previous.generation().checkpoint,
        next.generation().checkpoint
    );
    assert!(next.derived.live.get().is_some(), "live set not adopted");
    assert!(
        next.derived.docs.get().is_none(),
        "adoption expanded the shared 300-name row"
    );
    assert_eq!(next.live(), &live_documents(next.catalog()));
    let fresh = DocNames::new(next.catalog()).unwrap();
    let built = next.doc_names().unwrap();
    for doc in 0..next.catalog().next_doc().0 {
        assert_eq!(built.names(doc), fresh.names(doc), "document {doc}");
    }
    assert_eq!(built.names(shared.0).len(), 300);
}

/// A catalog publication that pinned the old content view before a content
/// publication landed keeps the newer content view rather than restoring
/// the one it pinned. This drives the two publishers concurrently, as the
/// daemon does when merging beside a refresh. `select` is held on the retired
/// list after its pin, the window in which the content view changes.
#[test]
fn a_catalog_publication_keeps_a_content_view_published_after_its_pin() {
    let tree = Tree::with_files(4);
    let engine = tree.engine();
    engine.attach_content(&tree.0.join("index")).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    let pinned = engine.pin();
    let newer = Arc::new(
        ferret_index::View::open_with_threads(
            &tree.0.join("index").join(CONTENT_DIR),
            &catalog_view(pinned.catalog(), pinned.live()),
            1,
        )
        .unwrap()
        .unwrap(),
    );
    assert!(!Arc::ptr_eq(pinned.content.as_ref().unwrap(), &newer));
    // The next catalog view, committed but not yet published.
    fs::write(tree.0.join("root/new.txt"), b"new publication").unwrap();
    let view = ferret_crawl::refresh(
        engine.writer.lock().unwrap().as_mut().unwrap(),
        RefreshRequest {
            expected_generation: pinned.generation(),
            scopes: vec![RefreshScope::Root(tree.0.join("root"))],
            rename_hints: Vec::new(),
            reason: RefreshReason::Burst,
        },
        &IndexOptions::default(),
    )
    .unwrap()
    .view;
    let pins = || Arc::strong_count(&engine.current.read().unwrap().names);
    let before = pins();

    let retired = engine.retired.lock().unwrap();
    std::thread::scope(|scope| {
        let select = scope.spawn(|| engine.select(view));
        while pins() == before {
            std::thread::yield_now();
        }
        engine.select_content(newer.clone());
        drop(retired);
        select.join().unwrap();
    });

    let current = engine.pin();
    assert_ne!(current.generation(), pinned.generation());
    assert!(
        Arc::ptr_eq(current.content.as_ref().unwrap(), &newer),
        "select restored the content view it pinned"
    );
}

/// A compaction starts a new checkpoint epoch, whose ids the previous sets
/// do not describe: the new view is cold until a query builds it.
#[test]
fn a_new_checkpoint_epoch_leaves_derived_sets_unbuilt() {
    let tree = Tree::with_files(4);
    let engine = tree.engine();
    tree.publish(&engine);
    let previous = engine.pin();
    warm(&previous);

    engine.compact().unwrap();

    let next = engine.pin();
    assert_ne!(
        previous.generation().checkpoint,
        next.generation().checkpoint
    );
    assert!(next.derived.live.get().is_none());
    assert!(next.derived.docs.get().is_none());
    assert_eq!(next.live(), &live_documents(next.catalog()));
}

/// Builds both derived sets through the query path, as the first content
/// query on a view does.
fn warm(session: &QuerySession) {
    let query = Query::from_args([b"text:alpha".as_slice()], SystemTime::now()).unwrap();
    session
        .search_content(&query, None, None, |_| ControlFlow::Continue(()))
        .unwrap();
    assert!(session.derived.live.get().is_some());
    assert!(session.derived.docs.get().is_some());
}

/// Both sets were carried by `select`, before any query, and equal what a
/// full build of the view makes.
fn assert_adopted_and_fresh(session: &QuerySession) {
    assert!(session.derived.live.get().is_some(), "live set not adopted");
    assert!(session.derived.docs.get().is_some(), "DocNames not adopted");
    assert_eq!(session.live(), &live_documents(session.catalog()));
    let fresh = DocNames::new(session.catalog()).unwrap();
    let adopted = session.doc_names().unwrap();
    for doc in 0..session.catalog().next_doc().0 {
        assert_eq!(adopted.names(doc), fresh.names(doc), "document {doc}");
    }
}

fn inode_named(catalog: &Catalog, name: &[u8]) -> ferret_catalog::InoId {
    catalog
        .name_reader()
        .runs_from(NameId(0))
        .find_map(|(id, edge)| match edge.target() {
            Target::Inode(inode) if edge.bytes == name && catalog.is_live_name(id) => Some(inode),
            _ => None,
        })
        .expect("live name exists")
}

fn document_named(catalog: &Catalog, name: &[u8]) -> ferret_catalog::DocId {
    catalog
        .doc(inode_named(catalog, name))
        .expect("name has a document")
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
/// rows stay in the base. The default limits would checkpoint this 99% dead
/// log at their 5% dead trigger, so they are raised: the fixture stands for
/// a large catalog below that trigger, where 5% of ten million documents is
/// half a million dead rows still in the base.
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

/// The cases that separate D77 C from its neighbours: a stored count caps
/// at the CPUs available, is not itself capped at 16, and gives way to the
/// environment; without one, the old `min(16, available)`.
#[test]
fn search_threads_takes_the_stored_count_within_the_cpus_available() {
    for (env, stored, available, threads) in [
        (None, Some(16), 32, 16),
        (None, Some(16), 4, 4),
        (None, Some(24), 32, 24),
        (None, None, 32, 16),
        (None, None, 8, 8),
        (Some(3), Some(16), 32, 3),
        (Some(48), Some(16), 32, 48),
    ] {
        assert_eq!(
            search_threads(env, stored, available),
            threads,
            "env {env:?}, stored {stored:?}, available {available}"
        );
    }
}

#[test]
fn only_a_positive_number_in_the_environment_overrides() {
    assert_eq!(env_threads(Some("6")), Some(6));
    for value in [None, Some(""), Some("0"), Some("-2"), Some("lots")] {
        assert_eq!(env_threads(value), None, "{value:?}");
    }
}

/// What a search in this process resolves `stored` to, the environment and
/// CPUs being whatever they are here.
fn expected_threads(stored: Option<usize>) -> usize {
    let env = env_threads(std::env::var("FERRET_SEARCH_THREADS").ok().as_deref());
    let available = std::thread::available_parallelism().map_or(1, |n| n.get());
    search_threads(env, stored, available)
}

/// An engine reads its index's settings for the thread count, whether it
/// was opened for queries or from a writer.
#[test]
fn an_engine_searches_on_the_stored_thread_count() {
    let tree = Tree::with_files(4);
    let index = tree.0.join("index");
    fs::write(
        index.join(crate::settings::FILE),
        "version = 1\nsearch_threads = 1\n",
    )
    .unwrap();
    assert_eq!(
        tree.engine().pin().search_threads(),
        expected_threads(Some(1))
    );
    let reader = Engine::open(&index).unwrap().unwrap();
    assert_eq!(reader.pin().search_threads(), expected_threads(Some(1)));
}

/// A settings file that does not parse is treated as absent: the search
/// runs, on the default count.
#[test]
fn a_corrupt_settings_file_falls_back_without_failing_the_search() {
    let tree = Tree::with_files(4);
    let index = tree.0.join("index");
    fs::write(index.join(crate::settings::FILE), "search_threads = lots\n").unwrap();
    let engine = Engine::open(&index).unwrap().unwrap();
    engine.open_content(&index).unwrap();
    let pin = engine.pin();
    let query = Query::from_args([b"text:alpha".as_slice()], SystemTime::now()).unwrap();
    let mut rows = 0;
    pin.search_content(&query, None, None, |_| {
        rows += 1;
        ControlFlow::Continue(())
    })
    .unwrap();
    assert_eq!(rows, 4);
    assert_eq!(pin.search_threads(), expected_threads(None));
}

/// Merge, follow and reader opens may reach publication out of order once
/// merges stop taking the catalog writer mutex. Genuine manifest successors
/// must survive delayed publication of an older sequence.
#[test]
fn an_older_content_publication_cannot_replace_a_newer_sequence() {
    let tree = Tree::with_files(4);
    let engine = tree.engine();
    engine.attach_content(&tree.0.join("index")).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    let older = engine.pin().content.unwrap();
    let saved = tree.0.join("older/index");
    fs::create_dir_all(&saved).unwrap();
    for entry in fs::read_dir(tree.0.join("index/index")).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            fs::copy(entry.path(), saved.join(entry.file_name())).unwrap();
        }
    }
    tree.publish(&engine);
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    let newer = engine.pin().content.unwrap();
    assert!(newer.manifest().sequence > older.manifest().sequence);

    engine.select_content(older);
    assert!(Arc::ptr_eq(&engine.pin().content.unwrap(), &newer));
    engine.open_content(&tree.0.join("older")).unwrap();
    assert!(Arc::ptr_eq(&engine.pin().content.unwrap(), &newer));
    // An absent reader open used to assign None directly, discarding even
    // a newer view already published for this catalog generation.
    engine.open_content(&tree.0.join("absent")).unwrap();
    assert!(Arc::ptr_eq(&engine.pin().content.unwrap(), &newer));
}

/// The merge formerly owned Engine.writer for its entire streaming pass.
/// Hold it in the real pacer, then require a refresh, a skipped follow and
/// direct verification to finish before releasing it.
#[test]
fn refresh_and_direct_verification_proceed_beside_a_held_merge() {
    let tree = Tree::with_files(100);
    let engine = tree.engine();
    engine.attach_content(&tree.0.join("index")).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    for i in 0..40 {
        fs::remove_file(tree.0.join(format!("root/d0/{i}.txt"))).unwrap();
    }
    tree.refresh(&engine);
    fs::write(tree.0.join("root/edited.txt"), b"uncoveredneedle").unwrap();
    let (reached, held) = std::sync::mpsc::sync_channel(1);
    let (release, resume) = std::sync::mpsc::sync_channel(1);
    let (refreshed, finished) = std::sync::mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        let merge = scope.spawn(|| {
            let resume = Mutex::new(Some(resume));
            let pace = |_| {
                if let Some(resume) = resume.lock().unwrap().take() {
                    reached.send(()).unwrap();
                    resume
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                }
                Ok(())
            };
            engine.merge_content(&Budget {
                pace: &pace,
                ..Budget::unbounded()
            })
        });
        held.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let refresh = scope.spawn(|| {
            tree.refresh(&engine);
            refreshed.send(()).unwrap();
        });
        let completed = finished.recv_timeout(std::time::Duration::from_secs(1));
        let verified = completed.is_ok().then(|| {
            assert!(
                engine
                    .try_follow_content(&Budget::unbounded(), None)
                    .unwrap()
                    .is_none()
            );
            let query =
                Query::from_args([b"text:uncoveredneedle".as_slice()], SystemTime::now()).unwrap();
            engine
                .pin()
                .search_content(&query, None, None, |_| ControlFlow::Continue(()))
                .unwrap()
                .rows
        });
        release.send(()).unwrap();
        assert!(merge.join().unwrap().unwrap().is_some());
        refresh.join().unwrap();
        assert!(
            completed.is_ok(),
            "refresh waited for the held content merge"
        );
        assert_eq!(verified, Some(1));
    });
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    let pin = engine.pin();
    assert!(
        Pinned::new(pin.content(), pin.live())
            .uncovered()
            .is_empty()
    );
}

/// Admission pins can lag a later follow before the merge worker takes the
/// content mutex. Reconciling against that old frontier would discard the
/// newer durable index, even if publication's sequence check hid the damage.
#[test]
fn an_overtaken_merge_admission_cannot_reconcile_newer_content_backwards() {
    let tree = Tree::with_files(4);
    let engine = tree.engine();
    engine.attach_content(&tree.0.join("index")).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    let admitted = engine.pin();
    tree.publish(&engine);
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    let newer = engine.pin().content.unwrap();
    assert!(newer.manifest().high_water > admitted.catalog.next_doc().0);
    assert!(
        engine
            .merge_content_at(&admitted, &Budget::unbounded())
            .unwrap()
            .is_none()
    );
    let content = engine.content.lock().unwrap();
    assert!(Arc::ptr_eq(&content.as_ref().unwrap().view(), &newer));
}

/// Removing a directory the checkpoint holds, children and a nested
/// directory with it, leaves it dead in the overlay with its own name
/// deleted. Adopting that view, after the refresh and again on a cold open,
/// must climb from it by its checkpoint edge rather than ask the view for
/// its deleted name.
#[test]
fn a_removed_checkpoint_directory_publishes_and_reopens() {
    let tree = Tree::with_files(300);
    fs::create_dir(tree.0.join("root/d2/nested")).unwrap();
    fs::write(tree.0.join("root/d2/nested/inner.txt"), b"alpha inner").unwrap();
    let engine = tree.compacted_engine();
    let checkpoint = engine.pin().generation().checkpoint;
    let removed = inode_named(engine.pin().catalog(), b"d2");
    assert_eq!(text_files(&engine.pin()), 301);

    fs::remove_dir_all(tree.0.join("root/d2")).unwrap();
    tree.refresh(&engine);
    let after = engine.pin();
    assert_eq!(after.generation().checkpoint, checkpoint, "compacted");
    assert!(!after.catalog().is_live_inode(removed));
    assert_eq!(text_files(&after), 200);
    drop((after, engine));

    let reopened = Engine::open(&tree.0.join("index")).unwrap().unwrap();
    assert_eq!(reopened.pin().generation().checkpoint, checkpoint);
    assert_eq!(text_files(&reopened.pin()), 200);
}

/// Rows a name search for `.txt` finds through the session's name index.
fn text_files(session: &QuerySession) -> usize {
    let query = Query::from_args([b".txt".as_slice()], SystemTime::now()).unwrap();
    let mut rows = 0;
    session
        .search(&query, |_| {
            rows += 1;
            ControlFlow::Continue(())
        })
        .unwrap();
    rows
}
