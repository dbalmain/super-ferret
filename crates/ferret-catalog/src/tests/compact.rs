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
    let old = crate::Catalog::open(&s.path).unwrap().unwrap();
    let generation = old.generation();
    let mut session = WriterSession::open(&s.path).unwrap();
    let new = session.compact().unwrap();
    assert_eq!(new.generation().sequence, generation.sequence);
    assert_ne!(new.generation().checkpoint, generation.checkpoint);
    assert!(
        !s.path
            .join(format!("snapshot.{}", generation.checkpoint))
            .exists()
    );
    old.load_all().unwrap();
    assert_eq!(super::paths(&old), super::paths(&new));
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
    assert_eq!(session.names_for(file).count(), 2);
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
