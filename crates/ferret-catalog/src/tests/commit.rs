//! The writer lock, commit outcomes, and readers across commits (D32).

use std::fs;
use std::os::unix::fs::PermissionsExt;

use super::{SNIFFER, Scratch, commit, dir_stat, file_stat, paths, reopen};
use crate::transaction::{FAIL_SYNC_DIR, SYNCED_DIRS};
use crate::{BeginError, Catalog, CommitError, Content, Transaction};

/// A generation with one root holding `names`.
fn fill(txn: &mut Transaction, names: &[&str]) {
    let mut w = txn.batch();
    let root = w.root(b"/g", dir_stat(1));
    for (i, name) in names.iter().enumerate() {
        w.file(
            root,
            name.as_bytes(),
            file_stat(10 + i as u64),
            Content::Unindexed,
        );
    }
    txn.add(w);
}

fn names(catalog: &Catalog) -> Vec<String> {
    paths(catalog).into_keys().collect()
}

fn leftovers(dir: &std::path::Path) -> Vec<String> {
    let mut found: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    found.sort();
    found
}

#[test]
fn a_second_writer_is_refused_until_the_first_is_done() {
    let scratch = Scratch::new("lock");
    let first = Transaction::begin(&scratch.path, SNIFFER).unwrap();
    assert!(matches!(
        Transaction::begin(&scratch.path, SNIFFER),
        Err(BeginError::Locked)
    ));
    drop(first);
    let mut second = Transaction::begin(&scratch.path, SNIFFER).unwrap();
    fill(&mut second, &["a"]);
    // Readers take no lock.
    assert!(Catalog::open(&scratch.path).unwrap().is_none());
    second.commit().unwrap();
    // The lock is released by commit.
    drop(Transaction::begin(&scratch.path, SNIFFER).unwrap());
}

#[test]
fn the_lock_is_released_even_while_a_forked_child_holds_its_descriptor() {
    let scratch = Scratch::new("lock-fork");
    let first = Transaction::begin(&scratch.path, SNIFFER).unwrap();
    // What a child forked by another thread holds until it execs.
    let child = first.lock_copy();
    drop(first);
    drop(Transaction::begin(&scratch.path, SNIFFER).unwrap());
    let mut txn = Transaction::begin(&scratch.path, SNIFFER).unwrap();
    let copy = txn.lock_copy();
    fill(&mut txn, &["a"]);
    txn.commit().unwrap();
    drop(Transaction::begin(&scratch.path, SNIFFER).unwrap());
    drop((child, copy));
}

#[test]
fn a_failure_before_the_rename_publishes_nothing() {
    let scratch = Scratch::new("fail-before");
    commit(&scratch.path, |txn| fill(txn, &["old"]));

    let mut txn = Transaction::begin(&scratch.path, SNIFFER).unwrap();
    fill(&mut txn, &["new"]);
    // A real failure: the temp file cannot be created in a read-only
    // directory.
    fs::set_permissions(&scratch.path, fs::Permissions::from_mode(0o555)).unwrap();
    let err = txn.commit().map(|_| ()).unwrap_err();
    fs::set_permissions(&scratch.path, fs::Permissions::from_mode(0o755)).unwrap();

    assert!(matches!(err, CommitError::Write(_)), "{err:?}");
    assert!(!err.published());
    assert_eq!(names(&reopen(&scratch.path)), ["/g/old"]);
    assert_eq!(leftovers(&scratch.path), ["catalog", "lock"]);
    drop(Transaction::begin(&scratch.path, SNIFFER).unwrap());
}

#[test]
fn a_failure_after_the_rename_is_published_but_undurable() {
    let scratch = Scratch::new("fail-after");
    commit(&scratch.path, |txn| fill(txn, &["old"]));

    let mut txn = Transaction::begin(&scratch.path, SNIFFER).unwrap();
    fill(&mut txn, &["new"]);
    FAIL_SYNC_DIR.set(true);
    let result = txn.commit().map(|_| ());
    FAIL_SYNC_DIR.set(false);
    let err = result.unwrap_err();

    assert!(matches!(err, CommitError::Undurable(_)), "{err:?}");
    assert!(
        err.published(),
        "callers must not read this as nothing published"
    );
    assert_eq!(names(&reopen(&scratch.path)), ["/g/new"]);
    assert_eq!(leftovers(&scratch.path), ["catalog", "lock"]);
    drop(Transaction::begin(&scratch.path, SNIFFER).unwrap());
}

#[test]
fn dropping_a_transaction_publishes_nothing_and_a_stale_temp_is_cleared() {
    let scratch = Scratch::new("drop");
    commit(&scratch.path, |txn| fill(txn, &["old"]));
    // What a writer killed mid-write leaves behind.
    fs::write(scratch.path.join("catalog.tmp"), b"half a catalog").unwrap();

    let mut txn = Transaction::begin(&scratch.path, SNIFFER).unwrap();
    assert_eq!(
        leftovers(&scratch.path),
        ["catalog", "lock"],
        "begin clears the stale temp"
    );
    fill(&mut txn, &["new"]);
    drop(txn);

    assert_eq!(names(&reopen(&scratch.path)), ["/g/old"]);
    assert_eq!(leftovers(&scratch.path), ["catalog", "lock"]);
    drop(Transaction::begin(&scratch.path, SNIFFER).unwrap());
}

#[test]
fn an_old_reader_keeps_its_generation_across_a_commit() {
    let scratch = Scratch::new("reader");
    commit(&scratch.path, |txn| fill(txn, &["one", "two"]));
    // Opened lazily: nothing but the header is read before the next commit
    // replaces the file, so every section comes from the held descriptor.
    let reader = Catalog::open(&scratch.path).unwrap().unwrap();

    commit(&scratch.path, |txn| fill(txn, &["three"]));
    reader.load_all().unwrap();
    assert_eq!(names(&reader), ["/g/one", "/g/two"]);
    assert_eq!(
        reader.inode(reader.name(crate::NameId(1)).child).stat,
        file_stat(11)
    );
    assert_eq!(names(&reopen(&scratch.path)), ["/g/three"]);
}

#[test]
fn a_corrupt_previous_generation_stops_the_writer() {
    let scratch = Scratch::new("corrupt-previous");
    commit(&scratch.path, |txn| fill(txn, &["a"]));
    let file = scratch.path.join("catalog");
    let mut bytes = fs::read(&file).unwrap();
    bytes.truncate(bytes.len() - 1);
    fs::write(&file, bytes).unwrap();

    assert!(matches!(
        Catalog::open(&scratch.path),
        Err(crate::OpenError::Decode(_))
    ));
    assert!(matches!(
        Transaction::begin(&scratch.path, SNIFFER),
        Err(BeginError::Previous(_))
    ));
}

/// The directories synced while `run` publishes, in order.
fn synced_during(run: impl FnOnce()) -> Vec<std::path::PathBuf> {
    SYNCED_DIRS.with_borrow_mut(Vec::clear);
    run();
    SYNCED_DIRS.with_borrow_mut(std::mem::take)
}

#[test]
// The first generation's directory and its ancestors were made with a bare
// `create_dir_all`, so their entries in their parents were never synced and a
// crash could lose a publication commit had acknowledged. Then they were
// synced only by the writer that created them, before the lock: a writer
// that found them already made (by one that lost the lock race before its
// syncs) synced nothing. So the directories are pre-made here, as that
// other writer would have left them, and the publisher must still sync the
// whole chain.
fn a_first_publication_syncs_every_ancestor_of_the_index_directory() {
    let scratch = Scratch::new("durable-mkdir");
    let dir = scratch.path.join("a/b");
    fs::create_dir_all(&dir).unwrap();
    let real = fs::canonicalize(&dir).unwrap();
    let publish = || {
        let mut txn = Transaction::begin(&dir, SNIFFER).unwrap();
        fill(&mut txn, &["x"]);
        txn.commit().unwrap();
    };
    let mut expected: Vec<_> = real.ancestors().skip(1).map(ToOwned::to_owned).collect();
    expected.push(dir.clone());
    assert_eq!(synced_during(publish), expected);
    // Later generations sync only the index directory, after the rename.
    assert_eq!(synced_during(publish), [dir]);
}
