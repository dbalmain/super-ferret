//! D119: a new root whose walk faulted with nothing old to anchor it is
//! withdrawn from the run, and the healthy disjoint roots publish. Every test
//! faults its root the way the field report did, with a self-looping
//! `.ferretignore` symlink, so the real walker raises the real `ReadIgnore`.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use ferret_catalog::{Catalog, ContentState, Target};

use super::index::Tmp;
use crate::{
    IndexError, IndexOptions, IoOp, Refresh, Report, RootChange, index, index_change,
    open_or_create, session_change,
};

fn options() -> IndexOptions {
    IndexOptions {
        workers: 4,
        ..IndexOptions::default()
    }
}

/// A fresh directory under the temp base, to be a root.
fn root(tmp: &Tmp, name: &str) -> PathBuf {
    let path = tmp.base.join(name);
    fs::create_dir_all(&path).unwrap();
    path
}

/// Makes `root`'s rules unreadable: its `.ferretignore` loops (ELOOP).
fn fault(root: &Path) {
    std::os::unix::fs::symlink(".ferretignore", root.join(".ferretignore")).unwrap();
}

fn repair(root: &Path) {
    fs::remove_file(root.join(".ferretignore")).unwrap();
}

fn open(tmp: &Tmp) -> Catalog {
    let catalog = Catalog::open(&tmp.cat()).unwrap().unwrap();
    catalog.load_all().unwrap();
    catalog
}

fn roots(catalog: &Catalog) -> Vec<PathBuf> {
    catalog
        .roots()
        .map(|(_, p)| PathBuf::from(std::ffi::OsStr::from_bytes(p)))
        .collect()
}

/// Every published name's path, ignored names included.
fn names(catalog: &Catalog) -> Vec<PathBuf> {
    let mut path = Vec::new();
    catalog
        .names()
        .map(|(id, _)| {
            path.clear();
            catalog.path(id, &mut path);
            PathBuf::from(std::ffi::OsStr::from_bytes(&path))
        })
        .collect()
}

fn hashed(catalog: &Catalog, path: &Path) -> Option<[u8; 16]> {
    let resolved = catalog.resolve(path.as_os_str().as_bytes())?;
    assert!(resolved.remainder.is_empty(), "{}", path.display());
    let Target::Inode(id) = resolved.target else {
        return None;
    };
    assert_eq!(
        catalog.state(id),
        ContentState::Hashed,
        "{}",
        path.display()
    );
    catalog.doc_hash(catalog.inode(id).doc?)
}

fn blake3_128(bytes: &[u8]) -> [u8; 16] {
    let mut hash = [0; 16];
    hash.copy_from_slice(&blake3::hash(bytes).as_bytes()[..16]);
    hash
}

/// The report withdrew exactly `bad`, for its looping ignore file.
fn assert_withdrawn(report: &Report, bad: &Path) {
    assert_eq!(report.failed_roots.len(), 1, "{:?}", report.failed_roots);
    let failed = &report.failed_roots[0];
    assert_eq!(failed.root, bad);
    assert!(
        failed
            .faults
            .iter()
            .all(|f| f.root == bad && f.op == IoOp::ReadIgnore),
        "{:?}",
        failed.faults
    );
    assert!(report.published.is_some());
    assert!(!report.refreshed.iter().any(|r| r == bad));
}

/// How a first index reaches the crawl: in process with no catalog (an
/// initial checkpoint), or through a writer session over the empty catalog a
/// daemon creates before its first command (the bounded log path).
#[derive(Clone, Copy, Debug)]
enum First {
    Checkpoint,
    Session,
}

fn first_index(tmp: &Tmp, how: First, add: &[PathBuf]) -> Result<Report, IndexError> {
    match how {
        First::Checkpoint => index_change(
            &tmp.cat(),
            RootChange { add, remove: &[] },
            Refresh::Only(add),
            &options(),
        ),
        First::Session => {
            let mut session = open_or_create(&tmp.cat(), &options()).unwrap();
            session_change(
                &mut session,
                RootChange { add, remove: &[] },
                Refresh::Only(add),
                &options(),
            )
        }
    }
}

#[test]
fn a_first_index_publishes_the_healthy_root_and_withdraws_the_faulted_one() {
    for how in [First::Checkpoint, First::Session] {
        // The faulted root walks first, then second; each named first, then
        // second.
        for (bad_name, good_name) in [("a-bad", "b-good"), ("b-bad", "a-good")] {
            for reversed in [false, true] {
                let tmp = Tmp::new(&format!("d119-{how:?}-{bad_name}-{reversed}"));
                let good = root(&tmp, good_name);
                let bad = root(&tmp, bad_name);
                fs::write(good.join("seen.txt"), b"healthy\n").unwrap();
                fs::write(bad.join("hidden.txt"), b"uncertain\n").unwrap();
                fs::create_dir(bad.join("sub")).unwrap();
                fs::write(bad.join("sub/deep.txt"), b"uncertain too\n").unwrap();
                fault(&bad);
                let mut add = vec![good.clone(), bad.clone()];
                if reversed {
                    add.reverse();
                }

                let report = first_index(&tmp, how, &add).unwrap();
                assert_withdrawn(&report, &bad);
                assert_eq!(report.refreshed, std::slice::from_ref(&good));
                let catalog = open(&tmp);
                assert_eq!(roots(&catalog), std::slice::from_ref(&good));
                let published = names(&catalog);
                assert!(
                    !published.iter().any(|p| p.starts_with(&bad)),
                    "{published:?}"
                );
                assert_eq!(
                    hashed(&catalog, &good.join("seen.txt")),
                    Some(blake3_128(b"healthy\n"))
                );
                drop(catalog);

                // Repaired, the withdrawn root is added like any new one.
                repair(&bad);
                let report = index_change(
                    &tmp.cat(),
                    RootChange {
                        add: std::slice::from_ref(&bad),
                        remove: &[],
                    },
                    Refresh::Only(std::slice::from_ref(&bad)),
                    &options(),
                )
                .unwrap();
                assert!(report.failed_roots.is_empty());
                let catalog = open(&tmp);
                let mut both = vec![good.clone(), bad.clone()];
                both.sort();
                assert_eq!(roots(&catalog), both);
                assert_eq!(
                    hashed(&catalog, &bad.join("sub/deep.txt")),
                    Some(blake3_128(b"uncertain too\n"))
                );
            }
        }
    }
}

#[test]
fn an_all_failed_run_publishes_nothing() {
    for how in [First::Checkpoint, First::Session] {
        let tmp = Tmp::new(&format!("d119-all-{how:?}"));
        let one = root(&tmp, "one");
        let two = root(&tmp, "two");
        fs::write(one.join("f"), b"f\n").unwrap();
        fault(&one);
        fault(&two);
        for add in [vec![one.clone()], vec![one.clone(), two.clone()]] {
            let error = first_index(&tmp, how, &add).unwrap_err();
            let IndexError::Coverage { faults, .. } = error else {
                panic!("expected a coverage refusal, got {error}");
            };
            assert_eq!(faults.len(), add.len(), "every root's fault is reported");
            match how {
                First::Checkpoint => assert!(Catalog::open(&tmp.cat()).unwrap().is_none()),
                First::Session => assert!(roots(&open(&tmp)).is_empty()),
            }
        }
    }
}

/// A faulted root nested in a new healthy one is not its own: it changes
/// which rules govern the outer root's subtree (D34), so the run stays atomic.
#[test]
fn a_faulted_root_nested_in_another_stays_atomic() {
    for how in [First::Checkpoint, First::Session] {
        let tmp = Tmp::new(&format!("d119-nested-{how:?}"));
        let outer = root(&tmp, "outer");
        let inner = root(&tmp, "outer/inner");
        fs::write(outer.join("f"), b"f\n").unwrap();
        fault(&inner);
        let error = first_index(&tmp, how, &[outer.clone(), inner.clone()]).unwrap_err();
        assert!(matches!(error, IndexError::Coverage { .. }), "{error}");
        match how {
            First::Checkpoint => assert!(Catalog::open(&tmp.cat()).unwrap().is_none()),
            First::Session => assert!(roots(&open(&tmp)).is_empty()),
        }
    }
}

/// One inode named under both roots. Whichever root's walk read it first, the
/// healthy name publishes its content; content named only under the
/// withdrawn root publishes nowhere.
#[test]
fn hard_linked_content_publishes_only_through_the_healthy_root() {
    for how in [First::Checkpoint, First::Session] {
        for (bad_name, good_name) in [("a-bad", "b-good"), ("b-bad", "a-good")] {
            let tmp = Tmp::new(&format!("d119-links-{how:?}-{bad_name}"));
            let good = root(&tmp, good_name);
            let bad = root(&tmp, bad_name);
            fs::write(bad.join("shared.txt"), b"shared content\n").unwrap();
            fs::hard_link(bad.join("shared.txt"), good.join("shared.txt")).unwrap();
            fs::write(bad.join("only.txt"), b"only under the bad root\n").unwrap();
            fs::hard_link(bad.join("only.txt"), bad.join("only-again.txt")).unwrap();
            fault(&bad);

            let report = first_index(&tmp, how, &[good.clone(), bad.clone()]).unwrap();
            assert_withdrawn(&report, &bad);
            let catalog = open(&tmp);
            assert_eq!(roots(&catalog), std::slice::from_ref(&good));
            assert_eq!(
                hashed(&catalog, &good.join("shared.txt")),
                Some(blake3_128(b"shared content\n"))
            );
            let only = blake3_128(b"only under the bad root\n");
            let docs: Vec<_> = (0..catalog.next_doc().0)
                .filter_map(|d| catalog.doc_hash(ferret_catalog::DocId(d)))
                .collect();
            assert!(!docs.contains(&only), "withdrawn-only content published");
            assert_eq!(catalog.doc_count(), 1, "{docs:?}");
            assert!(!names(&catalog).iter().any(|p| p.starts_with(&bad)));
        }
    }
}

/// Adding roots to an existing index: the old generation's roots stay, the
/// healthy addition publishes, and the faulted one is withdrawn. Adding only
/// the faulted root publishes nothing.
#[test]
fn adding_a_faulted_root_keeps_the_old_roots_and_publishes_the_healthy_addition() {
    let tmp = Tmp::new("d119-add");
    let old = root(&tmp, "old");
    fs::write(old.join("kept.txt"), b"kept\n").unwrap();
    index(
        &tmp.cat(),
        std::slice::from_ref(&old),
        Refresh::All,
        &options(),
    )
    .unwrap();
    let before = open(&tmp).generation();
    let good = root(&tmp, "new-good");
    let bad = root(&tmp, "new-bad");
    fs::write(good.join("added.txt"), b"added\n").unwrap();
    fs::write(bad.join("hidden.txt"), b"hidden\n").unwrap();
    fault(&bad);

    let only_bad = index_change(
        &tmp.cat(),
        RootChange {
            add: std::slice::from_ref(&bad),
            remove: &[],
        },
        Refresh::Only(std::slice::from_ref(&bad)),
        &options(),
    )
    .unwrap_err();
    assert!(
        matches!(only_bad, IndexError::Coverage { .. }),
        "{only_bad}"
    );
    assert_eq!(open(&tmp).generation(), before);

    let add = [bad.clone(), good.clone()];
    let report = index_change(
        &tmp.cat(),
        RootChange {
            add: &add,
            remove: &[],
        },
        Refresh::Only(&add),
        &options(),
    )
    .unwrap();
    assert_withdrawn(&report, &bad);
    assert_eq!(report.kept, std::slice::from_ref(&old));
    let catalog = open(&tmp);
    assert_eq!(roots(&catalog), [good.clone(), old.clone()]);
    assert!(!names(&catalog).iter().any(|p| p.starts_with(&bad)));
    assert_eq!(
        hashed(&catalog, &old.join("kept.txt")),
        Some(blake3_128(b"kept\n"))
    );
    assert_eq!(
        hashed(&catalog, &good.join("added.txt")),
        Some(blake3_128(b"added\n"))
    );
}

/// The input limit can trip after D119 has withdrawn a root: here the
/// healthy addition's walk fits the budget and reconciling it does not, so
/// the run falls back to a full rewalk. The fallback must still withdraw and
/// report the faulted root rather than publish as if it had never been asked
/// for.
#[test]
fn a_withdrawn_root_is_reported_when_reconciling_falls_back_to_a_full_rewalk() {
    let tmp = Tmp::new("d119-fallback");
    let old = root(&tmp, "old");
    fs::write(old.join("kept.txt"), b"kept\n").unwrap();
    index(
        &tmp.cat(),
        std::slice::from_ref(&old),
        Refresh::All,
        &options(),
    )
    .unwrap();
    let good = root(&tmp, "good");
    for n in 0..64 {
        fs::write(good.join(format!("f{n}")), b"").unwrap();
    }
    let bad = root(&tmp, "bad");
    fs::write(bad.join("hidden.txt"), b"hidden\n").unwrap();
    fault(&bad);
    let mut session = open_or_create(&tmp.cat(), &options()).unwrap();
    // Observing the 64 files charges about 70 records and reconciling them
    // about 400: 200 trips the limit after `protect` has withdrawn `bad`.
    session.set_input_limits(ferret_catalog::InputLimits {
        records: 200,
        owned_bytes: usize::MAX,
    });

    let add = [good.clone(), bad.clone()];
    let report = session_change(
        &mut session,
        RootChange {
            add: &add,
            remove: &[],
        },
        Refresh::Only(&add),
        &options(),
    )
    .unwrap();
    drop(session);
    assert!(report.input_fallback);
    assert_withdrawn(&report, &bad);
    let catalog = open(&tmp);
    assert_eq!(roots(&catalog), [good.clone(), old.clone()]);
    assert!(!names(&catalog).iter().any(|p| p.starts_with(&bad)));
}
