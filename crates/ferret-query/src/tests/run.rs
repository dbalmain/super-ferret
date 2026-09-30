//! Queries run against catalogs committed through the transaction: each
//! atom's results, the sections each strategy reads, and the fast paths.

use std::ops::ControlFlow;

use ferret_catalog::{Catalog, Content, ContentState, Kind, Section, Transaction};

use super::{DAY, Scratch, dir, file, find, lazy, now, paths, sample, stat};
use crate::{Query, Strategy};

#[test]
fn each_atom_finds_what_it_says() {
    let scratch = Scratch::new("atoms");
    let catalog = sample(&scratch);
    let cases: &[(&str, &[&str])] = &[
        ("readme", &["/r/docs/README.md", "/r/docs/readme.txt"]),
        ("case:README", &["/r/docs/README.md"]),
        ("case:readme", &["/r/docs/readme.txt"]),
        ("ext:MD", &["/r/docs/README.md"]),
        ("case:ext:RS", &["/r/src/lib.RS"]),
        ("ext:rst", &["/r/src/notes.rst"]),
        (
            "ext:rs",
            &[
                "/r/skip/kept.rs",
                "/r/src/deep/src/y.rs",
                "/r/src/deep/x.rs",
                "/r/src/lib.RS",
                "/r/src/main.rs",
                "/r/src/parse_HTTP.rs",
                "/t/a.rs",
            ],
        ),
        (
            "case:*.rs",
            &[
                "/r/skip/kept.rs",
                "/r/src/deep/src/y.rs",
                "/r/src/deep/x.rs",
                "/r/src/main.rs",
                "/r/src/parse_HTTP.rs",
                "/t/.rs",
                "/t/a.rs",
            ],
        ),
        (
            "src/**/*.rs",
            &[
                "/r/src/deep/src/y.rs",
                "/r/src/deep/x.rs",
                "/r/src/lib.RS",
                "/r/src/main.rs",
                "/r/src/parse_HTTP.rs",
            ],
        ),
        ("src/*/*.rs", &["/r/src/deep/x.rs"]),
        // A negated class stays inside its component: `deep/x.rs` would
        // match if `[!x]` could consume the `/`.
        (
            "src/*[!x]*.rs",
            &[
                "/r/src/deep/src/y.rs",
                "/r/src/lib.RS",
                "/r/src/main.rs",
                "/r/src/parse_HTTP.rs",
            ],
        ),
        ("/t/*", &["/t/.rs", "/t/a.rs"]),
        (
            r"re:^[a-z]\.rs$",
            &["/r/src/deep/src/y.rs", "/r/src/deep/x.rs", "/t/a.rs"],
        ),
        ("re:http", &["/r/src/parse_HTTP.rs"]),
        ("case:re:http", &[]),
        (
            "path:deep",
            &[
                "/r/src/deep",
                "/r/src/deep/src",
                "/r/src/deep/src/y.rs",
                "/r/src/deep/x.rs",
            ],
        ),
        ("deep/src", &["/r/src/deep/src", "/r/src/deep/src/y.rs"]),
        ("mtime:<1d", &["/r/src/main.rs"]),
        ("mtime:>100d", &["/r/src/big.bin"]),
        ("size:>100M", &["/r/src/big.bin"]),
        (
            "size:<10",
            &["/r/src/deep/src/y.rs", "/r/src/deep/x.rs", "/t/a.rs"],
        ),
        ("size:100", &["/r/src/lib.RS", "/r/src/main.rs"]),
        ("type:l", &["/r/link"]),
        ("type:f size:>100M", &["/r/src/big.bin"]),
        ("type:l mtime:<2d", &["/r/link"]),
        (
            "type:d",
            &["/r/docs", "/r/src", "/r/src/deep", "/r/src/deep/src"],
        ),
        (
            "rs size:<10",
            &["/r/src/deep/src/y.rs", "/r/src/deep/x.rs", "/t/a.rs"],
        ),
        ("ma rs", &["/r/src/main.rs"]),
        ("rs mtime:<1d type:f", &["/r/src/main.rs"]),
        ("nothing-has-this", &[]),
    ];
    // Each query on its own fresh open, so no earlier query has loaded a
    // section it needs: a missing load panics here rather than passing.
    for &(text, expect) in cases {
        assert_eq!(paths(&lazy(&scratch), text), expect, "{text:?} (fresh)");
    }
    // Then all of them on one catalog, as a long-lived reader would: loads
    // accumulate and must not change any answer.
    for &(text, expect) in cases {
        assert_eq!(paths(&catalog, text), expect, "{text:?} (shared)");
    }
}

#[test]
fn a_structural_directory_is_never_a_result_but_its_contents_are() {
    let scratch = Scratch::new("traversed");
    let catalog = sample(&scratch);
    assert_eq!(paths(&catalog, "skip"), Vec::<String>::new());
    assert_eq!(paths(&lazy(&scratch), "type:d mtime:>1d").len(), 4);
    assert_eq!(paths(&lazy(&scratch), "kept"), ["/r/skip/kept.rs"]);
}

#[test]
fn a_row_carries_the_inode_it_names() {
    let scratch = Scratch::new("rows");
    let catalog = sample(&scratch);
    let query = Query::parse("case:main.rs", now()).unwrap();
    catalog.load(&Section::INODE).unwrap();
    let mut rows = Vec::new();
    query
        .run(&catalog, |row| {
            rows.push((row.path.to_vec(), row.kind, catalog.inode(row.inode)));
            ControlFlow::Continue(())
        })
        .unwrap();
    assert_eq!(rows.len(), 1);
    let (path, kind, meta) = &rows[0];
    assert_eq!(path, b"/r/src/main.rs");
    assert_eq!(*kind, Kind::File);
    assert_eq!(meta.stat.ino, 15);
    assert_eq!(meta.stat.size, 100);
    assert_eq!(meta.state, ContentState::Hashed);
    assert!(meta.doc.is_some());
    assert_eq!(find(&catalog, "link").0, ["/r/link"]);
}

#[test]
fn a_name_query_reads_no_documents_and_no_inodes_even_for_a_hit() {
    // Rows once carried a decoded inode, so the first hit loaded every inode
    // column: 283 MB at 10M names for a listing that prints only paths.
    let scratch = Scratch::new("lazy-name");
    let catalog = sample(&scratch);
    assert!(find(&catalog, "nothing-has-this").0.is_empty());
    assert!(catalog.is_loaded(Section::NameHeap));
    let (found, _) = find(&catalog, "readme");
    assert_eq!(found.len(), 2);
    for &section in Section::INODE.iter().chain(&[Section::Docs]) {
        assert!(!catalog.is_loaded(section), "{section:?}");
    }
}

#[test]
fn a_metadata_query_scans_inodes_and_still_skips_documents() {
    let scratch = Scratch::new("lazy-meta");
    let catalog = sample(&scratch);
    let query = Query::parse("size:>100M", now()).unwrap();
    assert_eq!(query.strategy(), Strategy::InodeScan);
    let (found, stats) = find(&catalog, "size:>100M");
    assert_eq!(found, ["/r/src/big.bin"]);
    // Only the inode that passed is a candidate; the row reads no other
    // inode field.
    assert_eq!(stats.candidates, 1);
    assert!(catalog.is_loaded(Section::Size));
    assert!(catalog.is_loaded(Section::NameHeap));
    for &section in Section::INODE.iter().filter(|&&s| s != Section::Size) {
        assert!(!catalog.is_loaded(section), "{section:?}");
    }
    assert!(!catalog.is_loaded(Section::Docs));
}

#[test]
fn a_name_query_on_a_catalog_with_only_inodes_loaded() {
    // A caller that loaded the inode sections for its own reasons: the
    // query takes rows from them and loads only what it lacks.
    let scratch = Scratch::new("inodes-preloaded");
    sample(&scratch);
    let catalog = lazy(&scratch);
    catalog.load(&Section::INODE).unwrap();
    let (found, _) = find(&catalog, "readme");
    assert_eq!(found.len(), 2);
    assert!(!catalog.is_loaded(Section::Docs));
}

#[test]
fn a_metadata_query_nothing_passes_reads_only_its_field() {
    let scratch = Scratch::new("lazy-meta-empty");
    let catalog = sample(&scratch);
    let (found, stats) = find(&catalog, "size:>1T");
    assert!(found.is_empty());
    assert_eq!(stats.candidates, 0);
    assert!(catalog.is_loaded(Section::Size));
    for &section in Section::INODE.iter().filter(|&&s| s != Section::Size) {
        assert!(!catalog.is_loaded(section), "{section:?}");
    }
    for section in [
        Section::Names,
        Section::NameHeap,
        Section::DirNames,
        Section::Docs,
    ] {
        assert!(!catalog.is_loaded(section), "{section:?}");
    }
}

#[test]
fn a_heap_scan_with_a_metadata_atom_nothing_passes_loads_only_its_field() {
    // `ext:rs` names candidates for a heap scan; none passes `size:>1T`, so no
    // row is built and the inode columns the row would read stay on disk.
    let scratch = Scratch::new("lazy-meta-heap");
    let catalog = sample(&scratch);
    let query = Query::parse("ext:rs size:>1T", now()).unwrap();
    assert_eq!(query.strategy(), Strategy::HeapScan);
    let (found, stats) = find(&catalog, "ext:rs size:>1T");
    assert!(found.is_empty());
    assert!(stats.candidates > 0);
    assert!(catalog.is_loaded(Section::Size));
    for &section in Section::INODE.iter().filter(|&&s| s != Section::Size) {
        assert!(!catalog.is_loaded(section), "{section:?}");
    }
}

#[test]
fn an_extreme_mtime_is_an_age_not_an_overflow() {
    // `now - mtime` overflows i64 for both of these; the file holds whatever
    // it holds, so the age must be computed wide.
    let scratch = Scratch::new("mtime-extremes");
    let mut txn = Transaction::begin(&scratch.0, 1).unwrap();
    let mut w = txn.batch();
    let root = w.root(b"/x", dir(1));
    let mut at = |ino, name: &[u8], mtime_sec| {
        let mut stat = file(ino, 1, 0);
        stat.mtime_sec = mtime_sec;
        w.file(root, name, stat, Content::Unindexed);
    };
    at(2, b"ancient", i64::MIN);
    at(3, b"future", i64::MAX);
    txn.add(w);
    txn.commit().unwrap();
    let catalog = lazy(&scratch);
    // Through the inode scan, and through a heap scan's metadata test.
    assert_eq!(paths(&catalog, "mtime:>1y"), ["/x/ancient"]);
    assert_eq!(paths(&catalog, "mtime:<1d"), ["/x/future"]);
    assert_eq!(paths(&catalog, "e mtime:>1y"), ["/x/ancient"]);
    assert_eq!(paths(&catalog, "e mtime:<1d"), ["/x/future"]);
}

#[test]
fn a_heap_scan_tests_each_name_once_however_often_it_hits() {
    let scratch = Scratch::new("once");
    let catalog = sample(&scratch);
    // `parse_HTTP.rs` holds `T` twice; it is one candidate and one row.
    let (found, stats) = find(&catalog, "case:T");
    assert_eq!(found, ["/r/src/parse_HTTP.rs"]);
    assert_eq!((stats.candidates, stats.rows), (1, 1));
    // Folded, `t` hits readme.txt twice, parse_HTTP.rs twice, and so on.
    let (found, stats) = find(&catalog, "t");
    assert_eq!(stats.candidates, found.len() as u64);
}

#[test]
fn every_name_test_runs_except_the_one_the_scan_proved() {
    let scratch = Scratch::new("skip-test");
    let catalog = sample(&scratch);
    // `rs` drives (ties go first); `ma` must still be tested.
    assert_eq!(paths(&catalog, "rs ma"), ["/r/src/main.rs"]);
    // A glob's literal drives, but the glob is tested in full: notes.rst
    // holds `.rs` and is not `*.rs`.
    assert!(!paths(&catalog, "*.rs").iter().any(|p| p.ends_with(".rst")));
    assert_eq!(paths(&catalog, "notes"), ["/r/src/notes.rst"]);
}

#[test]
fn early_break_stops_the_run() {
    let scratch = Scratch::new("break");
    let catalog = sample(&scratch);
    let query = Query::parse("ext:rs", now()).unwrap();
    let mut seen = Vec::new();
    let stats = query
        .run(&catalog, |row| {
            seen.push(row.path.to_vec());
            if seen.len() == 2 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(stats.rows, 2);
}

/// A catalog of one root, `/m`, holding `f000` to `f{n-1}`, each of size
/// equal to its number.
fn many(scratch: &Scratch, n: u64) -> Catalog {
    let mut txn = Transaction::begin(&scratch.0, 1).unwrap();
    let mut w = txn.batch();
    let m = w.root(b"/m", dir(1));
    for i in 0..n {
        let name = format!("f{i:03}");
        w.file(
            m,
            name.as_bytes(),
            file(100 + i, i, DAY),
            Content::Unindexed,
        );
    }
    txn.add(w);
    txn.commit().unwrap();
    lazy(scratch)
}

#[test]
fn hits_far_apart_map_to_their_names() {
    let scratch = Scratch::new("locate");
    let catalog = many(&scratch, 300);
    let expect: Vec<String> = (200..300).map(|i| format!("/m/f{i}")).collect();
    assert_eq!(find(&catalog, "f2").0, expect);
    // Single hits with long gaps between them.
    let (found, _) = find(&catalog, "case:re:^f(007|150|299)$");
    assert_eq!(found, ["/m/f007", "/m/f150", "/m/f299"]);
    for i in [0, 1, 63, 64, 199, 298, 299] {
        let name = format!("f{i:03}");
        assert_eq!(find(&catalog, &name).0, [format!("/m/{name}")], "{name}");
    }
}

#[test]
fn every_row_carries_its_own_inode() {
    let scratch = Scratch::new("rows-own-inode");
    let catalog = many(&scratch, 300);
    let query = Query::parse("f", now()).unwrap();
    catalog.load(&[Section::Size]).unwrap();
    let mut sizes = Vec::new();
    query
        .run(&catalog, |row| {
            sizes.push(catalog.size(row.inode));
            ControlFlow::Continue(())
        })
        .unwrap();
    assert_eq!(sizes, (0..300).collect::<Vec<u64>>());
}

#[test]
fn a_root_at_slash_does_not_double_its_separator() {
    let scratch = Scratch::new("slash");
    let mut txn = Transaction::begin(&scratch.0, 1).unwrap();
    let mut w = txn.batch();
    let root = w.root(b"/", dir(1));
    let etc = w.dir(root, b"etc", dir(2));
    w.file(etc, b"passwd", file(3, 10, DAY), Content::Unindexed);
    w.file(root, b"vmlinuz", file(4, 10, DAY), Content::Unindexed);
    txn.add(w);
    txn.commit().unwrap();
    let catalog = lazy(&scratch);
    assert_eq!(paths(&catalog, ""), ["/etc", "/etc/passwd", "/vmlinuz"]);
    assert_eq!(paths(&catalog, "/etc/*"), ["/etc/passwd"]);
}

#[test]
fn a_metadata_pass_that_leaves_nothing_leaves_later_tests_sections_on_disk() {
    // Nothing is over a terabyte, so the size pass clears every bit and the
    // mtime and type passes never begin: their sections stay on disk.
    let scratch = Scratch::new("meta-short-circuit");
    sample(&scratch);
    for text in ["size:>1T mtime:<1d", "size:>1T type:l"] {
        let catalog = lazy(&scratch);
        let (found, stats) = find(&catalog, text);
        assert!(found.is_empty(), "{text:?}");
        assert_eq!(stats.candidates, 0, "{text:?}");
        assert!(catalog.is_loaded(Section::Size), "{text:?}");
        assert!(!catalog.is_loaded(Section::Mtime), "{text:?}");
        assert!(!catalog.is_loaded(Section::Links), "{text:?}");
    }
    // The control: main.rs passes `mtime:<1d`, so the size pass runs.
    let catalog = lazy(&scratch);
    assert!(find(&catalog, "mtime:<1d size:>1T").0.is_empty());
    assert!(catalog.is_loaded(Section::Mtime));
    assert!(catalog.is_loaded(Section::Size));
}

#[test]
fn metadata_passes_over_many_runs_match_each_inode_tested_alone() {
    // 300 inodes: the size pass clears the first runs of 64 whole, so the
    // mtime and type passes skip them, and the rest are partly set.
    let scratch = Scratch::new("meta-runs");
    let mut txn = Transaction::begin(&scratch.0, 1).unwrap();
    let mut w = txn.batch();
    let root = w.root(b"/w", dir(1));
    for j in 0..300u64 {
        let name = format!("f{j}");
        let age = (j % 7) as i64 * DAY + 1;
        if j % 10 == 0 {
            let link = stat(100 + j, 0o120_777, j, age);
            w.symlink(root, name.as_bytes(), link, b"x");
        } else {
            w.file(
                root,
                name.as_bytes(),
                file(100 + j, j, age),
                Content::Unindexed,
            );
        }
    }
    txn.add(w);
    txn.commit().unwrap();
    let expect = |keep: &dyn Fn(u64) -> bool| {
        let mut paths: Vec<_> = (0..300)
            .filter(|&j| keep(j))
            .map(|j| format!("/w/f{j}"))
            .collect();
        paths.sort();
        paths
    };
    let cases: [(&str, &dyn Fn(u64) -> bool); 3] = [
        ("size:>200 mtime:<3d", &|j| j > 200 && j % 7 <= 2),
        ("size:>200 type:l mtime:<3d", &|j| {
            j > 200 && j % 10 == 0 && j % 7 <= 2
        }),
        ("mtime:<1d size:<150", &|j| j < 150 && j % 7 == 0),
    ];
    for (text, keep) in cases {
        assert_eq!(paths(&lazy(&scratch), text), expect(keep), "{text:?}");
    }
}
