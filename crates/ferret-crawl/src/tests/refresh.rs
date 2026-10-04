//! Simulated bursts use the production crawler, resident writer and reader.
//! Expected views are always materialised by an independent full crawl.
use std::fs;

use ferret_catalog::{Catalog, InoId, WriterSession};

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
