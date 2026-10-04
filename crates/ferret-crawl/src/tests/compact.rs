//! Real crawl diffs cross budgets and compact; each final state uses the
//! materialised full-index oracle, including retained pre-fault rows.
use super::{checkpoint_oracle::listings, index::Tmp};
use crate::{
    IndexOptions, Refresh, RefreshOutcome, RefreshReason, RefreshRequest, RefreshScope, index,
    refresh,
};
use ferret_catalog::{Catalog, CompactionLimits, InoId, Target, WriterSession};
use std::fs;

fn options() -> IndexOptions {
    IndexOptions {
        workers: 4,
        ..IndexOptions::default()
    }
}
fn open(path: &std::path::Path) -> Catalog {
    let view = Catalog::open(path).unwrap().unwrap();
    view.load_all().unwrap();
    view
}
fn oracle(tmp: &Tmp, view: &Catalog) {
    let path = tmp.base.join("compact-oracle");
    if path.exists() {
        fs::remove_dir_all(&path).unwrap();
    }
    index(&path, &[tmp.tree()], Refresh::All, &options()).unwrap();
    assert_eq!(listings(view), listings(&open(&path)));
    assert_eq!(listings(view), listings(&open(&tmp.cat())));
}
fn burst(tmp: &Tmp, session: &mut WriterSession) -> crate::RefreshReport {
    let request = RefreshRequest {
        expected_generation: session.view().generation(),
        scopes: vec![RefreshScope::Root(tmp.tree())],
        rename_hints: Vec::new(),
        reason: RefreshReason::Burst,
    };
    refresh(session, request, &options()).unwrap()
}
fn dense(view: &Catalog) {
    assert_eq!(view.next_inode().0, view.inode_count());
    assert_eq!(view.next_name().0, view.name_count());
    assert_eq!(
        view.inode_ids().map(|id| id.0).collect::<Vec<_>>(),
        (0..view.inode_count()).collect::<Vec<_>>()
    );
    let mut queue: Vec<_> = view.roots().map(|(id, _)| id).collect();
    let mut at = 0;
    let mut next = queue.len() as u32;
    let mut name = 0;
    while at < queue.len() {
        let parent = queue[at];
        assert_eq!(parent.0, at as u32);
        for id in view.children(parent) {
            assert_eq!(id.0, name);
            name += 1;
            if let Target::Inode(child) = view.name(id).target()
                && view.is_directory(child)
            {
                assert_eq!(child.0, next);
                next += 1;
                assert_eq!(view.dir_name(child), Some(id));
                queue.push(child);
            }
        }
        at += 1;
    }
}

#[test]
fn a_diff_crossing_each_budget_builds_the_checked_final_checkpoint_without_appending_old_epoch_rows()
 {
    for case in ["bytes", "records", "dirty", "dead"] {
        let tmp = Tmp::new(&format!("compact-bound-{case}"));
        tmp.write("dir/a", b"a");
        tmp.write("b", b"b");
        index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
        let mut session = WriterSession::open(&tmp.cat()).unwrap();
        let limits = CompactionLimits {
            log_bytes: if case == "bytes" { 65 } else { u64::MAX },
            records: if case == "records" { 1 } else { u64::MAX },
            dirty_percent: if case == "dirty" { 1 } else { u32::MAX },
            dead_percent: if case == "dead" { 5 } else { u32::MAX },
        };
        session.set_compaction_limits(limits);
        let old = session.view();
        let log = fs::File::open(
            tmp.cat()
                .join(format!("changes.{}", old.generation().checkpoint)),
        )
        .unwrap();
        if case == "dead" {
            fs::remove_file(tmp.at("dir/a")).unwrap();
        } else {
            tmp.write("dir/a", b"changed");
        }
        let result = burst(&tmp, &mut session);
        assert!(
            matches!(result.outcome, RefreshOutcome::Checkpointed),
            "{case}"
        );
        assert_eq!(log.metadata().unwrap().len(), 64, "no over-budget append");
        assert_eq!(
            result.view.generation().sequence,
            old.generation().sequence + 1
        );
        assert_eq!(result.view.next_doc().0, session.view().next_doc().0);
        dense(&result.view);
        oracle(&tmp, &result.view);
        assert_eq!(session.budget_usage().records, 0);
        assert_eq!(session.budget_usage().dirty_inodes, 0);
    }
}

#[test]
fn generated_churn_compacts_dense_bfs_ids_preserving_docids_and_matches_a_full_crawl_after_every_burst()
 {
    let tmp = Tmp::new("compact-churn");
    tmp.write("stable", b"stable");
    tmp.write("dir/a", b"initial");
    index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
    let doc = session
        .view()
        .docs()
        .find(|(_, hash)| *hash == blake3::hash(b"stable").as_bytes()[..16])
        .unwrap()
        .0;
    let mut next_doc = session.view().next_doc().0;
    for step in 0..32 {
        let path = format!("dir/file-{}", step % 7);
        if step % 3 == 0 && tmp.at(&path).exists() {
            fs::remove_file(tmp.at(&path)).unwrap();
        } else {
            tmp.write(&path, format!("churn-{step}").as_bytes());
        }
        let result = burst(&tmp, &mut session);
        assert!(matches!(result.outcome, RefreshOutcome::Checkpointed));
        dense(&result.view);
        oracle(&tmp, &result.view);
        assert!(result.view.doc_hash(doc).is_some());
        assert!(result.view.next_doc().0 >= next_doc);
        next_doc = result.view.next_doc().0;
        let before = result.view.generation();
        assert!(matches!(
            burst(&tmp, &mut session).outcome,
            RefreshOutcome::Unchanged
        ));
        assert_eq!(session.view().generation(), before);
    }
}

#[test]
fn an_idle_compaction_at_unchanged_sequence_rejects_queued_numeric_scopes_before_dereferencing() {
    let tmp = Tmp::new("compact-stale");
    tmp.write("a", b"a");
    index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
    let expected_generation = session.view().generation();
    session.compact().unwrap();
    assert_eq!(
        session.view().generation().sequence,
        expected_generation.sequence
    );
    let req = RefreshRequest {
        expected_generation,
        scopes: vec![RefreshScope::Directory(InoId(u32::MAX))],
        rename_hints: Vec::new(),
        reason: RefreshReason::Burst,
    };
    let result = refresh(&mut session, req, &options()).unwrap();
    assert!(matches!(
        result.outcome,
        RefreshOutcome::RetryFromCurrent(_)
    ));
    oracle(&tmp, &result.view);
}
