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
    tmp.write("duplicate", b"stable");
    tmp.write("dir/a", b"initial");
    tmp.write("z/deep/file", b"nested");
    tmp.write(".git/config", b"");
    fs::hard_link(tmp.at("stable"), tmp.at("z/stable-alias")).unwrap();
    std::os::unix::fs::symlink("../stable", tmp.at("z/link")).unwrap();
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
        if step % 4 == 0 {
            let (from, to) = if tmp.at("z/deep").exists() {
                ("z/deep", "dir/moved")
            } else {
                ("dir/moved", "z/deep")
            };
            fs::rename(tmp.at(from), tmp.at(to)).unwrap();
        }
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
        assert_eq!(result.view.doc_references(doc), Some(2));
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

#[test]
fn retained_eio_scopes_and_opaque_eacces_survive_compaction_and_recovery() {
    use super::coverage::{Hook, retained_listings};
    use crate::walk::IoPoint;
    use crate::{IoOp, recrawl};
    use std::os::unix::ffi::OsStrExt;
    for denied in [false, true] {
        let tmp = Tmp::new(&format!("compact-fault-{denied}"));
        tmp.write("dir/old", b"retained");
        tmp.write("stable", b"old");
        index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
        let mut session = super::log_session(&tmp.cat()).unwrap();
        let before = session.view();
        tmp.write("stable", b"fresh");
        let hook = Hook::set(&tmp.tree(), move |point, path| {
            (point == IoPoint::Directory && path == std::path::Path::new("dir")).then(|| {
                (
                    IoOp::List,
                    std::io::Error::from_raw_os_error(if denied { 13 } else { 5 }),
                )
            })
        });
        recrawl(&mut session, &[tmp.tree()], Refresh::All, &options()).unwrap();
        let faulted = session.view();
        session.compact().unwrap();
        assert_eq!(listings(&session.view()), listings(&faulted));
        dense(&session.view());
        assert_eq!(listings(&open(&tmp.cat())), listings(&faulted));
        let root = session.view().roots().next().unwrap().0;
        let dir = session
            .view()
            .name(session.view().lookup(root, b"dir").unwrap())
            .child;
        assert_eq!(session.view().entry_count(dir), None);
        assert_eq!(session.view().retained_at(dir).is_some(), !denied);
        assert_eq!(session.view().children(dir).count(), usize::from(!denied));
        let generation = session.view().generation();
        recrawl(&mut session, &[tmp.tree()], Refresh::All, &options()).unwrap();
        assert_eq!(session.view().generation(), generation);
        let oracle_path = tmp.base.join("fault-oracle");
        if denied {
            index(&oracle_path, &[tmp.tree()], Refresh::All, &options()).unwrap();
            assert_eq!(listings(&session.view()), listings(&open(&oracle_path)));
            drop(hook);
        } else {
            drop(hook);
            index(&oracle_path, &[tmp.tree()], Refresh::All, &options()).unwrap();
            let path = tmp.at("dir").as_os_str().as_bytes().to_vec();
            assert_eq!(
                listings(&session.view()),
                retained_listings(
                    &open(&oracle_path),
                    &before,
                    std::slice::from_ref(&path),
                    std::slice::from_ref(&path)
                )
            );
        }
        burst(&tmp, &mut session);
        oracle(&tmp, &session.view());
    }
}
