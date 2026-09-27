//! Which roots a commit refreshes, keeps or drops (D34).

use super::{SNIFFER, Scratch, at, commit, dir_stat, file_stat, hash, link_stat, paths};
use crate::{Catalog, Content, ContentState, KeepError, Stat, Transaction, WorkTreeKind};

/// Everything observable about one root's subtree, keyed by path, with ids
/// resolved away (they are renumbered by every commit).
fn subtree(catalog: &Catalog, prefix: &str) -> Vec<String> {
    paths(catalog)
        .into_iter()
        .filter(|(path, _)| path.starts_with(prefix))
        .map(|(path, id)| {
            let inode = catalog.inode(id);
            let hash = inode.doc.and_then(|d| catalog.doc_hash(d));
            let tree = catalog
                .work_tree(id)
                .map(|w| (w.kind, w.common_id, w.common_dir.to_vec()));
            let traversed = catalog.kind(id) == crate::Kind::Dir && catalog.is_traversed(id);
            let target = catalog.link_target(id).map(<[u8]>::to_vec);
            format!(
                "{path} {:?} {:?} {:?} {hash:?} {tree:?} {traversed} {target:?}",
                inode.stat, inode.state, inode.doc
            )
        })
        .collect()
}

/// Roots `/a`, `/b` and `/c`, each with a little of everything.
fn three_roots(dir: &std::path::Path) -> Catalog {
    commit(dir, |txn| {
        let mut w = txn.batch();
        for (i, root) in ["/a", "/b", "/c"].into_iter().enumerate() {
            let base = 100 * (i as u64 + 1);
            let r = w.root(root.as_bytes(), dir_stat(base));
            let sub = w.dir(r, b"sub", dir_stat(base + 1));
            let skip = w.traversed_dir(r, b"skipped", dir_stat(base + 2));
            w.file(
                sub,
                b"f",
                file_stat(base + 3),
                Content::Hashed(hash(i as u8)),
            );
            w.file(skip, b"kept", file_stat(base + 4), Content::Binary);
            w.symlink(r, b"l", link_stat(base + 5), b"sub/f");
            w.work_tree(sub, WorkTreeKind::Linked, b"/repo/.git", (7, 1));
        }
        txn.add(w);
    })
}

#[test]
fn refreshing_one_root_keeps_another_unchanged_and_drops_the_rest() {
    let scratch = Scratch::new("roots-refresh");
    let first = three_roots(&scratch.path);

    let mut txn = Transaction::begin(&scratch.path, SNIFFER).unwrap();
    let mut w = txn.batch();
    let a = w.root(b"/a", dir_stat(100));
    w.file(a, b"new", file_stat(150), Content::Hashed(hash(9)));
    txn.add(w);
    txn.keep(b"/b").unwrap();
    let second = txn.commit().unwrap();

    assert_eq!(
        subtree(&second, "/b/"),
        subtree(&first, "/b/"),
        "kept root copied forward whole"
    );
    assert_eq!(
        second.roots().map(|(_, p)| p.to_vec()).collect::<Vec<_>>(),
        [b"/a".to_vec(), b"/b".to_vec()]
    );
    assert_eq!(
        paths(&second)
            .keys()
            .filter(|p| p.starts_with("/a/"))
            .collect::<Vec<_>>(),
        ["/a/new"]
    );
    assert!(
        subtree(&second, "/c").is_empty(),
        "a root neither refreshed nor kept is dropped"
    );
    // /a's and /c's old documents died; /b's lives on under its old id.
    let b_doc = |c: &Catalog| c.inode(at(c, "/b/sub/f")).doc;
    assert_eq!(b_doc(&second), b_doc(&first));
    assert_eq!(second.doc_count(), 2);
}

#[test]
fn a_fresh_observation_supersedes_a_kept_one() {
    let scratch = Scratch::new("roots-supersede");
    // One inode hard-linked into two roots.
    commit(&scratch.path, |txn| {
        let mut w = txn.batch();
        let a = w.root(b"/a", dir_stat(1));
        let b = w.root(b"/b", dir_stat(2));
        w.file(a, b"shared", file_stat(50), Content::Hashed(hash(1)));
        w.file(b, b"alias", file_stat(50), Content::Hashed(hash(1)));
        txn.add(w);
    });

    // Keep /a, which carries the old observation; refresh /b, which sees the
    // inode edited. /a sorts first, so the carried name is met first when
    // inodes are numbered, and the fresh observation must still win.
    let edited = Stat {
        mtime_sec: 1_800_000_000,
        ..file_stat(50)
    };
    let mut txn = Transaction::begin(&scratch.path, SNIFFER).unwrap();
    txn.keep(b"/a").unwrap();
    let mut w = txn.batch();
    let b = w.root(b"/b", dir_stat(2));
    w.file(b, b"alias", edited, Content::Hashed(hash(2)));
    txn.add(w);
    let catalog = txn.commit().unwrap();

    let (kept, fresh) = (at(&catalog, "/a/shared"), at(&catalog, "/b/alias"));
    assert_eq!(fresh, kept, "one inode row for both names");
    let row = catalog.inode(fresh);
    assert_eq!(row.stat, edited);
    assert_eq!(
        (row.state, row.doc.and_then(|d| catalog.doc_hash(d))),
        (ContentState::Hashed, Some(hash(2)))
    );
    assert_eq!(catalog.doc_count(), 1, "the old content died with the edit");
}

#[test]
fn nested_roots_are_separate_trees() {
    let scratch = Scratch::new("roots-nested");
    let catalog = commit(&scratch.path, |txn| {
        // The outer walk stopped at the boundary `x`; the inner root owns it.
        let mut w = txn.batch();
        let outer = w.root(b"/home/u", dir_stat(1));
        w.file(outer, b"notes", file_stat(2), Content::Unindexed);
        let inner = w.root(b"/home/u/x", dir_stat(3));
        w.file(inner, b"inside", file_stat(4), Content::Unindexed);
        txn.add(w);
    });
    assert_eq!(
        paths(&catalog).keys().collect::<Vec<_>>(),
        ["/home/u/notes", "/home/u/x/inside"]
    );
    assert_eq!(catalog.roots().count(), 2);
    assert_eq!(
        catalog.lookup(crate::InoId(0), b"x"),
        None,
        "no name edge into the inner root"
    );
}

#[test]
fn keep_refuses_an_unknown_root_and_a_sniffer_change() {
    let scratch = Scratch::new("roots-keep-refused");
    let mut txn = Transaction::begin(&scratch.path, SNIFFER).unwrap();
    assert_eq!(
        txn.keep(b"/a"),
        Err(KeepError::UnknownRoot(b"/a".to_vec())),
        "no previous generation"
    );
    drop(txn);

    three_roots(&scratch.path);
    let mut txn = Transaction::begin(&scratch.path, SNIFFER).unwrap();
    assert_eq!(
        txn.keep(b"/nope"),
        Err(KeepError::UnknownRoot(b"/nope".to_vec()))
    );
    drop(txn);
    let mut txn = Transaction::begin(&scratch.path, SNIFFER + 1).unwrap();
    assert_eq!(txn.keep(b"/a"), Err(KeepError::SnifferChanged));
}

#[test]
fn keeping_and_refreshing_one_root_is_an_error() {
    let scratch = Scratch::new("roots-keep-twice");
    three_roots(&scratch.path);
    let mut txn = Transaction::begin(&scratch.path, SNIFFER).unwrap();
    txn.keep(b"/a").unwrap();
    let mut w = txn.batch();
    w.root(b"/a", dir_stat(100));
    txn.add(w);
    assert!(matches!(
        txn.commit(),
        Err(crate::CommitError::Build(crate::BuildError::DuplicateRoot(
            _
        )))
    ));
}

/// Every path in `catalog`, asserting no path is named twice: a duplicated
/// subtree would collapse in the map, so the name count is checked too.
fn exact_paths(catalog: &Catalog) -> Vec<String> {
    let all = paths(catalog);
    assert_eq!(all.len(), catalog.name_count() as usize, "a path named twice");
    all.into_keys().collect()
}

fn roots_of(catalog: &Catalog) -> Vec<String> {
    catalog
        .roots()
        .map(|(_, p)| String::from_utf8_lossy(p).into_owned())
        .collect()
}

fn overlap(kept: &str, changed: &str) -> crate::CommitError {
    crate::CommitError::KeptRootOverlaps {
        kept: kept.as_bytes().to_vec(),
        changed: changed.as_bytes().to_vec(),
    }
}

fn assert_overlap(result: Result<Catalog, crate::CommitError>, kept: &str, changed: &str) {
    match result {
        Err(e) => assert_eq!(format!("{e:?}"), format!("{:?}", overlap(kept, changed))),
        Ok(_) => panic!("committed a kept root with {changed} changed inside it"),
    }
}

/// `/a` holding `top` and `b/f`, walked with `b` as an ordinary directory.
fn outer_only(txn: &mut Transaction) {
    let mut w = txn.batch();
    let a = w.root(b"/a", dir_stat(1));
    w.file(a, b"top", file_stat(10), Content::Unindexed);
    let b = w.dir(a, b"b", dir_stat(2));
    w.file(b, b"f", file_stat(11), Content::Unindexed);
    txn.add(w);
}

/// `/a` and `/a/b` as two roots: the outer walk stops at `b`.
fn outer_and_inner(txn: &mut Transaction) {
    let mut w = txn.batch();
    let a = w.root(b"/a", dir_stat(1));
    w.file(a, b"top", file_stat(10), Content::Unindexed);
    let b = w.root(b"/a/b", dir_stat(2));
    w.file(b, b"f", file_stat(11), Content::Unindexed);
    txn.add(w);
}

#[test]
fn adding_a_root_inside_a_kept_root_needs_the_outer_refreshed() {
    let scratch = Scratch::new("roots-add-inner");
    let first = commit(&scratch.path, outer_only);

    // Keeping /a would copy b/f forward while the new root /a/b also holds it.
    let mut txn = Transaction::begin(&scratch.path, SNIFFER).unwrap();
    txn.keep(b"/a").unwrap();
    let mut w = txn.batch();
    let b = w.root(b"/a/b", dir_stat(2));
    w.file(b, b"f", file_stat(11), Content::Unindexed);
    txn.add(w);
    assert_overlap(txn.commit(), "/a", "/a/b");
    let unchanged = Catalog::open(&scratch.path).unwrap().unwrap();
    assert_eq!(exact_paths(&unchanged), exact_paths(&first));

    // Refreshing /a alongside the new root is the valid transition.
    let second = commit(&scratch.path, outer_and_inner);
    assert_eq!(exact_paths(&second), ["/a/b/f", "/a/top"]);
    assert_eq!(roots_of(&second), ["/a", "/a/b"]);
}

#[test]
fn removing_a_root_inside_a_kept_root_needs_the_outer_refreshed() {
    let scratch = Scratch::new("roots-remove-inner");
    let first = commit(&scratch.path, outer_and_inner);

    // Keeping /a alone would drop /a/b and, with it, b/f: the old /a walk
    // stopped at the boundary.
    let mut txn = Transaction::begin(&scratch.path, SNIFFER).unwrap();
    txn.keep(b"/a").unwrap();
    assert_overlap(txn.commit(), "/a", "/a/b");
    let unchanged = Catalog::open(&scratch.path).unwrap().unwrap();
    assert_eq!(exact_paths(&unchanged), exact_paths(&first));

    // `b` is an ordinary directory of /a again, with a name of its own.
    let second = commit(&scratch.path, outer_only);
    assert_eq!(exact_paths(&second), ["/a/b", "/a/b/f", "/a/top"]);
    assert_eq!(roots_of(&second), ["/a"]);
}

#[test]
fn a_kept_inner_root_allows_the_outer_to_change() {
    // The overlap rule is one-way: a new outer walk stops at a kept inner
    // root's boundary, so nothing is duplicated or lost. Sibling paths that
    // share a prefix (`/a` and `/ab`) are not nested.
    let scratch = Scratch::new("roots-keep-inner");
    commit(&scratch.path, |txn| {
        let mut w = txn.batch();
        let b = w.root(b"/a/b", dir_stat(2));
        w.file(b, b"f", file_stat(11), Content::Unindexed);
        w.root(b"/a", dir_stat(1));
        txn.add(w);
    });
    let mut txn = Transaction::begin(&scratch.path, SNIFFER).unwrap();
    txn.keep(b"/a/b").unwrap();
    let mut w = txn.batch();
    w.root(b"/ab", dir_stat(3));
    txn.add(w);
    let catalog = txn.commit().unwrap();
    assert_eq!(exact_paths(&catalog), ["/a/b/f"]);
    assert_eq!(roots_of(&catalog), ["/a/b", "/ab"]);
}
