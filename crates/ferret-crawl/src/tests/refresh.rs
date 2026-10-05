//! Simulated bursts use the production crawler, resident writer and reader.
//! Expected views are always materialised by an independent full crawl.
use std::fs;

use std::os::unix::ffi::OsStrExt;

use ferret_catalog::{Catalog, InoId, Target, Transaction, WriterSession};

use super::checkpoint_oracle::listings;
use super::index::Tmp;
use crate::{
    IndexOptions, Refresh, RefreshOutcome, RefreshReason, RefreshRequest, RefreshScope, index,
    refresh,
};

fn options() -> IndexOptions {
    IndexOptions {
        workers: 4,
        ..IndexOptions::default()
    }
}
fn oracle(tmp: &Tmp, current: &Catalog) {
    oracle_with(tmp, &[tmp.tree()], &options(), current);
}
fn oracle_with(tmp: &Tmp, roots: &[std::path::PathBuf], opts: &IndexOptions, current: &Catalog) {
    let path = tmp.base.join("oracle");
    if path.exists() {
        fs::remove_dir_all(&path).unwrap();
    }
    index(&path, roots, Refresh::All, opts).unwrap();
    let fresh = Catalog::open(&path).unwrap().unwrap();
    fresh.load_all().unwrap();
    let disk = Catalog::open(&tmp.cat()).unwrap().unwrap();
    disk.load_all().unwrap();
    assert_eq!(listings(current), listings(&fresh));
    assert_eq!(listings(&disk), listings(&fresh));
    assert_eq!(current.policy(), fresh.policy());
    assert_eq!(disk.policy(), fresh.policy());
    assert_eq!(current.sniffer_version(), fresh.sniffer_version());
    assert_eq!(disk.sniffer_version(), fresh.sniffer_version());
    let paths = |view: &Catalog| view.roots().map(|(_, p)| p.to_vec()).collect::<Vec<_>>();
    assert_eq!(paths(current), paths(&fresh));
    assert_eq!(paths(&disk), paths(&fresh));
}
fn request(session: &WriterSession, scopes: Vec<RefreshScope>) -> RefreshRequest {
    RefreshRequest {
        expected_generation: session.view().generation(),
        scopes,
        rename_hints: Vec::new(),
        reason: RefreshReason::Burst,
    }
}
fn directory(view: &Catalog, tmp: &Tmp, path: &str) -> InoId {
    let path = tmp.at(path);
    let resolved = view.resolve(path.as_os_str().as_bytes()).unwrap();
    let Target::Inode(id) = resolved.target else {
        panic!("not a directory")
    };
    assert!(view.is_directory(id));
    id
}
fn entry(session: &WriterSession, tmp: &Tmp, parent: &str, name: &str) -> RefreshRequest {
    request(
        session,
        vec![RefreshScope::Entry {
            parent: directory(&session.view(), tmp, parent),
            basename: name.as_bytes().to_vec(),
        }],
    )
}

#[test]
fn a_simulated_burst_observes_final_state_deletion_and_adopts_the_same_epoch_view() {
    let tmp = Tmp::new("refresh-final-delete");
    tmp.write("a", b"old");
    tmp.write("b", b"old");
    index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    let old = session.view();
    tmp.write("a", b"intermediate");
    fs::remove_file(tmp.at("a")).unwrap();
    tmp.write("b", b"final");
    let req = request(&session, vec![RefreshScope::Root(tmp.tree())]);
    let result = refresh(&mut session, req, &options()).unwrap();
    let RefreshOutcome::Committed { changes } = result.outcome else {
        panic!("missing delta")
    };
    assert_eq!(result.base_generation, old.generation());
    let adopted = old.advance(result.base_generation, &changes).unwrap();
    assert_eq!(adopted.generation(), result.view.generation());
    assert_eq!(adopted.generation().checkpoint, old.generation().checkpoint);
    assert_eq!(listings(&adopted), listings(&result.view));
    oracle(&tmp, &result.view);
    let req = request(&session, vec![RefreshScope::Root(tmp.tree())]);
    assert!(matches!(
        refresh(&mut session, req, &options()).unwrap().outcome,
        RefreshOutcome::Unchanged
    ));
}

#[test]
fn an_entry_burst_with_global_policy_size_cap_or_sniffer_changes_refreshes_every_root() {
    let tmp = Tmp::new("refresh-global-transitions");
    tmp.write("left/a", b"left content");
    tmp.write("right/a", b"right content");
    let roots = [tmp.at("left"), tmp.at("right")];
    let mut opts = options();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    for step in 0..3 {
        match step {
            0 => opts.global = Some("a\n".into()),
            1 => {
                opts.global = None;
                opts.config.size_cap = 1;
            }
            _ => {
                opts.config.size_cap = u64::MAX;
                opts.sniffer += 1;
            }
        }
        let req = entry(&session, &tmp, "left", "a");
        let result = refresh(&mut session, req, &opts).unwrap();
        assert_eq!(result.report.refreshed.len(), roots.len());
        assert!(result.report.kept.is_empty());
        oracle_with(&tmp, &roots, &opts, &result.view);
    }
}

#[test]
fn a_scoped_burst_respects_nested_root_boundaries_and_overflow_refreshes_them_all() {
    let tmp = Tmp::new("refresh-nested-boundaries");
    tmp.write("outer/a", b"outer");
    tmp.write("outer/inner/b", b"inner");
    let roots = [tmp.at("outer"), tmp.at("outer/inner")];
    index(&tmp.cat(), &roots, Refresh::All, &options()).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    tmp.write("outer/a", b"outer changed");
    let req = entry(&session, &tmp, "outer", "a");
    let result = refresh(&mut session, req, &options()).unwrap();
    assert_eq!(result.report.refreshed, [roots[0].clone()]);
    assert_eq!(result.report.kept, [roots[1].clone()]);
    oracle_with(&tmp, &roots, &options(), &result.view);
    fs::remove_file(tmp.at("outer/inner/b")).unwrap();
    let req = entry(&session, &tmp, "outer/inner", "b");
    let result = refresh(&mut session, req, &options()).unwrap();
    assert_eq!(result.report.refreshed, [roots[1].clone()]);
    oracle_with(&tmp, &roots, &options(), &result.view);
    tmp.write("outer/new", b"new outer");
    tmp.write("outer/inner/new", b"new inner");
    let mut req = request(&session, vec![]);
    req.reason = RefreshReason::Overflow;
    let result = refresh(&mut session, req, &options()).unwrap();
    assert_eq!(result.report.refreshed.len(), roots.len());
    oracle_with(&tmp, &roots, &options(), &result.view);
}

#[test]
fn stale_sequence_and_epoch_requests_return_before_invalid_ids_are_dereferenced() {
    let tmp = Tmp::new("refresh-stale");
    tmp.write("a", b"old");
    index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    let initial = session.view().generation();
    tmp.write("a", b"new");
    let req = request(&session, vec![RefreshScope::Root(tmp.tree())]);
    refresh(&mut session, req, &options()).unwrap();
    for expected in [
        initial,
        ferret_catalog::Generation {
            checkpoint: initial.checkpoint + 1,
            sequence: session.view().generation().sequence,
            ..initial
        },
    ] {
        let mut req = request(&session, vec![RefreshScope::Directory(InoId(u32::MAX))]);
        req.expected_generation = expected;
        assert!(matches!(
            refresh(&mut session, req, &options()).unwrap().outcome,
            RefreshOutcome::RetryFromCurrent(_)
        ));
    }
    oracle(&tmp, &session.view());
}

#[test]
fn overflow_discards_invalid_scopes_and_requests_a_complete_backstop_recrawl() {
    let tmp = Tmp::new("refresh-overflow");
    tmp.write("a", b"old");
    index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    fs::remove_file(tmp.at("a")).unwrap();
    tmp.write("new/a", b"new");
    let mut req = request(&session, vec![RefreshScope::Directory(InoId(u32::MAX))]);
    req.reason = RefreshReason::Overflow;
    let result = refresh(&mut session, req, &options()).unwrap();
    assert_eq!(result.report.refreshed, [tmp.tree()]);
    oracle(&tmp, &result.view);
}

#[test]
fn an_entry_burst_refreshes_the_complete_parent_count_and_keeps_unrelated_subtrees() {
    let tmp = Tmp::new("refresh-entry-count");
    tmp.write("dir/a", b"old");
    tmp.write("dir/b", b"kept");
    tmp.write("outside/deep/a", b"kept");
    index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    let parent = directory(&session.view(), &tmp, "dir");
    let req = entry(&session, &tmp, "dir", "a");
    fs::remove_file(tmp.at("dir/a")).unwrap();
    let result = refresh(&mut session, req, &options()).unwrap();
    assert_eq!(result.view.entry_count(parent), Some(1));
    assert_eq!(result.report.counts.files_read, 0);
    assert_eq!(
        result.report.counts.dirs, 2,
        "root and selected parent only"
    );
    oracle(&tmp, &result.view);
}

#[test]
fn an_entry_scope_with_a_replaced_or_vanished_parent_promotes_to_a_containing_scope() {
    for replaced in [false, true] {
        let tmp = Tmp::new("refresh-parent-replaced");
        tmp.write("dir/a", b"old");
        tmp.write("outside/a", b"kept");
        index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
        let mut session = super::log_session(&tmp.cat()).unwrap();
        let req = entry(&session, &tmp, "dir", "a");
        fs::rename(tmp.at("dir"), tmp.base.join("displaced")).unwrap();
        if replaced {
            tmp.write("dir/new/b", b"new");
            tmp.write("dir/c", b"new");
        }
        let result = refresh(&mut session, req, &options()).unwrap();
        oracle(&tmp, &result.view);
    }
}

#[test]
fn an_entry_parent_with_changed_ctime_expands_its_subtree_instead_of_trusting_identity_alone() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = Tmp::new("refresh-parent-ctime");
    tmp.write("dir/a", b"selected");
    tmp.write("dir/deep/b", b"old descendant");
    index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    let req = entry(&session, &tmp, "dir", "a");
    tmp.write("dir/deep/b", b"new descendant");
    fs::set_permissions(tmp.at("dir"), fs::Permissions::from_mode(0o700)).unwrap();
    let result = refresh(&mut session, req, &options()).unwrap();
    assert_eq!(result.report.counts.dirs, 3);
    oracle(&tmp, &result.view);
}

#[test]
fn move_hints_including_wrong_hints_observe_both_final_endpoints_without_trusting_identity() {
    for wrong in [false, true] {
        let tmp = Tmp::new("refresh-move-hint");
        tmp.write("old/a", b"moved");
        tmp.write("new/other", b"other");
        index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
        let mut session = super::log_session(&tmp.cat()).unwrap();
        let mut req = request(&session, vec![]);
        let old_parent = directory(&session.view(), &tmp, "old");
        let new_parent = directory(&session.view(), &tmp, "new");
        let old_file = session
            .view()
            .name(session.view().lookup(old_parent, b"a").unwrap())
            .child;
        let other_file = session
            .view()
            .name(session.view().lookup(new_parent, b"other").unwrap())
            .child;
        req.rename_hints.push(crate::RenameHint {
            old_parent,
            old_name: b"a".to_vec(),
            new_parent,
            new_name: if wrong {
                b"other".to_vec()
            } else {
                b"a".to_vec()
            },
        });
        if wrong {
            fs::remove_file(tmp.at("old/a")).unwrap();
            tmp.write("new/other", b"different inode's new bytes");
        } else {
            fs::rename(tmp.at("old/a"), tmp.at("new/a")).unwrap();
        }
        let result = refresh(&mut session, req, &options()).unwrap();
        let name = result
            .view
            .lookup(new_parent, if wrong { b"other" } else { b"a" })
            .unwrap();
        assert_eq!(
            result.view.name(name).child,
            if wrong { other_file } else { old_file }
        );
        oracle(&tmp, &result.view);
    }
}

#[test]
fn a_scoped_directory_eacces_is_opaque_while_eio_retains_and_recovery_clears_coverage() {
    use super::coverage::{Hook, retained_listings};
    use crate::walk::IoPoint;
    use std::path::Path;
    for errno in [13, 5] {
        let tmp = Tmp::new("refresh-directory-fault");
        tmp.write("dir/old", b"old");
        tmp.write("outside/a", b"outside");
        index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
        let mut session = super::log_session(&tmp.cat()).unwrap();
        let before = session.view();
        let scope = directory(&before, &tmp, "dir");
        fs::remove_file(tmp.at("dir/old")).unwrap();
        tmp.write("dir/new", b"new");
        let hook = Hook::set(&tmp.tree(), move |point, path| {
            (point == IoPoint::Directory && path == Path::new("dir"))
                .then(|| (crate::IoOp::List, std::io::Error::from_raw_os_error(errno)))
        });
        let req = request(&session, vec![RefreshScope::Directory(scope)]);
        let result = refresh(&mut session, req, &options()).unwrap();
        let req = request(&session, vec![RefreshScope::Directory(scope)]);
        assert!(matches!(
            refresh(&mut session, req, &options()).unwrap().outcome,
            RefreshOutcome::Unchanged
        ));
        if errno == 13 {
            assert!(result.view.children(scope).next().is_none());
            assert_eq!(result.view.entry_count(scope), None);
            assert_eq!(result.view.retained_at(scope), None);
            oracle(&tmp, &result.view);
        } else {
            // Retention expectations come from the real pre-fault checkpoint.
            drop(hook);
            let path = tmp.base.join("oracle");
            index(&path, &[tmp.tree()], Refresh::All, &options()).unwrap();
            let fresh = Catalog::open(&path).unwrap().unwrap();
            fresh.load_all().unwrap();
            let paths = vec![tmp.at("dir").as_os_str().as_bytes().to_vec()];
            let expected = retained_listings(&fresh, &before, &paths, &paths);
            let disk = Catalog::open(&tmp.cat()).unwrap().unwrap();
            disk.load_all().unwrap();
            assert_eq!(listings(&result.view), expected);
            assert_eq!(listings(&disk), expected);
            let req = request(&session, vec![RefreshScope::Directory(scope)]);
            let result = refresh(&mut session, req, &options()).unwrap();
            assert_eq!(result.view.retained_at(scope), None);
            oracle(&tmp, &result.view);
            continue;
        }
        let req = request(&session, vec![RefreshScope::Directory(scope)]);
        assert!(matches!(
            refresh(&mut session, req, &options()).unwrap().outcome,
            RefreshOutcome::Unchanged
        ));
        drop(hook);
        let req = request(&session, vec![RefreshScope::Directory(scope)]);
        let result = refresh(&mut session, req, &options()).unwrap();
        oracle(&tmp, &result.view);
    }
}

#[test]
fn an_ignore_file_entry_expands_to_its_subtree_and_reinclusion_ancestors_follow_d29() {
    let tmp = Tmp::new("refresh-ignore");
    tmp.write("hidden/deep/a", b"included");
    tmp.write("hidden/deep/b", b"excluded");
    index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    for rules in ["hidden/\n!hidden/deep/a\n", "hidden/\n", ""] {
        let req = entry(&session, &tmp, "", ".ferretignore");
        tmp.write(".ferretignore", rules.as_bytes());
        let result = refresh(&mut session, req, &options()).unwrap();
        oracle(&tmp, &result.view);
    }
}

#[test]
fn a_fresh_checkpoint_with_unchanged_sequence_invalidates_old_epoch_scopes() {
    let tmp = Tmp::new("refresh-checkpoint-epoch");
    tmp.write("a", b"content");
    index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
    let session = super::log_session(&tmp.cat()).unwrap();
    let mut req = request(&session, vec![RefreshScope::Directory(InoId(u32::MAX))]);
    let before = session.view().generation();
    req.expected_generation = before;
    drop(session);
    let mut txn = Transaction::begin(&tmp.cat(), options().sniffer).unwrap();
    txn.keep(tmp.tree().as_os_str().as_bytes()).unwrap();
    let compacted = txn.checkpoint().unwrap();
    assert_eq!(compacted.generation().sequence, before.sequence);
    assert!(compacted.generation().checkpoint > before.checkpoint);
    let mut session = super::log_session(&tmp.cat()).unwrap();
    assert!(matches!(
        refresh(&mut session, req, &options()).unwrap().outcome,
        RefreshOutcome::RetryFromCurrent(_)
    ));
    oracle(&tmp, &session.view());
}

#[test]
fn checkpoint_fallback_refuses_a_scoped_batch_instead_of_dropping_untouched_children() {
    let tmp = Tmp::new("refresh-scoped-checkpoint");
    tmp.write("dir/a", b"kept child");
    index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
    let session = super::log_session(&tmp.cat()).unwrap();
    let old = session.view();
    let root = old.roots().next().unwrap().0;
    let mut batch = session.batch();
    let token = batch.root(tmp.tree().as_os_str().as_bytes(), old.inode(root).stat);
    batch.entry_count(token, old.entry_count(root).unwrap());
    batch.preserve(token, b"dir");
    let mut txn = session.into_checkpoint(options().sniffer);
    txn.set_policy(old.policy());
    txn.add(batch);
    assert!(matches!(
        txn.commit(),
        Err(ferret_catalog::CommitError::ScopedObservations)
    ));
    let disk = Catalog::open(&tmp.cat()).unwrap().unwrap();
    assert_eq!(disk.generation(), old.generation());
    oracle(&tmp, &old);
}

#[test]
fn a_large_directory_listing_streams_equal_rows_with_a_fixed_temporary_observation_cap() {
    let tmp = Tmp::new("refresh-observation-cap");
    for n in 0..9000 {
        tmp.write(&format!("dir/{n:05}"), b"same");
    }
    index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    let id = directory(&session.view(), &tmp, "dir");
    let req = request(&session, vec![RefreshScope::Directory(id)]);
    let result = refresh(&mut session, req, &options()).unwrap();
    assert!(matches!(result.outcome, RefreshOutcome::Unchanged));
    assert!(
        result.report.observation_rows_peak
            <= options().workers * ferret_catalog::batch::OBSERVATION_ROWS
    );
    oracle(&tmp, &result.view);
}

#[test]
fn deleting_one_hard_link_promotes_a_kept_alias_scope_inside_the_same_root() {
    let tmp = Tmp::new("refresh-kept-alias");
    tmp.write("left/a", b"shared");
    tmp.write("right/stable", b"stable");
    fs::hard_link(tmp.at("left/a"), tmp.at("right/b")).unwrap();
    index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    let req = entry(&session, &tmp, "left", "a");
    fs::remove_file(tmp.at("left/a")).unwrap();
    let result = refresh(&mut session, req, &options()).unwrap();
    oracle(&tmp, &result.view);
}

#[test]
fn generated_entry_and_directory_bursts_match_the_full_index_after_every_final_state() {
    for seed in 0..3 {
        let tmp = Tmp::new(&format!("refresh-generated-{seed}"));
        for n in 0..4 {
            tmp.write(&format!("d{n}/a"), b"initial");
        }
        index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
        let mut session = super::log_session(&tmp.cat()).unwrap();
        let mut state = seed + 1u64;
        for step in 0..24 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let dir = format!("d{}", state % 4);
            let next = format!("d{}", (state / 4) % 4);
            let parent = directory(&session.view(), &tmp, &dir);
            let other = directory(&session.view(), &tmp, &next);
            let mut req = request(
                &session,
                vec![RefreshScope::Entry {
                    parent,
                    basename: b"a".to_vec(),
                }],
            );
            match (state / 16) % 4 {
                0 => {
                    tmp.write(&format!("{dir}/a"), format!("step {step}").as_bytes());
                }
                1 => {
                    if tmp.at(&format!("{dir}/a")).exists() {
                        fs::remove_file(tmp.at(&format!("{dir}/a"))).unwrap();
                    }
                }
                2 if dir != next && tmp.at(&format!("{dir}/a")).exists() => {
                    req.rename_hints.push(crate::RenameHint {
                        old_parent: parent,
                        old_name: b"a".to_vec(),
                        new_parent: other,
                        new_name: b"a".to_vec(),
                    });
                    fs::rename(tmp.at(&format!("{dir}/a")), tmp.at(&format!("{next}/a"))).unwrap();
                }
                _ => {
                    req.scopes = vec![RefreshScope::Directory(parent)];
                    tmp.write(&format!("{dir}/nested/{step}"), b"nested");
                }
            }
            let result = refresh(&mut session, req, &options()).unwrap();
            oracle(&tmp, &result.view);
        }
    }
}

#[test]
fn conflicting_alias_content_observations_publish_shared_fault_and_match_a_faulted_full_index() {
    use super::coverage::Hook;
    use crate::observe::{CONTENT_IO_FAULTS, ContentIo};
    use crate::walk::IoPoint;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Path;
    struct ContentFault((u64, u64));
    impl Drop for ContentFault {
        fn drop(&mut self) {
            CONTENT_IO_FAULTS
                .lock()
                .unwrap()
                .retain(|(key, _)| *key != self.0);
        }
    }
    let tmp = Tmp::new("refresh-conflicting-aliases");
    tmp.write("left/a", b"shared");
    tmp.write("right/stable", b"stable");
    fs::hard_link(tmp.at("left/a"), tmp.at("right/b")).unwrap();
    index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    let left = directory(&session.view(), &tmp, "left");
    let right = directory(&session.view(), &tmp, "right");
    let req = request(
        &session,
        vec![
            RefreshScope::Entry {
                parent: left,
                basename: b"a".to_vec(),
            },
            RefreshScope::Entry {
                parent: right,
                basename: b"b".to_vec(),
            },
        ],
    );
    // A stable metadata change prevents content carry. One real open fails;
    // the other alias reads successfully, producing conflicting content states.
    fs::set_permissions(tmp.at("left/a"), fs::Permissions::from_mode(0o600)).unwrap();
    let stat = fs::metadata(tmp.at("left/a")).unwrap();
    let key = (stat.dev(), stat.ino());
    let content_fault = ContentFault(key);
    let hook = Hook::set(&tmp.tree(), move |point, path| {
        if point == IoPoint::Child {
            if path == Path::new("left/a") {
                CONTENT_IO_FAULTS
                    .lock()
                    .unwrap()
                    .push((key, ContentIo::Open));
            }
            if path == Path::new("right/b") {
                CONTENT_IO_FAULTS.lock().unwrap().retain(|(k, _)| *k != key);
            }
        }
        None
    });
    let opts = IndexOptions {
        workers: 1,
        ..options()
    };
    let result = refresh(&mut session, req, &opts).unwrap();
    let name = result.view.lookup(left, b"a").unwrap();
    let id = result.view.name(name).child;
    assert_eq!(result.view.state(id), ferret_catalog::ContentState::Fault);
    assert_eq!(result.view.doc(id), None);
    // The independent full crawl uses the same path-specific real I/O seam.
    // Keep it single-worker so one path's injection cannot affect another.
    let path = tmp.base.join("oracle");
    index(&path, &[tmp.tree()], Refresh::All, &opts).unwrap();
    let fresh = Catalog::open(&path).unwrap().unwrap();
    fresh.load_all().unwrap();
    assert_eq!(listings(&result.view), listings(&fresh));
    let disk = Catalog::open(&tmp.cat()).unwrap().unwrap();
    disk.load_all().unwrap();
    assert_eq!(listings(&disk), listings(&fresh));
    drop(hook);
    drop(content_fault);
    let req = entry(&session, &tmp, "left", "a");
    let result = refresh(&mut session, req, &options()).unwrap();
    oracle(&tmp, &result.view);
}
