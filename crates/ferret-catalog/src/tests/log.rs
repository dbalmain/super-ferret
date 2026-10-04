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
            Record::WorkTreePut {
                id: 0,
                kind: crate::WorkTreeKind::Main,
                common_id: (7, 8),
                path: b"/git".to_vec(),
            },
            Record::DocPut {
                id: 0,
                references: 1,
                hash: hash(1),
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
    s.path.join(format!(
        "changes.{}",
        crate::read::read_manifest(&s.path)
            .unwrap()
            .unwrap()
            .generation
            .checkpoint
    ))
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
    let effective = Catalog::open(&s.path).unwrap().unwrap();
    assert_eq!(effective.generation(), g);
    assert_eq!(
        effective.bytes_read(),
        crate::format::TABLE_END as u64 + 64 + 64 + 4 * 48 + 32
    );
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
        Point::PartialLogWrite,
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

#[test]
fn pinned_log_payloads_and_snapshot_sections_survive_unlink() {
    let s = fixture("log-unlink");
    let (g, c) = publish(&s);
    let pinned = Published::open(&s.path).unwrap().unwrap();
    // Retirement unlinks paths, never bytes. Exercise both still-unloaded
    // descriptors after unlink; there is no overlay compactor until M7.
    fs::remove_file(log_path(&s)).unwrap();
    fs::remove_file(s.path.join("snapshot.0")).unwrap();
    pinned.log().load_all().unwrap();
    pinned.checkpoint().load_all().unwrap();
    assert_eq!(pinned.generation(), g);
    assert_eq!(
        pinned
            .log()
            .records(Family::Inodes)
            .map(|(_, r)| r.clone())
            .collect::<Vec<_>>(),
        c.records
            .into_iter()
            .filter(|r| r.family() == Family::Inodes)
            .collect::<Vec<_>>()
    );
}

#[test]
fn recovery_sync_stop_releases_lock_and_never_adopts_a_complete_tail() {
    let s = fixture("log-recovery-stop");
    let p = Published::open(&s.path).unwrap().unwrap();
    let mut w = Writer::open(&s.path).unwrap();
    FAIL.set(Some(Point::LogSync));
    assert!(w.commit(p.generation(), &changes(&p)).is_err());
    FAIL.set(None);
    drop(w);
    FAIL.set(Some(Point::RecoverySync));
    assert!(Writer::open(&s.path).is_err());
    FAIL.set(None);
    let reopened = Writer::open(&s.path).unwrap();
    assert_eq!(reopened.generation(), p.generation());
    assert_eq!(fs::metadata(log_path(&s)).unwrap().len(), 64);
}

#[test]
fn orphan_cleanup_requires_a_valid_pair_and_checkpoint_begin_also_recovers() {
    let s = fixture("log-orphans");
    for name in [
        "snapshot.77",
        "changes.77",
        "current.tmp",
        "changes.tmp",
        "catalog.tmp",
    ] {
        fs::write(s.path.join(name), b"abandoned").unwrap();
    }
    let manifest = fs::read(s.path.join("current")).unwrap();
    fs::write(s.path.join("current"), b"damaged").unwrap();
    assert!(Writer::open(&s.path).is_err());
    assert!(Transaction::begin(&s.path, SNIFFER).is_err());
    assert!(s.path.join("catalog.tmp").exists());
    assert!(s.path.join("snapshot.77").exists());
    fs::write(s.path.join("current"), manifest).unwrap();
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(log_path(&s))
        .unwrap();
    std::io::Write::write_all(&mut file, b"unpublished").unwrap();
    drop(Transaction::begin(&s.path, SNIFFER).unwrap());
    assert_eq!(fs::metadata(log_path(&s)).unwrap().len(), 64);
    for name in [
        "snapshot.77",
        "changes.77",
        "current.tmp",
        "changes.tmp",
        "catalog.tmp",
    ] {
        assert!(!s.path.join(name).exists());
    }
}

#[test]
fn opening_retries_only_when_unlink_races_a_changed_manifest() {
    let s = fixture("log-open-race");
    let before = Published::open(&s.path).unwrap().unwrap().generation();
    crate::log::BEFORE_PAIR.with_borrow_mut(|hook| {
        *hook = Some(Box::new(|dir| {
            commit(dir, |txn| {
                let mut b = txn.batch();
                b.root(b"/next", dir_stat(3));
                txn.add(b);
            });
        }))
    });
    let after = Published::open(&s.path).unwrap().unwrap();
    assert_eq!(after.generation().checkpoint, before.checkpoint + 1);
    fs::remove_file(
        s.path
            .join(format!("changes.{}", after.generation().checkpoint)),
    )
    .unwrap();
    assert!(
        matches!(Published::open(&s.path),Err(OpenError::Io(e)) if e.kind()==std::io::ErrorKind::NotFound)
    );
}

#[test]
fn all_record_shapes_round_trip_and_invalid_records_publish_nothing() {
    let s = fixture("log-records");
    commit(&s.path, |txn| {
        let mut b = txn.batch();
        let root = b.root(b"/root", dir_stat(1));
        b.file(root, b"file", file_stat(2), Content::Hashed(hash(1)));
        let mut st = file_stat(3);
        st.mode = 0o120777;
        b.symlink(root, b"symlink", st, b"target");
        txn.add(b);
    });
    let p = Published::open(&s.path).unwrap().unwrap();
    let mut c = changes(&p);
    c.records.extend([
        Record::InodeDelete { id: 1 },
        Record::NameDelete { id: 0 },
        Record::DirPut {
            id: 0,
            name: None,
            entries: Some(1),
            flags: 5,
            retained_at: None,
        },
        Record::RootPut {
            id: 0,
            path: b"/root".to_vec(),
        },
        Record::RootDelete { id: 0 },
        Record::PolicyPut { hash: hash(4) },
        Record::LinkDelete { id: 1 },
        Record::WorkTreePut {
            id: 0,
            kind: crate::WorkTreeKind::Linked,
            common_id: (3, 4),
            path: b"/git".to_vec(),
        },
        Record::WorkTreeDelete { id: 0 },
        Record::DocDelete { id: 0 },
        Record::LifePut {
            id: 0,
            kind: Kind::Dir,
            flags: 0,
            names: 0,
        },
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
        Record::RootPut {
            id: 0,
            path: b"/root".to_vec(),
        },
        Record::DocPut {
            id: 0,
            references: 1,
            hash: hash(1),
        },
        Record::LinkPut {
            id: 2,
            target: b"new target".to_vec(),
        },
    ]);
    let mut w = Writer::open(&s.path).unwrap();
    let generation = w.commit(p.generation(), &c).unwrap();
    let valid = Published::open(&s.path).unwrap().unwrap();
    valid.log().load_all().unwrap();
    for family in [Family::Namespace, Family::Inodes, Family::Aux, Family::Docs] {
        assert_eq!(
            valid
                .log()
                .records(family)
                .map(|(_, r)| r.clone())
                .collect::<Vec<_>>(),
            c.records
                .iter()
                .filter(|r| r.family() == family)
                .cloned()
                .collect::<Vec<_>>()
        );
    }
    let prior = fs::read(s.path.join("current")).unwrap();
    let end = fs::metadata(log_path(&s)).unwrap().len();
    for record in [
        Record::NamePut {
            id: 0,
            parent: 0,
            child: 1,
            name: b"..".to_vec(),
        },
        Record::NamePut {
            id: 0,
            parent: 0,
            child: 1,
            name: b"slash/name".to_vec(),
        },
        Record::RootPut {
            id: 0,
            path: b"relative".to_vec(),
        },
        Record::LifePut {
            id: 1,
            kind: Kind::File,
            flags: 1,
            names: 1,
        },
        Record::InodePut {
            id: 1,
            kind: Kind::File,
            state: ContentState::Hashed,
            doc: None,
            stat: file_stat(2),
        },
        Record::DirPut {
            id: 0,
            name: None,
            entries: None,
            flags: 8,
            retained_at: Some(generation.sequence + 2),
        },
        Record::DocPut {
            id: 0,
            references: 0,
            hash: hash(4),
        },
    ] {
        c.records = vec![record];
        assert!(matches!(w.commit(generation, &c), Err(Error::Invalid(_))));
        assert_eq!(fs::read(s.path.join("current")).unwrap(), prior);
        assert_eq!(fs::metadata(log_path(&s)).unwrap().len(), end);
    }
}

fn reseal_framing(bytes: &mut [u8]) {
    let framing = 64 + 64 + 4 * 48;
    let footer = bytes.len() - 16;
    let digest = crate::checkpoint_checksum(&bytes[64..framing]);
    bytes[footer..].copy_from_slice(&digest);
}

#[test]
fn signed_bad_framing_and_payload_invariants_are_not_hidden_by_checksums() {
    let s = fixture("log-signed-invalid");
    publish(&s);
    let bytes = fs::read(log_path(&s)).unwrap();
    let tx = 64;
    let d = tx + 64;
    for (at, value) in [
        (tx + 8, 99u32.to_le_bytes().to_vec()),
        (tx + 12, 5u32.to_le_bytes().to_vec()),
        (tx + 16, u64::MAX.to_le_bytes().to_vec()),
        (tx + 24, 3u64.to_le_bytes().to_vec()),
        (tx + 32, 1u64.to_le_bytes().to_vec()),
        (tx + 40, u32::MAX.to_le_bytes().to_vec()),
        (tx + 44, u32::MAX.to_le_bytes().to_vec()),
        (tx + 52, 1u32.to_le_bytes().to_vec()),
        (tx + 56, 1u64.to_le_bytes().to_vec()),
        (d, 4u16.to_le_bytes().to_vec()),
        (d + 2, 1u16.to_le_bytes().to_vec()),
        (d + 4, 0u32.to_le_bytes().to_vec()),
        (d + 8, 0u64.to_le_bytes().to_vec()),
        (d + 16, u64::MAX.to_le_bytes().to_vec()),
        (d + 40, 1u64.to_le_bytes().to_vec()),
        (d + 48, 0u16.to_le_bytes().to_vec()),
    ] {
        let mut bad = bytes.clone();
        bad[at..at + value.len()].copy_from_slice(&value);
        reseal_framing(&mut bad);
        fs::write(log_path(&s), bad).unwrap();
        assert!(Published::open(&s.path).is_err(), "signed framing at {at}");
    }
    let offset = crate::format::u64_at(&bytes, d + 8) as usize + tx;
    let length = crate::format::u64_at(&bytes, d + 16) as usize;
    for (at, value) in [
        (offset, 99u8),
        (offset + 1, 1),
        (offset + 2, 1),
        (offset + 12, 7),
        (offset + 13, 1),
        (offset + 14, 1),
        (offset + 20, 1),
    ] {
        let mut bad = bytes.clone();
        bad[at] = value;
        let digest = crate::checkpoint_checksum(&bad[offset..offset + length]);
        bad[d + 24..d + 40].copy_from_slice(&digest);
        reseal_framing(&mut bad);
        fs::write(log_path(&s), bad).unwrap();
        let p = Published::open(&s.path).unwrap().unwrap();
        p.log().load(Family::Docs).unwrap();
        assert!(
            p.log().load(Family::Namespace).is_err(),
            "signed record at {at}"
        );
        assert!(Writer::open(&s.path).is_err());
    }
}

#[test]
fn allocation_limits_and_sequence_exhaustion_drive_the_real_writer() {
    let s = fixture("log-limits");
    let p = Published::open(&s.path).unwrap().unwrap();
    let mut w = Writer::open(&s.path).unwrap();
    let c = changes(&p);
    for counters in [
        [u32::MAX, 1, 1],
        [2, u32::MAX, 1],
        [1, 1, 1],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let mut bad = c.clone();
        bad.counters = counters;
        assert!(matches!(
            w.commit(p.generation(), &bad),
            Err(Error::Invalid(_))
        ));
    }
    for counts in [[3, 1, 1, 1], [2, 2, 1, 1], [2, 1, 3, 1], [2, 1, 1, 2]] {
        let mut bad = c.clone();
        bad.counts = counts;
        assert!(matches!(
            w.commit(p.generation(), &bad),
            Err(Error::Invalid(_))
        ));
    }
    // Set the persisted generation to the final usable sequence, resign the
    // actual framing and reopen through the real snapshot/log decoders.
    drop(w);
    let mut manifest = crate::read::read_manifest(&s.path).unwrap().unwrap();
    manifest.generation.sequence = u64::MAX - 1;
    manifest.checkpoint_sequence = u64::MAX - 1;
    let snapshot = s.path.join("snapshot.0");
    let mut bytes = fs::read(&snapshot).unwrap();
    bytes[64..72].copy_from_slice(&(u64::MAX - 1).to_le_bytes());
    let end = crate::format::TABLE_END;
    let digest = crate::checkpoint_checksum(&bytes[..end - 16]);
    bytes[end - 16..end].copy_from_slice(&digest);
    fs::write(snapshot, bytes).unwrap();
    fs::write(log_path(&s), crate::log::header(manifest.generation)).unwrap();
    fs::write(s.path.join("current"), manifest.encode()).unwrap();
    let mut w = Writer::open(&s.path).unwrap();
    assert!(matches!(
        w.commit(w.generation(), &c),
        Err(Error::Invalid(_))
    ));
    assert_eq!(fs::metadata(log_path(&s)).unwrap().len(), 64);
}

#[test]
fn simultaneous_writer_opens_have_one_winner_and_readers_do_not_lock() {
    let s = fixture("log-lock-race");
    let entered = std::sync::Arc::new(std::sync::Barrier::new(3));
    let release = std::sync::Arc::new(std::sync::Barrier::new(3));
    let (tx, rx) = std::sync::mpsc::channel();
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let path = s.path.clone();
            let entered = entered.clone();
            let release = release.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                entered.wait();
                let writer = Writer::open(&path);
                tx.send(writer.is_ok()).unwrap();
                release.wait();
                drop(writer);
            })
        })
        .collect();
    entered.wait();
    let mut wins = vec![rx.recv().unwrap(), rx.recv().unwrap()];
    wins.sort();
    assert_eq!(wins, [false, true]);
    Published::open(&s.path)
        .unwrap()
        .unwrap()
        .checkpoint()
        .load(&[Section::Names])
        .unwrap();
    release.wait();
    for thread in threads {
        thread.join().unwrap();
    }
    drop(Writer::open(&s.path).unwrap());
}

#[test]
fn m1_empty_prefix_upgrade_stops_never_publish_an_unchecked_header() {
    for point in [
        Point::HeaderSync,
        Point::PairSync,
        Point::ManifestSync,
        Point::ManifestRename,
        Point::DirectorySync,
    ] {
        let s = fixture(&format!("log-m1-upgrade-{point:?}"));
        let mut manifest = crate::read::read_manifest(&s.path).unwrap().unwrap();
        manifest.log_end = 0;
        fs::write(s.path.join("current"), manifest.encode()).unwrap();
        fs::remove_file(log_path(&s)).unwrap();
        let old = Published::open(&s.path).unwrap().unwrap();
        FAIL.set(Some(point));
        assert!(Writer::open(&s.path).is_err());
        FAIL.set(None);
        let reader = Published::open(&s.path).unwrap().unwrap();
        assert_eq!(reader.generation(), old.generation());
        assert_eq!(reader.log().transaction_count(), 0);
        let mut w = Writer::open(&s.path).unwrap();
        w.commit(w.generation(), &changes(&reader)).unwrap();
    }
}

#[test]
fn an_early_recovery_error_unlocks_even_with_a_copied_descriptor() {
    let s = fixture("log-early-unlock");
    let manifest = fs::read(s.path.join("current")).unwrap();
    fs::write(s.path.join("current"), b"bad manifest").unwrap();
    let lock = crate::lock::Lock::open(&s.path).unwrap_or_else(|_| panic!("first lock"));
    let child = lock.copy().unwrap();
    drop(lock);
    assert!(Writer::open(&s.path).is_err());
    assert!(Transaction::begin(&s.path, SNIFFER).is_err());
    fs::write(s.path.join("current"), manifest).unwrap();
    drop(Writer::open(&s.path).unwrap());
    drop(child);
}

#[test]
fn recovery_makes_an_undurable_selection_durable_before_retiring_its_old_pair() {
    let s = fixture("log-retirement-sync");
    let old = Published::open(&s.path).unwrap().unwrap();
    let mut txn = Transaction::begin(&s.path, SNIFFER).unwrap();
    let mut b = txn.batch();
    b.root(b"/next", dir_stat(3));
    txn.add(b);
    FAIL.set(Some(Point::ManifestRename));
    assert!(txn.commit().err().unwrap().published());
    FAIL.set(None);
    assert!(s.path.join("snapshot.0").exists());
    assert!(s.path.join("changes.0").exists());
    FAIL.set(Some(Point::RecoveryDirectorySync));
    assert!(Writer::open(&s.path).is_err());
    FAIL.set(None);
    assert!(s.path.join("snapshot.0").exists());
    assert!(s.path.join("changes.0").exists());
    drop(Writer::open(&s.path).unwrap());
    assert!(!s.path.join("snapshot.0").exists());
    assert!(!s.path.join("changes.0").exists());
    old.checkpoint().load_all().unwrap();
    old.log().load_all().unwrap();
}

#[test]
fn every_first_publication_barrier_including_each_ancestor_can_stop() {
    let baseline = Scratch::new("log-first-barriers");
    let dir = baseline.path.join("a/b");
    VISITED.with_borrow_mut(Vec::clear);
    commit(&dir, |txn| {
        let mut b = txn.batch();
        b.root(b"/root", dir_stat(1));
        txn.add(b);
    });
    let points = VISITED.with_borrow(Clone::clone);
    let rename = points
        .iter()
        .position(|&p| p == Point::ManifestRename)
        .unwrap();
    assert!(
        points[..rename]
            .iter()
            .filter(|&&p| p == Point::DirectorySync)
            .count()
            > 1,
        "nested index syncs each ancestor"
    );
    for (index, point) in points.iter().enumerate() {
        let s = Scratch::new(&format!("log-first-stop-{index}"));
        let dir = s.path.join("a/b");
        VISITED.with_borrow_mut(Vec::clear);
        crate::publication::STOP_AFTER.set(Some(index + 1));
        let result = Transaction::begin(&dir, SNIFFER)
            .map_err(|e| e.to_string())
            .and_then(|mut txn| {
                let mut b = txn.batch();
                b.root(b"/root", dir_stat(1));
                txn.add(b);
                txn.commit().map(|_| ()).map_err(|e| e.to_string())
            });
        crate::publication::STOP_AFTER.set(None);
        assert!(result.is_err(), "stop {index}");
        let p = Published::open(&dir).unwrap();
        assert_eq!(p.is_some(), index >= rename, "stop {index} {:?}", point);
        if let Some(p) = p {
            p.checkpoint().load_all().unwrap();
            p.log().load_all().unwrap();
        }
        // The real next writer succeeds even after begin's ancestor failure.
        drop(Transaction::begin(&dir, SNIFFER).unwrap());
    }
}

#[test]
fn optional_reserved_sentinels_and_impossible_reference_counts_are_rejected() {
    let s = fixture("log-option-limits");
    let p = Published::open(&s.path).unwrap().unwrap();
    let mut c = changes(&p);
    let mut w = Writer::open(&s.path).unwrap();
    for record in [
        Record::DirPut {
            id: 0,
            name: Some(u32::MAX),
            entries: None,
            flags: 0,
            retained_at: None,
        },
        Record::DirPut {
            id: 0,
            name: None,
            entries: Some(u32::MAX),
            flags: 0,
            retained_at: None,
        },
        Record::DirPut {
            id: 0,
            name: None,
            entries: None,
            flags: 0,
            retained_at: Some(u64::MAX),
        },
        Record::InodePut {
            id: 1,
            kind: Kind::File,
            state: ContentState::Unindexed,
            doc: Some(u32::MAX),
            stat: file_stat(2),
        },
        Record::LifePut {
            id: 1,
            kind: Kind::File,
            flags: 0,
            names: u32::MAX,
        },
        Record::DocPut {
            id: 0,
            references: u32::MAX,
            hash: hash(1),
        },
    ] {
        c.records = vec![record];
        assert!(matches!(
            w.commit(p.generation(), &c),
            Err(Error::Invalid(_))
        ));
    }
    assert_eq!(fs::metadata(log_path(&s)).unwrap().len(), 64);
    assert_eq!(
        Published::open(&s.path).unwrap().unwrap().generation(),
        p.generation()
    );
}

#[test]
fn recovery_syncs_an_adopted_append_manifest_without_obsolete_files_before_empty_commits() {
    // An append stopped after rename has no retired pair to trigger cleanup.
    let s = fixture("append-recovery-barrier");
    let p = Published::open(&s.path).unwrap().unwrap();
    let mut writer = Writer::open(&s.path).unwrap();
    FAIL.set(Some(Point::ManifestRename));
    assert!(
        writer
            .commit(p.generation(), &changes(&p))
            .unwrap_err()
            .published()
    );
    FAIL.set(None);
    drop(writer);
    let selected = Published::open(&s.path).unwrap().unwrap();
    assert_eq!(selected.generation().sequence, p.generation().sequence + 1);
    assert_eq!(
        fs::metadata(log_path(&s)).unwrap().len(),
        selected.log().committed_end()
    );
    assert!(!s.path.join("current.tmp").exists());
    FAIL.set(Some(Point::RecoveryDirectorySync));
    let refused = Writer::open(&s.path).is_err();
    FAIL.set(None);
    assert!(
        refused,
        "writer must establish durability independently of cleanup"
    );
    VISITED.with_borrow_mut(Vec::clear);
    let mut recovered = Writer::open(&s.path).unwrap();
    assert_eq!(
        VISITED.with_borrow(Clone::clone),
        [Point::RecoveryDirectorySync]
    );
    VISITED.with_borrow_mut(Vec::clear);
    let empty = ChangeSet {
        records: Vec::new(),
        counters: selected.counters(),
        counts: selected.counts(),
    };
    assert_eq!(
        recovered.commit(selected.generation(), &empty).unwrap(),
        selected.generation()
    );
    assert!(
        VISITED.with_borrow(|points| points.is_empty()),
        "resident empty commit has no IO barriers"
    );
}
