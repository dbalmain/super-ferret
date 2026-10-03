//! Semantic damage has valid wire checksums. Both the writer and the ordinary
//! effective reader must reject it, after the real shared record decoder.
use super::{SNIFFER, Scratch, commit, dir_stat, file_stat, hash};
use crate::log::{ChangeSet, Family, Published, Record, Writer};
use crate::{Catalog, Content, ContentState, InoId, Kind, NameId, Section, Transaction};
use std::fs;
fn fixture(label: &str) -> Scratch {
    let s = Scratch::new(label);
    commit(&s.path, |tx| {
        let mut b = tx.batch();
        let r = b.root(b"/r", dir_stat(1));
        b.entry_count(r, 2);
        b.file(r, b"old", file_stat(2), Content::Hashed(hash(1)));
        b.ignored(r, b"zz-ignored", Kind::File);
        tx.add(b);
    });
    s
}
fn changes(s: &Scratch, records: Vec<Record>) -> ChangeSet {
    let p = Published::open(&s.path).unwrap().unwrap();
    ChangeSet {
        records,
        counters: p.counters(),
        counts: p.counts(),
    }
}
fn signed(s: &Scratch, c: &ChangeSet) {
    let mut m = crate::read::read_manifest(&s.path).unwrap().unwrap();
    let seq = m.generation.sequence + 1;
    let bytes = crate::log::encode(c, seq, m.generation.sequence).unwrap();
    let mut file = fs::read(s.path.join("changes.0")).unwrap();
    file.extend(bytes);
    m.generation.sequence = seq;
    m.counters = c.counters;
    m.counts = c.counts;
    m.log_end = file.len() as u64;
    fs::write(s.path.join("changes.0"), file).unwrap();
    fs::write(s.path.join("current"), m.encode()).unwrap();
}
#[test]
fn signed_semantic_damage_drives_real_effective_reader_and_writer() {
    let cases: Vec<(&str, Vec<Record>, bool)> = vec![
        ("missing root", vec![Record::RootDelete { id: 0 }], false),
        (
            "wrong incoming edge",
            vec![Record::DirPut {
                id: 0,
                name: Some(0),
                entries: Some(1),
                flags: 4,
                retained_at: None,
            }],
            false,
        ),
        (
            "wrong name references",
            vec![Record::LifePut {
                id: 1,
                kind: Kind::File,
                flags: 0,
                names: 2,
            }],
            false,
        ),
        (
            "dead parent",
            vec![Record::NamePut {
                id: 0,
                parent: 1,
                child: 1,
                name: b"bad".to_vec(),
            }],
            false,
        ),
        (
            "root is a file",
            vec![Record::RootPut {
                id: 1,
                path: b"/file".to_vec(),
            }],
            false,
        ),
        (
            "document hash changed",
            vec![Record::DocPut {
                id: 0,
                references: 1,
                hash: hash(2),
            }],
            true,
        ),
        (
            "wrong document references",
            vec![Record::DocPut {
                id: 0,
                references: 2,
                hash: hash(1),
            }],
            true,
        ),
        (
            "link target on file",
            vec![Record::LinkPut {
                id: 1,
                target: b"target".to_vec(),
            }],
            true,
        ),
        (
            "worktree on file",
            vec![Record::WorkTreePut {
                id: 1,
                kind: crate::WorkTreeKind::Main,
                common_id: (1, 2),
                path: b"/git".to_vec(),
            }],
            true,
        ),
    ];
    for (label, records, full) in cases {
        let s = fixture(&format!("overlay-invalid-{label}"));
        let c = changes(&s, records);
        let before = fs::read(s.path.join("current")).unwrap();
        let mut w = Writer::open(&s.path).unwrap();
        assert!(w.commit(w.generation(), &c).is_err(), "writer {label}");
        drop(w);
        assert_eq!(fs::read(s.path.join("current")).unwrap(), before, "{label}");
        signed(&s, &c);
        let view = Catalog::open(&s.path).unwrap().unwrap();
        let result = if full {
            view.load_all()
        } else {
            view.load(&[Section::Roots])
        };
        assert!(result.is_err(), "reader {label}");
    }
}
#[test]
fn birth_fields_and_declared_live_counts_are_checked_before_access() {
    for case in 0..3 {
        let s = fixture(&format!("overlay-birth-{case}"));
        let mut c = changes(
            &s,
            vec![
                Record::LifePut {
                    id: 2,
                    kind: Kind::File,
                    flags: 0,
                    names: 1,
                },
                Record::NamePut {
                    id: 2,
                    parent: 0,
                    child: 2,
                    name: b"new".to_vec(),
                },
            ],
        );
        c.counters[0] += 1;
        c.counters[1] += 1;
        c.counts[0] += 1;
        c.counts[1] += 1;
        if case == 1 {
            c.counts[0] -= 1;
        }
        if case == 2 {
            c.records.retain(|r| !matches!(r, Record::LifePut { .. }));
        }
        signed(&s, &c);
        let view = Catalog::open(&s.path).unwrap().unwrap();
        assert!(view.load(&[Section::Size]).is_err(), "case {case}");
        assert!(!view.is_loaded(Section::Size));
        assert!(!view.is_loaded(Section::Names));
        assert!(!view.is_loaded(Section::NameHeap));
    }
}
#[test]
fn unused_payload_damage_does_not_fail_a_name_or_metadata_query() {
    let s = fixture("overlay-lazy-damage");
    let mut w = Writer::open(&s.path).unwrap();
    let c = changes(
        &s,
        vec![
            Record::NamePut {
                id: 0,
                parent: 0,
                child: 1,
                name: b"renamed".to_vec(),
            },
            Record::InodePut {
                id: 1,
                kind: Kind::File,
                state: ContentState::Hashed,
                doc: Some(0),
                stat: file_stat(2),
            },
            Record::DocPut {
                id: 0,
                references: 1,
                hash: hash(1),
            },
        ],
    );
    w.commit(w.generation(), &c).unwrap();
    drop(w);
    let path = s.path.join("changes.0");
    let original = fs::read(&path).unwrap();
    let tx = 64;
    let count = crate::format::u32_at(&original, tx + 12) as usize;
    for family in [Family::Inodes, Family::Docs] {
        let mut bad = original.clone();
        let d = (0..count)
            .map(|i| tx + 64 + i * 48)
            .find(|&d| u16::from_le_bytes([bad[d], bad[d + 1]]) == family as u16)
            .unwrap();
        let offset = tx + crate::format::u64_at(&bad, d + 8) as usize;
        bad[offset] ^= 1;
        fs::write(&path, bad).unwrap();
        let names = Catalog::open(&s.path).unwrap().unwrap();
        names.load(&[Section::Roots]).unwrap();
        assert_eq!(names.name(NameId(0)).bytes, b"renamed");
        assert!(!names.is_loaded(Section::Size));
        assert!(
            names
                .load(&[if family == Family::Inodes {
                    Section::Size
                } else {
                    Section::Docs
                }])
                .is_err()
        );
        if family == Family::Docs {
            let meta = Catalog::open(&s.path).unwrap().unwrap();
            meta.load(&[Section::Size]).unwrap();
            assert_eq!(meta.size(InoId(1)), file_stat(2).size);
            assert!(!meta.is_loaded(Section::Names));
        }
    }
    fs::write(path, original).unwrap();
}
#[test]
fn pinned_effective_views_and_keep_survive_later_append_and_epoch_unlink() {
    let s = fixture("overlay-pinned-keep");
    let c = changes(
        &s,
        vec![Record::NamePut {
            id: 0,
            parent: 0,
            child: 1,
            name: b"first".to_vec(),
        }],
    );
    let mut w = Writer::open(&s.path).unwrap();
    w.commit(w.generation(), &c).unwrap();
    let old = Catalog::open(&s.path).unwrap().unwrap();
    let mut c = c;
    c.records = vec![Record::NamePut {
        id: 0,
        parent: 0,
        child: 1,
        name: b"second".to_vec(),
    }];
    w.commit(w.generation(), &c).unwrap();
    drop(w);
    let mut tx = Transaction::begin(&s.path, SNIFFER).unwrap();
    assert_eq!(tx.previous().unwrap().name(NameId(0)).bytes, b"second");
    tx.keep(b"/r").unwrap();
    let new = tx.commit().unwrap();
    assert!(!s.path.join("snapshot.0").exists());
    assert!(!s.path.join("changes.0").exists());
    old.load_all().unwrap();
    assert_eq!(old.name(NameId(0)).bytes, b"first");
    assert_eq!(new.name(NameId(0)).bytes, b"second");
    assert_ne!(old.generation().checkpoint, new.generation().checkpoint);
}

#[test]
fn incremental_orphan_and_type_damage_is_rejected_after_a_checked_predecessor() {
    for case in 0..2 {
        let s = fixture(&format!("overlay-incremental-damage-{case}"));
        let mut w = Writer::open(&s.path).unwrap();
        let first = changes(
            &s,
            vec![Record::NamePut {
                id: 0,
                parent: 0,
                child: 1,
                name: b"first".to_vec(),
            }],
        );
        w.commit(w.generation(), &first).unwrap();
        let mut bad = changes(
            &s,
            if case == 0 {
                vec![Record::NameDelete { id: 0 }]
            } else {
                vec![
                    Record::InodePut {
                        id: 1,
                        kind: Kind::Symlink,
                        state: ContentState::Unindexed,
                        doc: None,
                        stat: crate::Stat {
                            mode: 0o120777,
                            ..file_stat(2)
                        },
                    },
                    Record::DocDelete { id: 0 },
                ]
            },
        );
        if case == 0 {
            bad.counts[1] -= 1;
        } else {
            bad.counts[3] -= 1;
        }
        let before = fs::read(s.path.join("current")).unwrap();
        assert!(
            w.commit(w.generation(), &bad).is_err(),
            "writer case {case}"
        );
        assert_eq!(fs::read(s.path.join("current")).unwrap(), before);
        drop(w);
        signed(&s, &bad);
        let view = Catalog::open(&s.path).unwrap().unwrap();
        assert!(view.load_all().is_err(), "reader case {case}");
    }
}
#[test]
fn same_sequence_key_removals_and_additions_form_one_run() {
    let s = fixture("overlay-same-sequence-keys");
    let mut w = Writer::open(&s.path).unwrap();
    // Two old keys and one addition occupy different geometric levels if
    // inserted separately. Their equal sequences must still retain the put.
    let mut c = changes(
        &s,
        vec![
            Record::NamePut {
                id: 0,
                parent: 0,
                child: 1,
                name: b"old".to_vec(),
            },
            Record::NameDelete { id: 1 },
        ],
    );
    c.counts[1] -= 1;
    w.commit(w.generation(), &c).unwrap();
    for view in [w.view(), Catalog::open(&s.path).unwrap().unwrap()] {
        view.load_all().unwrap();
        assert_eq!(view.lookup(InoId(0), b"old"), Some(NameId(0)));
        assert_eq!(view.children(InoId(0)).collect::<Vec<_>>(), vec![NameId(0)]);
        assert_eq!(view.lookup(InoId(0), b"zz-ignored"), None);
    }
}
