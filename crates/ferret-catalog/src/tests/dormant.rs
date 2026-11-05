//! D60 permissions through real transactions, log replay and checkpoint writes.
use super::{SNIFFER, Scratch, commit, dir_stat, file_stat, hash, reopen};
use crate::log::{ChangeSet, Record};
use crate::{Catalog, Content, ContentState, Coverage, DocId, InoId, Kind, WriterSession};

fn fixture(label: &str, count: u8) -> Scratch {
    let scratch = Scratch::new(label);
    commit(&scratch.path, |tx| {
        let mut batch = tx.batch();
        let root = batch.root(b"/r", dir_stat(1));
        for n in 0..count {
            batch.file(
                root,
                &[b'a' + n],
                file_stat(u64::from(n) + 2),
                Content::Hashed(hash(n)),
            );
        }
        tx.add(batch);
    });
    scratch
}

fn session(s: &Scratch) -> WriterSession {
    let mut session = WriterSession::open(&s.path).unwrap();
    session.set_compaction_limits(crate::CompactionLimits {
        log_bytes: u64::MAX,
        records: u64::MAX,
        dirty_percent: u32::MAX,
        dead_percent: u32::MAX,
    });
    session.set_coverage(Some(Coverage::new(u32::MAX, vec![])));
    session
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

fn inode(view: &Catalog, doc: u32, content: bool) -> Record {
    let id = InoId(doc + 1);
    Record::InodePut {
        id: id.0,
        kind: Kind::File,
        state: if content {
            ContentState::Hashed
        } else {
            ContentState::Unindexed
        },
        doc: content.then_some(doc),
        stat: view.inode(id).stat,
    }
}
fn die(session: &mut WriterSession, doc: u32) {
    let view = session.view();
    session
        .commit(
            &changes(
                &view,
                vec![inode(&view, doc, false), Record::DocDelete { id: doc }],
                view.doc_count() - 1,
            ),
            SNIFFER,
        )
        .unwrap();
}
fn revive(
    session: &mut WriterSession,
    doc: u32,
    hash: crate::Hash,
) -> Result<Catalog, crate::log::Error> {
    let view = session.view();
    session.commit(
        &changes(
            &view,
            vec![
                inode(&view, doc, true),
                Record::DocPut {
                    id: doc,
                    references: 1,
                    hash,
                },
            ],
            view.doc_count() + 1,
        ),
        SNIFFER,
    )
}

#[test]
fn revival_consumes_permission_without_allocating_and_replays_after_compaction() {
    let s = fixture("dormant-replay", 2);
    let mut session = session(&s);
    let next = session.view().next_doc();
    die(&mut session, 0);
    let dead = reopen(&s.path);
    assert_eq!(dead.docs().count(), 1);
    assert_eq!(
        dead.dormant_docs().collect::<Vec<_>>(),
        [(DocId(0), hash(0))]
    );
    assert_eq!(session.returning_document(hash(0)), Some(DocId(0)));
    assert_eq!(session.document(hash(0)), None);
    assert!(revive(&mut session, 0, hash(9)).is_err());
    let live = revive(&mut session, 0, hash(0)).unwrap();
    assert_eq!(live.next_doc(), next);
    assert_eq!(live.doc_references(DocId(0)), Some(1));
    assert_eq!(live.dormant_docs().count(), 0);
    assert_eq!(reopen(&s.path).doc_hash(DocId(0)), Some(hash(0)));
    die(&mut session, 0);
    session.compact().unwrap();
    drop(session);
    let mut session = WriterSession::open(&s.path).unwrap();
    assert_eq!(session.view().dormant_hash(DocId(0)), Some(hash(0)));
    revive(&mut session, 0, hash(0)).unwrap();
    session.compact().unwrap();
    assert_eq!(reopen(&s.path).next_doc(), next);
}

#[test]
fn forgetting_is_terminal_and_noop_forgetting_does_not_publish() {
    let s = fixture("dormant-forget", 2);
    let mut session = session(&s);
    die(&mut session, 0);
    let before = session.view().generation();
    assert_eq!(
        session.forget_dormant(&[1..=1]).unwrap().generation(),
        before
    );
    session.forget_dormant(&[0..=0]).unwrap();
    assert!(revive(&mut session, 0, hash(0)).is_err());
    assert!(session.view().dormant_hash(DocId(0)).is_none());
    drop(session);
    let mut session = WriterSession::open(&s.path).unwrap();
    assert!(revive(&mut session, 0, hash(0)).is_err());
}

#[test]
fn advancing_coverage_never_authorizes_a_previous_uncovered_or_unreadable_death() {
    for unreadable in [false, true] {
        let s = fixture(&format!("dormant-ineligible-{unreadable}"), 2);
        let mut session = session(&s);
        session.set_coverage(Some(Coverage::new(
            if unreadable { 2 } else { 0 },
            if unreadable { vec![0] } else { vec![] },
        )));
        die(&mut session, 0);
        session.set_coverage(Some(Coverage::new(2, vec![])));
        assert_eq!(session.returning_document(hash(0)), None);
        assert!(revive(&mut session, 0, hash(0)).is_err());
        // An arbitrary dead row cannot be made dormant after its death.
        let view = session.view();
        assert!(
            session
                .commit(
                    &changes(
                        &view,
                        vec![Record::DocDormant {
                            id: 0,
                            hash: hash(0)
                        }],
                        view.doc_count()
                    ),
                    SNIFFER
                )
                .is_err()
        );
    }
}

#[test]
fn cap_evicts_lowest_ids_and_catalog_only_writers_allocate_fresh() {
    let s = fixture("dormant-cap", 4);
    let mut session = session(&s);
    session.set_dormant_cap(2);
    for doc in 0..4 {
        die(&mut session, doc);
    }
    assert_eq!(
        session
            .view()
            .dormant_docs()
            .map(|row| row.0)
            .collect::<Vec<_>>(),
        [DocId(2), DocId(3)]
    );
    assert!(revive(&mut session, 0, hash(0)).is_err());
    session.set_coverage(None);
    assert_eq!(session.returning_document(hash(2)), None);
    drop(session);
    let mut tx = crate::Transaction::begin(&s.path, SNIFFER).unwrap();
    let mut batch = tx.batch();
    let root = batch.root(b"/r", dir_stat(1));
    batch.file(root, b"c", file_stat(4), Content::Hashed(hash(2)));
    tx.add(batch);
    let view = tx.commit().unwrap();
    assert_eq!(view.docs().next().unwrap().0, DocId(4));
    assert_eq!(view.next_doc(), DocId(5));
    assert_eq!(view.dormant_docs().count(), 2);
}

#[test]
fn full_rebuild_preserves_and_consumes_permissions_and_live_hash_wins() {
    let s = fixture("dormant-rebuild", 2);
    let mut session = session(&s);
    die(&mut session, 0);
    for alive in [true, false, true] {
        let mut batch = session.checkpoint_batch(0);
        let root = batch.root(b"/r", dir_stat(1));
        if alive {
            batch.file(root, b"a", file_stat(2), Content::Hashed(hash(0)));
        }
        batch.file(root, b"b", file_stat(3), Content::Hashed(hash(1)));
        let view = session
            .rebuild_checkpoint(vec![batch], SNIFFER, [0; 16])
            .unwrap();
        assert_eq!(view.next_doc(), DocId(2));
        assert_eq!(view.doc_hash(DocId(0)).is_some(), alive);
        assert_eq!(view.dormant_hash(DocId(0)).is_some(), !alive);
        assert_eq!(reopen(&s.path).next_doc(), DocId(2));
    }
}

#[test]
fn dormant_revocation_crash_cuts_never_restore_a_published_permission() {
    use crate::publication::{FAIL, Point};
    for point in [
        Point::PartialLogWrite,
        Point::LogWrite,
        Point::LogSync,
        Point::ManifestSync,
        Point::ManifestRename,
        Point::DirectorySync,
    ] {
        let s = fixture(&format!("dormant-crash-{point:?}"), 2);
        let mut session = session(&s);
        die(&mut session, 0);
        FAIL.set(Some(point));
        let result = session.clear_dormant();
        FAIL.set(None);
        assert!(result.is_err(), "{point:?}");
        let published = result.err().unwrap().published();
        drop(session);
        let mut recovered = WriterSession::open(&s.path).unwrap();
        let permission = recovered.view().dormant_hash(DocId(0)).is_some();
        assert_eq!(permission, !published, "{point:?}");
        assert_eq!(revive(&mut recovered, 0, hash(0)).is_ok(), permission);
    }
}

/// Encode the previous published format from an empty-dormant fixture. Section
/// payloads and log records are byte-identical in v5; only framing changes.
fn downgrade_fixture_to_v5(s: &Scratch) {
    use crate::format::{HEADER, TABLE_END, u32_at, u64_at};
    use crate::generation::checksum;
    let path = super::snapshot(&s.path);
    let current = std::fs::read(&path).unwrap();
    let table = HEADER + 27 * 32;
    let mut legacy = current[..table].to_vec();
    legacy.extend_from_slice(&current[table + 32..TABLE_END - 16]);
    legacy.extend_from_slice(&[0; 16]);
    let end = legacy.len();
    legacy[8..12].copy_from_slice(&5u32.to_le_bytes());
    legacy[20..24].copy_from_slice(&27u32.to_le_bytes());
    for i in 0..27 {
        let at = HEADER + i * 32;
        let offset = u64_at(&legacy, at) - 32;
        legacy[at..at + 8].copy_from_slice(&offset.to_le_bytes());
    }
    let digest = checksum(&legacy[..end - 16]);
    legacy[end - 16..].copy_from_slice(&digest);
    legacy.extend_from_slice(&current[TABLE_END..]);
    std::fs::write(path, legacy).unwrap();
    let manifest_path = s.path.join("current");
    let mut manifest = std::fs::read(&manifest_path).unwrap();
    let checkpoint = u64_at(&manifest, 32);
    let path = s.path.join(format!("changes.{checkpoint}"));
    let mut log = std::fs::read(&path).unwrap();
    log[8..12].copy_from_slice(&5u32.to_le_bytes());
    let digest = checksum(&log[..48]);
    log[48..64].copy_from_slice(&digest);
    let mut at = 64;
    while at < log.len() {
        let len = u64_at(&log, at + 16) as usize;
        let framing = 64 + u32_at(&log, at + 12) as usize * 48;
        log[at + 8..at + 12].copy_from_slice(&5u32.to_le_bytes());
        let digest = checksum(&log[at..at + framing]);
        log[at + len - 16..at + len].copy_from_slice(&digest);
        at += len;
    }
    std::fs::write(path, log).unwrap();
    manifest[..8].copy_from_slice(b"FERRETCR");
    manifest[8..12].copy_from_slice(&5u32.to_le_bytes());
    let digest = checksum(&manifest[..112]);
    manifest[112..].copy_from_slice(&digest);
    std::fs::write(manifest_path, manifest).unwrap();
}

#[test]
fn v5_checkpoint_and_log_migrate_without_invalidating_legacy_docids() {
    let s = fixture("dormant-migration", 2);
    let mut session = session(&s);
    let view = session.view();
    let record = match inode(&view, 0, true) {
        Record::InodePut {
            id,
            kind,
            state,
            doc,
            mut stat,
        } => {
            stat.ctime_nsec += 1;
            Record::InodePut {
                id,
                kind,
                state,
                doc,
                stat,
            }
        }
        _ => unreachable!(),
    };
    session
        .commit(
            &changes(
                &view,
                vec![
                    record,
                    Record::NamePut {
                        id: 0,
                        parent: 0,
                        child: 1,
                        name: b"a".to_vec(),
                        revealed: true,
                    },
                ],
                view.doc_count(),
            ),
            SNIFFER,
        )
        .unwrap();
    let before = session.view();
    drop(session);
    downgrade_fixture_to_v5(&s);
    let legacy = reopen(&s.path);
    assert_eq!(legacy.format_version(), 5);
    assert_eq!(legacy.generation(), before.generation());
    let mut session = WriterSession::open(&s.path).unwrap();
    let migrated = session.view();
    assert_eq!(migrated.format_version(), 6);
    assert_eq!(
        migrated.generation().incarnation,
        before.generation().incarnation
    );
    assert_eq!(migrated.generation().sequence, before.generation().sequence);
    assert_eq!(migrated.next_doc(), before.next_doc());
    assert_eq!(
        migrated.docs().collect::<Vec<_>>(),
        before.docs().collect::<Vec<_>>()
    );
    assert_eq!(migrated.dormant_docs().count(), 0);
    assert!(migrated.is_revealed(crate::NameId(0)));
    // Old live coverage can authorize a subsequent death; migration itself
    // creates no permissions for historical holes.
    session.set_coverage(Some(Coverage::new(2, vec![])));
    die(&mut session, 0);
    assert_eq!(session.view().dormant_hash(DocId(0)), Some(hash(0)));
}

#[test]
fn live_hash_wins_and_a_later_death_replaces_the_older_same_hash_candidate() {
    let s = fixture("dormant-same-hash", 1);
    let mut writer = session(&s);
    die(&mut writer, 0);
    drop(writer);
    // A catalog-only writer follows B and allocates a second identity while
    // preserving the older, still valid content permission.
    let view = commit(&s.path, |tx| {
        let mut batch = tx.batch();
        let root = batch.root(b"/r", dir_stat(1));
        batch.file(root, b"a", file_stat(2), Content::Hashed(hash(0)));
        tx.add(batch);
    });
    assert_eq!(view.doc_hash(DocId(1)), Some(hash(0)));
    assert_eq!(view.dormant_hash(DocId(0)), Some(hash(0)));
    let mut writer = session(&s);
    assert_eq!(writer.returning_document(hash(0)), Some(DocId(1)));
    let view = writer.view();
    let records = vec![
        Record::InodePut {
            id: 1,
            kind: Kind::File,
            state: ContentState::Unindexed,
            doc: None,
            stat: view.inode(InoId(1)).stat,
        },
        Record::DocDelete { id: 1 },
    ];
    writer.commit(&changes(&view, records, 0), SNIFFER).unwrap();
    assert_eq!(writer.returning_document(hash(0)), Some(DocId(1)));
    assert_eq!(
        writer.view().dormant_docs().collect::<Vec<_>>(),
        [(DocId(1), hash(0))]
    );
    writer.compact().unwrap();
    drop(writer);
    let writer = session(&s);
    assert_eq!(writer.returning_document(hash(0)), Some(DocId(1)));
    assert_eq!(writer.document(hash(0)), None);
}
