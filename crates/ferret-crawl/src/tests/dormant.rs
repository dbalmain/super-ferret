//! D60 fix round 3: writer hash/identity caches through checked catalog
//! transitions, reopen/recovery and the real reconciliation allocator.
use std::path::Path;

use ferret_catalog::log::{ChangeSet, Record};
use ferret_catalog::{
    Catalog, Content, ContentState, Coverage, DocId, Hash, InoId, Kind, Stat, Transaction,
    WriterSession,
};

use super::{Scratch, log_session};

const HASH: Hash = [1; 16];
const SNIFFER: u32 = 1;

fn stat(ino: u64, directory: bool) -> Stat {
    Stat {
        dev: 7,
        ino,
        mode: if directory { 0o040755 } else { 0o100644 },
        nlink: 1,
        ..Stat::default()
    }
}

fn checkpoint(path: &Path) {
    let mut transaction = Transaction::begin(path, SNIFFER).unwrap();
    let mut batch = transaction.batch();
    let root = batch.root(b"/r", stat(1, true));
    batch.file(root, b"a", stat(2, false), Content::Hashed(HASH));
    batch.entry_count(root, 1);
    transaction.add(batch);
    transaction.commit().unwrap();
}

fn changes(view: &Catalog, records: Vec<Record>, live: u32) -> ChangeSet {
    ChangeSet {
        records,
        counters: [view.next_inode().0, view.next_name().0, view.next_doc().0],
        counts: [
            view.inode_count(),
            view.name_count(),
            view.dir_count(),
            live,
        ],
    }
}

fn binding(doc: Option<u32>) -> Record {
    Record::InodePut {
        id: 1,
        kind: Kind::File,
        state: if doc.is_some() {
            ContentState::Hashed
        } else {
            ContentState::Unindexed
        },
        doc,
        stat: stat(2, false),
    }
}

fn set_content(writer: &mut WriterSession, doc: u32, live: bool) {
    let view = writer.view();
    let record = if live {
        Record::DocPut {
            id: doc,
            references: 1,
            hash: HASH,
        }
    } else {
        Record::DocDelete { id: doc }
    };
    writer
        .commit(
            &changes(
                &view,
                vec![binding(live.then_some(doc)), record],
                u32::from(live),
            ),
            SNIFFER,
        )
        .unwrap();
}

/// Catalog-only rebuilding can leave live 1 beside the still-retained 0.
fn live_beside_dormant(label: &str) -> Scratch {
    let scratch = Scratch::new(label);
    checkpoint(&scratch.path);
    let mut writer = log_session(&scratch.path).unwrap();
    writer.set_coverage(Some(Coverage::new(1, vec![])));
    set_content(&mut writer, 0, false);
    drop(writer);
    checkpoint(&scratch.path);
    let writer = log_session(&scratch.path).unwrap();
    assert_eq!(writer.view().dormant_hash(DocId(0)), Some(HASH));
    assert_eq!(writer.document(HASH), Some(DocId(1)));
    scratch
}

/// Exercise allocation and publication, including a new distinct inode with
/// the returning hash. The continuing file alone would hide a broken lookup.
fn reconcile_copy(writer: &mut WriterSession) -> Catalog {
    let mut batch = writer.batch();
    let root = batch.root(b"/r", stat(1, true));
    batch.file(root, b"a", stat(2, false), Content::Hashed(HASH));
    batch.file(root, b"b", stat(3, false), Content::Hashed(HASH));
    batch.entry_count(root, 2);
    batch.finish_observations();
    let changes =
        crate::reconcile::changes(writer, &[batch], &["/r".into()], &[], [0; 16], SNIFFER)
            .unwrap()
            .unwrap();
    writer.commit(&changes, SNIFFER).unwrap()
}

fn revived_lower_id_is_reused(recover: bool) {
    let scratch = live_beside_dormant(if recover {
        "d60-hash-recover"
    } else {
        "d60-hash-open"
    });
    let mut writer = log_session(&scratch.path).unwrap();
    writer.set_coverage(Some(Coverage::new(1, vec![])));
    set_content(&mut writer, 1, false); // Higher id is uncovered and forgotten.
    set_content(&mut writer, 0, true);
    assert_eq!(writer.document(HASH), Some(DocId(0)));
    // Verify the ordering that exposed the bug: the higher tombstone follows
    // the lower live put in the real effective document projection.
    assert_eq!(
        writer.view().changed_docs().collect::<Vec<_>>(),
        [(DocId(0), true), (DocId(1), false)]
    );
    if recover {
        writer.recover().unwrap();
    } else {
        drop(writer);
        writer = log_session(&scratch.path).unwrap();
    }
    let lookup = writer.document(HASH);
    let after = reconcile_copy(&mut writer);
    assert_eq!(
        (
            lookup,
            writer.document(HASH),
            after.doc_count(),
            after.next_doc()
        ),
        (Some(DocId(0)), Some(DocId(0)), 1, DocId(2)),
        "lower live hash must survive writer reconstruction and deduplicate the new file"
    );
    let root = after.roots().next().unwrap().0;
    for name in [b"a", b"b"] {
        assert_eq!(
            after.doc(after.name(after.lookup(root, name).unwrap()).child),
            Some(DocId(0))
        );
    }
}

#[test]
fn revived_lower_id_is_reused_after_writer_reopen() {
    revived_lower_id_is_reused(false);
}

#[test]
fn revived_lower_id_is_reused_after_writer_recover() {
    revived_lower_id_is_reused(true);
}

/// A contract of `commit`, which accepts this checked transaction: a lower id
/// revives while the higher same-hash id dies. Reconciliation cannot build it
/// from a correct lookup, which would hand the file the live higher id, but
/// it sorts document puts before deaths, the order that lost the live hash.
#[test]
fn live_hash_wins_over_same_transaction_deaths_in_either_record_order() {
    let mut wrong = Vec::new();
    for covered in [false, true] {
        for reverse in [false, true] {
            let scratch = live_beside_dormant(&format!("d60-hash-batch-{covered}-{reverse}"));
            let mut writer = log_session(&scratch.path).unwrap();
            writer.set_coverage(Some(Coverage::new(if covered { 2 } else { 1 }, vec![])));
            let view = writer.view();
            let mut records = vec![
                binding(Some(0)),
                Record::DocPut {
                    id: 0,
                    references: 1,
                    hash: HASH,
                },
                Record::DocDelete { id: 1 },
            ];
            if reverse {
                records.reverse();
            }
            writer.commit(&changes(&view, records, 1), SNIFFER).unwrap();
            let lookup = writer.document(HASH);
            let after = reconcile_copy(&mut writer);
            if (lookup, after.doc_count(), after.next_doc()) != (Some(DocId(0)), 1, DocId(2)) {
                wrong.push((
                    covered,
                    reverse,
                    lookup,
                    after.doc_count(),
                    after.next_doc(),
                ));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "same-hash live puts lost to deaths: {wrong:?}"
    );
}

/// Reconciliation retires a file whose reused kernel identity now names a
/// symlink and allocates a new inode for it, in one transaction. Its records
/// are sorted retirement first; the writer's identity cache must not depend
/// on that, since `commit` accepts the same transaction in any order.
#[test]
fn live_identity_wins_over_same_transaction_retirement_in_either_record_order() {
    let mut wrong = Vec::new();
    for reverse in [false, true] {
        let scratch = Scratch::new(&format!("d60-identity-batch-{reverse}"));
        checkpoint(&scratch.path);
        let mut writer = log_session(&scratch.path).unwrap();
        let mut batch = writer.batch();
        let root = batch.root(b"/r", stat(1, true));
        batch.symlink(
            root,
            b"a",
            Stat {
                mode: 0o120777,
                ..stat(2, false)
            },
            b"t",
        );
        batch.entry_count(root, 1);
        batch.finish_observations();
        let mut delta =
            crate::reconcile::changes(&writer, &[batch], &["/r".into()], &[], [0; 16], SNIFFER)
                .unwrap()
                .unwrap();
        assert!(
            delta.records.contains(&Record::InodeDelete { id: 1 }),
            "{:?}",
            delta.records
        );
        if reverse {
            delta.records.reverse();
        }
        writer.commit(&delta, SNIFFER).unwrap();
        let lookup = writer.identity((7, 2));
        if lookup != Some(InoId(2)) {
            wrong.push((reverse, lookup));
        }
        writer.recover().unwrap();
        assert_eq!(writer.identity((7, 2)), Some(InoId(2)));
    }
    assert!(
        wrong.is_empty(),
        "live identity lost to a retired row: {wrong:?}"
    );
}
