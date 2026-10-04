//! Checkpoint publication uses the production compactor and M2 I/O seam.
use super::{SNIFFER, Scratch, at, commit, dir_stat, file_stat, hash, reopen, snapshot};
use crate::publication::{FAIL, Point};
use crate::{Content, Handle, WriterSession};
use std::fs;

fn fixture(label: &str) -> Scratch {
    let s = Scratch::new(label);
    commit(&s.path, |txn| {
        let mut b = txn.batch();
        let root = b.root(b"/root", dir_stat(1));
        let dir = b.dir(root, b"sub", dir_stat(2));
        b.file(dir, b"a", file_stat(3), Content::Hashed(hash(1)));
        b.file(root, b"alias", file_stat(3), Content::Hashed(hash(1)));
        b.file(root, b"other", file_stat(4), Content::Hashed(hash(2)));
        txn.add(b);
    });
    s
}

#[test]
fn pinned_unloaded_readers_and_unchanged_sequence_handles_survive_dense_remapping() {
    let s = fixture("compact-pinned");
    let mut session = WriterSession::open(&s.path).unwrap();
    session.set_compaction_limits(crate::CompactionLimits {
        log_bytes: u64::MAX,
        records: u64::MAX,
        dirty_percent: u32::MAX,
        dead_percent: u32::MAX,
    });
    let view = session.view();
    let id = at(&view, "/root/other");
    let mut inode = view.inode(id);
    inode.stat.ctime_nsec += 1;
    // Birth before the old first file in basename order forces its inode and
    // alias names to move; a ctime-only update never exercised remapping.
    let child = view.next_inode().0;
    let doc = view.next_doc().0;
    let changes = crate::log::ChangeSet {
        counters: [child + 1, view.next_name().0 + 1, doc + 1],
        counts: [
            view.inode_count() + 1,
            view.name_count() + 1,
            view.dir_count(),
            view.doc_count() + 1,
        ],
        records: vec![
            crate::log::Record::InodePut {
                id: id.0,
                kind: crate::Kind::File,
                state: inode.state,
                doc: inode.doc.map(|id| id.0),
                stat: inode.stat,
            },
            crate::log::Record::LifePut {
                id: child,
                kind: crate::Kind::File,
                flags: 0,
                names: 1,
            },
            crate::log::Record::InodePut {
                id: child,
                kind: crate::Kind::File,
                state: crate::ContentState::Hashed,
                doc: Some(doc),
                stat: file_stat(5),
            },
            crate::log::Record::NamePut {
                id: view.next_name().0,
                parent: view.roots().next().unwrap().0.0,
                child,
                name: b"a-first".to_vec(),
            },
            crate::log::Record::DocPut {
                id: doc,
                references: 1,
                hash: hash(3),
            },
        ],
    };
    session.commit(&changes, SNIFFER).unwrap();
    let old = crate::Catalog::open(&s.path).unwrap().unwrap();
    let generation = old.generation();
    let new = session.compact().unwrap();
    assert_eq!(new.generation().sequence, generation.sequence);
    assert_ne!(new.generation().checkpoint, generation.checkpoint);
    assert!(
        !s.path
            .join(format!("snapshot.{}", generation.checkpoint))
            .exists()
    );
    old.load_all().unwrap();
    assert_eq!(
        super::paths(&old).into_keys().collect::<Vec<_>>(),
        super::paths(&new).into_keys().collect::<Vec<_>>()
    );
    assert_eq!(
        old.inode(at(&old, "/root/other")),
        new.inode(at(&new, "/root/other"))
    );
    assert!(
        new.checked_inode(Handle {
            generation,
            id: crate::InoId(u32::MAX)
        })
        .is_err()
    );
    assert!(
        new.checked_name(Handle {
            generation,
            id: crate::NameId(u32::MAX)
        })
        .is_err()
    );
    assert_eq!(new.next_inode().0, new.inode_count());
    assert_eq!(new.next_name().0, new.name_count());
    assert_eq!(new.next_doc(), old.next_doc());
    let file = at(&new, "/root/sub/a");
    assert_ne!(
        file,
        at(&old, "/root/sub/a"),
        "surviving inode really remapped"
    );
    let aliases: std::collections::BTreeSet<_> = session.names_for(file).collect();
    let expected: std::collections::BTreeSet<_> = [
        new.lookup(new.roots().next().unwrap().0, b"alias").unwrap(),
        new.lookup(at(&new, "/root/sub"), b"a").unwrap(),
    ]
    .into_iter()
    .collect();
    assert_eq!(aliases, expected, "alias inverse rebuilt in the new epoch");
    assert_ne!(
        expected,
        old.names()
            .filter(|(id, _)| old.name(*id).child == at(&old, "/root/sub/a"))
            .map(|(id, _)| id)
            .collect()
    );
    assert_eq!(new.doc_references(new.doc(file).unwrap()), Some(1));
    assert_eq!(session.identity(new.identity(file)), Some(file));
    assert_eq!(session.budget_usage().records, 0);
    assert_eq!(session.budget_usage().log_bytes, 64);
    assert_eq!(reopen(&s.path).generation(), new.generation());
}

#[test]
fn compacting_the_same_effective_state_twice_has_identical_packed_sections() {
    let s = fixture("compact-deterministic");
    let mut session = WriterSession::open(&s.path).unwrap();
    session.compact().unwrap();
    let a = fs::read(snapshot(&s.path)).unwrap();
    session.compact().unwrap();
    let b = fs::read(snapshot(&s.path)).unwrap();
    assert_eq!(&a[..56], &b[..56]);
    assert_ne!(&a[56..64], &b[56..64]);
    assert_eq!(&a[64..crate::format::HEADER], &b[64..crate::format::HEADER]);
    // Only the epoch field and its head digest differ. Section descriptors,
    // offsets, payload checksums and every packed row must be identical.
    assert_eq!(
        &a[crate::format::HEADER..crate::format::TABLE_END - 16],
        &b[crate::format::HEADER..crate::format::TABLE_END - 16]
    );
    assert_eq!(
        &a[crate::format::TABLE_END..],
        &b[crate::format::TABLE_END..]
    );
}

#[test]
fn crashes_at_every_compaction_checkpoint_boundary_select_an_old_or_new_complete_pair() {
    for (i, point) in [
        Point::SnapshotSync,
        Point::SnapshotRename,
        Point::HeaderSync,
        Point::HeaderRename,
        Point::PairSync,
        Point::ManifestSync,
        Point::ManifestRename,
        Point::DirectorySync,
    ]
    .into_iter()
    .enumerate()
    {
        let s = fixture(&format!("compact-stop-{i}"));
        let old = reopen(&s.path);
        let mut session = WriterSession::open(&s.path).unwrap();
        FAIL.set(Some(point));
        assert!(session.compact().is_err(), "{point:?}");
        FAIL.set(None);
        drop(session);
        let recovered = WriterSession::open(&s.path).unwrap();
        let view = recovered.view();
        assert_eq!(super::paths(&view), super::paths(&old));
        let published = matches!(point, Point::ManifestRename | Point::DirectorySync);
        assert_eq!(
            view.generation().checkpoint != old.generation().checkpoint,
            published,
            "{point:?}"
        );
        assert_eq!(view.next_doc(), old.next_doc());
        assert_eq!(view.sniffer_version(), SNIFFER);
        assert!(!s.path.join("catalog.tmp").exists());
        assert!(!s.path.join("current.tmp").exists());
        assert!(!s.path.join("changes.tmp").exists());
    }
}

#[test]
fn independent_retained_search_suppression_survives_packed_checkpoint_remapping() {
    let s = fixture("compact-suppressed");
    let mut session = WriterSession::open(&s.path).unwrap();
    session.set_compaction_limits(crate::CompactionLimits {
        log_bytes: u64::MAX,
        records: u64::MAX,
        dirty_percent: u32::MAX,
        dead_percent: u32::MAX,
    });
    let before = session.view();
    let dir = at(&before, "/root/sub");
    let changes = crate::log::ChangeSet {
        counters: [
            before.next_inode().0,
            before.next_name().0,
            before.next_doc().0,
        ],
        counts: [
            before.inode_count(),
            before.name_count(),
            before.dir_count(),
            before.doc_count(),
        ],
        records: vec![crate::log::Record::DirPut {
            id: dir.0,
            name: before.dir_name(dir).map(|id| id.0),
            entries: None,
            flags: 2 | 8,
            retained_at: Some(before.generation().sequence),
        }],
    };
    session.commit(&changes, SNIFFER).unwrap();
    assert!(!session.view().is_traversed(dir));
    assert!(session.view().is_search_suppressed(dir));
    session.compact().unwrap();
    let new = reopen(&s.path);
    let dir = at(&new, "/root/sub");
    assert!(!new.is_traversed(dir));
    assert!(new.is_search_suppressed(dir));
    assert_eq!(new.retained_at(dir), Some(before.generation().sequence));
    assert_eq!(new.children(dir).count(), 1);
}

#[test]
fn repeated_overwrites_count_once_and_deletions_are_dead_rather_than_dirty_after_reopen() {
    use crate::log::{ChangeSet, Record};
    let s = fixture("compact-budget-replay");
    let mut session = WriterSession::open(&s.path).unwrap();
    session.set_compaction_limits(crate::CompactionLimits {
        log_bytes: u64::MAX,
        records: u64::MAX,
        dirty_percent: u32::MAX,
        dead_percent: u32::MAX,
    });
    for step in 0..7 {
        let view = session.view();
        let id = at(&view, "/root/other");
        let mut inode = view.inode(id);
        inode.stat.ctime_nsec += step + 1;
        let changes = ChangeSet {
            counters: [view.next_inode().0, view.next_name().0, view.next_doc().0],
            counts: [
                view.inode_count(),
                view.name_count(),
                view.dir_count(),
                view.doc_count(),
            ],
            records: vec![Record::InodePut {
                id: id.0,
                kind: crate::Kind::File,
                stat: inode.stat,
                state: inode.state,
                doc: inode.doc.map(|id| id.0),
            }],
        };
        session.commit(&changes, SNIFFER).unwrap();
    }
    assert_eq!(session.budget_usage().dirty_inodes, 1);
    assert_eq!(session.budget_usage().records, 7);
    let view = session.view();
    let root = view.roots().next().unwrap().0;
    let name = view.lookup(root, b"other").unwrap();
    let id = view.name(name).child;
    let changes = ChangeSet {
        counters: [view.next_inode().0, view.next_name().0, view.next_doc().0],
        counts: [
            view.inode_count() - 1,
            view.name_count() - 1,
            view.dir_count(),
            view.doc_count() - 1,
        ],
        records: vec![
            Record::InodeDelete { id: id.0 },
            Record::NameDelete { id: name.0 },
            Record::DocDelete {
                id: view.doc(id).unwrap().0,
            },
        ],
    };
    session.commit(&changes, SNIFFER).unwrap();
    let usage = session.budget_usage();
    assert_eq!(usage.dirty_inodes, 0);
    assert_eq!(usage.dead_inodes, 1);
    assert_eq!(usage.dead_names, 1);
    drop(session);
    let mut session = WriterSession::open(&s.path).unwrap();
    assert_eq!(session.budget_usage(), usage);
    assert_eq!(
        crate::log::Published::open(&s.path)
            .unwrap()
            .unwrap()
            .budget_usage()
            .unwrap(),
        usage
    );
    let next_doc = session.view().next_doc();
    assert!(session.compact_if_needed().unwrap().is_some());
    assert!(session.compact_if_needed().unwrap().is_none());
    assert_eq!(session.view().next_doc(), next_doc);
    assert_eq!(session.view().doc_count(), 1);
    assert_eq!(session.budget_usage().dead_inodes, 0);
    assert_eq!(
        session.budget_usage().base_inodes,
        session.view().inode_count()
    );
}

#[test]
fn cumulative_bursts_cross_the_record_limit_before_the_triggering_delta_is_appended() {
    let s = fixture("compact-cumulative");
    let mut session = WriterSession::open(&s.path).unwrap();
    session.set_compaction_limits(crate::CompactionLimits {
        log_bytes: u64::MAX,
        records: 3,
        dirty_percent: u32::MAX,
        dead_percent: u32::MAX,
    });
    let log = fs::File::open(s.path.join("changes.0")).unwrap();
    let original = session.view().generation();
    let mut end = 64;
    for step in 1..=3 {
        let view = session.view();
        let id = at(&view, "/root/other");
        let mut inode = view.inode(id);
        inode.stat.ctime_nsec += 1;
        let changes = crate::log::ChangeSet {
            counters: [view.next_inode().0, view.next_name().0, view.next_doc().0],
            counts: [
                view.inode_count(),
                view.name_count(),
                view.dir_count(),
                view.doc_count(),
            ],
            records: vec![crate::log::Record::InodePut {
                id: id.0,
                kind: crate::Kind::File,
                state: inode.state,
                doc: inode.doc.map(|id| id.0),
                stat: inode.stat,
            }],
        };
        session.commit(&changes, SNIFFER).unwrap();
        assert_eq!(
            session.view().generation().sequence,
            original.sequence + step
        );
        if step < 3 {
            assert_eq!(session.view().generation().checkpoint, original.checkpoint);
            end = log.metadata().unwrap().len();
            assert_eq!(session.budget_usage().records, step);
        } else {
            assert_ne!(session.view().generation().checkpoint, original.checkpoint);
            assert_eq!(log.metadata().unwrap().len(), end);
            assert_eq!(session.budget_usage().records, 0);
            assert_eq!(
                reopen(&s.path).inode(at(&session.view(), "/root/other")),
                inode
            );
        }
    }
}
