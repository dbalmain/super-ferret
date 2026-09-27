//! Batches → commit → reopen, numbering, hard links and path resolution.

use super::{Scratch, at, commit, dir_stat, file_stat, hash, link_stat, paths};
use crate::{
    Batch, BuildError, Catalog, CommitError, Content, ContentState, DocId, InoId, Kind, NameId,
    Transaction, WorkTreeKind,
};

/// Fills `/r` across three batches, the way three walk workers would: each
/// directory's children split between batches, and parents minted in a
/// batch other than the child's. `order` permutes which batch gets which
/// share, to show numbering does not depend on it.
fn fill_tree(txn: &Transaction, order: [usize; 3]) -> Vec<Batch> {
    let mut b: Vec<Batch> = (0..3).map(|_| txn.batch()).collect();
    let [x, y, z] = order;
    let root = b[x].root(b"/r", dir_stat(1));
    let src = b[y].dir(root, b"src", dir_stat(2));
    let target = b[z].traversed_dir(root, b"target", dir_stat(3));
    let docs = b[x].dir(root, b"docs", dir_stat(4));
    b[z].file(src, b"main.rs", file_stat(10), Content::Hashed(hash(1)));
    b[x].file(src, b"lib.rs", file_stat(11), Content::Hashed(hash(2)));
    b[y].file(root, b"README", file_stat(12), Content::Hashed(hash(3)));
    b[y].file(target, b"keep.txt", file_stat(13), Content::Hashed(hash(4)));
    b[z].file(docs, b"logo.png", file_stat(14), Content::Binary);
    b[x].file(docs, b"big.iso", file_stat(15), Content::Unindexed);
    b[y].file(docs, b"racy.log", file_stat(16), Content::Fault);
    b[z].symlink(root, b"link", link_stat(17), b"src/main.rs");
    b[x].work_tree(root, WorkTreeKind::Main, b"/r/.git", (7, 99));
    b[y].work_tree(
        src,
        WorkTreeKind::Submodule,
        b"/r/.git/modules/src",
        (7, 98),
    );
    b
}

fn commit_tree(dir: &std::path::Path, order: [usize; 3]) -> Catalog {
    commit(dir, |txn| {
        for batch in fill_tree(txn, order) {
            txn.add(batch);
        }
    })
}

#[test]
fn batches_commit_and_reopen() {
    let scratch = Scratch::new("round-trip");
    let committed = commit_tree(&scratch.path, [0, 1, 2]);
    let catalog = Catalog::open(&scratch.path).unwrap().unwrap();

    let all = paths(&catalog);
    let expect = [
        "/r/README",
        "/r/docs",
        "/r/docs/big.iso",
        "/r/docs/logo.png",
        "/r/docs/racy.log",
        "/r/link",
        "/r/src",
        "/r/src/lib.rs",
        "/r/src/main.rs",
        "/r/target",
        "/r/target/keep.txt",
    ];
    assert_eq!(all.keys().map(String::as_str).collect::<Vec<_>>(), expect);
    assert_eq!(
        paths(&committed),
        all,
        "commit returns the generation it wrote"
    );

    // Directories are 0..dirs, roots first; everything else follows.
    assert_eq!(
        (
            catalog.dir_count(),
            catalog.inode_count(),
            catalog.name_count()
        ),
        (4, 12, 11)
    );
    assert_eq!(
        catalog.roots().collect::<Vec<_>>(),
        [(InoId(0), &b"/r"[..])]
    );
    for path in ["/r/docs", "/r/src", "/r/target"] {
        let dir = at(&catalog, path);
        assert!(dir.0 < 4 && catalog.kind(dir) == Kind::Dir, "{path}");
        assert_eq!(catalog.name(catalog.dir_name(dir).unwrap()).child, dir);
    }
    assert_eq!(catalog.dir_name(InoId(0)), None);
    assert!(catalog.is_traversed(at(&catalog, "/r/target")));
    assert!(!catalog.is_traversed(at(&catalog, "/r/src")));

    // Names sorted by (parent, name), the heap holding them in that order.
    let rows: Vec<_> = catalog
        .names()
        .map(|(id, bytes)| (catalog.name(id).parent, bytes.to_vec()))
        .collect();
    assert!(rows.windows(2).all(|w| w[0] < w[1]));
    let joined: Vec<u8> = rows
        .iter()
        .flat_map(|(_, n)| n.iter().copied().chain([0]))
        .collect();
    assert_eq!(catalog.name_heap(), joined);
    for (offset, _) in catalog.name_heap().iter().enumerate() {
        let id = catalog.name_at(offset).unwrap();
        let start = catalog.name_heap()[..=offset]
            .iter()
            .filter(|&&b| b == 0)
            .count()
            - usize::from(catalog.name_heap()[offset] == 0);
        assert_eq!(id, NameId(start as u32), "offset {offset}");
    }
    assert_eq!(catalog.name_at(catalog.name_heap().len()), None);

    // Lookup by name within a directory.
    let src = at(&catalog, "/r/src");
    assert_eq!(
        catalog.name(catalog.lookup(src, b"lib.rs").unwrap()).bytes,
        b"lib.rs"
    );
    assert_eq!(catalog.lookup(src, b"nope.rs"), None);
    assert_eq!(
        catalog
            .lookup(InoId(0), b"src")
            .map(|n| catalog.name(n).child),
        Some(src)
    );

    // Inode rows, content states and documents.
    let main = catalog.inode(at(&catalog, "/r/src/main.rs"));
    assert_eq!(main.stat, file_stat(10));
    assert_eq!(main.state, ContentState::Hashed);
    assert_eq!(catalog.doc_hash(main.doc.unwrap()), Some(hash(1)));
    let state = |path| catalog.inode(at(&catalog, path)).state;
    assert_eq!(state("/r/docs/logo.png"), ContentState::Binary);
    assert_eq!(state("/r/docs/big.iso"), ContentState::Unindexed);
    assert_eq!(state("/r/docs/racy.log"), ContentState::Fault);
    assert_eq!(catalog.inode(at(&catalog, "/r/docs/racy.log")).doc, None);
    assert_eq!(catalog.doc_count(), 4);
    assert_eq!(catalog.next_doc(), DocId(4));

    // New DocIds follow inode order, which is breadth-first path order.
    let doc_of = |path| catalog.inode(at(&catalog, path)).doc.unwrap().0;
    assert!(doc_of("/r/README") < doc_of("/r/src/lib.rs"));
    assert!(doc_of("/r/src/lib.rs") < doc_of("/r/src/main.rs"));
    assert!(doc_of("/r/src/main.rs") < doc_of("/r/target/keep.txt"));

    let link = at(&catalog, "/r/link");
    assert_eq!(catalog.kind(link), Kind::Symlink);
    assert_eq!(catalog.link_target(link), Some(&b"src/main.rs"[..]));
    assert_eq!(catalog.link_target(at(&catalog, "/r/README")), None);
    assert_eq!(catalog.kind(at(&catalog, "/r/README")), Kind::File);

    let trees: Vec<_> = catalog
        .work_trees()
        .map(|w| (w.dir, w.kind, w.common_id, w.common_dir.to_vec()))
        .collect();
    assert_eq!(
        trees,
        [
            (InoId(0), WorkTreeKind::Main, (7, 99), b"/r/.git".to_vec()),
            (
                src,
                WorkTreeKind::Submodule,
                (7, 98),
                b"/r/.git/modules/src".to_vec()
            ),
        ]
    );
    assert_eq!(
        catalog.work_tree(src).map(|w| w.kind),
        Some(WorkTreeKind::Submodule)
    );
    assert_eq!(catalog.work_tree(at(&catalog, "/r/docs")), None);
    assert_eq!(catalog.sniffer_version(), super::SNIFFER);
}

/// Workers finish in any order, and a re-run must not reorder the file for
/// it: the same observations give byte-identical snapshots.
#[test]
fn numbering_does_not_depend_on_which_worker_saw_what() {
    let scratch = Scratch::new("determinism");
    let file = scratch.path.join("catalog");
    commit_tree(&scratch.path, [0, 1, 2]);
    let first = std::fs::read(&file).unwrap();
    let other = Scratch::new("determinism-2");
    commit_tree(&other.path, [2, 0, 1]);
    assert_eq!(std::fs::read(other.path.join("catalog")).unwrap(), first);
}

#[test]
fn hard_linked_files_share_one_inode_row_and_aliased_directories_do_not() {
    let scratch = Scratch::new("hard-links");
    let catalog = commit(&scratch.path, |txn| {
        let (mut a, mut b) = (txn.batch(), txn.batch());
        let root = a.root(b"/h", dir_stat(1));
        let one = a.dir(root, b"one", dir_stat(2));
        let two = b.dir(root, b"two", dir_stat(3));
        // One inode under two names, reported by different workers.
        a.file(one, b"x", file_stat(50), Content::Hashed(hash(5)));
        b.file(two, b"y", file_stat(50), Content::Hashed(hash(5)));
        // A distinct inode with the same content.
        b.file(two, b"copy", file_stat(51), Content::Hashed(hash(5)));
        // One directory inode reached by two names (a bind mount): two rows.
        a.dir(one, b"bound", dir_stat(60));
        b.dir(two, b"bound", dir_stat(60));
        txn.add(a);
        txn.add(b);
    });
    let (x, y, copy) = (
        at(&catalog, "/h/one/x"),
        at(&catalog, "/h/two/y"),
        at(&catalog, "/h/two/copy"),
    );
    assert_eq!(x, y, "hard links share a row");
    assert_ne!(x, copy);
    assert_eq!(
        catalog.inode(x).doc,
        catalog.inode(copy).doc,
        "equal content, one document"
    );
    assert_eq!(catalog.doc_count(), 1);
    assert_ne!(at(&catalog, "/h/one/bound"), at(&catalog, "/h/two/bound"));
    // 5 directory rows, and two file rows for three file names.
    assert_eq!((catalog.dir_count(), catalog.inode_count()), (5, 7));
}

#[test]
fn two_fresh_observations_that_disagree_are_a_content_fault() {
    let scratch = Scratch::new("disagree");
    let catalog = commit(&scratch.path, |txn| {
        let mut a = txn.batch();
        let root = a.root(b"/d", dir_stat(1));
        a.file(root, b"a", file_stat(50), Content::Hashed(hash(5)));
        let edited = crate::Stat {
            mtime_sec: 1_800_000_000,
            ..file_stat(50)
        };
        a.file(root, b"b", edited, Content::Hashed(hash(6)));
        txn.add(a);
    });
    let row = catalog.inode(at(&catalog, "/d/a"));
    assert_eq!(at(&catalog, "/d/a"), at(&catalog, "/d/b"));
    assert_eq!((row.state, row.doc), (ContentState::Fault, None));
    assert_eq!(
        row.stat,
        file_stat(50),
        "one observation's stat is kept whole"
    );
    assert_eq!(catalog.doc_count(), 0);
}

#[test]
fn paths_resolve_upwards_to_the_root() {
    let scratch = Scratch::new("paths");
    let catalog = commit(&scratch.path, |txn| {
        let mut a = txn.batch();
        let slash = a.root(b"/", dir_stat(1));
        a.file(slash, b"top", file_stat(2), Content::Unindexed);
        let root = a.root(b"/home/u", dir_stat(3));
        let mut dir = root;
        for (i, name) in [&b"a"[..], b"b b", b"c\xff"].into_iter().enumerate() {
            dir = a.dir(dir, name, dir_stat(10 + i as u64));
        }
        a.file(dir, b"leaf\n", file_stat(20), Content::Unindexed);
        txn.add(a);
    });
    let leaf = catalog.names().find(|(_, n)| *n == b"leaf\n").unwrap().0;
    let mut path = b"prefix:".to_vec();
    catalog.path(leaf, &mut path);
    assert_eq!(
        path, b"prefix:/home/u/a/b b/c\xff/leaf\n",
        "names are bytes, never re-encoded"
    );

    let mut path = Vec::new();
    catalog.path(
        catalog.names().find(|(_, n)| *n == b"top").unwrap().0,
        &mut path,
    );
    assert_eq!(path, b"/top", "the root `/` is not doubled");

    let mut path = Vec::new();
    catalog.dir_path(catalog.name(leaf).parent, &mut path);
    assert_eq!(path, b"/home/u/a/b b/c\xff");
}

#[test]
fn inconsistent_batches_publish_nothing() {
    let scratch = Scratch::new("invalid");
    let build_error = |fill: &dyn Fn(&Transaction) -> Vec<Batch>| {
        let mut txn = Transaction::begin(&scratch.path, super::SNIFFER).unwrap();
        for batch in fill(&txn) {
            txn.add(batch);
        }
        match txn.commit() {
            Err(CommitError::Build(e)) => e,
            other => panic!("expected a build error, got {:?}", other.map(|_| ())),
        }
    };

    // A parent token from a batch that was never added.
    let e = build_error(&|txn| {
        let mut lost = txn.batch();
        let dir = lost.root(b"/lost", dir_stat(1));
        let mut b = txn.batch();
        b.file(dir, b"f", file_stat(2), Content::Unindexed);
        vec![b]
    });
    assert!(matches!(e, BuildError::UnknownToken(_)));

    let e = build_error(&|txn| {
        let (mut a, mut b) = (txn.batch(), txn.batch());
        a.root(b"/twice", dir_stat(1));
        b.root(b"/twice", dir_stat(2));
        vec![a, b]
    });
    assert_eq!(e, BuildError::DuplicateRoot(b"/twice".to_vec()));

    let e = build_error(&|txn| {
        let (mut a, mut b) = (txn.batch(), txn.batch());
        let root = a.root(b"/d", dir_stat(1));
        a.file(root, b"same", file_stat(2), Content::Unindexed);
        b.file(root, b"same", file_stat(3), Content::Unindexed);
        vec![a, b]
    });
    assert_eq!(e, BuildError::DuplicateName(b"same".to_vec()));

    for bad in [&b""[..], b".", b"..", b"a/b", b"nul\0"] {
        let e = build_error(&|txn| {
            let mut a = txn.batch();
            let root = a.root(b"/d", dir_stat(1));
            a.file(root, bad, file_stat(2), Content::Unindexed);
            vec![a]
        });
        assert_eq!(e, BuildError::BadName(bad.to_vec()));
    }

    let e = build_error(&|txn| {
        let mut a = txn.batch();
        let root = a.root(b"/d", dir_stat(1));
        a.work_tree(root, WorkTreeKind::Main, b"/d/.git", (1, 2));
        a.work_tree(root, WorkTreeKind::Main, b"/d/.git", (1, 2));
        vec![a]
    });
    assert_eq!(e, BuildError::DuplicateWorkTree);

    assert!(
        Catalog::open(&scratch.path).unwrap().is_none(),
        "nothing was published"
    );
}

/// The name heap's limit counts the name being added: nine 10-byte entries
/// (nine bytes and a NUL) then one of 20 end at 110, past a limit of 100,
/// though the heap was below it before the last name.
#[test]
fn a_name_that_would_carry_the_heap_past_its_limit_is_too_large() {
    let scratch = Scratch::new("heap-limit");
    let build = |last: &[u8]| {
        crate::build::HEAP_LIMIT.set(Some(100));
        let mut txn = Transaction::begin(&scratch.path, super::SNIFFER).unwrap();
        let mut b = txn.batch();
        let root = b.root(b"/h", dir_stat(1));
        for i in 0..9u8 {
            let name = [b'a' + i; 9];
            b.file(root, &name, file_stat(10 + u64::from(i)), Content::Unindexed);
        }
        b.file(root, last, file_stat(30), Content::Unindexed);
        txn.add(b);
        let result = txn.commit().map(|c| c.name_count());
        crate::build::HEAP_LIMIT.set(None);
        result
    };
    // Exactly at the limit: 100 bytes.
    assert_eq!(build(b"zzzzzzzzz").unwrap(), 10);
    match build(b"zzzzzzzzzzzzzzzzzzz") {
        Err(CommitError::Build(BuildError::TooLarge)) => {}
        other => panic!("expected TooLarge, got {:?}", other.map(|_| ())),
    }
}
