//! Scoped faults use the real walker, batch API, durable writer and disk
//! reader.
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::checkpoint_oracle;
use super::index::Tmp;
use crate::walk::{IO_HOOKS, IoHook, IoPoint};
use crate::{IndexOptions, IoOp, Refresh, index, recrawl};
use checkpoint_oracle::{Listing, listings};
use ferret_catalog::{Catalog, Contents, InoId, Target, WriterSession};

struct Hook(PathBuf);
impl Hook {
    fn set(
        root: &Path,
        body: impl Fn(IoPoint, &Path) -> Option<(IoOp, std::io::Error)> + Send + Sync + 'static,
    ) -> Self {
        let hook: IoHook = Arc::new(body);
        IO_HOOKS.lock().unwrap().push((root.to_owned(), hook));
        Self(root.to_owned())
    }
}
impl Drop for Hook {
    fn drop(&mut self) {
        IO_HOOKS.lock().unwrap().retain(|(root, _)| *root != self.0);
    }
}
fn options() -> IndexOptions {
    IndexOptions {
        workers: 4,
        ..IndexOptions::default()
    }
}
fn open(path: &Path) -> Catalog {
    let c = Catalog::open(path).unwrap().unwrap();
    c.load_all().unwrap();
    c
}
fn directory(c: &Catalog, root: &Path, name: &[u8]) -> InoId {
    let r = c
        .roots()
        .find(|(_, p)| *p == root.as_os_str().as_bytes())
        .unwrap()
        .0;
    if name.is_empty() {
        r
    } else {
        c.name(c.lookup(r, name).unwrap()).child
    }
}
fn expected(tmp: &Tmp, before: &Catalog, scopes: &[&str], markers: &[&str]) -> Vec<Listing> {
    let path = tmp.base.join("oracle");
    if path.exists() {
        fs::remove_dir_all(&path).unwrap();
    }
    index(&path, &[tmp.tree()], Refresh::All, &options()).unwrap();
    let paths = |names: &[&str]| {
        names
            .iter()
            .map(|name| {
                (if name.is_empty() {
                    tmp.tree()
                } else {
                    tmp.at(name)
                })
                .as_os_str()
                .as_bytes()
                .to_vec()
            })
            .collect::<Vec<_>>()
    };
    retained_listings(&open(&path), before, &paths(scopes), &paths(markers))
}
/// Expected retained state comes from actual pre-fault rows. Fresh trustworthy
/// rows come from a separate real full checkpoint, not a model of
/// reconciliation.
pub(crate) fn retained_listings(
    fresh: &Catalog,
    before: &Catalog,
    scopes: &[Vec<u8>],
    markers: &[Vec<u8>],
) -> Vec<Listing> {
    let inside = |path: &[u8]| {
        scopes.iter().any(|scope| {
            path == scope || (path.starts_with(scope) && path.get(scope.len()) == Some(&b'/'))
        })
    };
    let old = listings(before);
    let mut rows: Vec<_> = listings(fresh)
        .into_iter()
        .filter(|r| !inside(&r.path))
        .collect();
    rows.extend(old.iter().filter(|r| inside(&r.path)).cloned());
    for row in &mut rows {
        if markers.contains(&row.path) {
            row.entries = None;
            row.retained_at = Some(
                old.iter()
                    .find(|r| r.path == row.path)
                    .and_then(|r| r.retained_at)
                    .unwrap_or(before.generation().sequence),
            );
        }
    }
    rows.sort();
    rows
}

#[test]
fn every_typed_namespace_fault_row_retains_a_checked_scope() {
    let cases = [
        (IoPoint::Directory, "dir", IoOp::List, 5, "dir", "dir"),
        (IoPoint::Directory, "dir", IoOp::List, 2, "dir", "dir"),
        (IoPoint::Directory, "dir", IoOp::OpenDir, 13, "dir", "dir"),
        (IoPoint::Directory, "dir", IoOp::Reopen, 5, "dir", "dir"),
        (IoPoint::Directory, "dir", IoOp::Reopen, -1, "dir", "dir"),
        (IoPoint::Child, "dir/a", IoOp::Lstat, 13, "dir/a", "dir"),
        (IoPoint::Child, "dir/a", IoOp::Readlink, 2, "dir/a", "dir"),
        (
            IoPoint::Directory,
            "dir",
            IoOp::ReadIgnore,
            13,
            "dir",
            "dir",
        ),
        (IoPoint::Directory, "dir", IoOp::ProbeGit, 2, "dir", "dir"),
        (IoPoint::Child, "dir/a", IoOp::ReadIgnore, 5, "dir", "dir"),
        (IoPoint::Child, "dir/a", IoOp::ProbeGit, 2, "dir", "dir"),
        (IoPoint::Child, "dir", IoOp::OpenDir, 13, "dir", "dir"),
        (IoPoint::Root, "", IoOp::OpenDir, 2, "", ""),
        (IoPoint::Root, "", IoOp::Lstat, 2, "", ""),
        (IoPoint::Root, "", IoOp::List, 5, "", ""),
    ];
    for (i, &(point, path, op, error, scope, marker)) in cases.iter().enumerate() {
        let tmp = Tmp::new(&format!("fault-table-{i}"));
        tmp.write("dir/a", b"content");
        tmp.write("dir/deep/b", b"nested");
        tmp.write("stable", b"stable");
        if op == IoOp::Readlink {
            fs::remove_file(tmp.at("dir/a")).unwrap();
            std::os::unix::fs::symlink("../stable", tmp.at("dir/a")).unwrap();
        }
        let opts = options();
        let roots = [tmp.tree()];
        index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
        let mut session = WriterSession::open(&tmp.cat()).unwrap();
        let before = session.view();
        let hook = Hook::set(&tmp.tree(), move |at, rel| {
            (at == point && rel == Path::new(path)).then(|| {
                (
                    op,
                    if error == -1 {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "directory identity changed",
                        )
                    } else {
                        std::io::Error::from_raw_os_error(error)
                    },
                )
            })
        });
        let report = recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        assert_eq!(report.coverage_faults.len(), 1, "case {i}");
        let fault = &report.coverage_faults[0];
        assert_eq!(fault.op, op, "case {i}");
        assert_eq!(
            fault.error.raw_os_error(),
            (error != -1).then_some(error),
            "case {i}"
        );
        assert!(
            matches!(
                (&fault.context, point),
                (crate::CoverageContext::Root, IoPoint::Root)
                    | (crate::CoverageContext::Directory(_), IoPoint::Directory)
                    | (crate::CoverageContext::Child { .. }, IoPoint::Child)
            ),
            "case {i}"
        );
        assert_eq!(report.protected_scopes, 1, "case {i}");
        let faulted = session.view();
        let generation = faulted.generation();
        let repeated = recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        assert!(repeated.published.is_none(), "case {i}");
        assert_eq!(session.view().generation(), generation, "case {i}");
        drop(hook);
        let want = expected(&tmp, &before, &[scope], &[marker]);
        assert_eq!(listings(&faulted), want, "case {i}");
        assert_eq!(listings(&open(&tmp.cat())), want, "disk case {i}");
        recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        assert_eq!(
            listings(&session.view()),
            expected(&tmp, &before, &[], &[]),
            "recovery case {i}"
        );
    }
}

#[test]
fn partial_listing_across_workers_discards_the_observed_prefix_and_retains_stale_counts() {
    let tmp = Tmp::new("fault-partial");
    for i in 0..20 {
        tmp.write(&format!("dir/child-{i}/a"), b"old");
    }
    tmp.write("outside", b"stable");
    tmp.write("dir/.git/HEAD", b"ref: refs/heads/main\n");
    let roots = [tmp.tree()];
    let opts = options();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
    let before = session.view();
    let old_dir = directory(&before, &tmp.tree(), b"dir");
    let old_common = before.work_tree(old_dir).unwrap().common_id;
    // Replacing Git metadata during a faulty listing must not clear or replace
    // retained auxiliary state; recovery adopts it with fresh children.
    fs::rename(tmp.at("dir/.git"), tmp.base.join("displaced-git")).unwrap();
    tmp.write("dir/.git/HEAD", b"ref: refs/heads/main\n");
    for i in 0..20 {
        fs::write(tmp.at(&format!("dir/child-{i}/a")), b"new version").unwrap();
    }
    let hook = Hook::set(&tmp.tree(), |point, path| {
        (point == IoPoint::Listing(8) && path == Path::new("dir"))
            .then(|| (IoOp::List, std::io::Error::from_raw_os_error(5)))
    });
    let report = recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    assert!(report.counts.files_read > 0, "prefix really walked");
    let faulted = session.view();
    let dir = directory(&faulted, &tmp.tree(), b"dir");
    assert_eq!(faulted.work_tree(dir).unwrap().common_id, old_common);
    assert_eq!(faulted.entry_count(dir), None);
    assert_eq!(faulted.has_children(dir), None);
    assert_eq!(
        faulted.contents(Target::Inode(dir)),
        Some(Contents::Unreadable)
    );
    assert_eq!(faulted.retained_at(dir), Some(before.generation().sequence));
    drop(hook);
    assert_eq!(
        listings(&faulted),
        expected(&tmp, &before, &["dir"], &["dir"])
    );
    assert_eq!(listings(&faulted), listings(&open(&tmp.cat())));
    recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    assert_eq!(session.view().retained_at(dir), None);
    assert_ne!(session.view().work_tree(dir).unwrap().common_id, old_common);
    assert_eq!(listings(&session.view()), expected(&tmp, &before, &[], &[]));
}

#[test]
fn global_policy_and_sniffer_transitions_under_protection_abort() {
    for sniffer in [false, true] {
        let tmp = Tmp::new(if sniffer {
            "fault-sniffer"
        } else {
            "fault-policy"
        });
        tmp.write("dir/a", b"content");
        let opts = options();
        let roots = [tmp.tree()];
        index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
        let mut session = WriterSession::open(&tmp.cat()).unwrap();
        let before = session.view().generation();
        let mut changed = opts.clone();
        if sniffer {
            changed.sniffer += 1;
        } else {
            changed.global = Some("a\n".into());
        }
        let _hook = Hook::set(&tmp.tree(), |point, path| {
            (point == IoPoint::Directory && path == Path::new("dir"))
                .then(|| (IoOp::List, std::io::Error::from_raw_os_error(5)))
        });
        assert!(matches!(
            recrawl(&mut session, &roots, Refresh::All, &changed),
            Err(crate::IndexError::Coverage { .. })
        ));
        assert_eq!(session.view().generation(), before);
    }
}

#[test]
fn new_opaque_and_replaced_directories_have_distinct_retention_anchors() {
    for replaced in [false, true] {
        let tmp = Tmp::new(if replaced {
            "fault-replaced"
        } else {
            "fault-new"
        });
        tmp.write("parent/stable", b"stable");
        if replaced {
            tmp.write("parent/dir/old", b"previous subtree");
        }
        let opts = options();
        let roots = [tmp.tree()];
        index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
        let mut session = WriterSession::open(&tmp.cat()).unwrap();
        let before = session.view();
        // Keep the old inode allocated so this is definitely a replacement.
        if replaced {
            fs::rename(tmp.at("parent/dir"), tmp.base.join("displaced")).unwrap();
        }
        tmp.write("parent/dir/new", b"new untrusted child");
        let hook = Hook::set(&tmp.tree(), |point, path| {
            (point == IoPoint::Directory && path == Path::new("parent/dir"))
                .then(|| (IoOp::List, std::io::Error::from_raw_os_error(5)))
        });
        let report = recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        assert_eq!(report.protected_scopes, 1);
        let faulted = session.view();
        assert!(
            recrawl(&mut session, &roots, Refresh::All, &opts)
                .unwrap()
                .published
                .is_none(),
            "even a new opaque directory has no invented trustworthy sequence on repeat"
        );
        drop(hook);
        if replaced {
            assert_eq!(
                listings(&faulted),
                expected(&tmp, &before, &["parent"], &["parent"])
            );
        } else {
            let parent = directory(&faulted, &tmp.tree(), b"parent");
            let dir = faulted.name(faulted.lookup(parent, b"dir").unwrap()).child;
            assert_eq!(faulted.entry_count(dir), None);
            assert_eq!(
                faulted.retained_at(dir),
                None,
                "no invented trustworthy old sequence"
            );
            assert_eq!(faulted.children(dir).count(), 0);
            let fresh = expected(&tmp, &before, &[], &[]);
            let path = tmp.at("parent/dir").as_os_str().as_bytes().to_vec();
            let mut want: Vec<_> = fresh
                .into_iter()
                .filter(|r| {
                    r.path == path
                        || !(r.path.starts_with(&path) && r.path.get(path.len()) == Some(&b'/'))
                })
                .collect();
            want.iter_mut().find(|r| r.path == path).unwrap().entries = None;
            assert_eq!(listings(&faulted), want);
        }
        assert_eq!(listings(&faulted), listings(&open(&tmp.cat())));
        recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        assert_eq!(listings(&session.view()), expected(&tmp, &before, &[], &[]));
    }
}

#[test]
fn overlapping_protection_scopes_reduce_to_the_outermost_boundary() {
    let tmp = Tmp::new("fault-overlap");
    tmp.write("dir/deep/a", b"old");
    tmp.write("dir/b", b"old sibling");
    let opts = options();
    let roots = [tmp.tree()];
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
    let before = session.view();
    fs::write(tmp.at("dir/deep/a"), b"untrusted replacement content").unwrap();
    let hook = Hook::set(&tmp.tree(), |point, path| {
        (point == IoPoint::Directory && [Path::new("dir"), Path::new("dir/deep")].contains(&path))
            .then(|| (IoOp::ReadIgnore, std::io::Error::from_raw_os_error(5)))
    });
    let report = recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    assert_eq!(report.coverage_faults.len(), 2);
    assert_eq!(report.protected_scopes, 1);
    drop(hook);
    assert_eq!(
        listings(&session.view()),
        expected(&tmp, &before, &["dir"], &["dir"])
    );
    assert_eq!(listings(&session.view()), listings(&open(&tmp.cat())));
}

#[test]
fn unknown_context_and_unanchored_new_root_faults_block_publication() {
    for (point, operation) in [
        (IoPoint::Root, IoOp::Readlink),
        (IoPoint::Child, IoOp::List),
    ] {
        let tmp = Tmp::new(&format!("fault-unknown-{point:?}"));
        tmp.write("dir/a", b"content");
        let opts = options();
        let roots = [tmp.tree()];
        index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
        let mut session = WriterSession::open(&tmp.cat()).unwrap();
        let before = session.view().generation();
        let _hook = Hook::set(&tmp.tree(), move |at, path| {
            (at == point && (point == IoPoint::Root || path == Path::new("dir")))
                .then(|| (operation, std::io::Error::from_raw_os_error(5)))
        });
        assert!(matches!(
            recrawl(&mut session, &roots, Refresh::All, &opts),
            Err(crate::IndexError::Coverage { .. })
        ));
        assert_eq!(session.view().generation(), before);
    }
    let tmp = Tmp::new("fault-unanchored-root");
    tmp.write("a", b"content");
    let opts = options();
    let roots = [tmp.tree()];
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let other = tmp.base.join("other-root");
    fs::create_dir(&other).unwrap();
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
    let before = session.view().generation();
    let _hook = Hook::set(&other, |point, _| {
        (point == IoPoint::Root).then(|| (IoOp::OpenDir, std::io::Error::from_raw_os_error(2)))
    });
    assert!(matches!(
        recrawl(&mut session, &[tmp.tree(), other], Refresh::All, &opts),
        Err(crate::IndexError::Coverage { .. })
    ));
    assert_eq!(session.view().generation(), before);
}

#[test]
fn missing_existing_root_is_retained_and_root_boundary_edits_under_protection_abort() {
    let tmp = Tmp::new("fault-root-boundary");
    tmp.write("dir/a", b"content");
    let opts = options();
    let roots = [tmp.tree()];
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
    let before = session.view();
    let hook = Hook::set(&tmp.at("dir"), |point, _| {
        (point == IoPoint::Root).then(|| (IoOp::OpenDir, std::io::Error::from_raw_os_error(5)))
    });
    assert!(matches!(
        recrawl(
            &mut session,
            &[tmp.tree(), tmp.at("dir")],
            Refresh::All,
            &opts
        ),
        Err(crate::IndexError::Coverage { .. })
    ));
    // The configured nested root is a walker boundary and emits no Dir event.
    // Fault the root itself so the changed boundary cannot be hidden by
    // retention.
    drop(hook);
    let hook = Hook::set(&tmp.tree(), |point, _| {
        (point == IoPoint::Root).then(|| (IoOp::OpenDir, std::io::Error::from_raw_os_error(2)))
    });
    assert!(matches!(
        recrawl(
            &mut session,
            &[tmp.tree(), tmp.at("dir")],
            Refresh::All,
            &opts
        ),
        Err(crate::IndexError::Coverage { .. })
    ));
    assert_eq!(session.view().generation(), before.generation());
    drop(hook);
    fs::rename(tmp.tree(), tmp.base.join("moved-outside-root")).unwrap();
    let report = recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    assert_eq!(report.protected_scopes, 1);
    let mut want = listings(&before);
    let root = tmp.tree().as_os_str().as_bytes().to_vec();
    let row = want.iter_mut().find(|r| r.path == root).unwrap();
    row.entries = None;
    row.retained_at = Some(before.generation().sequence);
    assert_eq!(listings(&session.view()), want);
    assert_eq!(listings(&open(&tmp.cat())), want);
}

struct ContentHook((u64, u64), crate::observe::ContentIo);
impl Drop for ContentHook {
    fn drop(&mut self) {
        crate::observe::CONTENT_IO_FAULTS
            .lock()
            .unwrap()
            .retain(|item| *item != (self.0, self.1));
    }
}

#[test]
fn content_open_stat_read_and_closing_stat_faults_publish_valid_rows_without_minting_docs() {
    use crate::ContentFault;
    use crate::observe::{CONTENT_IO_FAULTS, ContentIo};
    use std::os::unix::fs::MetadataExt;
    for op in [
        ContentIo::Open,
        ContentIo::Stat,
        ContentIo::Read,
        ContentIo::AfterStat,
    ] {
        let tmp = Tmp::new(&format!("fault-content-{op:?}"));
        let path = tmp.write("a", b"old");
        tmp.write("stable", b"stable");
        let opts = options();
        let roots = [tmp.tree()];
        index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
        let mut session = WriterSession::open(&tmp.cat()).unwrap();
        let before = session.view();
        let id = directory(&before, &tmp.tree(), b"a");
        let next_doc = before.next_doc();
        fs::write(&path, b"new version must be read").unwrap();
        let stat = fs::metadata(&path).unwrap();
        let key = (stat.dev(), stat.ino());
        CONTENT_IO_FAULTS.lock().unwrap().push((key, op));
        let hook = ContentHook(key, op);
        let report = recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        assert!(report.coverage_faults.is_empty());
        assert_eq!(report.protected_scopes, 0);
        assert_eq!(report.content_faults.len(), 1);
        assert!(matches!((&report.content_faults[0].1, op),
            (ContentFault::Open(e), ContentIo::Open) | (ContentFault::Read(e), ContentIo::Read)
            | (ContentFault::Stat(e), ContentIo::Stat | ContentIo::AfterStat) if e.raw_os_error() == Some(5)));
        let current = session.view();
        assert_eq!(directory(&current, &tmp.tree(), b"a"), id);
        assert_eq!(current.state(id), ferret_catalog::ContentState::Fault);
        assert_eq!(current.doc(id), None);
        assert_eq!(current.next_doc(), next_doc);
        // The fresh full checkpoint encounters the same injected real reader
        // error. Its expected state is produced by the actual crawl/build API.
        assert_eq!(listings(&current), expected(&tmp, &before, &[], &[]));
        assert_eq!(listings(&current), listings(&open(&tmp.cat())));
        assert!(
            recrawl(&mut session, &roots, Refresh::All, &opts)
                .unwrap()
                .published
                .is_none()
        );
        drop(hook);
        recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        assert_eq!(listings(&session.view()), expected(&tmp, &before, &[], &[]));
    }
}

#[test]
fn child_lstat_not_found_alone_deletes_the_old_edge_and_directory_subtree() {
    for remove_during_walk in [false, true] {
        let tmp = Tmp::new("fault-vanished-directory");
        tmp.write("dir/deep/a", b"old");
        tmp.write("stable", b"stable");
        let opts = options();
        let roots = [tmp.tree()];
        index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
        let mut session = WriterSession::open(&tmp.cat()).unwrap();
        let before = session.view();
        let dir = directory(&before, &tmp.tree(), b"dir");
        let path = tmp.at("dir");
        let hook = Hook::set(&tmp.tree(), move |point, rel| {
            if point == IoPoint::Child && rel == Path::new("dir") {
                if remove_during_walk {
                    fs::remove_dir_all(&path).unwrap();
                }
                Some((IoOp::Lstat, std::io::Error::from_raw_os_error(2)))
            } else {
                None
            }
        });
        let report = recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        assert_eq!(report.counts.vanished, 1);
        assert!(report.coverage_faults.is_empty());
        assert_eq!(report.protected_scopes, 0);
        assert!(!session.view().is_live_inode(dir));
        assert_eq!(listings(&session.view()), listings(&open(&tmp.cat())));
        if !remove_during_walk {
            // A stable injected disappearance must match the real full builder
            // seeing the same syscall fault, without modelling the deletion
            // rule.
            assert_eq!(listings(&session.view()), expected(&tmp, &before, &[], &[]));
        }
        drop(hook);
        // First-pass root stat/count describe the pre-disappearance listing. A
        // stable retry supplies the same observations as the fresh full oracle.
        recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        assert_eq!(listings(&session.view()), expected(&tmp, &before, &[], &[]));
    }
}

#[test]
fn new_unknown_child_edge_protects_its_old_parent_and_bad_patterns_do_not_protect() {
    for operation in [IoOp::Lstat, IoOp::Readlink] {
        let tmp = Tmp::new(&format!("fault-new-edge-{operation:?}"));
        tmp.write("parent/stable", b"old");
        let opts = options();
        let roots = [tmp.tree()];
        index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
        let mut session = WriterSession::open(&tmp.cat()).unwrap();
        let before = session.view();
        tmp.write("parent/new", b"new");
        let hook = Hook::set(&tmp.tree(), move |point, rel| {
            (point == IoPoint::Child && rel == Path::new("parent/new"))
                .then(|| (operation, std::io::Error::from_raw_os_error(13)))
        });
        let report = recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        assert_eq!(report.protected_scopes, 1);
        drop(hook);
        assert_eq!(
            listings(&session.view()),
            expected(&tmp, &before, &["parent"], &["parent"])
        );
    }
    let tmp = Tmp::new("fault-pattern");
    tmp.write("a.skip", b"ignored");
    tmp.write("b", b"included");
    tmp.write(".ferretignore", b"bad\\\n*.skip\n");
    let opts = options();
    let roots = [tmp.tree()];
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
    let before = session.view();
    let report = recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    assert_eq!(report.counts.pattern_errors, 1);
    assert!(report.coverage_faults.is_empty());
    assert_eq!(report.protected_scopes, 0);
    assert!(report.published.is_none());
    assert_eq!(listings(&session.view()), expected(&tmp, &before, &[], &[]));
}

#[test]
fn a_proved_replaced_root_cannot_retain_and_a_disjoint_root_removal_can_commit() {
    let tmp = Tmp::new("fault-replaced-root");
    tmp.write("a", b"old");
    let opts = options();
    let roots = [tmp.tree()];
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
    let before = session.view().generation();
    fs::rename(tmp.tree(), tmp.base.join("old-root-held")).unwrap();
    tmp.write("new", b"new");
    let hook = Hook::set(&tmp.tree(), |point, _| {
        (point == IoPoint::Directory).then(|| {
            (
                IoOp::List,
                std::io::Error::new(std::io::ErrorKind::InvalidData, "root identity changed"),
            )
        })
    });
    assert!(matches!(
        recrawl(&mut session, &roots, Refresh::All, &opts),
        Err(crate::IndexError::Coverage { .. })
    ));
    assert_eq!(session.view().generation(), before);
    drop(hook);
    drop(session);

    let tmp = Tmp::new("fault-disjoint-root-edit");
    tmp.write("a", b"old");
    let other = tmp.base.join("other");
    fs::create_dir(&other).unwrap();
    fs::write(other.join("b"), b"other").unwrap();
    index(&tmp.cat(), &[tmp.tree(), other], Refresh::All, &opts).unwrap();
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
    let before = session.view();
    let hook = Hook::set(&tmp.tree(), |point, _| {
        (point == IoPoint::Root).then(|| (IoOp::List, std::io::Error::from_raw_os_error(5)))
    });
    let report = recrawl(&mut session, &[tmp.tree()], Refresh::All, &opts).unwrap();
    assert!(report.published.is_some());
    assert_eq!(report.protected_scopes, 1);
    drop(hook);
    assert_eq!(
        listings(&session.view()),
        expected(&tmp, &before, &[""], &[""])
    );
    assert_eq!(listings(&session.view()), listings(&open(&tmp.cat())));
}

#[test]
fn protected_names_survive_last_link_moves_and_fresh_outside_aliases_update_the_shared_inode() {
    for moved in [false, true] {
        let tmp = Tmp::new(if moved {
            "fault-last-link-move"
        } else {
            "fault-fresh-alias"
        });
        let inside = tmp.write("dir/a", b"old payload");
        let outside = tmp.at("outside");
        if !moved {
            fs::hard_link(&inside, &outside).unwrap();
        }
        let opts = options();
        let roots = [tmp.tree()];
        index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
        let mut session = WriterSession::open(&tmp.cat()).unwrap();
        let before = session.view();
        let dir = directory(&before, &tmp.tree(), b"dir");
        let name = before.lookup(dir, b"a").unwrap();
        let inode = before.name(name).child;
        if moved {
            fs::rename(&inside, &outside).unwrap();
        } else {
            fs::write(
                &outside,
                b"new content from the trusted outside observation",
            )
            .unwrap();
        }
        let hook = Hook::set(&tmp.tree(), |point, path| {
            (point == IoPoint::Directory && path == Path::new("dir"))
                .then(|| (IoOp::List, std::io::Error::from_raw_os_error(5)))
        });
        recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        let current = session.view();
        assert_eq!(current.lookup(dir, b"a"), Some(name));
        let root = directory(&current, &tmp.tree(), b"");
        let external = current.lookup(root, b"outside").unwrap();
        assert_ne!(
            external, name,
            "protection wins over inferred singleton rename"
        );
        assert_eq!(current.name(external).child, inode);
        assert_eq!(
            current
                .doc(inode)
                .and_then(|doc| current.doc_references(doc)),
            Some(1)
        );
        drop(hook);
        let mut want = expected(&tmp, &before, &["dir"], &["dir"]);
        let fresh = expected(&tmp, &before, &[], &[]);
        // Namespace comes from the actual pre-fault checkpoint. Physical inode
        // data comes from the actual full checkpoint's trustworthy outside
        // alias.
        let mut physical = fresh
            .iter()
            .find(|r| r.path == outside.as_os_str().as_bytes())
            .unwrap()
            .clone();
        physical.path = inside.as_os_str().as_bytes().to_vec();
        let target = want.iter_mut().find(|r| r.path == physical.path).unwrap();
        *target = physical;
        assert_eq!(listings(&current), want);
        assert_eq!(listings(&current), listings(&open(&tmp.cat())));
        recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        assert_eq!(listings(&session.view()), expected(&tmp, &before, &[], &[]));
    }
}

#[test]
fn faults_below_new_or_replaced_parents_protect_a_proven_unchanged_ancestor() {
    for replaced in [false, true] {
        for listing in [false, true] {
            let tmp = Tmp::new("fault-new-parent");
            tmp.write("stable", b"stable");
            if replaced {
                tmp.write("parent/old", b"old subtree");
            }
            let roots = [tmp.tree()];
            let opts = options();
            index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
            let mut session = WriterSession::open(&tmp.cat()).unwrap();
            let before = session.view();
            if replaced {
                fs::rename(tmp.at("parent"), tmp.base.join("displaced")).unwrap();
            }
            tmp.write("parent/dir/new", b"untrusted");
            tmp.write("parent/new", b"untrusted");
            let hook = Hook::set(&tmp.tree(), move |point, path| {
                (if listing {
                    point == IoPoint::Directory && path == Path::new("parent/dir")
                } else {
                    point == IoPoint::Child && path == Path::new("parent/new")
                })
                .then(|| {
                    (
                        if listing { IoOp::List } else { IoOp::Lstat },
                        std::io::Error::from_raw_os_error(5),
                    )
                })
            });
            let report = recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
            assert_eq!(report.protected_scopes, 1);
            let faulted = session.view();
            assert!(
                recrawl(&mut session, &roots, Refresh::All, &opts)
                    .unwrap()
                    .published
                    .is_none()
            );
            drop(hook);
            assert_eq!(listings(&faulted), expected(&tmp, &before, &[""], &[""]));
            assert_eq!(listings(&faulted), listings(&open(&tmp.cat())));
            recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
            assert_eq!(listings(&session.view()), expected(&tmp, &before, &[], &[]));
        }
    }
}

#[test]
fn an_outer_old_scope_discards_nested_new_opaque_scopes() {
    let tmp = Tmp::new("fault-overlap-opaque");
    tmp.write("parent/stable", b"old");
    let roots = [tmp.tree()];
    let opts = options();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
    let before = session.view();
    tmp.write("parent/new/child", b"untrusted");
    let hook = Hook::set(&tmp.tree(), |point, path| {
        (point == IoPoint::Directory
            && [Path::new("parent"), Path::new("parent/new")].contains(&path))
        .then(|| (IoOp::List, std::io::Error::from_raw_os_error(5)))
    });
    let report = recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    assert_eq!(report.coverage_faults.len(), 2);
    assert_eq!(report.protected_scopes, 1);
    let faulted = session.view();
    drop(hook);
    assert_eq!(
        listings(&faulted),
        expected(&tmp, &before, &["parent"], &["parent"])
    );
    assert_eq!(listings(&faulted), listings(&open(&tmp.cat())));
}

#[test]
fn a_fault_after_a_directory_move_retains_a_live_old_ancestor() {
    for (remove_parent, new_parent) in [(false, false), (true, false), (true, true)] {
        let tmp = Tmp::new("fault-moved-directory");
        tmp.write("old/dir/child", b"old subtree");
        if !new_parent {
            tmp.write("new/stable", b"stable");
        }
        let roots = [tmp.tree()];
        let opts = options();
        index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
        let mut session = WriterSession::open(&tmp.cat()).unwrap();
        let before = session.view();
        if new_parent {
            tmp.write("new/stable", b"stable");
        }
        fs::rename(tmp.at("old/dir"), tmp.at("new/dir")).unwrap();
        if remove_parent {
            fs::remove_dir(tmp.at("old")).unwrap();
        }
        let hook = Hook::set(&tmp.tree(), |point, path| {
            (point == IoPoint::Directory && path == Path::new("new/dir"))
                .then(|| (IoOp::List, std::io::Error::from_raw_os_error(5)))
        });
        let report = recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        assert_eq!(report.protected_scopes, 1);
        let faulted = session.view();
        drop(hook);
        assert_eq!(listings(&faulted), expected(&tmp, &before, &[""], &[""]));
        assert_eq!(listings(&faulted), listings(&open(&tmp.cat())));
        recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        assert_eq!(listings(&session.view()), expected(&tmp, &before, &[], &[]));
    }
}
