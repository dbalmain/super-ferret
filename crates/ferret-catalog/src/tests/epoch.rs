//! Checks identity before id interpretation and exercises published wire bytes.

use super::{Scratch, commit, dir_stat, file_stat, hash, paths, reopen, snapshot};
use crate::generation::{MANIFEST_LEN, Manifest, checksum};
use crate::{
    Catalog, Content, DecodeError, Generation, Handle, InoId, NameId, OpenError, Section,
    Transaction,
};

#[test]
fn stale_handles_fail_before_even_an_out_of_range_id_is_interpreted() {
    let scratch = Scratch::new("epoch-handles");
    let first = commit(&scratch.path, |txn| {
        let mut batch = txn.batch();
        let root = batch.root(b"/root", dir_stat(1));
        batch.file(root, b"a", file_stat(2), Content::Hashed(hash(1)));
        txn.add(batch);
    });
    let current = first.generation();
    assert_eq!(
        first.checked_name(Handle {
            generation: current,
            id: NameId(0)
        }),
        Ok(NameId(0))
    );
    for stale in [
        Generation {
            incarnation: [0; 16],
            ..current
        },
        Generation {
            checkpoint: current.checkpoint + 1,
            ..current
        },
        Generation {
            sequence: current.sequence + 1,
            ..current
        },
    ] {
        assert!(
            first
                .checked_inode(Handle {
                    generation: stale,
                    id: InoId(u32::MAX)
                })
                .is_err()
        );
        assert!(
            first
                .checked_name(Handle {
                    generation: stale,
                    id: NameId(u32::MAX)
                })
                .is_err()
        );
    }
    let mut next = Transaction::begin(&scratch.path, 1).unwrap();
    next.keep(b"/root").unwrap();
    let second = next.commit().unwrap();
    assert_eq!(second.generation().incarnation, current.incarnation);
    assert!(second.generation().checkpoint > current.checkpoint);
    assert!(
        second
            .checked_name(Handle {
                generation: current,
                id: NameId(0)
            })
            .is_err()
    );
    // Old descriptors remain readable after the retired filename is unlinked.
    first.load_all().unwrap();
    assert_eq!(first.name(NameId(0)).bytes, b"a");
}

#[test]
fn every_manifest_truncation_and_single_bit_flip_is_refused_by_open() {
    let scratch = Scratch::new("epoch-manifest");
    commit(&scratch.path, |txn| {
        let mut batch = txn.batch();
        batch.root(b"/root", dir_stat(1));
        txn.add(batch);
    });
    let path = scratch.path.join("current");
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes.len(), MANIFEST_LEN);
    assert_eq!(&bytes[..12], b"FERRETCR\x04\0\0\0");
    for len in 0..bytes.len() {
        std::fs::write(&path, &bytes[..len]).unwrap();
        assert!(Catalog::open(&scratch.path).is_err(), "truncated to {len}");
    }
    for at in 0..bytes.len() {
        for bit in 0..8 {
            let mut bad = bytes.clone();
            bad[at] ^= 1 << bit;
            std::fs::write(&path, bad).unwrap();
            assert!(
                Catalog::open(&scratch.path).is_err(),
                "byte {at}, bit {bit}"
            );
        }
    }
    std::fs::write(&path, bytes).unwrap();
    assert!(Catalog::open(&scratch.path).is_ok());
}

#[test]
fn a_signed_manifest_cannot_apply_another_incarnation_or_epoch_to_a_snapshot() {
    let scratch = Scratch::new("epoch-mismatch");
    let catalog = commit(&scratch.path, |txn| {
        let mut batch = txn.batch();
        batch.root(b"/root", dir_stat(1));
        txn.add(batch);
    });
    let snapshot_bytes = std::fs::read(snapshot(&scratch.path)).unwrap();
    for change_epoch in [false, true] {
        let mut manifest = catalog.manifest();
        if change_epoch {
            manifest.generation.checkpoint += 1;
            std::fs::write(
                scratch
                    .path
                    .join(format!("snapshot.{}", manifest.generation.checkpoint)),
                &snapshot_bytes,
            )
            .unwrap();
        } else {
            manifest.generation.incarnation[0] ^= 1;
        }
        std::fs::write(scratch.path.join("current"), manifest.encode()).unwrap();
        assert!(matches!(
            Catalog::open(&scratch.path),
            Err(OpenError::Decode(DecodeError::Corrupt(
                "checkpoint identity"
            )))
        ));
    }
}

#[test]
fn signed_reserved_values_and_exhausted_counters_are_refused() {
    let scratch = Scratch::new("epoch-limits");
    let catalog = commit(&scratch.path, |txn| {
        let mut batch = txn.batch();
        batch.root(b"/root", dir_stat(1));
        txn.add(batch);
    });
    let original = catalog.manifest().encode();
    for (at, value) in [
        (12, 1u64),
        (96, 1),
        (32, u64::MAX),
        (40, u64::MAX),
        (68, u64::from(u32::MAX - 15)),
        (72, u64::from(u32::MAX)),
    ] {
        let mut bytes = original;
        let width = if at == 68 || at == 72 || at == 12 {
            4
        } else {
            8
        };
        bytes[at..at + width].copy_from_slice(&value.to_le_bytes()[..width]);
        let digest = checksum(&bytes[..112]);
        bytes[112..].copy_from_slice(&digest);
        std::fs::write(scratch.path.join("current"), bytes).unwrap();
        assert!(Catalog::open(&scratch.path).is_err(), "reserved at {at}");
    }
    for generation in [
        Generation {
            checkpoint: u64::MAX - 1,
            ..catalog.generation()
        },
        Generation {
            sequence: u64::MAX - 1,
            ..catalog.generation()
        },
    ] {
        assert!(generation.successor().is_err());
    }
}

#[test]
fn v3_import_preserves_roots_sparse_doc_ids_and_dense_base_rows() {
    let scratch = Scratch::new("epoch-import");
    std::fs::create_dir_all(&scratch.path).unwrap();
    let legacy = include_bytes!("v3.catalog");
    assert_eq!(&legacy[8..12], &3u32.to_le_bytes());
    std::fs::write(scratch.path.join("catalog"), legacy).unwrap();
    assert!(matches!(
        Catalog::open(&scratch.path),
        Err(OpenError::Decode(DecodeError::Version(3)))
    ));
    let imported = Transaction::import_v3(&scratch.path, hash(9)).unwrap();
    assert_eq!(
        imported
            .roots()
            .map(|(_, path)| path.to_vec())
            .collect::<Vec<_>>(),
        [b"/alpha".to_vec(), b"/beta".to_vec()]
    );
    assert_eq!(
        imported
            .docs()
            .map(|(id, hash)| (id.0, hash))
            .collect::<Vec<_>>(),
        [(1, hash(3)), (2, hash(2))]
    );
    assert_eq!(imported.next_doc().0, 3);
    assert_eq!(imported.policy(), hash(9));
    assert_eq!(imported.doc_references(crate::DocId(1)), Some(1));
    assert_eq!(imported.doc_references(crate::DocId(2)), Some(2));
    assert_eq!(imported.next_inode().0, imported.inode_count());
    assert_eq!(imported.next_name().0, imported.name_count());
    let rows = paths(&imported);
    assert_eq!(rows["/alpha/sub/hard"], rows["/alpha/sub/keep"]);
    for dir in 0..imported.dir_count() {
        assert_eq!(imported.retained_at(InoId(dir)), None);
    }
    assert!(!scratch.path.join("catalog").exists());
    assert_eq!(paths(&reopen(&scratch.path)), rows);
    assert!(Transaction::import_v3(&scratch.path, hash(0)).is_err());
}

#[test]
fn coverage_policy_and_inode_reference_counts_survive_checkpoint_and_keep() {
    let scratch = Scratch::new("epoch-writer-fields");
    commit(&scratch.path, |txn| {
        txn.set_policy(hash(7));
        let mut batch = txn.batch();
        let root = batch.root(b"/root", dir_stat(1));
        batch.retained_at(root, Some(0));
        for (name, ino) in [(b"a", 2), (b"b", 2), (b"c", 3)] {
            batch.file(root, name, file_stat(ino), Content::Hashed(hash(1)));
        }
        txn.add(batch);
    });
    let mut txn = Transaction::begin(&scratch.path, 1).unwrap();
    txn.keep(b"/root").unwrap();
    let catalog = txn.commit().unwrap();
    assert_eq!(catalog.retained_at(InoId(0)), Some(0));
    assert_eq!(catalog.policy(), hash(7));
    assert_eq!(catalog.doc_references(crate::DocId(0)), Some(2));
    let lazy = Catalog::open(&scratch.path).unwrap().unwrap();
    lazy.load(&[Section::Names]).unwrap();
    assert!(!lazy.is_loaded(Section::DocRefs));
    assert!(!lazy.is_loaded(Section::RetainedAt));
    assert!(!lazy.is_loaded(Section::Policy));
    assert!(Manifest::decode(&std::fs::read(scratch.path.join("current")).unwrap()).is_ok());
}

#[test]
fn reserved_and_future_retention_sequences_fail_before_publication() {
    for sequence in [1, u64::MAX] {
        let scratch = Scratch::new(&format!("epoch-retention-{sequence}"));
        let mut txn = Transaction::begin(&scratch.path, 1).unwrap();
        let mut batch = txn.batch();
        let root = batch.root(b"/root", dir_stat(1));
        batch.retained_at(root, Some(sequence));
        txn.add(batch);
        assert!(
            matches!(txn.commit(), Err(crate::CommitError::Build(crate::BuildError::FutureRetention { retained_at, .. })) if retained_at == sequence)
        );
        assert!(Catalog::open(&scratch.path).unwrap().is_none());
    }
}
