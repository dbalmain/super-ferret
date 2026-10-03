//! Real-tree recrawls, real durable writer and disk reader. Every prefix is
//! compared with a fresh full crawl/checkpoint using M3's semantic oracle.
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use ferret_catalog::{Catalog, InoId, WriterSession};

use super::index::Tmp;
use crate::{IndexOptions, Refresh, index, recrawl};

#[path = "../../../ferret-catalog/tests/support/listing.rs"]
mod checkpoint_oracle;
use checkpoint_oracle::listings;

fn options() -> IndexOptions {
    IndexOptions {
        workers: 2,
        ..IndexOptions::default()
    }
}
fn open(path: &Path) -> Catalog {
    let c = Catalog::open(path).unwrap().unwrap();
    c.load_all().unwrap();
    c
}
fn oracle(tmp: &Tmp, roots: &[PathBuf], options: &IndexOptions, effective: &Catalog) {
    let path = tmp.base.join("oracle");
    if path.exists() {
        fs::remove_dir_all(&path).unwrap();
    }
    index(&path, roots, Refresh::All, options).unwrap();
    let full = open(&path);
    assert_eq!(listings(effective), listings(&full));
    assert_eq!(listings(effective), listings(&open(&tmp.cat())));
}
fn file(c: &Catalog, root: &Path, name: &[u8]) -> InoId {
    let root = c
        .roots()
        .find(|(_, p)| *p == root.as_os_str().as_bytes())
        .unwrap()
        .0;
    c.name(c.lookup(root, name).unwrap()).child
}

#[test]
fn unchanged_pass_writes_zero_bytes_and_publishes_no_generation() {
    let tmp = Tmp::new("recrawl-unchanged");
    tmp.write("a", b"content");
    let roots = [tmp.tree()];
    let opts = options();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
    let previous = session.view().generation();
    let files: Vec<_> = fs::read_dir(tmp.cat())
        .unwrap()
        .map(|e| {
            let path = e.unwrap().path();
            let meta = fs::metadata(&path).unwrap();
            (
                path.clone(),
                meta.len(),
                meta.mtime(),
                meta.mtime_nsec(),
                fs::read(path).unwrap(),
            )
        })
        .collect();
    for _ in 0..2 {
        let report = recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        assert!(report.published.is_none());
        assert_eq!(session.view().generation(), previous);
        assert_eq!(report.counts.files_read, 0);
        for (path, size, sec, nsec, bytes) in &files {
            let meta = fs::metadata(path).unwrap();
            assert_eq!(
                (meta.len(), meta.mtime(), meta.mtime_nsec()),
                (*size, *sec, *nsec)
            );
            assert_eq!(fs::read(path).unwrap(), *bytes);
        }
        oracle(&tmp, &roots, &opts, &session.view());
    }
}

#[test]
fn metadata_only_change_keeps_docid_and_inoid() {
    let tmp = Tmp::new("recrawl-metadata");
    let path = tmp.write("a", b"content");
    let roots = [tmp.tree()];
    let opts = options();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
    let old = session.view();
    let id = file(&old, &roots[0], b"a");
    let doc = old.doc(id);
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    let current = session.view();
    assert_eq!(file(&current, &roots[0], b"a"), id);
    assert_eq!(current.doc(id), doc);
    assert_eq!(old.generation().checkpoint, current.generation().checkpoint);
    oracle(&tmp, &roots, &opts, &current);
}

#[test]
fn rename_and_moving_last_hard_link_keep_inode_and_document() {
    let tmp = Tmp::new("recrawl-rename");
    tmp.write("a", b"content");
    let roots = [tmp.tree()];
    let opts = options();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = WriterSession::open(&tmp.cat()).unwrap();
    let old = session.view();
    let id = file(&old, &roots[0], b"a");
    let doc = old.doc(id);
    for (from, to) in [("a", "b"), ("b", "c"), ("c", "a")] {
        fs::rename(tmp.at(from), tmp.at(to)).unwrap();
        recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        let current = session.view();
        assert_eq!(file(&current, &roots[0], to.as_bytes()), id);
        assert_eq!(current.doc(id), doc);
        oracle(&tmp, &roots, &opts, &current);
    }
}
