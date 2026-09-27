//! A corrupt or truncated snapshot is an error, never a panic.

use super::{Scratch, commit, dir_stat, file_stat, hash, link_stat};
use std::path::Path;

use crate::format::SECTIONS;
use crate::{Catalog, Content, DecodeError, InoId, NameId, OpenError, Section, WorkTreeKind};

/// A small snapshot with every section non-empty, built in its own scratch
/// directory `name`.
fn sample(name: &str) -> Vec<u8> {
    let scratch = Scratch::new(name);
    commit(&scratch.path, |txn| {
        let mut w = txn.batch();
        let root = w.root(b"/s", dir_stat(1));
        let sub = w.dir(root, b"sub", dir_stat(2));
        let skip = w.traversed_dir(sub, b"skip", dir_stat(3));
        w.file(sub, b"a.rs", file_stat(10), Content::Hashed(hash(1)));
        w.file(skip, b"b.bin", file_stat(11), Content::Binary);
        w.file(root, b"c", file_stat(12), Content::Hashed(hash(2)));
        w.symlink(root, b"d", link_stat(13), b"sub/a.rs");
        w.work_tree(sub, WorkTreeKind::Linked, b"/repo/.git", (1, 2));
        let other = w.root(b"/t", dir_stat(4));
        w.file(other, b"e", file_stat(14), Content::Fault);
        txn.add(w);
    });
    std::fs::read(scratch.path.join("catalog")).unwrap()
}

/// Reads everything a catalog offers through every accessor whose sections
/// are loaded. A panic anywhere here is the failure validation exists to
/// prevent.
fn exercise(catalog: &Catalog) -> usize {
    use Section::*;
    let has = |sections: &[Section]| sections.iter().all(|&s| catalog.is_loaded(s));
    let mut touched = 0;
    let mut path = Vec::new();
    if has(&[Names, NameHeap]) {
        let paths = has(&[DirNames, Roots, Strings]);
        for (id, bytes) in catalog.names() {
            if paths {
                path.clear();
                catalog.path(id, &mut path);
            }
            let name = catalog.name(id);
            touched += bytes.len()
                + path.len()
                + usize::from(catalog.lookup(name.parent, bytes).is_some());
        }
        for offset in 0..=catalog.name_heap().len() {
            touched += catalog.name_at(offset).map_or(0, |n| n.0 as usize);
        }
        for dir in 0..catalog.dir_count() {
            touched += catalog.children(InoId(dir)).count();
        }
    }
    for id in (0..catalog.inode_count()).map(InoId) {
        if has(&[Inodes, States]) {
            touched += catalog.inode(id).doc.map_or(0, |d| d.0 as usize);
        }
        // Reads a single row when the section is not loaded; never fails on
        // a file that has not changed since it was opened.
        let inode = catalog.read_inode(id).unwrap();
        if has(&[Docs]) {
            touched += inode
                .doc
                .and_then(|d| catalog.doc_hash(d))
                .map_or(0, |h| h[0] as usize);
        }
        if has(&[Links, Strings]) {
            touched += catalog.link_target(id).map_or(0, <[u8]>::len);
            touched += catalog.kind(id) as usize;
        }
        if has(&[WorkTrees, Strings]) {
            touched += catalog.work_tree(id).map_or(0, |w| w.common_dir.len());
        }
    }
    for dir in (0..catalog.dir_count()).map(InoId) {
        if has(&[DirNames, Names, NameHeap, Roots, Strings]) {
            path.clear();
            catalog.dir_path(dir, &mut path);
            touched += path.len();
        }
        if has(&[Traversed]) {
            touched += usize::from(catalog.is_traversed(dir));
        }
        if has(&[DirNames]) {
            touched += catalog.dir_name(dir).map_or(0, |n| n.0 as usize);
        }
    }
    if has(&[Roots, Strings]) {
        touched += catalog.roots().count();
    }
    if has(&[WorkTrees, Strings]) {
        touched += catalog.work_trees().count();
    }
    if has(&[Docs]) {
        touched += catalog.docs().count();
    }
    touched
}

/// Writes `bytes` as a catalog file in `dir`, for the lazy reader.
fn write_catalog(dir: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("catalog"), bytes).unwrap();
}

/// Opens `dir` lazily and loads `sections`: `None` when the open or the load
/// is refused.
fn lazy(dir: &Path, sections: &[Section]) -> Option<Catalog> {
    let catalog = Catalog::open(dir).ok()??;
    catalog.load(sections).ok()?;
    Some(catalog)
}

/// Every name is found by `lookup` in its parent under its own bytes: the
/// binary search agrees with iteration. Validation must guarantee this for
/// any file it accepts, not only avoid panics.
fn assert_lookups_agree(catalog: &Catalog, context: &str) {
    for (id, bytes) in catalog.names() {
        assert_eq!(
            catalog.lookup(catalog.name(id).parent, bytes),
            Some(id),
            "{context}: lookup of {:?} disagrees with iteration",
            String::from_utf8_lossy(bytes)
        );
    }
}

#[test]
fn the_sample_decodes_and_every_section_is_populated() {
    let catalog = Catalog::from_bytes(sample("decode-populated")).unwrap();
    assert!(exercise(&catalog) > 0);
    assert_lookups_agree(&catalog, "sample");
    assert_eq!(catalog.roots().count(), 2);
    assert_eq!(catalog.work_trees().count(), 1);
    assert_eq!(catalog.doc_count(), 2);
    assert!(catalog.link_target(catalog.name(NameId(1)).child).is_some());
}

#[test]
fn every_truncation_is_an_error() {
    let bytes = sample("decode-truncate");
    let scratch = Scratch::new("decode-truncate-lazy");
    for len in 0..bytes.len() {
        assert!(
            Catalog::from_bytes(bytes[..len].to_vec()).is_err(),
            "truncated to {len}"
        );
        // The lazy reader refuses at open: the table no longer tiles the
        // file, so no section is ever read from a truncated one.
        write_catalog(&scratch.path, &bytes[..len]);
        assert!(
            Catalog::open(&scratch.path).is_err(),
            "lazily opened truncated to {len}"
        );
    }
    let mut longer = bytes;
    longer.push(0);
    write_catalog(&scratch.path, &longer);
    assert!(matches!(
        Catalog::open(&scratch.path),
        Err(OpenError::Decode(DecodeError::Layout))
    ));
    assert_eq!(Catalog::from_bytes(longer).err(), Some(DecodeError::Layout));
}

#[test]
fn a_file_truncated_under_an_open_reader_is_an_error_on_load() {
    // The writer only renames, but a file truncated in place (by anything
    // else) must fail the next section read, not hand back short bytes.
    let bytes = sample("decode-shrink");
    let scratch = Scratch::new("decode-shrink-lazy");
    write_catalog(&scratch.path, &bytes);
    let catalog = Catalog::open(&scratch.path).unwrap().unwrap();
    catalog.load(&[Section::Names]).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(scratch.path.join("catalog"))
        .unwrap()
        .set_len(bytes.len() as u64 - 30)
        .unwrap();
    assert!(matches!(
        catalog.load(&[Section::Docs]),
        Err(OpenError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof
    ));
    assert!(catalog.is_loaded(Section::Names) && !catalog.is_loaded(Section::Docs));
}

#[test]
fn every_single_bit_flip_is_an_error_or_reads_safely() {
    let bytes = sample("decode-flip");
    let scratch = Scratch::new("decode-flip-lazy");
    let (mut rejected, mut accepted) = (0, 0);
    for at in 0..bytes.len() {
        for bit in 0..8 {
            let mut flipped = bytes.clone();
            flipped[at] ^= 1 << bit;
            let context = format!("flip at {at} bit {bit}");
            write_catalog(&scratch.path, &flipped);
            // Loading any one section (with what it needs) either fails or
            // leaves every accessor it enables safe.
            for section in SECTIONS {
                if let Some(catalog) = lazy(&scratch.path, &[section]) {
                    exercise(&catalog);
                }
            }
            // Loaded lazily, a whole file is accepted exactly when the
            // whole-file decoder accepts it.
            let whole = lazy(&scratch.path, &SECTIONS);
            match Catalog::from_bytes(flipped) {
                Err(_) => {
                    assert!(whole.is_none(), "{context}: accepted only lazily");
                    rejected += 1;
                }
                Ok(catalog) => {
                    assert!(whole.is_some(), "{context}: refused only lazily");
                    exercise(&catalog);
                    assert_lookups_agree(&catalog, &context);
                    accepted += 1;
                    // The header and section table are fully checked.
                    assert!(
                        at >= 200 || (12..20).contains(&at),
                        "flip at {at} bit {bit} accepted"
                    );
                }
            }
        }
    }
    // Most flips land in values nothing indexes by (times, hashes, bytes of a
    // name), which decode reads back as different values.
    assert!(
        rejected > 0 && accepted > 0,
        "{rejected} rejected, {accepted} accepted"
    );
}

#[test]
fn a_corrupt_section_fails_only_the_load_that_reads_it() {
    // A flipped doc id (made to repeat its predecessor) is invisible to a
    // name query, which never reads the docs; loading them refuses.
    let mut bytes = sample("decode-partial");
    let docs = Catalog::from_bytes(bytes.clone()).unwrap();
    assert_eq!(docs.doc_count(), 2);
    let table = 24 + Section::Docs as usize * 16;
    let start = u64::from_le_bytes(bytes[table..table + 8].try_into().unwrap()) as usize;
    let first = bytes[start..start + 4].to_vec();
    bytes[start + 20..start + 24].copy_from_slice(&first);
    let scratch = Scratch::new("decode-partial-lazy");
    write_catalog(&scratch.path, &bytes);

    let catalog = Catalog::open(&scratch.path).unwrap().unwrap();
    let names = [
        Section::Names,
        Section::NameHeap,
        Section::DirNames,
        Section::Roots,
    ];
    catalog.load(&names).unwrap();
    assert!(exercise(&catalog) > 0);
    assert!(matches!(
        catalog.load(&[Section::Docs]),
        Err(OpenError::Decode(DecodeError::Corrupt("docs")))
    ));
    assert!(!catalog.is_loaded(Section::Docs));
}

#[test]
fn a_section_loads_what_it_is_checked_against() {
    // The dir-names check reads the name rows, which read the heap; roots
    // read dir names and strings. Loading the dependent loads the rest, so a
    // cross-section check never runs against a section that is not there.
    let scratch = Scratch::new("decode-needs");
    write_catalog(&scratch.path, &sample("decode-needs-sample"));
    let catalog = Catalog::open(&scratch.path).unwrap().unwrap();
    assert!(SECTIONS.iter().all(|&s| !catalog.is_loaded(s)));
    assert_eq!(catalog.bytes_read(), 200);
    catalog.load(&[Section::Roots]).unwrap();
    let loaded: Vec<Section> = SECTIONS
        .into_iter()
        .filter(|&s| catalog.is_loaded(s))
        .collect();
    assert_eq!(
        loaded,
        [
            Section::Names,
            Section::NameHeap,
            Section::DirNames,
            Section::Roots,
            Section::Strings
        ]
    );
}

#[test]
fn a_directory_whose_name_points_upwards_is_rejected() {
    // The one corruption that would make a path walk loop: a directory's
    // name edge whose parent is not lower-numbered. Root 0 holds `a` and
    // `sub` (dir 1), and `sub` holds `x`, so rewriting `sub`'s edge to name
    // itself as parent keeps the name rows sorted, and only the dir-names
    // check can catch it.
    let scratch = Scratch::new("decode-loop");
    let catalog = commit(&scratch.path, |txn| {
        let mut w = txn.batch();
        let root = w.root(b"/s", dir_stat(1));
        w.file(root, b"a", file_stat(10), Content::Unindexed);
        let sub = w.dir(root, b"sub", dir_stat(2));
        w.file(sub, b"x", file_stat(11), Content::Unindexed);
        txn.add(w);
    });
    let sub = catalog.dir_name(InoId(1)).unwrap();
    assert_eq!((sub, catalog.name(sub).parent), (NameId(1), InoId(0)));

    let mut bytes = std::fs::read(scratch.path.join("catalog")).unwrap();
    let names_start = u64::from_le_bytes(bytes[24..32].try_into().unwrap()) as usize;
    let parent_field = names_start + sub.0 as usize * 12;
    bytes[parent_field..parent_field + 4].copy_from_slice(&1u32.to_le_bytes());
    assert_eq!(
        Catalog::from_bytes(bytes).err(),
        Some(DecodeError::Corrupt("dir names"))
    );
}

#[test]
fn siblings_out_of_order_are_rejected() {
    // Root /s holds c, d and sub. Rewriting c to g leaves every offset,
    // parent and terminator valid, but the siblings read g, d, sub, and
    // `lookup(root, "d")` would binary-search past d and miss it.
    let scratch = Scratch::new("decode-order");
    let catalog = commit(&scratch.path, |txn| {
        let mut w = txn.batch();
        let root = w.root(b"/s", dir_stat(1));
        w.file(root, b"c", file_stat(10), Content::Unindexed);
        w.file(root, b"d", file_stat(11), Content::Unindexed);
        w.dir(root, b"sub", dir_stat(2));
        txn.add(w);
    });
    assert_eq!(catalog.name(NameId(0)).bytes, b"c");

    let mut bytes = std::fs::read(scratch.path.join("catalog")).unwrap();
    let heap_start = u64::from_le_bytes(bytes[40..48].try_into().unwrap()) as usize;
    assert_eq!(bytes[heap_start], b'c');
    bytes[heap_start] = b'g';
    assert_eq!(
        Catalog::from_bytes(bytes).err(),
        Some(DecodeError::Corrupt("name order"))
    );
}

#[test]
fn header_errors_say_what_is_wrong() {
    let bytes = sample("decode-header");
    let mut magic = bytes.clone();
    magic[0] = b'X';
    assert_eq!(
        Catalog::from_bytes(magic).err(),
        Some(DecodeError::NotACatalog)
    );
    let mut version = bytes;
    version[8..12].copy_from_slice(&99u32.to_le_bytes());
    assert_eq!(
        Catalog::from_bytes(version).err(),
        Some(DecodeError::Version(99))
    );
}

/// Every public accessor of catalog content, keyed by the one section its doc
/// says it needs, exercised over every id the sample has. `None` is an
/// accessor that needs no section (the table, or a read that loads for
/// itself). The load-state probes `is_loaded` and `bytes_read` are left out:
/// they report on the reader and read no section.
type Accessor = (Option<Section>, &'static str, fn(&Catalog) -> usize);

const ACCESSORS: &[Accessor] = {
    use Section::*;
    fn inodes(c: &Catalog) -> impl Iterator<Item = InoId> {
        (0..c.inode_count()).map(InoId)
    }
    fn dirs(c: &Catalog) -> impl Iterator<Item = InoId> {
        (0..c.dir_count()).map(InoId)
    }
    fn ids(c: &Catalog) -> impl Iterator<Item = NameId> {
        (0..c.name_count()).map(NameId)
    }
    &[
        (None, "counts", |c| {
            (c.dir_count() + c.inode_count() + c.name_count() + c.doc_count()) as usize
                + c.next_doc().0 as usize
                + c.sniffer_version() as usize
                + c.section_sizes().map(|(_, len)| len as usize).sum::<usize>()
        }),
        (None, "read_inode", |c| {
            inodes(c)
                .map(|i| c.read_inode(i).unwrap().stat.size as usize)
                .sum()
        }),
        (Some(NameHeap), "name_heap", |c| c.name_heap().len()),
        (Some(Names), "names", |c| {
            c.names().map(|(_, b)| b.len()).sum()
        }),
        (Some(Names), "name", |c| {
            ids(c).map(|i| c.name(i).bytes.len()).sum()
        }),
        (Some(Names), "name_start", |c| {
            ids(c).map(|i| c.name_start(i)).sum()
        }),
        (Some(Names), "child", |c| {
            ids(c).map(|i| c.child(i).0 as usize).sum()
        }),
        (Some(Names), "name_at", |c| {
            (0..=c.name_heap().len())
                .filter_map(|o| c.name_at(o))
                .count()
        }),
        (Some(Names), "children", |c| {
            dirs(c).map(|d| c.children(d).count()).sum()
        }),
        (Some(Names), "lookup", |c| {
            ids(c)
                .filter(|&i| c.lookup(c.name(i).parent, c.name(i).bytes).is_some())
                .count()
        }),
        (Some(DirNames), "dir_name", |c| {
            dirs(c).filter_map(|d| c.dir_name(d)).count()
        }),
        (Some(Roots), "roots", |c| {
            c.roots().map(|(_, p)| p.len()).sum()
        }),
        (Some(Roots), "dir_path", |c| {
            dirs(c)
                .map(|d| {
                    let mut out = Vec::new();
                    c.dir_path(d, &mut out);
                    out.len()
                })
                .sum()
        }),
        (Some(Roots), "path", |c| {
            ids(c)
                .map(|i| {
                    let mut out = Vec::new();
                    c.path(i, &mut out);
                    out.len()
                })
                .sum()
        }),
        (Some(Traversed), "is_traversed", |c| {
            dirs(c).filter(|&d| c.is_traversed(d)).count()
        }),
        (Some(Inodes), "inode", |c| {
            inodes(c).map(|i| c.inode(i).state as usize).sum()
        }),
        (Some(Links), "link_target", |c| {
            inodes(c).filter_map(|i| c.link_target(i)).count()
        }),
        (Some(Links), "kind", |c| {
            inodes(c).map(|i| c.kind(i) as usize).sum()
        }),
        (Some(WorkTrees), "work_trees", |c| {
            c.work_trees().map(|w| w.common_dir.len()).sum()
        }),
        (Some(WorkTrees), "work_tree", |c| {
            dirs(c).filter_map(|d| c.work_tree(d)).count()
        }),
        (Some(Docs), "docs", |c| c.docs().count()),
        (Some(Docs), "doc_hash", |c| {
            (0..c.next_doc().0)
                .filter_map(|d| c.doc_hash(crate::DocId(d)))
                .count()
        }),
    ]
};

#[test]
fn every_accessor_needs_only_the_section_it_documents() {
    // A section's load must bring everything its accessors read (as Links
    // brings Strings); otherwise a caller that loads exactly what the doc
    // says panics on a section it never heard of. One fresh open per
    // accessor, so nothing loaded for another can hide a gap.
    let scratch = Scratch::new("decode-accessors");
    write_catalog(&scratch.path, &sample("decode-accessors-src"));
    for &(section, name, call) in ACCESSORS {
        let catalog = Catalog::open(&scratch.path).unwrap().unwrap();
        if let Some(section) = section {
            catalog.load(&[section]).unwrap();
        }
        let touched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| call(&catalog)))
            .unwrap_or_else(|_| panic!("{name} read a section loading {section:?} did not load"));
        assert!(touched > 0, "{name}: the sample must exercise it");
    }
}
