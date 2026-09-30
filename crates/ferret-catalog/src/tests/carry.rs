//! Hash carry-over (D26), `DocId` reuse, dead documents (D36 B) and the
//! sniffer version (D37).

use super::{SNIFFER, Scratch, at, commit, dir_stat, file_stat, hash};
use crate::{Content, ContentState, DocId, Stat, Transaction};

/// One root `/c` holding `(name, stat, content)` files.
fn generation(dir: &std::path::Path, files: &[(&str, Stat, Content)]) -> crate::Catalog {
    commit(dir, |txn| {
        let mut b = txn.batch();
        let root = b.root(b"/c", dir_stat(1));
        for (name, stat, content) in files {
            b.file(root, name.as_bytes(), *stat, *content);
        }
        txn.add(b);
    })
}

#[test]
fn carry_needs_the_whole_key() {
    let scratch = Scratch::new("carry-key");
    let old = [
        ("hashed", file_stat(10), Content::Hashed(hash(1))),
        ("binary", file_stat(11), Content::Binary),
        ("big", file_stat(12), Content::Unindexed),
        ("racy", file_stat(13), Content::Fault),
    ];
    generation(&scratch.path, &old);
    let txn = Transaction::begin(&scratch.path, SNIFFER).unwrap();

    assert_eq!(txn.carry(&file_stat(10)), Some(Content::Hashed(hash(1))));
    assert_eq!(
        txn.carry(&file_stat(11)),
        Some(Content::Binary),
        "an unchanged binary is not re-read"
    );
    assert_eq!(txn.carry(&file_stat(12)), Some(Content::Unindexed));
    assert_eq!(txn.carry(&file_stat(13)), None, "a fault is retried");
    assert_eq!(txn.carry(&file_stat(14)), None, "a new inode");

    // The discriminating cases: same (dev, ino), one field of the key moved.
    let moved = [
        Stat {
            mtime_nsec: 7,
            ..file_stat(10)
        },
        Stat {
            mtime_sec: 1,
            ..file_stat(10)
        },
        Stat {
            ctime_nsec: 7,
            ..file_stat(10)
        },
        Stat {
            size: 1,
            ..file_stat(10)
        },
        Stat {
            dev: 8,
            ..file_stat(10)
        },
    ];
    for stat in moved {
        assert_eq!(txn.carry(&stat), None, "{stat:?}");
    }
    // Fields outside the key do not block it: their change moves ctime.
    assert_eq!(
        txn.carry(&Stat {
            mode: 0o100_600,
            ..file_stat(10)
        }),
        Some(Content::Hashed(hash(1)))
    );
}

#[test]
fn a_directory_is_never_carried() {
    let scratch = Scratch::new("carry-dir");
    generation(&scratch.path, &[]);
    let txn = Transaction::begin(&scratch.path, SNIFFER).unwrap();
    assert_eq!(txn.carry(&dir_stat(1)), None);
}

#[test]
fn a_sniffer_change_carries_nothing() {
    let scratch = Scratch::new("carry-sniffer");
    generation(
        &scratch.path,
        &[("hashed", file_stat(10), Content::Hashed(hash(1)))],
    );
    let txn = Transaction::begin(&scratch.path, SNIFFER + 1).unwrap();
    assert_eq!(txn.carry(&file_stat(10)), None);
    let catalog = {
        let mut txn = txn;
        let mut b = txn.batch();
        b.root(b"/c", dir_stat(1));
        txn.add(b);
        txn.commit().unwrap()
    };
    assert_eq!(catalog.sniffer_version(), SNIFFER + 1);
}

#[test]
fn doc_ids_carry_and_known_content_reuses_its_id() {
    let scratch = Scratch::new("doc-ids");
    let first = generation(
        &scratch.path,
        &[
            ("a", file_stat(10), Content::Hashed(hash(1))),
            ("b", file_stat(11), Content::Hashed(hash(2))),
        ],
    );
    let doc = |c: &crate::Catalog, path| c.inode(at(c, path)).doc.unwrap();
    let (a, b) = (doc(&first, "/c/a"), doc(&first, "/c/b"));
    assert_eq!(first.next_doc(), DocId(2));

    // `a` is unchanged, so its hash is carried and its DocId with it; `b` is
    // edited to new content; `c` is a new inode holding `a`'s content; `d`
    // and `e` are two new inodes sharing one new content.
    let edited = Stat {
        mtime_sec: 1_800_000_000,
        ..file_stat(11)
    };
    let mut txn = Transaction::begin(&scratch.path, SNIFFER).unwrap();
    let carried = txn.carry(&file_stat(10)).unwrap();
    assert_eq!(txn.carry(&edited), None);
    let mut batch = txn.batch();
    let root = batch.root(b"/c", dir_stat(1));
    batch.file(root, b"a", file_stat(10), carried);
    batch.file(root, b"b", edited, Content::Hashed(hash(3)));
    batch.file(root, b"c", file_stat(12), Content::Hashed(hash(1)));
    batch.file(root, b"d", file_stat(13), Content::Hashed(hash(4)));
    batch.file(root, b"e", file_stat(14), Content::Hashed(hash(4)));
    txn.add(batch);
    let second = txn.commit().unwrap();

    assert_eq!(doc(&second, "/c/a"), a);
    assert_eq!(doc(&second, "/c/c"), a, "known content keeps its DocId");
    assert_eq!(
        doc(&second, "/c/b"),
        DocId(2),
        "new content takes the next id"
    );
    assert_eq!(doc(&second, "/c/d"), DocId(3));
    assert_eq!(
        doc(&second, "/c/e"),
        DocId(3),
        "equal new content, one DocId"
    );
    assert_eq!(second.doc_hash(b), None, "b's old content is dead");
    assert_eq!(second.next_doc(), DocId(4));
}

#[test]
fn dead_docs_are_dropped_and_the_counter_never_goes_back() {
    let scratch = Scratch::new("dead-docs");
    let files = [
        ("x", file_stat(10), Content::Hashed(hash(1))),
        ("y", file_stat(11), Content::Hashed(hash(2))),
        ("z", file_stat(12), Content::Hashed(hash(3))),
    ];
    let first = generation(&scratch.path, &files);
    assert_eq!(
        first.docs().map(|(id, _)| id.0).collect::<Vec<_>>(),
        [0, 1, 2]
    );

    // Only x survives: y and z's rows go, and the counter stays at 3.
    let second = generation(&scratch.path, &files[..1]);
    assert_eq!(second.docs().collect::<Vec<_>>(), [(DocId(0), hash(1))]);
    assert_eq!(second.next_doc(), DocId(3));

    // y's content comes back (a revert): a new id, not its old one (D36 B).
    let third = generation(&scratch.path, &files[..2]);
    assert_eq!(third.inode(at(&third, "/c/y")).doc, Some(DocId(3)));
    assert_eq!(third.next_doc(), DocId(4));

    // Emptying the catalog does not reset the counter either.
    let empty = generation(&scratch.path, &[]);
    assert_eq!((empty.doc_count(), empty.next_doc()), (0, DocId(4)));
    let state = third.inode(at(&third, "/c/x")).state;
    assert_eq!(state, ContentState::Hashed);
}

/// The docs' id column's width: 0 when the ids have no holes.
fn doc_id_width(catalog: &crate::Catalog) -> u32 {
    let widths: Vec<u32> = catalog
        .column_widths()
        .filter(|&(section, _, _)| section == crate::Section::Docs)
        .map(|(_, width, _)| width)
        .collect();
    assert_eq!(widths.len(), 1);
    widths[0]
}

#[test]
fn doc_ids_with_and_without_holes_read_back_by_id() {
    // Ids 0, 1, 2 have no holes; with x dead, 1 and 2 have none either but
    // start above 0; then x's content returns under id 3, leaving 1, 2, 3.
    // With z dead, 1 and 3 leave a hole. Each generation must read every
    // live id back, dead and unassigned ids as none, both through the dense
    // path (width 0, a row by subtraction) and the sparse one (a search).
    let scratch = Scratch::new("doc-id-holes");
    let file = |name, ino, n| (name, file_stat(ino), Content::Hashed(hash(n)));
    let (x, y, z) = (file("x", 10, 1), file("y", 11, 2), file("z", 12, 3));
    let steps: [(&[_], &[u32], u32); 4] = [
        (&[x, y, z], &[0, 1, 2], 0),
        (&[y, z], &[1, 2], 0),
        (&[y, z, file("w", 13, 1)], &[1, 2, 3], 0),
        (&[y, file("w", 13, 1)], &[1, 3], 1),
    ];
    for (files, ids, width) in steps {
        let catalog = generation(&scratch.path, files);
        let live: Vec<u32> = catalog.docs().map(|(id, _)| id.0).collect();
        assert_eq!(live, ids);
        assert_eq!(doc_id_width(&catalog), width, "ids {ids:?}");
        for id in 0..catalog.next_doc().0 + 2 {
            let expect = catalog.docs().find(|&(d, _)| d.0 == id).map(|(_, h)| h);
            assert_eq!(catalog.doc_hash(DocId(id)), expect, "ids {ids:?}, id {id}");
        }
    }

    // Ids 0 and 2, then 0, 2 and 5: holes of one and three, sparse.
    let scratch = Scratch::new("doc-id-sparse");
    let (a, b, c) = (file("a", 20, 7), file("b", 21, 8), file("c", 22, 9));
    generation(&scratch.path, &[a, b, c]);
    let sparse = generation(&scratch.path, &[a, c]);
    assert_eq!(
        sparse.docs().map(|(id, _)| id.0).collect::<Vec<_>>(),
        [0, 2]
    );
    assert!(doc_id_width(&sparse) > 0);
    let (d, e) = (file("d", 23, 10), file("e", 24, 11));
    generation(&scratch.path, &[a, c, d, e]);
    let sparse = generation(&scratch.path, &[a, c, e]);
    assert_eq!(
        sparse.docs().map(|(id, _)| id.0).collect::<Vec<_>>(),
        [0, 2, 4]
    );
    assert!(doc_id_width(&sparse) > 0);
    for id in 0..6 {
        let expect = [(0, 7), (2, 9), (4, 11)]
            .into_iter()
            .find(|&(d, _)| d == id)
            .map(|(_, n)| hash(n));
        assert_eq!(sparse.doc_hash(DocId(id)), expect, "id {id}");
    }
}
