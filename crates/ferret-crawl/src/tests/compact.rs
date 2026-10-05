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
        let births = u32::from(case != "dead");
        assert_eq!(result.view.next_doc().0, old.next_doc().0 + births);
        for (doc, hash) in old.docs() {
            if result.view.docs().any(|(_, live)| live == hash) {
                assert_eq!(result.view.doc_hash(doc), Some(hash));
            }
        }
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

#[test]
fn eleven_deletions_and_an_unreadable_highest_inode_report_the_surviving_alias_after_automatic_compaction()
 {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    // Both callers formerly dereferenced reporting ids from the retired epoch.
    for resident in [false, true] {
        let tmp = Tmp::new(&format!("compact-content-fault-{resident}"));
        for n in 0..12 {
            tmp.write(&format!("file-{n:02}"), format!("contents-{n}").as_bytes());
        }
        index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
        let old = open(&tmp.cat());
        let survivor = old
            .inode_ids()
            .filter(|&id| !old.is_directory(id))
            .max_by_key(|id| id.0)
            .unwrap();
        let name = old
            .names()
            .find(|(id, _)| old.name(*id).child == survivor)
            .unwrap()
            .0;
        let mut path = Vec::new();
        old.path(name, &mut path);
        let path = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&path));
        for entry in fs::read_dir(tmp.tree()).unwrap() {
            let entry = entry.unwrap();
            if entry.path() != path {
                fs::remove_file(entry.path()).unwrap();
            }
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let report = if resident {
            let mut session = WriterSession::open(&tmp.cat()).unwrap();
            crate::recrawl(&mut session, &[tmp.tree()], Refresh::All, &options()).unwrap()
        } else {
            index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap()
        };
        assert_eq!(report.content_faults.len(), 1);
        assert_eq!(report.content_faults[0].0, path);
        assert!(
            matches!(&report.content_faults[0].1, crate::ContentFault::Open(e) if e.kind() == std::io::ErrorKind::PermissionDenied)
        );
        let new = open(&tmp.cat());
        assert_ne!(new.generation().checkpoint, old.generation().checkpoint);
        let id = new.inode_ids().find(|&id| !new.is_directory(id)).unwrap();
        assert_ne!(id, survivor, "the survivor was renumbered");
        assert_eq!(new.state(id), ferret_catalog::ContentState::Fault);
        oracle(&tmp, &new);
    }
}

#[test]
fn small_record_and_owned_name_budgets_discard_scoped_diffs_and_rewalk_all_roots_preserving_docids()
{
    use ferret_catalog::InputLimits;
    use std::os::unix::ffi::OsStrExt;
    for by_bytes in [false, true] {
        let tmp = Tmp::new(&format!("input-budget-{by_bytes}"));
        tmp.write("changed/a", b"old");
        tmp.write("untouched/stable", b"stable");
        for n in 0..40 {
            tmp.write(
                &format!("changed/{}-{n:02}", "long-owned-name".repeat(6)),
                b"initial",
            );
        }
        index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
        let mut session = WriterSession::open(&tmp.cat()).unwrap();
        let old = session.view();
        let root = old.roots().next().unwrap().0;
        let dir = old.name(old.lookup(root, b"changed").unwrap()).child;
        let stable = old
            .docs()
            .find(|(_, hash)| *hash == blake3::hash(b"stable").as_bytes()[..16])
            .unwrap()
            .0;
        let limits = InputLimits {
            records: if by_bytes { usize::MAX } else { 8 },
            owned_bytes: if by_bytes { 512 } else { usize::MAX },
        };
        session.set_input_limits(limits);
        for entry in fs::read_dir(tmp.at("changed")).unwrap() {
            fs::write(entry.unwrap().path(), b"replacement").unwrap();
        }
        let request = RefreshRequest {
            expected_generation: old.generation(),
            scopes: vec![RefreshScope::Directory(dir)],
            rename_hints: Vec::new(),
            reason: RefreshReason::Burst,
        };
        let result = refresh(&mut session, request, &options()).unwrap();
        assert!(matches!(result.outcome, RefreshOutcome::Checkpointed));
        assert!(result.report.input_fallback && result.report.input_usage.exceeded);
        assert!(result.report.input_usage.records <= limits.records);
        assert!(result.report.input_usage.owned_bytes <= limits.owned_bytes);
        assert_eq!(result.view.doc_hash(stable), old.doc_hash(stable));
        assert_eq!(result.view.next_doc().0, old.next_doc().0 + 1);
        assert!(
            result
                .view
                .resolve(tmp.at("untouched/stable").as_os_str().as_bytes())
                .is_some()
        );
        assert!(
            matches!(
                WriterSession::open(&tmp.cat()),
                Err(ferret_catalog::log::Error::Locked)
            ),
            "fallback kept the writer lock"
        );
        for id in result
            .view
            .inode_ids()
            .filter(|&id| !result.view.is_directory(id))
        {
            assert_eq!(session.identity(result.view.identity(id)), Some(id));
        }
        oracle(&tmp, &result.view);
    }
}

#[test]
fn bounded_full_rewalk_keeps_transient_subtrees_and_publishes_directory_eacces_opaque() {
    use super::coverage::{Hook, retained_listings};
    use crate::walk::IoPoint;
    use crate::{IoOp, recrawl};
    use std::os::unix::ffi::OsStrExt;
    for denied in [false, true] {
        let tmp = Tmp::new(&format!("bounded-fault-{denied}"));
        tmp.write("dir/old", b"retained");
        tmp.write("outside", b"old");
        index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
        let mut session = WriterSession::open(&tmp.cat()).unwrap();
        let before = session.view();
        session.set_input_limits(ferret_catalog::InputLimits {
            records: 0,
            owned_bytes: 0,
        });
        tmp.write("outside", b"trustworthy change");
        tmp.write("dir/old", b"unobserved change");
        let hook = Hook::set(&tmp.tree(), move |point, path| {
            (point == IoPoint::Directory && path == std::path::Path::new("dir")).then(|| {
                (
                    IoOp::List,
                    std::io::Error::from_raw_os_error(if denied { 13 } else { 5 }),
                )
            })
        });
        let result = burst(&tmp, &mut session);
        assert!(result.report.input_fallback);
        assert!(matches!(result.outcome, RefreshOutcome::Checkpointed));
        let dir = result
            .view
            .name(
                result
                    .view
                    .lookup(result.view.roots().next().unwrap().0, b"dir")
                    .unwrap(),
            )
            .child;
        assert_eq!(result.view.entry_count(dir), None);
        assert_eq!(result.view.children(dir).count(), usize::from(!denied));
        assert_eq!(result.view.retained_at(dir).is_some(), !denied);
        if denied {
            oracle(&tmp, &result.view);
            drop(hook);
        } else {
            drop(hook);
            let fresh_path = tmp.base.join("bounded-fault-oracle");
            index(&fresh_path, &[tmp.tree()], Refresh::All, &options()).unwrap();
            let scope = tmp.at("dir").as_os_str().as_bytes().to_vec();
            assert_eq!(
                listings(&result.view),
                retained_listings(
                    &open(&fresh_path),
                    &before,
                    std::slice::from_ref(&scope),
                    std::slice::from_ref(&scope)
                )
            );
        }
        recrawl(&mut session, &[tmp.tree()], Refresh::All, &options()).unwrap();
        oracle(&tmp, &session.view());
    }
}

#[test]
fn bounded_full_rewalk_under_protected_policy_or_sniffer_transitions_leaves_the_generation_intact()
{
    use super::coverage::Hook;
    use crate::walk::IoPoint;
    use crate::{IoOp, recrawl};
    for sniffer in [false, true] {
        let tmp = Tmp::new(&format!("bounded-transition-{sniffer}"));
        tmp.write("dir/old", b"old");
        tmp.write("outside", b"old");
        index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
        let mut session = WriterSession::open(&tmp.cat()).unwrap();
        let before = session.view();
        session.set_input_limits(ferret_catalog::InputLimits {
            records: 0,
            owned_bytes: 0,
        });
        tmp.write("outside", b"changed");
        let hook = Hook::set(&tmp.tree(), |point, path| {
            (point == IoPoint::Directory && path == std::path::Path::new("dir"))
                .then(|| (IoOp::List, std::io::Error::from_raw_os_error(5)))
        });
        let mut changed = options();
        if sniffer {
            changed.sniffer += 1;
        } else {
            changed.global = Some("unrelated-rule\n".into());
        }
        assert!(matches!(
            recrawl(&mut session, &[tmp.tree()], Refresh::All, &changed),
            Err(crate::IndexError::Coverage { .. })
        ));
        assert_eq!(session.view().generation(), before.generation());
        assert_eq!(open(&tmp.cat()).generation(), before.generation());
        assert_eq!(listings(&session.view()), listings(&before));
        drop(hook);
        recrawl(&mut session, &[tmp.tree()], Refresh::All, &changed).unwrap();
        assert_ne!(
            session.view().generation().checkpoint,
            before.generation().checkpoint
        );
    }
}

#[test]
fn bounded_full_rewalk_fresh_alias_supersedes_the_retained_alias_observation() {
    use super::coverage::{Hook, retained_listings};
    use crate::IoOp;
    use crate::walk::IoPoint;
    use std::os::unix::ffi::OsStrExt;
    let tmp = Tmp::new("bounded-retained-alias");
    tmp.write("dir/alias", b"old content");
    fs::hard_link(tmp.at("dir/alias"), tmp.at("fresh-alias")).unwrap();
    index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
    let before = session.view();
    session.set_input_limits(ferret_catalog::InputLimits {
        records: 0,
        owned_bytes: 0,
    });
    tmp.write("fresh-alias", b"fresh shared content");
    let hook = Hook::set(&tmp.tree(), |point, path| {
        (point == IoPoint::Directory && path == std::path::Path::new("dir"))
            .then(|| (IoOp::List, std::io::Error::from_raw_os_error(5)))
    });
    let result = burst(&tmp, &mut session);
    assert!(result.report.input_fallback);
    assert!(
        result.report.content_faults.is_empty(),
        "retained observations yield to fresh aliases"
    );
    drop(hook);
    let path = tmp.base.join("alias-full-oracle");
    index(&path, &[tmp.tree()], Refresh::All, &options()).unwrap();
    let scope = tmp.at("dir").as_os_str().as_bytes().to_vec();
    assert_eq!(
        listings(&result.view),
        retained_listings(&open(&path), &before, &[], &[scope])
    );
}

#[test]
fn unchanged_children_with_lstat_eio_and_long_names_exhaust_the_shared_byte_guard() {
    use super::coverage::{Hook, retained_listings};
    use crate::IoOp;
    use crate::walk::IoPoint;
    use std::os::unix::ffi::OsStrExt;
    let tmp = Tmp::new("bounded-child-fault-paths");
    for n in 0..32 {
        tmp.write(&format!("{n:02}-{}", "x".repeat(200)), b"unchanged");
    }
    index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
    let before = session.view();
    session.set_input_limits(ferret_catalog::InputLimits {
        records: usize::MAX,
        owned_bytes: 8192,
    });
    let hook = Hook::set(&tmp.tree(), |point, _| {
        (point == IoPoint::Child).then(|| (IoOp::Lstat, std::io::Error::from_raw_os_error(5)))
    });
    let result = burst(&tmp, &mut session);
    // These unchanged children create no changed file observations. Their
    // owned coverage paths must trip the guard before reconciliation begins.
    assert!(result.report.input_fallback);
    assert!(result.report.input_usage.exceeded);
    assert!(result.report.input_usage.owned_bytes <= 8192);
    assert!(matches!(result.outcome, RefreshOutcome::Checkpointed));
    assert_eq!(
        result.report.coverage_faults.len(),
        32,
        "rewalk reports every fault"
    );
    drop(hook);
    let fresh = tmp.base.join("child-fault-oracle");
    index(&fresh, &[tmp.tree()], Refresh::All, &options()).unwrap();
    let root = tmp.tree().as_os_str().as_bytes().to_vec();
    assert_eq!(
        listings(&result.view),
        retained_listings(&open(&fresh), &before, &[], std::slice::from_ref(&root))
    );
    assert_eq!(listings(&result.view), listings(&open(&tmp.cat())));
}

#[test]
fn one_denied_and_one_retained_directory_inside_a_forced_fallback_preserve_the_full_tree_oracle() {
    use super::coverage::{Hook, retained_listings};
    use crate::IoOp;
    use crate::walk::IoPoint;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    for workers in [1, 4] {
        let opts = IndexOptions {
            workers,
            ..options()
        };
        let tmp = Tmp::new(&format!("bounded-mixed-small-faults-{workers}"));
        for dir in 0..32 {
            for file in 0..16 {
                tmp.write(
                    &format!("dir-{dir:02}/file-{file:02}"),
                    format!("old-{dir}-{file}").as_bytes(),
                );
            }
        }
        std::os::unix::fs::symlink("file-00", tmp.at("dir-02/link")).unwrap();
        tmp.write("dir-01/nested/child", b"retained nested child");
        index(&tmp.cat(), &[tmp.tree()], Refresh::All, &opts).unwrap();
        let mut session = WriterSession::open(&tmp.cat()).unwrap();
        let before = session.view();
        session.set_input_limits(ferret_catalog::InputLimits {
            records: 0,
            owned_bytes: 0,
        });
        tmp.write("dir-02/file-00", b"fresh content");
        tmp.write("dir-01/file-00", b"unobserved content");
        let hook = Hook::set(&tmp.tree(), |point, path| {
            if point != IoPoint::Directory {
                return None;
            }
            let errno = if path == Path::new("dir-00") {
                13
            } else if path == Path::new("dir-01") {
                5
            } else {
                return None;
            };
            Some((IoOp::List, std::io::Error::from_raw_os_error(errno)))
        });
        let expected_generation = session.view().generation();
        let result = refresh(
            &mut session,
            RefreshRequest {
                expected_generation,
                scopes: vec![RefreshScope::Root(tmp.tree())],
                rename_hints: Vec::new(),
                reason: RefreshReason::Burst,
            },
            &opts,
        )
        .unwrap();
        assert!(result.report.input_fallback);
        assert!(matches!(result.outcome, RefreshOutcome::Checkpointed));
        assert_eq!(
            result.report.coverage_faults.len(),
            1,
            "D26 denials are covered, not reported as retained faults"
        );
        let root = result.view.roots().next().unwrap().0;
        let denied = result
            .view
            .name(result.view.lookup(root, b"dir-00").unwrap())
            .child;
        let retained = result
            .view
            .name(result.view.lookup(root, b"dir-01").unwrap())
            .child;
        assert_eq!(result.view.children(denied).count(), 0);
        assert_eq!(result.view.entry_count(denied), None);
        assert_eq!(result.view.retained_at(denied), None);
        assert_eq!(result.view.children(retained).count(), 17);
        assert_eq!(result.view.entry_count(retained), None);
        assert!(result.view.retained_at(retained).is_some());
        // The full oracle sees the real EACCES observation too; only EIO is
        // disabled, so its expected subtree comes from the actual old
        // checkpoint.
        drop(hook);
        let hook = Hook::set(&tmp.tree(), |point, path| {
            (point == IoPoint::Directory && path == Path::new("dir-00"))
                .then(|| (IoOp::List, std::io::Error::from_raw_os_error(13)))
        });
        let fresh = tmp.base.join("mixed-fault-oracle");
        index(&fresh, &[tmp.tree()], Refresh::All, &opts).unwrap();
        let scope = tmp.at("dir-01").as_os_str().as_bytes().to_vec();
        assert_eq!(
            listings(&result.view),
            retained_listings(
                &open(&fresh),
                &before,
                std::slice::from_ref(&scope),
                std::slice::from_ref(&scope)
            )
        );
        assert_eq!(listings(&result.view), listings(&open(&tmp.cat())));
        drop(hook);
        burst(&tmp, &mut session);
        oracle(&tmp, &session.view());
    }
}
