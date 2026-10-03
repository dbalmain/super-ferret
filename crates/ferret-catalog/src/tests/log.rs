//! The real publication/recovery path, including deterministic stops after I/O.
use super::{SNIFFER, Scratch, commit, dir_stat, file_stat, hash};
use crate::log::{ChangeSet, Error, Family, Published, Record, Writer};
use crate::publication::{FAIL, Point, VISITED};
use crate::{Catalog, Content, ContentState, Kind, OpenError, Section, Transaction};
use std::fs;

fn fixture(name: &str) -> Scratch {
    let scratch = Scratch::new(name);
    commit(&scratch.path, |txn| {
        let mut b = txn.batch();
        let root = b.root(b"/root", dir_stat(1));
        b.file(root, b"file", file_stat(2), Content::Hashed(hash(1)));
        txn.add(b);
    });
    scratch
}
fn changes(p: &Published) -> ChangeSet {
    ChangeSet {
        counters: p.counters(),
        counts: p.counts(),
        records: vec![
            Record::LifePut {
                id: 1,
                kind: Kind::File,
                flags: 0,
                names: 1,
            },
            Record::NamePut {
                id: 0,
                parent: 0,
                child: 1,
                name: b"new".to_vec(),
            },
            Record::InodePut {
                id: 1,
                kind: Kind::File,
                state: ContentState::Hashed,
                doc: Some(0),
                stat: file_stat(2),
            },
            Record::LinkPut {
                id: 1,
                target: b"target".to_vec(),
            },
            Record::DocPut {
                id: 0,
                references: 1,
                hash: hash(2),
            },
        ],
    }
}
fn publish(s: &Scratch) -> (crate::Generation, ChangeSet) {
    let p = Published::open(&s.path).unwrap().unwrap();
    let changes = changes(&p);
    let mut w = Writer::open(&s.path).unwrap();
    let g = w.commit(p.generation(), &changes).unwrap();
    (g, changes)
}
fn log_path(s: &Scratch) -> std::path::PathBuf {
    s.path.join("changes.0")
}

#[test]
fn framing_open_is_header_only_and_families_load_once() {
    let s = fixture("log-lazy");
    let (g, changes) = publish(&s);
    let p = Published::open(&s.path).unwrap().unwrap();
    assert_eq!(p.generation(), g);
    assert_eq!(p.checkpoint().bytes_read(), crate::format::TABLE_END as u64);
    assert_eq!(p.log().bytes_read(), 64 + 64 + 4 * 48 + 32);
    for f in [Family::Namespace, Family::Inodes, Family::Aux, Family::Docs] {
        p.log().load(f).unwrap();
        let bytes = p.log().bytes_read();
        p.log().load(f).unwrap();
        assert_eq!(p.log().bytes_read(), bytes);
        assert_eq!(
            p.log()
                .records(f)
                .map(|(_, r)| r.clone())
                .collect::<Vec<_>>(),
            changes
                .records
                .iter()
                .filter(|r| r.family() == f)
                .cloned()
                .collect::<Vec<_>>()
        );
    }
    assert_eq!(
        p.log().bytes_read(),
        fs::metadata(log_path(&s)).unwrap().len()
    );
    assert!(matches!(
        Catalog::open(&s.path),
        Err(OpenError::OverlayRequired(_))
    ));
}

#[test]
fn every_published_append_truncation_is_refused_and_every_unpublished_suffix_is_discarded() {
    let s = fixture("log-truncations");
    let old = fs::read(s.path.join("current")).unwrap();
    publish(&s);
    let current = fs::read(s.path.join("current")).unwrap();
    let bytes = fs::read(log_path(&s)).unwrap();
    for len in 0..bytes.len() {
        fs::write(log_path(&s), &bytes[..len]).unwrap();
        assert!(Published::open(&s.path).is_err(), "published length {len}");
    }
    fs::write(s.path.join("current"), &old).unwrap();
    for len in 64..=bytes.len() {
        fs::write(log_path(&s), &bytes[..len]).unwrap();
        let p = Published::open(&s.path).unwrap().unwrap();
        assert_eq!(p.log().transaction_count(), 0);
        drop(Writer::open(&s.path).unwrap());
        assert_eq!(
            fs::metadata(log_path(&s)).unwrap().len(),
            64,
            "unpublished length {len}"
        );
    }
    fs::write(log_path(&s), bytes).unwrap();
    fs::write(s.path.join("current"), current).unwrap();
    Published::open(&s.path)
        .unwrap()
        .unwrap()
        .log()
        .load_all()
        .unwrap();
}

#[test]
fn every_log_byte_flip_is_checked_by_framing_or_its_first_family_load() {
    let s = fixture("log-flips");
    publish(&s);
    let path = log_path(&s);
    let bytes = fs::read(&path).unwrap();
    for at in 0..bytes.len() {
        for bit in 0..8 {
            let mut bad = bytes.clone();
            bad[at] ^= 1 << bit;
            fs::write(&path, &bad).unwrap();
            if let Ok(Some(p)) = Published::open(&s.path) {
                assert!(p.log().load_all().is_err(), "byte {at} bit {bit}");
                assert!(
                    matches!(Writer::open(&s.path), Err(Error::Previous(_))),
                    "recovery byte {at} bit {bit}"
                );
            }
        }
    }
    fs::write(path, bytes).unwrap();
}

#[test]
fn unloaded_corruption_and_later_append_do_not_change_pinned_reads() {
    let s = fixture("log-pinned");
    let (g, c) = publish(&s);
    let old = Published::open(&s.path).unwrap().unwrap();
    let mut writer = Writer::open(&s.path).unwrap();
    writer.commit(g, &c).unwrap();
    drop(writer);
    assert_eq!(old.log().transaction_count(), 1);
    old.log().load_all().unwrap();
    let mut bytes = fs::read(log_path(&s)).unwrap();
    let inode_descriptor = 64 + 64 + 48;
    let offset = crate::format::u64_at(&bytes, inode_descriptor + 8) as usize + 64;
    bytes[offset] ^= 1;
    fs::write(log_path(&s), bytes).unwrap();
    let new = Published::open(&s.path).unwrap().unwrap();
    new.log().load(Family::Namespace).unwrap();
    new.checkpoint().load(&[Section::Names]).unwrap();
    assert!(new.log().load(Family::Inodes).is_err());
    assert!(Writer::open(&s.path).is_err());
}

#[test]
fn normal_commits_have_three_barriers_and_empty_commits_do_nothing() {
    let s = fixture("log-barriers");
    let p = Published::open(&s.path).unwrap().unwrap();
    let mut w = Writer::open(&s.path).unwrap();
    VISITED.with_borrow_mut(Vec::clear);
    let empty = ChangeSet {
        records: vec![],
        counters: p.counters(),
        counts: p.counts(),
    };
    let current = fs::read(s.path.join("current")).unwrap();
    let log = fs::read(log_path(&s)).unwrap();
    assert_eq!(w.commit(p.generation(), &empty).unwrap(), p.generation());
    assert!(VISITED.with_borrow(|v| v.is_empty()));
    assert_eq!(fs::read(s.path.join("current")).unwrap(), current);
    assert_eq!(fs::read(log_path(&s)).unwrap(), log);
    w.commit(p.generation(), &changes(&p)).unwrap();
    assert_eq!(
        VISITED.with_borrow(Clone::clone),
        [
            Point::LogWrite,
            Point::LogSync,
            Point::ManifestSync,
            Point::ManifestRename,
            Point::DirectorySync
        ]
    );
}

#[test]
fn every_append_sync_and_rename_stop_recovers_the_manifest_prefix() {
    for point in [
        Point::LogWrite,
        Point::LogSync,
        Point::ManifestSync,
        Point::ManifestRename,
        Point::DirectorySync,
    ] {
        let s = fixture(&format!("log-stop-{point:?}"));
        let p = Published::open(&s.path).unwrap().unwrap();
        let c = changes(&p);
        let mut w = Writer::open(&s.path).unwrap();
        FAIL.set(Some(point));
        let result = w.commit(p.generation(), &c);
        FAIL.set(None);
        let error = result.unwrap_err();
        let visible = matches!(point, Point::ManifestRename | Point::DirectorySync);
        assert_eq!(error.published(), visible);
        assert!(matches!(w.commit(p.generation(), &c), Err(Error::Poisoned)));
        drop(w);
        let reader = Published::open(&s.path).unwrap().unwrap();
        assert_eq!(
            reader.generation().sequence,
            p.generation().sequence + u64::from(visible)
        );
        reader.log().load_all().unwrap();
        let mut recovered = Writer::open(&s.path).unwrap();
        assert_eq!(
            fs::metadata(log_path(&s)).unwrap().len(),
            reader.log().committed_end()
        );
        recovered.commit(reader.generation(), &c).unwrap();
    }
}

#[test]
fn every_checkpoint_sync_and_rename_stop_keeps_a_readable_pair() {
    for point in [
        Point::SnapshotSync,
        Point::SnapshotRename,
        Point::HeaderSync,
        Point::HeaderRename,
        Point::PairSync,
        Point::ManifestSync,
        Point::ManifestRename,
        Point::DirectorySync,
    ] {
        let s = fixture(&format!("checkpoint-stop-{point:?}"));
        let old = Published::open(&s.path).unwrap().unwrap();
        let mut txn = Transaction::begin(&s.path, SNIFFER).unwrap();
        let mut b = txn.batch();
        b.root(b"/new", dir_stat(3));
        txn.add(b);
        FAIL.set(Some(point));
        let result = txn.commit();
        FAIL.set(None);
        let error = result.err().unwrap();
        let visible = matches!(point, Point::ManifestRename | Point::DirectorySync);
        assert_eq!(error.published(), visible);
        let reader = Published::open(&s.path).unwrap().unwrap();
        assert_eq!(
            reader.generation().checkpoint,
            old.generation().checkpoint + u64::from(visible)
        );
        reader.checkpoint().load_all().unwrap();
        drop(Writer::open(&s.path).unwrap());
        old.checkpoint().load_all().unwrap();
        old.log().load_all().unwrap();
    }
}

#[test]
fn lock_and_stale_generation_are_checked_before_bad_ids() {
    let s = fixture("log-stale");
    let p = Published::open(&s.path).unwrap().unwrap();
    let mut w = Writer::open(&s.path).unwrap();
    assert!(matches!(Writer::open(&s.path), Err(Error::Locked)));
    assert!(matches!(
        Transaction::begin(&s.path, SNIFFER),
        Err(crate::BeginError::Locked)
    ));
    let mut c = changes(&p);
    c.records = vec![Record::InodeDelete { id: u32::MAX }];
    for g in [
        crate::Generation {
            checkpoint: 1,
            ..p.generation()
        },
        crate::Generation {
            incarnation: [9; 16],
            ..p.generation()
        },
        crate::Generation {
            sequence: 1,
            ..p.generation()
        },
    ] {
        assert!(matches!(w.commit(g, &c), Err(Error::Stale(_))));
    }
    assert!(matches!(
        w.commit(p.generation(), &c),
        Err(Error::Invalid(_))
    ));
}
