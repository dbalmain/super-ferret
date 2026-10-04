//! Scoped faults use the real walker, batch API, durable writer and disk reader.
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ferret_catalog::{Catalog, Contents, InoId, Target, WriterSession};
use super::index::Tmp;
use crate::{IndexOptions, IoOp, Refresh, index, recrawl};
use crate::walk::{IO_HOOKS, IoHook, IoPoint};
#[path = "../../../ferret-catalog/tests/support/listing.rs"]
mod checkpoint_oracle;
use checkpoint_oracle::{Listing, listings};

struct Hook(PathBuf);
impl Hook {
    fn set(root: &Path, body: impl Fn(IoPoint, &Path) -> Option<(IoOp, std::io::Error)> + Send + Sync + 'static) -> Self {
        let hook: IoHook = Arc::new(body);
        IO_HOOKS.lock().unwrap().push((root.to_owned(), hook));
        Self(root.to_owned())
    }
}
impl Drop for Hook {
    fn drop(&mut self) { IO_HOOKS.lock().unwrap().retain(|(root, _)| *root != self.0); }
}
fn options() -> IndexOptions { IndexOptions { workers: 4, ..IndexOptions::default() } }
fn open(path: &Path) -> Catalog {
    let c = Catalog::open(path).unwrap().unwrap(); c.load_all().unwrap(); c
}
fn directory(c: &Catalog, root: &Path, name: &[u8]) -> InoId {
    let r = c.roots().find(|(_, p)| *p == root.as_os_str().as_bytes()).unwrap().0;
    if name.is_empty() { r } else { c.name(c.lookup(r, name).unwrap()).child }
}
fn expected(tmp: &Tmp, before: &Catalog, scopes: &[&str], markers: &[&str]) -> Vec<Listing> {
    let path = tmp.base.join("oracle");
    if path.exists() { fs::remove_dir_all(&path).unwrap(); }
    index(&path, &[tmp.tree()], Refresh::All, &options()).unwrap();
    let paths = |names: &[&str]| names.iter().map(|name| (if name.is_empty() { tmp.tree() } else { tmp.at(name) }).as_os_str().as_bytes().to_vec()).collect::<Vec<_>>();
    retained_listings(&open(&path), before, &paths(scopes), &paths(markers))
}
/// Expected retained state comes from actual pre-fault rows. Fresh trustworthy
/// rows come from a separate real full checkpoint, not a model of reconciliation.
pub(crate) fn retained_listings(
    fresh: &Catalog,
    before: &Catalog,
    scopes: &[Vec<u8>],
    markers: &[Vec<u8>],
) -> Vec<Listing> {
    let inside = |path: &[u8]| scopes.iter().any(|scope| path == scope
        || (path.starts_with(scope) && path.get(scope.len()) == Some(&b'/')));
    let old = listings(before);
    let mut rows: Vec<_> = listings(fresh).into_iter().filter(|r| !inside(&r.path)).collect();
    rows.extend(old.iter().filter(|r| inside(&r.path)).cloned());
    for row in &mut rows {
        if markers.contains(&row.path) {
            row.entries = None;
            row.retained_at = Some(old.iter().find(|r| r.path == row.path)
                .and_then(|r| r.retained_at).unwrap_or(before.generation().sequence));
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
        (IoPoint::Child, "dir/a", IoOp::Lstat, 13, "dir/a", "dir"),
        (IoPoint::Child, "dir/a", IoOp::Readlink, 2, "dir/a", "dir"),
        (IoPoint::Directory, "dir", IoOp::ReadIgnore, 13, "dir", "dir"),
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
        tmp.write("dir/a", b"content"); tmp.write("dir/deep/b", b"nested"); tmp.write("stable", b"stable");
        let opts = options(); let roots = [tmp.tree()];
        index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
        let mut session = WriterSession::open(&tmp.cat()).unwrap(); let before = session.view();
        let hook = Hook::set(&tmp.tree(), move |at, rel| (at == point && rel == Path::new(path))
            .then(|| (op, std::io::Error::from_raw_os_error(error))));
        let report = recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        assert_eq!(report.coverage_faults.len(), 1, "case {i}");
        assert_eq!(report.protected_scopes, 1, "case {i}");
        let faulted = session.view(); let generation = faulted.generation();
        let repeated = recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        assert!(repeated.published.is_none(), "case {i}");
        assert_eq!(session.view().generation(), generation, "case {i}");
        drop(hook);
        let want = expected(&tmp, &before, &[scope], &[marker]);
        assert_eq!(listings(&faulted), want, "case {i}");
        assert_eq!(listings(&open(&tmp.cat())), want, "disk case {i}");
        recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        assert_eq!(listings(&session.view()), expected(&tmp, &before, &[], &[]), "recovery case {i}");
    }
}

#[test]
fn partial_listing_across_workers_discards_the_observed_prefix_and_retains_stale_counts() {
    let tmp = Tmp::new("fault-partial");
    for i in 0..20 { tmp.write(&format!("dir/child-{i}/a"), b"old"); }
    tmp.write("outside", b"stable");
    let roots = [tmp.tree()]; let opts = options();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = WriterSession::open(&tmp.cat()).unwrap(); let before = session.view();
    for i in 0..20 { fs::write(tmp.at(&format!("dir/child-{i}/a")), b"new version").unwrap(); }
    let hook = Hook::set(&tmp.tree(), |point, path| (point == IoPoint::Listing(8) && path == Path::new("dir"))
        .then(|| (IoOp::List, std::io::Error::from_raw_os_error(5))));
    let report = recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    assert!(report.counts.files_read > 0, "prefix really walked");
    let faulted = session.view(); let dir = directory(&faulted, &tmp.tree(), b"dir");
    assert_eq!(faulted.entry_count(dir), None);
    assert_eq!(faulted.has_children(dir), None);
    assert_eq!(faulted.contents(Target::Inode(dir)), Some(Contents::Unreadable));
    assert_eq!(faulted.retained_at(dir), Some(before.generation().sequence));
    drop(hook);
    assert_eq!(listings(&faulted), expected(&tmp, &before, &["dir"], &["dir"]));
    assert_eq!(listings(&faulted), listings(&open(&tmp.cat())));
    recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    assert_eq!(session.view().retained_at(dir), None);
    assert_eq!(listings(&session.view()), expected(&tmp, &before, &[], &[]));
}

#[test]
fn global_policy_and_sniffer_transitions_under_protection_abort() {
    for sniffer in [false, true] {
        let tmp = Tmp::new(if sniffer { "fault-sniffer" } else { "fault-policy" });
        tmp.write("dir/a", b"content");
        let opts = options(); let roots = [tmp.tree()];
        index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
        let mut session = WriterSession::open(&tmp.cat()).unwrap(); let before = session.view().generation();
        let mut changed = opts.clone();
        if sniffer { changed.sniffer += 1; } else { changed.global = Some("a\n".into()); }
        let _hook = Hook::set(&tmp.tree(), |point, path| (point == IoPoint::Directory && path == Path::new("dir"))
            .then(|| (IoOp::List, std::io::Error::from_raw_os_error(5))));
        assert!(matches!(recrawl(&mut session, &roots, Refresh::All, &changed), Err(crate::IndexError::Coverage { .. })));
        assert_eq!(session.view().generation(), before);
    }
}
