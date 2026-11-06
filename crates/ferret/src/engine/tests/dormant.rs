//! D60 interleavings drive the host gates, actual crawler and index writer.
use super::*;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

fn covered(files: usize) -> (Tree, Engine) {
    let tree = Tree::with_files(files);
    let engine = tree.engine();
    engine.attach_content(&tree.0.join("index")).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    (tree, engine)
}
fn remove(tree: &Tree, engine: &Engine, docs: std::ops::Range<usize>) {
    for i in docs {
        fs::remove_file(tree.0.join(format!("root/d0/{i}.txt"))).unwrap();
    }
    tree.refresh(engine);
}
fn restore(tree: &Tree, engine: &Engine, doc: usize) {
    tree.add(doc, 1);
    tree.refresh(engine);
}
fn id(engine: &Engine, doc: usize) -> u32 {
    document_named(engine.pin().catalog(), format!("{doc}.txt").as_bytes()).0
}
fn barrier() -> (SyncSender<()>, Receiver<()>) {
    sync_channel(1)
}
fn wait(receiver: &Receiver<()>) {
    receiver.recv_timeout(Duration::from_secs(30)).unwrap();
}

/// The production pacer runs after durable revocation and writer release.
/// The body must finish while streaming remains parked in that pacer.
fn while_merging(
    engine: &Engine,
    all: bool,
    cancelled: &AtomicBool,
    body: impl FnOnce(),
) -> Result<Option<Merged>, Error> {
    let (reached, held) = barrier();
    let (release, resume) = barrier();
    std::thread::scope(|scope| {
        let merge = scope.spawn(|| {
            let resume = Mutex::new(Some(resume));
            let pace = |_| {
                if let Some(resume) = resume.lock().unwrap().take() {
                    reached.send(()).unwrap();
                    wait(&resume);
                }
                Ok(())
            };
            let stopped = || cancelled.load(Ordering::Acquire);
            let budget = Budget {
                pace: &pace,
                cancelled: &stopped,
                ..Budget::unbounded()
            };
            if all {
                engine.merge_all_content(&budget)
            } else {
                engine.merge_content(&budget)
            }
        });
        wait(&held);
        body();
        release.send(()).unwrap();
        merge.join().unwrap()
    })
}
fn indexed(engine: &Engine, doc: u32) {
    use ferret_index::{Atom, Source};
    let pin = engine.pin();
    let view = pin.content().unwrap();
    assert!(view.lookup(b"alpha").unwrap().contains(&doc));
    let term = view.lookup(b"0").unwrap();
    assert!(
        term.contains(&doc),
        "rare term/fingerprint lost returning doc"
    );
    let tri = Source::Trigrams(view)
        .read(&Atom::Trigram(*b"alp"))
        .unwrap()
        .unwrap();
    assert!(tri.probe().test(doc).is_some());
}

/// The catalog still sees a live id after a racing death and merge purge.
/// Restore the same inode without publishing it, so actual verification can
/// distinguish a lost candidate from a vanished file.
#[test]
fn query_only_open_cannot_lose_a_document_purged_after_its_catalog_read() {
    for (one_shot, reopen) in [(false, false), (true, false), (false, true), (true, true)] {
        let (tree, writer) = covered(10);
        let tree = Arc::new(tree);
        let index = tree.0.join("index");
        let reader = if one_shot {
            Engine::open_one_shot(&index)
        } else {
            Engine::open(&index)
        }
        .unwrap()
        .unwrap();
        let previous = if reopen {
            reader.open_content(&index).unwrap();
            Some(reader.pin().content.unwrap())
        } else {
            None
        };
        let generation = reader.generation();
        let moved = tree.clone();
        CONTENT_OPEN_HOOK.set(Some(Box::new(move || {
            let path = moved.0.join("root/d0/0.txt");
            let hidden = moved.0.join("hidden");
            fs::rename(&path, &hidden).unwrap();
            moved.refresh(&writer);
            let merged = writer
                .merge_all_content(&Budget::unbounded())
                .unwrap()
                .unwrap();
            assert_eq!(merged.purged, 1);
            fs::rename(hidden, path).unwrap();
        })));
        reader.open_content(&index).unwrap();
        let pin = reader.pin();
        assert_eq!(pin.generation(), generation);
        match previous.as_ref() {
            Some(previous) => assert!(Arc::ptr_eq(pin.content.as_ref().unwrap(), previous)),
            None => assert!(pin.content().is_none()),
        }
        let query = Query::from_args([b"text:alpha".as_slice()], SystemTime::now()).unwrap();
        if previous.is_none() {
            assert!(matches!(
                pin.search_content(&query, Some(0), None, |_| ControlFlow::Continue(())),
                Err(ferret_query::RunError::IndexIncomplete {
                    uncovered: 10,
                    live: 10
                })
            ));
        }
        assert_eq!(
            pin.search_content(&query, None, None, |_| ControlFlow::Continue(()))
                .unwrap()
                .rows,
            10,
            "the old catalog's live document must remain a candidate"
        );
        let grep = ferret_verify::GrepFlags::default()
            .compile("alpha")
            .unwrap();
        let candidates = pin.grep_candidates(&grep, &|| false).unwrap();
        assert!(candidates.contains(document_named(pin.catalog(), b"0.txt").0, &|| false));
    }
}

#[test]
fn returning_content_and_empty_success_reuse_ids_without_follow_reads() {
    let tree = Tree::with_files(10);
    let engine = tree.compacted_engine();
    engine.attach_content(&tree.0.join("index")).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    warm(&engine.pin());
    let old = id(&engine, 0);
    fs::write(tree.0.join("root/empty"), []).unwrap();
    tree.refresh(&engine);
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    let empty = document_named(engine.pin().catalog(), b"empty");
    let next = engine.pin().catalog().next_doc();
    remove(&tree, &engine, 0..1);
    fs::remove_file(tree.0.join("root/empty")).unwrap();
    tree.refresh(&engine);
    fs::write(tree.0.join("root/empty"), []).unwrap();
    restore(&tree, &engine, 0);
    assert_eq!(id(&engine, 0), old);
    assert_eq!(document_named(engine.pin().catalog(), b"empty"), empty);
    assert_eq!(engine.pin().catalog().next_doc(), next);
    assert_adopted_and_fresh(&engine.pin());
    assert_eq!(
        engine
            .follow_content(&Budget::unbounded(), None)
            .unwrap()
            .docs,
        0
    );
    indexed(&engine, old);
}

#[test]
fn revival_between_daemon_admission_and_setup_uses_the_fresh_filter_pin() {
    let (tree, engine) = covered(10);
    let old = id(&engine, 0);
    remove(&tree, &engine, 0..4);
    let admitted = engine.pin();
    assert!(
        admitted
            .content()
            .unwrap()
            .merge_resources(admitted.live())
            .is_some()
    );
    restore(&tree, &engine, 0);
    assert_eq!(id(&engine, 0), old);
    let merged = engine
        .merge_content_at(&admitted, &Budget::unbounded())
        .unwrap()
        .unwrap();
    assert_eq!(merged.purged, 3);
    assert_eq!(merged.segment.unwrap().docs, 7);
    indexed(&engine, old);
}

#[test]
fn revocation_wins_and_a_return_during_streaming_allocates_above_the_frontier() {
    for all in [false, true] {
        let (tree, engine) = covered(10);
        let old = id(&engine, 0);
        let next = engine.pin().catalog().next_doc().0;
        remove(&tree, &engine, 0..4);
        let merged = while_merging(&engine, all, &AtomicBool::new(false), || {
            assert!(
                engine
                    .pin()
                    .catalog()
                    .dormant_hash(ferret_catalog::DocId(old))
                    .is_none()
            );
            restore(&tree, &engine, 0);
            assert_eq!(id(&engine, 0), next);
            let pin = engine.pin();
            assert!(
                Pinned::new(pin.content(), pin.live())
                    .uncovered()
                    .contains(next)
            );
            let query = Query::from_args([b"text:alpha".as_slice()], SystemTime::now()).unwrap();
            assert_eq!(
                pin.search_content(&query, None, None, |_| ControlFlow::Continue(()))
                    .unwrap()
                    .rows,
                7
            );
        })
        .unwrap()
        .unwrap();
        assert_eq!(merged.purged, 4);
        assert!(
            !engine
                .pin()
                .content()
                .unwrap()
                .lookup(b"alpha")
                .unwrap()
                .contains(&old)
        );
        assert_eq!(
            engine
                .follow_content(&Budget::unbounded(), None)
                .unwrap()
                .docs,
            1
        );
        indexed(&engine, next);
    }
}

#[test]
fn live_at_pin_death_revival_death_can_refresh_during_streaming() {
    let (tree, engine) = covered(10);
    let old = id(&engine, 0);
    remove(&tree, &engine, 1..5);
    let merged = while_merging(&engine, false, &AtomicBool::new(false), || {
        remove(&tree, &engine, 0..1);
        restore(&tree, &engine, 0);
        assert_eq!(id(&engine, 0), old);
        remove(&tree, &engine, 0..1);
    })
    .unwrap()
    .unwrap();
    assert_eq!(merged.segment.unwrap().docs, 6);
    assert_eq!(merged.purged, 4);
    restore(&tree, &engine, 0);
    assert_eq!(id(&engine, 0), old);
    assert_eq!(
        engine
            .follow_content(&Budget::unbounded(), None)
            .unwrap()
            .docs,
        0
    );
    indexed(&engine, old);
}

#[test]
fn dead_fraction_retry_is_rechecked_after_eight_seven_eight_live_churn() {
    let (tree, engine) = covered(10);
    remove(&tree, &engine, 0..2);
    assert!(
        engine
            .merge_content(&Budget::unbounded())
            .unwrap()
            .is_none()
    );
    remove(&tree, &engine, 2..3);
    let admitted = engine.pin();
    let cancelled = AtomicBool::new(false);
    // Park a merge while it pinned 2 live, then kill 2. Its death permission
    // survives cancellation, since it was live in that merge's filter pin.
    restore(&tree, &engine, 2);
    let result = while_merging(&engine, true, &cancelled, || {
        remove(&tree, &engine, 2..3);
        cancelled.store(true, Ordering::Release);
    });
    assert!(matches!(
        result,
        Err(Error::Content(ferret_index::Error::Cancelled))
    ));
    restore(&tree, &engine, 2);
    assert!(
        engine
            .merge_content_at(&admitted, &Budget::unbounded())
            .unwrap()
            .is_none()
    );
    assert!(
        engine
            .merge_content(&Budget::unbounded())
            .unwrap()
            .is_none()
    );
}

#[test]
fn all_dead_merge_revokes_before_removing_its_inputs() {
    let (tree, engine) = covered(10);
    remove(&tree, &engine, 0..10);
    assert_eq!(engine.pin().catalog().dormant_docs().count(), 10);
    let merged = engine.merge_content(&Budget::unbounded()).unwrap().unwrap();
    assert!(merged.segment.is_none());
    assert_eq!(merged.purged, 10);
    assert_eq!(engine.pin().catalog().dormant_docs().count(), 0);
    restore(&tree, &engine, 0);
    assert_eq!(id(&engine, 0), 10);
}

#[test]
fn unreadable_deaths_never_revive_before_or_after_global_marker_pruning() {
    for death_before_pin in [false, true] {
        let tree = Tree::with_files(10);
        let engine = tree.engine();
        engine.attach_content(&tree.0.join("index")).unwrap();
        let old = id(&engine, 0);
        fs::remove_file(tree.0.join("root/d0/0.txt")).unwrap();
        let followed = engine
            .follow_content(
                &Budget {
                    bytes: 35,
                    ..Budget::unbounded()
                },
                None,
            )
            .unwrap();
        assert_eq!(followed.unreadable, 1);
        engine.follow_content(&Budget::unbounded(), None).unwrap();
        let content = engine.pin().content.unwrap();
        assert_eq!(content.manifest().segments.len(), 2);
        assert!(content.manifest().unreadable.contains(&old));
        // The merge targets the second segment. Pruning of unreadable ids is
        // global, so this also covers pruning outside the selected inputs.
        tree.add(0, 1);
        remove(&tree, &engine, 6..9);
        if death_before_pin {
            remove(&tree, &engine, 0..1);
            engine.merge_content(&Budget::unbounded()).unwrap().unwrap();
            assert!(
                !engine
                    .pin()
                    .content()
                    .unwrap()
                    .manifest()
                    .unreadable
                    .contains(&old)
            );
        } else {
            while_merging(&engine, false, &AtomicBool::new(false), || {
                remove(&tree, &engine, 0..1);
                assert!(
                    engine
                        .pin()
                        .catalog()
                        .dormant_hash(ferret_catalog::DocId(old))
                        .is_none()
                );
            })
            .unwrap()
            .unwrap();
            assert!(
                engine
                    .pin()
                    .content()
                    .unwrap()
                    .manifest()
                    .unreadable
                    .contains(&old)
            );
        }
        restore(&tree, &engine, 0);
        assert_ne!(id(&engine, 0), old);
    }
}

#[test]
fn death_before_follow_is_not_retroactively_retained_when_the_frontier_advances() {
    let tree = Tree::with_files(10);
    let engine = tree.engine();
    engine.attach_content(&tree.0.join("index")).unwrap();
    let old = id(&engine, 0);
    remove(&tree, &engine, 0..1);
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    assert!(old < engine.pin().content().unwrap().manifest().frontier);
    assert!(
        engine
            .pin()
            .catalog()
            .dormant_hash(ferret_catalog::DocId(old))
            .is_none()
    );
    restore(&tree, &engine, 0);
    assert_ne!(id(&engine, 0), old);
}

#[test]
fn follow_holds_writer_until_its_content_publication_precedes_death() {
    let tree = Tree::with_files(10);
    let engine = tree.engine();
    engine.attach_content(&tree.0.join("index")).unwrap();
    let old = id(&engine, 0);
    let (reached, held) = barrier();
    let (release, resume) = barrier();
    std::thread::scope(|scope| {
        let follow = scope.spawn(|| {
            let resume = Mutex::new(Some(resume));
            let pace = |_| {
                if let Some(resume) = resume.lock().unwrap().take() {
                    reached.send(()).unwrap();
                    wait(&resume);
                }
                Ok(())
            };
            engine.follow_content(
                &Budget {
                    pace: &pace,
                    ..Budget::unbounded()
                },
                None,
            )
        });
        wait(&held);
        assert!(matches!(
            engine.writer.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
        fs::remove_file(tree.0.join("root/d0/0.txt")).unwrap();
        let (started, attempted) = barrier();
        let tree = &tree;
        let engine = &engine;
        let death = scope.spawn(move || {
            started.send(()).unwrap();
            tree.refresh(engine);
        });
        wait(&attempted);
        release.send(()).unwrap();
        follow.join().unwrap().unwrap();
        death.join().unwrap();
    });
    // All reads completed before the pacer reached segment output. Thus the
    // serialized death sees successful coverage, even though the path vanished.
    assert!(
        engine
            .pin()
            .catalog()
            .dormant_hash(ferret_catalog::DocId(old))
            .is_some()
    );
    restore(&tree, &engine, 0);
    assert_eq!(id(&engine, 0), old);
}

#[test]
fn size_tier_merge_uses_current_liveness_and_revokes_its_exact_ranges() {
    let tree = Tree::with_files(10);
    let engine = tree.engine();
    engine.attach_content(&tree.0.join("index")).unwrap();
    for _ in 0..10 {
        engine
            .follow_content(
                &Budget {
                    bytes: 1,
                    ..Budget::unbounded()
                },
                None,
            )
            .unwrap();
    }
    assert_eq!(
        engine.pin().content().unwrap().manifest().segments.len(),
        10
    );
    let old = id(&engine, 0);
    remove(&tree, &engine, 0..1);
    let merged = while_merging(&engine, false, &AtomicBool::new(false), || {
        restore(&tree, &engine, 0);
        assert_ne!(id(&engine, 0), old);
    })
    .unwrap()
    .unwrap();
    assert_eq!(merged.inputs, 10);
    assert_eq!(merged.purged, 1);
}

#[test]
fn cancellation_after_revocation_loses_only_cache_opportunities() {
    let (tree, engine) = covered(10);
    let old = id(&engine, 0);
    remove(&tree, &engine, 0..4);
    let cancelled = AtomicBool::new(false);
    let result = while_merging(&engine, false, &cancelled, || {
        cancelled.store(true, Ordering::Release);
    });
    assert!(matches!(
        result,
        Err(Error::Content(ferret_index::Error::Cancelled))
    ));
    restore(&tree, &engine, 0);
    assert_ne!(id(&engine, 0), old);
    engine.merge_content(&Budget::unbounded()).unwrap().unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    indexed(&engine, id(&engine, 0));
}

#[test]
fn reset_waits_for_streaming_then_durably_clears_new_permissions() {
    let (tree, engine) = covered(10);
    let old = id(&engine, 0);
    remove(&tree, &engine, 1..5);
    let (reset_started, attempted) = barrier();
    let (reset_done, completed) = barrier();
    std::thread::scope(|scope| {
        let reset = Mutex::new(None);
        while_merging(&engine, false, &AtomicBool::new(false), || {
            remove(&tree, &engine, 0..1);
            assert!(
                engine
                    .pin()
                    .catalog()
                    .dormant_hash(ferret_catalog::DocId(old))
                    .is_some()
            );
            *reset.lock().unwrap() = Some(scope.spawn(|| {
                reset_started.send(()).unwrap();
                engine.reset_content().unwrap();
                reset_done.send(()).unwrap();
            }));
            wait(&attempted);
            assert!(completed.try_recv().is_err());
        })
        .unwrap()
        .unwrap();
        reset.into_inner().unwrap().unwrap().join().unwrap();
        wait(&completed);
    });
    assert_eq!(engine.pin().content().unwrap().manifest().frontier, 0);
    assert_eq!(engine.pin().catalog().dormant_docs().count(), 0);
    let disk = Catalog::open(&tree.0.join("index")).unwrap().unwrap();
    disk.load_all().unwrap();
    assert_eq!(disk.dormant_docs().count(), 0);
    restore(&tree, &engine, 0);
    assert_ne!(id(&engine, 0), old);
}

#[test]
fn unusable_index_open_and_empty_index_restart_clear_permissions_before_revival() {
    for crash_gap in [false, true] {
        let (tree, engine) = covered(10);
        let old = id(&engine, 0);
        remove(&tree, &engine, 0..1);
        assert_eq!(engine.pin().catalog().dormant_docs().count(), 1);
        drop(engine);
        // Corrupt a referenced file so open, rather than just fits(), must
        // replace the index. This is the previously missing startup outcome.
        let dir = tree.0.join("index/index");
        let segment = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "seg"))
            .unwrap();
        fs::write(segment, b"broken").unwrap();
        if crash_gap {
            // Drive the real open that resets content, then abandon it before
            // the host clears catalog permissions: the startup crash gap.
            let catalog = Catalog::open(&tree.0.join("index")).unwrap().unwrap();
            catalog.load_all().unwrap();
            let live = live_documents(&catalog);
            let opened = IndexWriter::open(&dir, &catalog_view(&catalog, &live)).unwrap();
            assert!(opened.reset_at_open());
            drop(opened);
        }
        let engine = tree.engine();
        engine.attach_content(&tree.0.join("index")).unwrap();
        assert_eq!(engine.pin().catalog().dormant_docs().count(), 0);
        // Follow must not advance far enough to disguise the empty startup.
        restore(&tree, &engine, 0);
        assert_ne!(id(&engine, 0), old);
        let disk = Catalog::open(&tree.0.join("index")).unwrap().unwrap();
        disk.load_all().unwrap();
        assert_eq!(disk.dormant_docs().count(), 0);
    }
}

#[test]
fn merge_crash_child() {
    let Some(base) = std::env::var_os("FERRET_D60_MERGE_CHILD").map(PathBuf::from) else {
        return;
    };
    let engine = Engine::from_writer(WriterSession::open(&base.join("index")).unwrap());
    engine.attach_content(&base.join("index")).unwrap();
    let kill = AtomicBool::new(std::env::var_os("FERRET_D60_LIVE_AT_PIN").is_some());
    let pace = |_| {
        if kill.swap(false, Ordering::AcqRel) {
            fs::remove_file(base.join("root/d0/0.txt")).unwrap();
            engine
                .refresh(
                    RefreshRequest {
                        expected_generation: engine.generation(),
                        scopes: vec![RefreshScope::Root(base.join("root"))],
                        rename_hints: vec![],
                        reason: RefreshReason::Burst,
                    },
                    &IndexOptions::default(),
                )
                .unwrap();
        }
        Ok(())
    };
    let result = engine.merge_content(&Budget {
        pace: &pace,
        ..Budget::unbounded()
    });
    assert!(result.is_err(), "crash injection was not reached");
}

#[test]
fn merge_publication_crash_cuts_preserve_exactly_the_valid_revival_permissions() {
    for point in [
        "MergeMidway",
        "TermSynced",
        "TermRenamed",
        "PairRenamed",
        "SegmentWritten",
        "ManifestSynced",
        "ManifestRenamed",
        "ManifestDirectorySynced",
        "Retired",
    ] {
        for live_at_pin in [false, true] {
            let (tree, engine) = covered(10);
            let old = id(&engine, 0);
            remove(&tree, &engine, 1..5);
            if !live_at_pin {
                remove(&tree, &engine, 0..1);
            }
            drop(engine);
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "engine::tests::dormant::merge_crash_child",
                    "--nocapture",
                ])
                .env("FERRET_D60_MERGE_CHILD", &tree.0)
                .env("FERRET_INDEX_TEST_FAIL", point);
            if live_at_pin {
                child.env("FERRET_D60_LIVE_AT_PIN", "1");
            }
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "{point:?}, live={live_at_pin}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let engine = tree.engine();
            engine.attach_content(&tree.0.join("index")).unwrap();
            restore(&tree, &engine, 0);
            assert_eq!(
                id(&engine, 0) == old,
                live_at_pin,
                "{point}, live={live_at_pin}"
            );
            engine.follow_content(&Budget::unbounded(), None).unwrap();
            indexed(&engine, id(&engine, 0));
        }
    }
}

#[test]
fn failed_reattach_or_reset_disables_the_old_writer_before_any_revival() {
    for reset in [false, true] {
        let (tree, engine) = covered(10);
        let old = id(&engine, 0);
        remove(&tree, &engine, 0..1);
        let manifest = tree.0.join("index/index/manifest");
        fs::remove_file(&manifest).unwrap();
        fs::create_dir(&manifest).unwrap();
        let result = if reset {
            engine.reset_content()
        } else {
            engine.attach_content(&tree.0.join("index"))
        };
        assert!(result.is_err());
        assert!(engine.pin().content().is_none());
        assert!(matches!(
            engine.follow_content(&Budget::unbounded(), None),
            Err(Error::NoContentIndex)
        ));
        restore(&tree, &engine, 0);
        assert_ne!(id(&engine, 0), old);
        fs::remove_dir(manifest).unwrap();
        engine.attach_content(&tree.0.join("index")).unwrap();
        assert_eq!(engine.pin().catalog().dormant_docs().count(), 0);
    }
}
