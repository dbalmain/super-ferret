//! A corrupt or truncated snapshot is an error, never a panic.

use super::{Scratch, commit, dir_stat, file_stat, hash, link_stat};
use crate::{Catalog, Content, DecodeError, InoId, NameId, WorkTreeKind};

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

/// Reads everything a decoded catalog offers, through every accessor. A
/// panic anywhere here is the failure the decoder exists to prevent.
fn exercise(catalog: &Catalog) -> usize {
    let mut touched = 0;
    let mut path = Vec::new();
    for (id, bytes) in catalog.names() {
        path.clear();
        catalog.path(id, &mut path);
        let name = catalog.name(id);
        touched +=
            bytes.len() + path.len() + usize::from(catalog.lookup(name.parent, bytes).is_some());
    }
    for offset in 0..=catalog.name_heap().len() {
        touched += catalog.name_at(offset).map_or(0, |n| n.0 as usize);
    }
    for id in 0..catalog.inode_count() {
        let id = InoId(id);
        let inode = catalog.inode(id);
        touched += inode
            .doc
            .and_then(|d| catalog.doc_hash(d))
            .map_or(0, |h| h[0] as usize);
        touched += catalog.link_target(id).map_or(0, <[u8]>::len);
        touched += catalog.kind(id) as usize;
        touched += catalog.work_tree(id).map_or(0, |w| w.common_dir.len());
    }
    for dir in 0..catalog.dir_count() {
        path.clear();
        catalog.dir_path(InoId(dir), &mut path);
        touched += path.len() + usize::from(catalog.is_traversed(InoId(dir)));
        touched += catalog.dir_name(InoId(dir)).map_or(0, |n| n.0 as usize);
        touched += catalog.children(InoId(dir)).count();
    }
    touched += catalog.roots().count() + catalog.work_trees().count() + catalog.docs().count();
    touched
}

#[test]
fn the_sample_decodes_and_every_section_is_populated() {
    let catalog = Catalog::from_bytes(sample("decode-populated")).unwrap();
    assert!(exercise(&catalog) > 0);
    assert_eq!(catalog.roots().count(), 2);
    assert_eq!(catalog.work_trees().count(), 1);
    assert_eq!(catalog.doc_count(), 2);
    assert!(catalog.link_target(catalog.name(NameId(1)).child).is_some());
}

#[test]
fn every_truncation_is_an_error() {
    let bytes = sample("decode-truncate");
    for len in 0..bytes.len() {
        assert!(
            Catalog::from_bytes(bytes[..len].to_vec()).is_err(),
            "truncated to {len}"
        );
    }
    let mut longer = bytes;
    longer.push(0);
    assert_eq!(Catalog::from_bytes(longer).err(), Some(DecodeError::Layout));
}

#[test]
fn every_single_bit_flip_is_an_error_or_reads_safely() {
    let bytes = sample("decode-flip");
    let (mut rejected, mut accepted) = (0, 0);
    for at in 0..bytes.len() {
        for bit in 0..8 {
            let mut flipped = bytes.clone();
            flipped[at] ^= 1 << bit;
            match Catalog::from_bytes(flipped) {
                Err(_) => rejected += 1,
                Ok(catalog) => {
                    exercise(&catalog);
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
