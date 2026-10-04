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
    let path = tmp.base.join("oracle");
    if path.exists() {
        fs::remove_dir_all(&path).unwrap();
    }
    index(&path, &[tmp.tree()], Refresh::All, &options()).unwrap();
    let fresh = Catalog::open(&path).unwrap().unwrap();
    fresh.load_all().unwrap();
    let disk = Catalog::open(&tmp.cat()).unwrap().unwrap();
    disk.load_all().unwrap();
    assert_eq!(listings(current), listings(&fresh));
    assert_eq!(listings(&disk), listings(&fresh));
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
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
    let old = session.view();
    tmp.write("a", b"intermediate");
    fs::remove_file(tmp.at("a")).unwrap();
    tmp.write("b", b"final");
    let req = request(&session, vec![RefreshScope::Root(tmp.tree())]);
    let result = refresh(&mut session, req, &options()).unwrap();
    let RefreshOutcome::Committed { changes } = result.outcome else {
        panic!("missing delta")
    };
    let adopted = old.advance(old.generation(), &changes).unwrap();
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
fn stale_sequence_and_epoch_requests_return_before_invalid_ids_are_dereferenced() {
    let tmp = Tmp::new("refresh-stale");
    tmp.write("a", b"old");
    index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
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
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
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
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
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
        let mut session = WriterSession::open(&tmp.cat()).unwrap();
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
fn move_hints_including_wrong_hints_observe_both_final_endpoints_without_trusting_identity() {
    for wrong in [false, true] {
        let tmp = Tmp::new("refresh-move-hint");
        tmp.write("old/a", b"moved");
        tmp.write("new/other", b"other");
        index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
        let mut session = WriterSession::open(&tmp.cat()).unwrap();
        let mut req = request(&session, vec![]);
        let old_parent = directory(&session.view(), &tmp, "old");
        let new_parent = directory(&session.view(), &tmp, "new");
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
        oracle(&tmp, &result.view);
    }
}

#[test]
fn an_ignore_file_entry_expands_to_its_subtree_and_reinclusion_ancestors_follow_d29() {
    let tmp = Tmp::new("refresh-ignore");
    tmp.write("hidden/deep/a", b"included");
    tmp.write("hidden/deep/b", b"excluded");
    index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
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
    let session = WriterSession::open(&tmp.cat()).unwrap();
    let mut req = request(&session, vec![RefreshScope::Directory(InoId(u32::MAX))]);
    let before = session.view().generation();
    req.expected_generation = before;
    drop(session);
    let mut txn = Transaction::begin(&tmp.cat(), options().sniffer).unwrap();
    txn.keep(tmp.tree().as_os_str().as_bytes()).unwrap();
    let compacted = txn.checkpoint().unwrap();
    assert_eq!(compacted.generation().sequence, before.sequence);
    assert!(compacted.generation().checkpoint > before.checkpoint);
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
    assert!(matches!(
        refresh(&mut session, req, &options()).unwrap().outcome,
        RefreshOutcome::RetryFromCurrent(_)
    ));
    oracle(&tmp, &session.view());
}

#[test]
fn a_large_directory_listing_streams_equal_rows_with_a_fixed_temporary_observation_cap() {
    let tmp = Tmp::new("refresh-observation-cap");
    for n in 0..9000 {
        tmp.write(&format!("dir/{n:05}"), b"same");
    }
    index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
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
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
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
        let mut session = WriterSession::open(&tmp.cat()).unwrap();
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
    use crate::observe::{CONTENT_IO_FAULTS, ContentIo};
    use crate::walk::{IO_HOOKS, IoPoint};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Path;
    use std::sync::Arc;
    let tmp = Tmp::new("refresh-conflicting-aliases");
    tmp.write("left/a", b"shared");
    tmp.write("right/stable", b"stable");
    fs::hard_link(tmp.at("left/a"), tmp.at("right/b")).unwrap();
    index(&tmp.cat(), &[tmp.tree()], Refresh::All, &options()).unwrap();
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
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
    IO_HOOKS.lock().unwrap().push((
        tmp.tree(),
        Arc::new(move |point, path| {
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
        }),
    ));
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
    IO_HOOKS
        .lock()
        .unwrap()
        .retain(|(root, _)| *root != tmp.tree());
    CONTENT_IO_FAULTS.lock().unwrap().retain(|(k, _)| *k != key);
    let req = entry(&session, &tmp, "left", "a");
    let result = refresh(&mut session, req, &options()).unwrap();
    oracle(&tmp, &result.view);
}
