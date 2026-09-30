//! A corrupt or truncated snapshot is an error, never a panic.

use super::{SNIFFER, Scratch, commit, dir_stat, file_stat, hash, link_stat};
use std::path::Path;

use crate::format::{self, COLUMNS, Coding, Column, HEADER, SECTIONS, TABLE_END};
use crate::packed;
use crate::{
    BeginError, Catalog, Content, DecodeError, InoId, NameId, OpenError, Section, Stat,
    Transaction, WorkTreeKind,
};

/// A small snapshot with every section non-empty and every column wider
/// than 0 bits, built in its own scratch directory `name`. Dev and mode have
/// three values, so their dictionary checks read every index; owner has two,
/// so its check reads none.
fn sample(name: &str) -> Vec<u8> {
    let scratch = Scratch::new(name);
    commit(&scratch.path, |txn| {
        let mut w = txn.batch();
        let root = w.root(b"/s", dir_stat(1));
        let sub = w.dir(root, b"sub", dir_stat(2));
        let skip = w.traversed_dir(sub, b"skip", dir_stat(3));
        w.file(sub, b"a.rs", file_stat(10), Content::Hashed(hash(1)));
        let odd = Stat {
            dev: 8,
            uid: 100_000,
            gid: 100_000,
            mtime_sec: -86_400,
            nlink: 3,
            ..file_stat(11)
        };
        w.file(skip, b"b.bin", odd, Content::Binary);
        w.file(root, b"c", file_stat(12), Content::Hashed(hash(2)));
        w.symlink(root, b"d", link_stat(13), b"sub/a.rs");
        w.work_tree(sub, WorkTreeKind::Linked, b"/repo/.git", (1, 2));
        w.entry_count(sub, 2);
        let other = w.root(
            b"/t",
            Stat {
                dev: 9,
                ..dir_stat(4)
            },
        );
        w.file(other, b"e", file_stat(14), Content::Fault);
        txn.add(w);
    });
    std::fs::read(scratch.path.join("catalog")).unwrap()
}

/// Where `section` starts in `bytes`, from its table.
fn section_start(bytes: &[u8], section: Section) -> usize {
    let at = HEADER + section as usize * 16;
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize
}

/// Where `column`'s descriptor lies in `bytes`.
fn descriptor(column: Column) -> usize {
    TABLE_END - (COLUMNS.len() - column as usize) * 16
}

/// Overwrites row `row` of `column` with the packed value `raw`, leaving
/// everything else as it was. In a blocked column, `raw` is above the row's
/// block's base, at its block's width.
fn set_raw(bytes: &mut [u8], column: Column, row: usize, raw: u64) {
    let layout = format::decode_table(bytes, bytes.len() as u64).unwrap();
    let placed = layout.columns[column as usize];
    let start =
        section_start(bytes, column.section()) + placed.start + placed.desc.dict_len as usize * 8;
    let (values, width, row) = match column.coding() {
        coding if coding.is_blocked() => {
            let count = layout.count(column.rows()) as u32;
            let entry = start + row / packed::BLOCK_ROWS * packed::BLOCK_ENTRY as usize + 8;
            let word = u64::from_le_bytes(bytes[entry..entry + 8].try_into().unwrap());
            let table = (packed::blocks(count) * packed::BLOCK_ENTRY) as usize;
            let values = start + table + (word >> 8) as usize;
            (values, (word & 0x7F) as u32, row % packed::BLOCK_ROWS)
        }
        _ => (start, placed.desc.width, row),
    };
    assert!(
        raw <= packed::mask(width),
        "{raw} does not fit {width} bits"
    );
    let width = width as usize;
    for bit in 0..width {
        let at = row * width + bit;
        let byte = &mut bytes[values + at / 8];
        *byte = (*byte & !(1 << (at % 8))) | (((raw >> bit) & 1) as u8) << (at % 8);
    }
}

/// Overwrites the base of block `block` of the blocked column `column`: every
/// row of the block moves with it.
fn set_block_base(bytes: &mut [u8], column: Column, block: usize, base: u64) {
    let layout = format::decode_table(bytes, bytes.len() as u64).unwrap();
    let placed = layout.columns[column as usize];
    let entry = section_start(bytes, column.section())
        + placed.start
        + block * packed::BLOCK_ENTRY as usize;
    bytes[entry..entry + 8].copy_from_slice(&base.to_le_bytes());
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
        if has(&Section::INODE) {
            touched = touched.wrapping_add(catalog.inode(id).stat.dev as usize);
        }
        if has(&[Dev, Ino]) {
            touched = touched.wrapping_add(catalog.identity(id).0 as usize);
        }
        if has(&[Size]) {
            touched = touched.wrapping_add(catalog.size(id) as usize);
        }
        if has(&[Mtime]) {
            touched = touched.wrapping_add(catalog.mtime(id) as usize);
        }
        if has(&[Ctime]) {
            touched = touched.wrapping_add(catalog.ctime(id) as usize);
        }
        if has(&[Mode]) {
            touched = touched.wrapping_add(catalog.mode(id) as usize);
        }
        if has(&[Owner]) {
            touched = touched.wrapping_add(catalog.owner(id).0 as usize);
        }
        if has(&[Nlink]) {
            touched = touched.wrapping_add(catalog.nlink(id) as usize);
        }
        if has(&[States]) {
            touched = touched.wrapping_add(catalog.state(id) as usize);
        }
        if has(&[Doc, Docs]) {
            touched = touched.wrapping_add(
                catalog
                    .doc(id)
                    .and_then(|d| catalog.doc_hash(d))
                    .map_or(0, |h| h[0] as usize),
            );
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
        if has(&[Entries]) {
            touched += catalog.entry_count(dir).map_or(0, |n| n as usize);
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
                    // The head of the file is fully checked, but for the
                    // sniffer, the next DocId, the counts and the columns'
                    // bases. A count can flip into another consistent
                    // catalog: one more directory, when padding bits read as
                    // a name edge that fits, makes the first file a
                    // directory. The table and every width and dictionary
                    // length are exact.
                    let descriptors = descriptor(Column::NameParent);
                    let base = at >= descriptors && (at - descriptors) % 16 < 8;
                    assert!(
                        at >= TABLE_END || (12..20).contains(&at) || (24..36).contains(&at) || base,
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
    // Doc ids moved past the next DocId (the id column's base set to it) are
    // invisible to a name query, which never reads the docs; loading them
    // refuses.
    let mut bytes = sample("decode-partial");
    let docs = Catalog::from_bytes(bytes.clone()).unwrap();
    assert_eq!(docs.doc_count(), 2);
    let next = u64::from(docs.next_doc().0);
    set_descriptor(&mut bytes, Column::DocId, 0, &next.to_le_bytes());
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
fn doc_ids_that_stop_increasing_are_rejected() {
    // Ids 0, 2 and 3 (1 died) are stored as 0, 1, 1 above each row; the last
    // set to 0 reads back as id 2 again, and `doc_hash` would search a
    // column that is not sorted.
    let scratch = Scratch::new("decode-doc-order");
    let file = |name: &[u8], ino, n| (name.to_vec(), file_stat(ino), Content::Hashed(hash(n)));
    let files = [
        file(b"a", 10, 1),
        file(b"b", 11, 2),
        file(b"c", 12, 3),
        file(b"d", 13, 4),
    ];
    for kept in [
        &files[..],
        &[files[0].clone(), files[2].clone(), files[3].clone()],
    ] {
        commit(&scratch.path, |txn| {
            let mut w = txn.batch();
            let root = w.root(b"/s", dir_stat(1));
            for (name, stat, content) in kept {
                w.file(root, name, *stat, *content);
            }
            txn.add(w);
        });
    }
    let mut bytes = std::fs::read(scratch.path.join("catalog")).unwrap();
    let catalog = Catalog::from_bytes(bytes.clone()).unwrap();
    let ids: Vec<u32> = catalog.docs().map(|(id, _)| id.0).collect();
    assert_eq!(ids, [0, 2, 3]);
    set_raw(&mut bytes, Column::DocId, 2, 0);
    assert_corrupt(bytes, "docs");
}

#[test]
fn a_blocked_column_whose_table_misplaces_a_block_is_rejected_on_load() {
    // The first block's entry moved one byte on, or made wider than the
    // descriptor's widest block: its reads would land on the wrong bits, or
    // past the column's end.
    let bytes = sample("decode-blocked");
    let layout = format::decode_table(&bytes, bytes.len() as u64).unwrap();
    let scratch = Scratch::new("decode-blocked-lazy");
    let blocked = COLUMNS.into_iter().filter(|c| c.coding().is_blocked());
    assert_eq!(blocked.clone().count(), 13);
    for column in blocked {
        let placed = layout.columns[column as usize];
        let entry = section_start(&bytes, column.section()) + placed.start + 8;
        let width = u64::from(placed.desc.width);
        for (what, word) in [("offset", 1 << 8 | width), ("width", width + 1)] {
            let mut bad = bytes.clone();
            bad[entry..entry + 8].copy_from_slice(&word.to_le_bytes());
            let label = column.section().label();
            let context = format!("{column:?} {what}");
            assert_eq!(
                Catalog::from_bytes(bad.clone()).err(),
                Some(DecodeError::Corrupt(label)),
                "{context}"
            );
            write_catalog(&scratch.path, &bad);
            let catalog = Catalog::open(&scratch.path).unwrap().unwrap();
            assert!(
                matches!(
                    catalog.load(&[column.section()]),
                    Err(OpenError::Decode(DecodeError::Corrupt(l))) if l == label
                ),
                "{context}"
            );
        }
    }
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
    assert_eq!(catalog.bytes_read(), TABLE_END as u64);
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
    set_raw(&mut bytes, Column::NameParent, sub.0 as usize, 1);
    assert_eq!(
        Catalog::from_bytes(bytes).err(),
        Some(DecodeError::Corrupt("dir names"))
    );
}

#[test]
// A name whose child is a directory other than through that directory's
// recorded edge was accepted: here the root's file `f` is rewritten to name
// the root itself. The root's edge is NONE, so the one-way check passed, and
// `Transaction::keep` then copied the root into itself forever, growing
// memory under the writer lock.
fn a_name_that_makes_a_directory_its_own_descendant_is_rejected() {
    let scratch = Scratch::new("decode-cycle");
    let catalog = commit(&scratch.path, |txn| {
        let mut w = txn.batch();
        let root = w.root(b"/s", dir_stat(1));
        w.file(root, b"f", file_stat(10), Content::Unindexed);
        txn.add(w);
    });
    assert_eq!(catalog.child(NameId(0)), InoId(1));
    drop(catalog);

    let path = scratch.path.join("catalog");
    let mut bytes = std::fs::read(&path).unwrap();
    // One name: its child's block is width 0, so the value is the base.
    set_block_base(&mut bytes, Column::NameChild, 0, 0);
    std::fs::write(&path, &bytes).unwrap();
    // Retention is the walk that looped (about 1 GB in 5 s before the fix);
    // the writer must refuse the generation before it can keep anything.
    assert!(matches!(
        Transaction::begin(&scratch.path, SNIFFER),
        Err(BeginError::Previous(OpenError::Decode(
            DecodeError::Corrupt("dir names")
        )))
    ));
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
    let heap_start = section_start(&bytes, Section::NameHeap);
    assert_eq!(bytes[heap_start], b'c');
    bytes[heap_start] = b'g';
    assert_eq!(
        Catalog::from_bytes(bytes).err(),
        Some(DecodeError::Corrupt("name order"))
    );
}

/// A snapshot of root `/s` holding the given files, and its bytes.
fn files_under_root(name: &str, files: &[&[u8]]) -> Vec<u8> {
    let scratch = Scratch::new(name);
    commit(&scratch.path, |txn| {
        let mut w = txn.batch();
        let root = w.root(b"/s", dir_stat(1));
        for (i, file) in files.iter().enumerate() {
            w.file(root, file, file_stat(10 + i as u64), Content::Unindexed);
        }
        txn.add(w);
    });
    std::fs::read(scratch.path.join("catalog")).unwrap()
}

fn assert_corrupt(bytes: Vec<u8>, what: &'static str) {
    assert_eq!(
        Catalog::from_bytes(bytes).err(),
        Some(DecodeError::Corrupt(what))
    );
}

#[test]
fn a_heap_with_a_nul_inside_a_name_is_rejected() {
    // One file `ab`: heap `ab\0`. Rewritten to `a\0\0` the single name span
    // still starts at 0, is non-empty and ends in NUL, so only the count of
    // NULs against names (two against one) sees the NUL inside it.
    let mut bytes = files_under_root("decode-nul-inside", &[b"ab"]);
    let heap = section_start(&bytes, Section::NameHeap);
    assert_eq!(&bytes[heap..heap + 3], b"ab\0");
    bytes[heap + 1] = 0;
    assert_corrupt(bytes, "name order");
}

#[test]
fn a_first_name_that_does_not_start_the_heap_is_rejected() {
    // Files `ab` and `cd`: heap `ab\0cd\0`, offsets 0 and 3. Offset 0 set to 1
    // is still below offset 1, inside the heap, and names `b` with its NUL
    // last, so only the first-offset-is-zero rule rejects it. Accepted, the
    // heap byte 0 belongs to no name and `name_at(0)` underflows.
    let mut bytes = files_under_root("decode-first-offset", &[b"ab", b"cd"]);
    set_raw(&mut bytes, Column::NameOffset, 0, 1);
    assert_corrupt(bytes, "names");
}

#[test]
fn a_name_not_ending_in_nul_is_rejected() {
    // Files `abc` and `de`: heap `abc\0de\0`, offsets 0 and 4. Offset 1 set to
    // 5 makes the first span `abc\0d`, ending in `d`, and the second `e`. The
    // heap is untouched so the NUL count holds, both names are non-empty, and
    // they are in order, so only the terminator check rejects it.
    let mut bytes = files_under_root("decode-nul-last", &[b"abc", b"de"]);
    set_raw(&mut bytes, Column::NameOffset, 1, 5);
    assert_corrupt(bytes, "name order");
}

#[test]
fn an_empty_name_is_rejected() {
    // Files `a` and `bc`: heap `a\0bc\0`, offsets 0 and 2. The heap becomes
    // `\0bcd\0` and offset 1 is set to 1: two NULs for two names, each span
    // ending in NUL and in order, but the first span is just the NUL.
    let mut bytes = files_under_root("decode-empty-name", &[b"a", b"bc"]);
    let heap = section_start(&bytes, Section::NameHeap);
    assert_eq!(&bytes[heap..heap + 5], b"a\0bc\0");
    bytes[heap..heap + 5].copy_from_slice(b"\0bcd\0");
    set_raw(&mut bytes, Column::NameOffset, 1, 1);
    assert_corrupt(bytes, "name order");
}

#[test]
fn a_dropped_root_row_is_rejected() {
    // Roots `/s` and `/t` are the two unnamed directories. Dropping the last
    // row leaves a well-formed table of one, so only the count against the
    // unnamed directories sees it; accepted, paths under `/t` read wrong.
    let bytes = sample("decode-roots-dropped");
    assert_corrupt(resize_section(&bytes, Section::Roots, -8), "roots");
}

#[test]
fn directories_with_swapped_name_edges_are_rejected() {
    // Root holds directories `a` (dir 1, name 0) and `b` (dir 2, name 1).
    // Swapping their edges keeps every parent lower and the count of
    // directory-child names, so only the check that a directory's edge names
    // it sees it; accepted, `dir_path` returns the other directory's name.
    let scratch = Scratch::new("decode-swapped-edges");
    let catalog = commit(&scratch.path, |txn| {
        let mut w = txn.batch();
        let root = w.root(b"/s", dir_stat(1));
        w.dir(root, b"a", dir_stat(2));
        w.dir(root, b"b", dir_stat(3));
        txn.add(w);
    });
    assert_eq!(catalog.dir_name(InoId(1)), Some(NameId(0)));
    assert_eq!(catalog.dir_name(InoId(2)), Some(NameId(1)));
    let mut bytes = std::fs::read(scratch.path.join("catalog")).unwrap();
    set_raw(&mut bytes, Column::DirName, 1, 1);
    set_raw(&mut bytes, Column::DirName, 2, 0);
    assert_corrupt(bytes, "dir names");
}

/// Resizes `section` by `delta` bytes, keeping the table tiling the file, so
/// only the section's own length check can object. Bytes are added as zeros
/// and removed from the end.
fn resize_section(bytes: &[u8], section: Section, delta: isize) -> Vec<u8> {
    let table = |i: usize| HEADER + i * 16;
    let at = section as usize;
    let read = |i: usize, off: usize| {
        u64::from_le_bytes(bytes[table(i) + off..][..8].try_into().unwrap()) as usize
    };
    let (start, len) = (read(at, 0), read(at, 8));
    let end = start + len;
    let mut out = bytes[..start].to_vec();
    if delta < 0 {
        out.extend_from_slice(&bytes[start..end - delta.unsigned_abs()]);
    } else {
        out.extend_from_slice(&bytes[start..end]);
        out.extend(std::iter::repeat_n(0, delta.unsigned_abs()));
    }
    out.extend_from_slice(&bytes[end..]);
    let new_len = (len as isize + delta) as u64;
    out[table(at) + 8..][..8].copy_from_slice(&new_len.to_le_bytes());
    for i in at + 1..SECTIONS.len() {
        let offset = (read(i, 0) as isize + delta) as u64;
        out[table(i)..][..8].copy_from_slice(&offset.to_le_bytes());
    }
    out
}

/// A column section holds exactly its columns' dictionaries and padded
/// values: a byte too few or too many, or a whole value's worth, is refused
/// at open, before any section is read.
#[test]
fn a_column_section_of_the_wrong_length_is_rejected_at_open() {
    let bytes = sample("decode-column-length");
    assert!(Catalog::from_bytes(bytes.clone()).is_ok());
    let scratch = Scratch::new("decode-column-length-lazy");
    let mut sections: Vec<Section> = COLUMNS.iter().map(|c| c.section()).collect();
    sections.dedup();
    for section in sections {
        for delta in [-8, -1, 1, 8] {
            let bad = resize_section(&bytes, section, delta);
            let expect = DecodeError::Corrupt(section.label());
            assert_eq!(
                Catalog::from_bytes(bad.clone()).err(),
                Some(expect),
                "{section:?} {delta}"
            );
            write_catalog(&scratch.path, &bad);
            assert!(
                matches!(
                    Catalog::open(&scratch.path),
                    Err(OpenError::Decode(DecodeError::Corrupt(what))) if what == section.label()
                ),
                "{section:?} {delta}"
            );
        }
    }
}

/// Sets `column`'s descriptor field at `offset` (0 base, 8 width, 12
/// dictionary length) to `value`.
fn set_descriptor(bytes: &mut [u8], column: Column, offset: usize, value: &[u8]) {
    let at = descriptor(column) + offset;
    bytes[at..at + value.len()].copy_from_slice(value);
}

#[test]
fn a_width_over_64_is_rejected_even_where_no_value_is_read() {
    // With no names, a names column is only its padding at any width, so the
    // length check passes; a width of 65 must still not reach a mask.
    let scratch = Scratch::new("decode-width");
    commit(&scratch.path, |txn| {
        let mut w = txn.batch();
        w.root(b"/s", dir_stat(1));
        txn.add(w);
    });
    let mut bytes = std::fs::read(scratch.path.join("catalog")).unwrap();
    assert!(Catalog::from_bytes(bytes.clone()).is_ok());
    set_descriptor(&mut bytes, Column::NameParent, 8, &65u32.to_le_bytes());
    assert_eq!(
        Catalog::from_bytes(bytes).err(),
        Some(DecodeError::Corrupt("names"))
    );
}

#[test]
fn a_dictionary_on_a_column_that_has_none_is_rejected() {
    // A size column claiming one dictionary value, and 8 bytes longer to
    // match: without the check its values would be read 8 bytes late.
    let mut bytes = resize_section(&sample("decode-stray-dict"), Section::Size, 8);
    set_descriptor(&mut bytes, Column::Size, 12, &1u32.to_le_bytes());
    assert_eq!(
        Catalog::from_bytes(bytes).err(),
        Some(DecodeError::Corrupt("size"))
    );
}

#[test]
fn a_dictionary_index_past_the_end_is_rejected_on_load() {
    // Mode has three values in two bits, so index 3 fits the width and
    // misses the dictionary.
    let bytes = sample("decode-dict-index");
    let mut bad = bytes.clone();
    set_raw(&mut bad, Column::Mode, 0, 3);
    let scratch = Scratch::new("decode-dict-index-lazy");
    write_catalog(&scratch.path, &bad);
    let catalog = Catalog::open(&scratch.path).unwrap().unwrap();
    assert!(matches!(
        catalog.load(&[Section::Mode]),
        Err(OpenError::Decode(DecodeError::Corrupt("mode")))
    ));
    assert_eq!(
        Catalog::from_bytes(bad).err(),
        Some(DecodeError::Corrupt("mode"))
    );
}

#[test]
fn a_dictionary_base_that_reaches_past_the_end_is_rejected() {
    // Owner has two values in one bit, so its check reads no index; a base
    // of 1, or one that wraps, moves the indices past the end and must make
    // it read them all.
    let bytes = sample("decode-dict-base");
    for base in [1, u64::MAX] {
        let mut bad = bytes.clone();
        set_descriptor(&mut bad, Column::Owner, 0, &base.to_le_bytes());
        assert_eq!(
            Catalog::from_bytes(bad).err(),
            Some(DecodeError::Corrupt("owner")),
            "base {base}"
        );
    }
}

#[test]
fn counts_that_cannot_hold_are_rejected_at_open() {
    // More directories than inodes, and the id space's "none" as a count.
    let bytes = sample("decode-counts");
    let inodes = u32::from_le_bytes(bytes[28..32].try_into().unwrap());
    for (at, value) in [(24, inodes + 1), (28, u32::MAX), (32, u32::MAX)] {
        let mut bad = bytes.clone();
        bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
        assert_eq!(
            Catalog::from_bytes(bad).err(),
            Some(DecodeError::Corrupt("counts")),
            "at {at}"
        );
    }
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

/// Every public accessor of catalog content, keyed by the sections its doc
/// says it needs, exercised over every id the sample has. No sections is an
/// accessor that reads only the head of the file. The load-state probes
/// `is_loaded` and `bytes_read` are left out: they report on the reader and
/// read no section.
type Accessor = (&'static [Section], &'static str, fn(&Catalog) -> usize);

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
        (&[], "counts", |c| {
            (c.dir_count() + c.inode_count() + c.name_count() + c.doc_count()) as usize
                + c.next_doc().0 as usize
                + c.sniffer_version() as usize
                + c.head_len() as usize
                + c.section_sizes()
                    .map(|(_, len)| len as usize)
                    .sum::<usize>()
                + c.column_widths()
                    .map(|(_, width, dict)| (width + dict) as usize)
                    .sum::<usize>()
        }),
        (&[NameHeap], "name_heap", |c| c.name_heap().len()),
        (&[Names], "names", |c| c.names().map(|(_, b)| b.len()).sum()),
        (&[Names], "name", |c| {
            ids(c).map(|i| c.name(i).bytes.len()).sum()
        }),
        (&[Names], "name_start", |c| {
            ids(c).map(|i| c.name_start(i)).sum()
        }),
        (&[Names], "child", |c| {
            ids(c).map(|i| c.child(i).0 as usize).sum()
        }),
        (&[Names], "name_at", |c| {
            (0..=c.name_heap().len())
                .filter_map(|o| c.name_at(o))
                .count()
        }),
        (&[Names], "children", |c| {
            dirs(c).map(|d| c.children(d).count()).sum()
        }),
        (&[Names], "lookup", |c| {
            ids(c)
                .filter(|&i| c.lookup(c.name(i).parent, c.name(i).bytes).is_some())
                .count()
        }),
        (&[DirNames], "dir_name", |c| {
            dirs(c).filter_map(|d| c.dir_name(d)).count()
        }),
        (&[Roots], "roots", |c| c.roots().map(|(_, p)| p.len()).sum()),
        (&[Roots], "dir_path", |c| {
            dirs(c)
                .map(|d| {
                    let mut out = Vec::new();
                    c.dir_path(d, &mut out);
                    out.len()
                })
                .sum()
        }),
        (&[Roots], "path", |c| {
            ids(c)
                .map(|i| {
                    let mut out = Vec::new();
                    c.path(i, &mut out);
                    out.len()
                })
                .sum()
        }),
        (&[Entries], "entry_count", |c| {
            dirs(c).filter_map(|d| c.entry_count(d)).count()
        }),
        (&[Traversed], "is_traversed", |c| {
            dirs(c).filter(|&d| c.is_traversed(d)).count()
        }),
        (&Section::INODE, "inode", |c| {
            inodes(c).map(|i| c.inode(i).stat.ino as usize).sum()
        }),
        (&[Size], "size", |c| {
            inodes(c).map(|i| c.size(i) as usize).sum()
        }),
        (&[Mtime], "mtime", |c| {
            inodes(c).map(|i| c.mtime(i).unsigned_abs() as usize).sum()
        }),
        (&[Ctime], "ctime", |c| {
            inodes(c).map(|i| c.ctime(i).unsigned_abs() as usize).sum()
        }),
        (&[Mode], "mode", |c| {
            inodes(c).map(|i| c.mode(i) as usize).sum()
        }),
        (&[Owner], "owner", |c| {
            inodes(c).map(|i| c.owner(i).1 as usize).sum()
        }),
        (&[Nlink], "nlink", |c| {
            inodes(c).map(|i| c.nlink(i) as usize).sum()
        }),
        (&[Doc], "doc", |c| {
            inodes(c).filter_map(|i| c.doc(i)).count()
        }),
        (&[States], "state", |c| {
            inodes(c).map(|i| c.state(i) as usize).sum()
        }),
        (&[Links], "link_target", |c| {
            inodes(c).filter_map(|i| c.link_target(i)).count()
        }),
        (&[Links], "kind", |c| {
            inodes(c).map(|i| c.kind(i) as usize).sum()
        }),
        (&[WorkTrees], "work_trees", |c| {
            c.work_trees().map(|w| w.common_dir.len()).sum()
        }),
        (&[WorkTrees], "work_tree", |c| {
            dirs(c).filter_map(|d| c.work_tree(d)).count()
        }),
        (&[Docs], "docs", |c| c.docs().count()),
        (&[Docs], "doc_hash", |c| {
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
    for &(sections, name, call) in ACCESSORS {
        let catalog = Catalog::open(&scratch.path).unwrap().unwrap();
        catalog.load(sections).unwrap();
        let touched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| call(&catalog)))
            .unwrap_or_else(|_| panic!("{name} read a section loading {sections:?} did not load"));
        assert!(touched > 0, "{name}: the sample must exercise it");
    }
}
