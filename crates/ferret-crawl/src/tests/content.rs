//! [`Documents`] on a real indexed tree: it returns a document's bytes only
//! while the file and every directory above it are the catalogued ones.

use std::fs;
use std::os::unix::fs::symlink;

use ferret_catalog::{Catalog, DocId};

use super::index::Tmp;
use crate::{ContentFault, Documents, IndexOptions, Refresh, index};

/// Indexes `files` under a fresh tree; returns the catalog and each file's
/// DocId, in `files` order.
fn indexed(tmp: &Tmp, files: &[(&str, &[u8])]) -> (Catalog, Vec<DocId>) {
    for (rel, bytes) in files {
        tmp.write(rel, bytes);
    }
    index(
        &tmp.cat(),
        &[tmp.tree()],
        Refresh::All,
        &IndexOptions::default(),
    )
    .unwrap();
    let catalog = Catalog::open(&tmp.cat()).unwrap().unwrap();
    catalog.load_all().unwrap();
    let docs = files
        .iter()
        .map(|(rel, _)| {
            let path = tmp.at(rel);
            let resolved = catalog
                .resolve(path.as_os_str().as_encoded_bytes())
                .unwrap();
            let ferret_catalog::Target::Inode(inode) = resolved.target else {
                panic!("{rel} is not catalogued");
            };
            catalog.doc(inode).unwrap()
        })
        .collect();
    (catalog, docs)
}

fn read(documents: &mut Documents, catalog: &Catalog, doc: DocId) -> Result<Vec<u8>, ContentFault> {
    let mut out = vec![b'x'; 3];
    documents.read(catalog, doc, &mut out).map(|()| out)
}

const FILES: [(&str, &[u8]); 3] = [
    ("a/b/one.txt", b"first file\n"),
    ("a/b/two.txt", b"second\n"),
    ("top.txt", b"at the root\n"),
];

#[test]
fn every_document_reads_back_whole() {
    let tmp = Tmp::new("content-read");
    let (catalog, docs) = indexed(&tmp, &FILES);
    let mut documents = Documents::new(&catalog).unwrap();
    for ((_, bytes), &doc) in FILES.iter().zip(&docs) {
        assert_eq!(read(&mut documents, &catalog, doc).unwrap(), *bytes);
    }
    assert_eq!(documents.files_read, 3);
    assert!(
        read(&mut documents, &catalog, catalog.next_doc()).is_err(),
        "no such document"
    );
}

#[test]
fn a_changed_file_is_refused() {
    let tmp = Tmp::new("content-changed");
    let (catalog, docs) = indexed(&tmp, &FILES);
    fs::write(tmp.at("a/b/one.txt"), b"rewritten, longer\n").unwrap();
    let mut documents = Documents::new(&catalog).unwrap();
    assert!(matches!(
        read(&mut documents, &catalog, docs[0]),
        Err(ContentFault::Changed)
    ));
    assert_eq!(read(&mut documents, &catalog, docs[1]).unwrap(), FILES[1].1);
}

#[test]
fn a_directory_swapped_for_a_symlink_or_a_copy_is_refused() {
    let tmp = Tmp::new("content-swapped");
    let (catalog, docs) = indexed(&tmp, &FILES);
    // `a` becomes a symlink to the very same directory, moved: the path
    // resolves to the catalogued inodes, but only by following a link.
    fs::rename(tmp.at("a"), tmp.base.join("elsewhere")).unwrap();
    symlink(tmp.base.join("elsewhere"), tmp.at("a")).unwrap();
    let mut documents = Documents::new(&catalog).unwrap();
    assert!(read(&mut documents, &catalog, docs[0]).is_err());
    assert_eq!(read(&mut documents, &catalog, docs[2]).unwrap(), FILES[2].1);

    // `a/b` is a fresh directory holding a new link to the same file: the
    // directory's inode differs, and the link moved the file's ctime.
    fs::remove_file(tmp.at("a")).unwrap();
    fs::rename(tmp.base.join("elsewhere"), tmp.at("a")).unwrap();
    assert_eq!(
        read(&mut Documents::new(&catalog).unwrap(), &catalog, docs[0]).unwrap(),
        FILES[0].1
    );
    fs::rename(tmp.at("a/b"), tmp.at("a/old")).unwrap();
    fs::create_dir(tmp.at("a/b")).unwrap();
    fs::hard_link(tmp.at("a/old/one.txt"), tmp.at("a/b/one.txt")).unwrap();
    let mut documents = Documents::new(&catalog).unwrap();
    assert!(matches!(
        read(&mut documents, &catalog, docs[0]),
        Err(ContentFault::Changed)
    ));
}

/// Warm the sibling cache before moving its parent or a higher ancestor.
/// A descriptor for the moved tree must not verify its former path.
#[test]
fn a_cached_directory_is_revalidated_after_replacement() {
    for ancestor in ["a/b", "a"] {
        for link in [false, true] {
            let tmp = Tmp::new(if link { "cached-link" } else { "cached-copy" });
            let (catalog, docs) = indexed(&tmp, &FILES);
            let mut documents = Documents::new(&catalog).unwrap();
            assert_eq!(read(&mut documents, &catalog, docs[0]).unwrap(), FILES[0].1);
            fs::rename(tmp.at(ancestor), tmp.base.join("moved")).unwrap();
            if link {
                symlink(tmp.base.join("moved"), tmp.at(ancestor)).unwrap();
            } else {
                fs::create_dir(tmp.at(ancestor)).unwrap();
            }
            assert!(
                read(&mut documents, &catalog, docs[1]).is_err(),
                "{ancestor}, link={link}"
            );
            assert_eq!(read(&mut documents, &catalog, docs[2]).unwrap(), FILES[2].1);
        }
    }
}

/// Cancellation after chunks have been read must interrupt the same checked
/// reader production queries use, rather than waiting for the complete file.
#[test]
fn current_reads_check_cancellation_between_chunks() {
    use std::cell::Cell;
    let tmp = Tmp::new("content-cancel-chunks");
    let (catalog, _) = indexed(&tmp, &[("a.txt", b"alpha")]);
    let path = tmp.at("a.txt");
    fs::File::create(&path).unwrap().set_len(8 << 20).unwrap();
    let name = catalog
        .resolve(path.as_os_str().as_encoded_bytes())
        .unwrap()
        .name
        .unwrap();
    let calls = Cell::new(0);
    let cancelled = || {
        calls.set(calls.get() + 1);
        calls.get() >= 4
    };
    let mut reader = Documents::by_name(&catalog).unwrap();
    let mut bytes = Vec::new();
    let result = reader.read_current_name_until(&catalog, name, &mut bytes, &cancelled);
    assert!(
        result.is_err(),
        "completed {} bytes despite cancellation",
        bytes.len()
    );
    assert!(
        bytes.len() >= 256 << 10,
        "cancel during, not before, the read"
    );
    assert!(bytes.len() < 8 << 20);
    assert_eq!(reader.files_read, 0);
}
