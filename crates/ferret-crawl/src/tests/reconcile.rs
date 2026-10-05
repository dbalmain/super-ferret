//! Real-tree recrawls, real durable writer and disk reader. Every prefix is
//! compared with a fresh full crawl/checkpoint using M3's semantic oracle.
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use ferret_catalog::{Catalog, InoId};

use super::index::Tmp;
use crate::{IndexOptions, Refresh, index, recrawl};

use super::checkpoint_oracle;
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
    let mut session = super::log_session(&tmp.cat()).unwrap();
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
    let mut session = super::log_session(&tmp.cat()).unwrap();
    let old = session.view();
    let id = file(&old, &roots[0], b"a");
    let doc = old.doc(id);
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    let current = session.view();
    assert_eq!(file(&current, &roots[0], b"a"), id);
    assert_eq!(current.doc(id), doc);
    assert_eq!(old.generation().checkpoint, current.generation().checkpoint);
    let published = ferret_catalog::log::Published::open(&tmp.cat())
        .unwrap()
        .unwrap();
    published.log().load_all().unwrap();
    for family in [
        ferret_catalog::log::Family::Namespace,
        ferret_catalog::log::Family::Aux,
        ferret_catalog::log::Family::Docs,
    ] {
        assert_eq!(published.log().records(family).count(), 0);
    }
    assert_eq!(
        published
            .log()
            .records(ferret_catalog::log::Family::Inodes)
            .count(),
        1
    );
    oracle(&tmp, &roots, &opts, &current);
}

#[test]
fn rename_and_moving_last_hard_link_keep_inode_and_document() {
    let tmp = Tmp::new("recrawl-rename");
    tmp.write("a", b"content");
    let roots = [tmp.tree()];
    let opts = options();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
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

#[test]
fn delete_then_create_same_path_with_new_inode_gets_new_ids() {
    let tmp = Tmp::new("recrawl-replace");
    tmp.write("a", b"old content");
    let roots = [tmp.tree()];
    let opts = options();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    let old = session.view();
    let id = file(&old, &roots[0], b"a");
    let doc = old.doc(id);
    // Allocate while the old inode is live, ensuring a genuinely different ino.
    let replacement = tmp.write("replacement", b"new content");
    assert_ne!(
        fs::metadata(&replacement).unwrap().ino(),
        old.inode(id).stat.ino
    );
    fs::rename(replacement, tmp.at("a")).unwrap();
    recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    let current = session.view();
    let new = file(&current, &roots[0], b"a");
    assert_ne!(id, new);
    assert_ne!(doc, current.doc(new));
    assert!(!current.is_live_inode(id));
    let root = current.roots().next().unwrap().0;
    assert_ne!(old.lookup(root, b"a"), current.lookup(root, b"a"));
    oracle(&tmp, &roots, &opts, &current);
    // Returning old content must allocate a fresh document, not resurrect a
    // dead id.
    fs::write(tmp.at("a"), b"old content").unwrap();
    recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    assert_ne!(session.view().doc(new), doc);
    oracle(&tmp, &roots, &opts, &session.view());
}

#[test]
fn inode_number_reuse_with_changed_ctime_never_carries_old_content() {
    use ferret_catalog::log::{ChangeSet, Record, Writer};
    let tmp = Tmp::new("recrawl-reuse");
    tmp.write("a", b"old");
    let roots = [tmp.tree()];
    let opts = options();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let old = open(&tmp.cat());
    let id = file(&old, &roots[0], b"a");
    let replacement = tmp.write("replacement", b"new");
    fs::rename(replacement, tmp.at("a")).unwrap();
    let meta = fs::metadata(tmp.at("a")).unwrap();
    // Kernel-number reuse cannot be forced portably. Install the adversarial
    // previous observation through the real writer: every carry-key field
    // equals the replacement except ctime, and the old document still says old.
    let mut stat = old.inode(id).stat;
    stat.ino = meta.ino();
    stat.dev = meta.dev();
    stat.size = meta.size();
    stat.mtime_sec = meta.mtime();
    stat.mtime_nsec = meta.mtime_nsec() as u32;
    stat.ctime_sec = meta.ctime() - 1;
    stat.ctime_nsec = meta.ctime_nsec() as u32;
    let mut writer = Writer::open(&tmp.cat()).unwrap();
    writer
        .commit(
            old.generation(),
            &ChangeSet {
                counters: [old.next_inode().0, old.next_name().0, old.next_doc().0],
                counts: [
                    old.inode_count(),
                    old.name_count(),
                    old.dir_count(),
                    old.doc_count(),
                ],
                records: vec![Record::InodePut {
                    id: id.0,
                    kind: old.kind(id),
                    state: old.state(id),
                    doc: old.doc(id).map(|d| d.0),
                    stat,
                }],
            },
        )
        .unwrap();
    drop(writer);
    let report = index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    assert_eq!(report.counts.carried, 0);
    assert_eq!(report.counts.files_read, 1);
    let current = open(&tmp.cat());
    assert_ne!(
        current.doc(id).and_then(|d| current.doc_hash(d)),
        old.doc(id).and_then(|d| old.doc_hash(d))
    );
    oracle(&tmp, &roots, &opts, &current);
}

#[test]
fn hard_links_across_kept_and_refreshed_roots_share_final_references() {
    let tmp = Tmp::new("recrawl-hardlink-roots");
    let a = tmp.write("left/a", b"content");
    fs::create_dir_all(tmp.at("right")).unwrap();
    fs::hard_link(&a, tmp.at("right/a")).unwrap();
    let roots = [tmp.at("left"), tmp.at("right")];
    let opts = options();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    let old = session.view();
    let id = file(&old, &roots[0], b"a");
    let doc = old.doc(id).unwrap();
    for (from, to) in [("left/a", "left/b"), ("left/b", "left/c")] {
        fs::rename(tmp.at(from), tmp.at(to)).unwrap();
        let report = recrawl(&mut session, &roots, Refresh::Only(&roots[..1]), &opts).unwrap();
        assert_eq!(report.kept, roots[1..]);
        let current = session.view();
        assert_eq!(file(&current, &roots[1], b"a"), id);
        assert_eq!(current.doc_references(doc), Some(1));
        assert_eq!(session.name_references(id), 2);
        oracle(&tmp, &roots, &opts, &current);
    }
    fs::remove_file(tmp.at("left/c")).unwrap();
    recrawl(&mut session, &roots, Refresh::Only(&roots[..1]), &opts).unwrap();
    let current = session.view();
    assert!(current.is_live_inode(id));
    assert_eq!(current.doc(id), Some(doc));
    assert_eq!(session.name_references(id), 1);
    oracle(&tmp, &roots, &opts, &current);
    fs::write(tmp.at("right/a"), b"changed").unwrap();
    recrawl(&mut session, &roots, Refresh::Only(&roots[1..]), &opts).unwrap();
    oracle(&tmp, &roots, &opts, &session.view());
}

#[test]
fn policy_sniffer_and_nested_root_boundary_changes_d34_match_full_index() {
    let tmp = Tmp::new("recrawl-policy-boundaries");
    tmp.write("outer/a", b"content");
    tmp.write("outer/inner/b", b"content");
    tmp.write("kept/c", b"another");
    let mut roots = vec![tmp.at("kept"), tmp.at("outer")];
    let mut opts = options();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    let epoch = session.view().generation().checkpoint;
    roots.push(tmp.at("outer/inner"));
    let report = recrawl(&mut session, &roots, Refresh::Only(&[]), &opts).unwrap();
    assert!(report.refreshed.contains(&tmp.at("outer")));
    oracle(&tmp, &roots, &opts, &session.view());
    roots.pop();
    recrawl(&mut session, &roots, Refresh::Only(&[]), &opts).unwrap();
    oracle(&tmp, &roots, &opts, &session.view());
    opts.global = Some("a\n".to_owned());
    let report = recrawl(&mut session, &roots, Refresh::Only(&[]), &opts).unwrap();
    assert_eq!(report.refreshed.len(), roots.len());
    oracle(&tmp, &roots, &opts, &session.view());
    opts.config.size_cap = 1;
    recrawl(&mut session, &roots, Refresh::Only(&[]), &opts).unwrap();
    oracle(&tmp, &roots, &opts, &session.view());
    opts.config.size_cap = u64::MAX;
    opts.sniffer += 1;
    recrawl(&mut session, &roots, Refresh::Only(&[]), &opts).unwrap();
    assert_eq!(session.view().sniffer_version(), opts.sniffer);
    assert_eq!(open(&tmp.cat()).sniffer_version(), opts.sniffer);
    assert_eq!(session.view().generation().checkpoint, epoch);
    oracle(&tmp, &roots, &opts, &session.view());
    assert!(
        recrawl(&mut session, &roots, Refresh::All, &opts)
            .unwrap()
            .published
            .is_none()
    );
    oracle(&tmp, &roots, &opts, &session.view());
}

#[test]
fn moving_directory_keeps_descendant_parent_ids_and_overwrites_destination() {
    let tmp = Tmp::new("recrawl-dir-move");
    tmp.write("a/child", b"content");
    tmp.write("b/gone", b"other");
    let roots = [tmp.tree()];
    let opts = options();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    let old = session.view();
    let dir = file(&old, &roots[0], b"a");
    let child_name = old.lookup(dir, b"child").unwrap();
    fs::remove_dir_all(tmp.at("b")).unwrap();
    fs::rename(tmp.at("a"), tmp.at("b")).unwrap();
    recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    let current = session.view();
    assert_eq!(file(&current, &roots[0], b"b"), dir);
    assert_eq!(current.lookup(dir, b"child"), Some(child_name));
    oracle(&tmp, &roots, &opts, &current);
    fs::create_dir(tmp.at("later")).unwrap();
    fs::rename(tmp.at("b"), tmp.at("later/moved")).unwrap();
    recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    oracle(&tmp, &roots, &opts, &session.view());
}

#[test]
fn generated_recrawl_sequences_match_fresh_materialised_checkpoints_at_every_prefix() {
    for seed in 1..=8u64 {
        let tmp = Tmp::new(&format!("recrawl-generated-{seed}"));
        let roots = [tmp.tree()];
        let opts = options();
        for name in ["a", "b", "c"] {
            tmp.write(name, name.as_bytes());
        }
        index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
        let mut session = super::log_session(&tmp.cat()).unwrap();
        let mut rng = seed;
        for step in 0..32 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let names = ["a", "b", "c", "d", "e", "f"];
            let a = tmp.at(names[(rng as usize >> 8) % names.len()]);
            let b = tmp.at(names[(rng as usize >> 16) % names.len()]);
            match rng % 6 {
                0 => {
                    fs::write(&a, format!("{seed}-{step}")).unwrap();
                }
                1 if a.exists() => {
                    fs::remove_file(&a).unwrap();
                }
                2 if a.exists() && a != b => {
                    fs::rename(&a, &b).unwrap();
                }
                3 if a.exists() && !b.exists() => {
                    fs::hard_link(&a, &b).unwrap();
                }
                4 if a.exists() => {
                    fs::set_permissions(
                        &a,
                        fs::Permissions::from_mode(if step % 2 == 0 { 0o600 } else { 0o644 }),
                    )
                    .unwrap();
                }
                _ => {}
            }
            recrawl(&mut session, &roots, Refresh::All, &opts)
                .unwrap_or_else(|e| panic!("seed {seed}, prefix {step}: {e}"));
            oracle(&tmp, &roots, &opts, &session.view());
        }
    }
}

#[test]
fn moving_last_hard_link_by_create_and_delete_neither_kills_nor_recreates_ids() {
    let tmp = Tmp::new("recrawl-last-link");
    tmp.write("a", b"content");
    let roots = [tmp.tree()];
    let opts = options();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    let old = session.view();
    let id = file(&old, &roots[0], b"a");
    let doc = old.doc(id);
    for (from, to) in [("a", "b"), ("b", "a")] {
        fs::hard_link(tmp.at(from), tmp.at(to)).unwrap();
        fs::remove_file(tmp.at(from)).unwrap();
        recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        let current = session.view();
        assert_eq!(file(&current, &roots[0], to.as_bytes()), id);
        assert_eq!(current.doc(id), doc);
        assert_eq!(session.name_references(id), 1);
        oracle(&tmp, &roots, &opts, &current);
    }
}

#[test]
fn ambiguous_hard_link_renames_allocate_edges_but_keep_inode_and_document() {
    let tmp = Tmp::new("recrawl-ambiguous-links");
    tmp.write("a", b"content");
    fs::hard_link(tmp.at("a"), tmp.at("b")).unwrap();
    let roots = [tmp.tree()];
    let opts = options();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    let old = session.view();
    let root = old.roots().next().unwrap().0;
    let id = file(&old, &roots[0], b"a");
    let doc = old.doc(id);
    let old_names = [
        old.lookup(root, b"a").unwrap(),
        old.lookup(root, b"b").unwrap(),
    ];
    for (from, to) in [("a", "c"), ("b", "d")] {
        fs::rename(tmp.at(from), tmp.at(to)).unwrap();
    }
    recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    let current = session.view();
    for name in [b"c", b"d"] {
        assert_eq!(file(&current, &roots[0], name), id);
        assert_eq!(current.doc(id), doc);
        assert!(!old_names.contains(&current.lookup(root, name).unwrap()));
    }
    oracle(&tmp, &roots, &opts, &current);
}

#[test]
fn explicit_root_removals_preserve_shared_inodes_until_the_final_root_disappears() {
    let tmp = Tmp::new("recrawl-remove-roots");
    let a = tmp.write("left/a", b"content");
    fs::create_dir_all(tmp.at("right")).unwrap();
    fs::hard_link(a, tmp.at("right/a")).unwrap();
    let roots = [tmp.at("left"), tmp.at("right")];
    let opts = options();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    let old = session.view();
    let id = file(&old, &roots[0], b"a");
    let doc = old.doc(id).unwrap();
    recrawl(&mut session, &roots[1..], Refresh::Only(&[]), &opts).unwrap();
    let current = session.view();
    assert!(current.is_live_inode(id));
    assert_eq!(current.doc(id), Some(doc));
    assert_eq!(current.doc_references(doc), Some(1));
    oracle(&tmp, &roots[1..], &opts, &current);
    recrawl(&mut session, &[], Refresh::Only(&[]), &opts).unwrap();
    let current = session.view();
    assert!(!current.is_live_inode(id));
    assert_eq!(current.doc_hash(doc), None);
    oracle(&tmp, &[], &opts, &current);
}

#[test]
fn shared_hash_replacement_reference_changes_are_one_final_set() {
    let tmp = Tmp::new("recrawl-shared-hash");
    tmp.write("a", b"same");
    tmp.write("b", b"same");
    let roots = [tmp.tree()];
    let opts = options();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    let old = session.view();
    let id = file(&old, &roots[0], b"a");
    let doc = old.doc(id).unwrap();
    assert_eq!(old.doc_references(doc), Some(2));
    for from in ["a", "b"] {
        // New inode, same hash: retiring one binding and adding another must
        // keep the still-live document instead of a transient death/birth.
        let replacement = tmp.write("replacement", b"same");
        fs::rename(replacement, tmp.at(from)).unwrap();
        recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        let current = session.view();
        assert_eq!(
            current.doc(file(&current, &roots[0], from.as_bytes())),
            Some(doc)
        );
        assert_eq!(current.doc_references(doc), Some(2));
        oracle(&tmp, &roots, &opts, &current);
    }
}

#[test]
fn final_log_order_is_identical_across_walk_worker_counts() {
    let tmp = Tmp::new("recrawl-order");
    for name in ["a", "b", "c"] {
        tmp.write(&format!("dir/{name}"), name.as_bytes());
    }
    fs::hard_link(tmp.at("dir/a"), tmp.at("alias")).unwrap();
    let roots = [tmp.tree()];
    let left = tmp.cat();
    let right = tmp.base.join("other-index");
    for (path, workers) in [(&left, 1), (&right, 8)] {
        let opts = IndexOptions {
            workers,
            ..options()
        };
        index(path, &roots, Refresh::All, &opts).unwrap();
    }
    fs::rename(tmp.at("dir"), tmp.at("moved")).unwrap();
    tmp.write("fresh/a", b"new");
    tmp.write("fresh/b", b"new");
    fs::remove_file(tmp.at("alias")).unwrap();
    for (path, workers) in [(&left, 1), (&right, 8)] {
        let opts = IndexOptions {
            workers,
            ..options()
        };
        index(path, &roots, Refresh::All, &opts).unwrap();
        oracle(&tmp, &roots, &opts, &open(path));
    }
    let read_log = |path: &Path| {
        let catalog = open(path);
        let bytes =
            fs::read(path.join(format!("changes.{}", catalog.generation().checkpoint))).unwrap();
        bytes[64..].to_vec()
    };
    assert_eq!(read_log(&left), read_log(&right));
}

#[test]
fn directory_eacces_retires_old_subtree_and_recovery_matches_full_checkpoint() {
    let tmp = Tmp::new("recrawl-incomplete");
    tmp.write("dir/a", b"content");
    for i in 0..8 {
        tmp.write(&format!("stable-{i}"), b"unchanged");
    }
    let roots = [tmp.tree()];
    let opts = options();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    let old = session.view();
    fs::set_permissions(tmp.at("dir"), fs::Permissions::from_mode(0o000)).unwrap();
    let report = recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    assert_eq!(report.protected_scopes, 0);
    assert!(report.coverage_faults.is_empty());
    let dir = file(&session.view(), &roots[0], b"dir");
    assert_eq!(session.view().entry_count(dir), None);
    assert_eq!(session.view().retained_at(dir), None);
    assert_eq!(session.view().children(dir).count(), 0);
    oracle(&tmp, &roots, &opts, &session.view());
    assert_eq!(listings(&session.view()), listings(&open(&tmp.cat())));
    assert_eq!(
        session.view().generation().checkpoint,
        old.generation().checkpoint
    );
    assert!(
        recrawl(&mut session, &roots, Refresh::All, &opts)
            .unwrap()
            .published
            .is_none()
    );
    drop(session);
    assert!(
        index(&tmp.cat(), &roots, Refresh::All, &opts)
            .unwrap()
            .published
            .is_none()
    );
    fs::set_permissions(tmp.at("dir"), fs::Permissions::from_mode(0o755)).unwrap();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    oracle(&tmp, &roots, &opts, &open(&tmp.cat()));
}

#[test]
fn local_policy_traversal_collapse_and_work_tree_changes_match_full_checkpoint() {
    let tmp = Tmp::new("recrawl-traversal-worktree");
    tmp.write("hidden/deep/file.txt", b"content");
    tmp.write("repo/a", b"other");
    let roots = [tmp.tree()];
    let opts = IndexOptions {
        global: Some(".git/\n".into()),
        ..options()
    };
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    for rules in ["hidden/\n!hidden/deep/file.txt\n", "hidden/\n", ""] {
        tmp.write(".ferretignore", rules.as_bytes());
        recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
        oracle(&tmp, &roots, &opts, &session.view());
    }
    fs::create_dir(tmp.at("repo/.git")).unwrap();
    recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    oracle(&tmp, &roots, &opts, &session.view());
    fs::remove_dir(tmp.at("repo/.git")).unwrap();
    recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    oracle(&tmp, &roots, &opts, &session.view());
}

#[test]
fn reopening_session_preserves_document_hash_holes_and_never_resurrects_docids() {
    let tmp = Tmp::new("recrawl-reopen-hashes");
    tmp.write("a", b"old");
    tmp.write("b", b"shared");
    let roots = [tmp.tree()];
    let opts = options();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let old = open(&tmp.cat());
    let id = file(&old, &roots[0], b"a");
    let retired = old.doc(id).unwrap();
    let shared = old.doc(file(&old, &roots[0], b"b")).unwrap();
    fs::write(tmp.at("a"), b"shared").unwrap();
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    assert_eq!(session.view().doc(id), Some(shared));
    assert_eq!(session.view().doc_hash(retired), None);
    assert!(
        recrawl(&mut session, &roots, Refresh::All, &opts)
            .unwrap()
            .published
            .is_none()
    );
    oracle(&tmp, &roots, &opts, &session.view());
    fs::write(tmp.at("a"), b"old").unwrap();
    recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    assert_ne!(session.view().doc(id), Some(retired));
    oracle(&tmp, &roots, &opts, &session.view());
    drop(session);
    let mut session = super::log_session(&tmp.cat()).unwrap();
    assert!(
        recrawl(&mut session, &roots, Refresh::All, &opts)
            .unwrap()
            .published
            .is_none()
    );
    oracle(&tmp, &roots, &opts, &session.view());
}

#[test]
fn compact_equal_observation_joins_a_new_alias_before_conflict_resolution() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use ferret_catalog::ContentState;

    use super::index::Hook;
    use crate::index::Probe;

    let tmp = Tmp::new("recrawl-compact-alias-conflict");
    tmp.write("a", b"content");
    fs::create_dir(tmp.at("z")).unwrap();
    let roots = [tmp.tree()];
    let opts = IndexOptions {
        workers: 1,
        ..options()
    };
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    let old = session.view();
    let id = file(&old, &roots[0], b"a");
    let changed = Arc::new(AtomicBool::new(false));
    let tree = tmp.tree();
    let hook = Hook::set(&tree, {
        let tree = tree.clone();
        let changed = Arc::clone(&changed);
        move |probe| {
            if let Probe::Carried(path) = probe
                && path == Path::new("a")
                && !changed.swap(true, Ordering::SeqCst)
            {
                fs::hard_link(tree.join("a"), tree.join("z/alias")).unwrap();
            }
        }
    });
    recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    drop(hook);
    assert!(changed.load(Ordering::SeqCst));
    let current = session.view();
    // The old single-name row was fully equal and compacted. The new alias
    // observes nlink/ctime after the mutation. Omitting the compact row from
    // the residue would publish Hashed instead of the required shared Fault.
    assert_eq!(file(&current, &roots[0], b"a"), id);
    assert_eq!(current.state(id), ContentState::Fault);
    assert_eq!(current.doc(id), None);
    assert_eq!(session.name_references(id), 2);
    assert_eq!(listings(&current), listings(&open(&tmp.cat())));
    // A stable retry is compared with a fresh full crawl. The raced pass is
    // deliberately Fault and cannot equal a later stable full observation.
    recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    oracle(&tmp, &roots, &opts, &session.view());
}

#[test]
fn reused_residue_alias_expansion_exhausts_the_input_guard() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::index::Hook;
    use crate::index::Probe;

    const ALIASES: usize = 2000;
    let tmp = Tmp::new("recrawl-residue-alias-guard");
    tmp.write("content", b"shared");
    fs::create_dir(tmp.at("aaa")).unwrap();
    for n in 0..ALIASES {
        fs::hard_link(tmp.at("content"), tmp.at(&format!("aaa/link-{n:04}"))).unwrap();
    }
    tmp.write("m", b"trigger");
    fs::create_dir(tmp.at("zzz")).unwrap();
    let roots = [tmp.tree()];
    let opts = IndexOptions {
        workers: 1,
        ..options()
    };
    index(&tmp.cat(), &roots, Refresh::All, &opts).unwrap();
    let mut session = super::log_session(&tmp.cat()).unwrap();
    session.set_input_limits(ferret_catalog::InputLimits {
        records: usize::MAX,
        owned_bytes: 8192,
    });
    let changed = Arc::new(AtomicBool::new(false));
    let tree = tmp.tree();
    let hook = Hook::set(&tree, {
        let tree = tree.clone();
        let changed = Arc::clone(&changed);
        move |probe| {
            if let Probe::Carried(path) = probe
                && path == Path::new("m")
                && !changed.swap(true, Ordering::SeqCst)
            {
                // By now "aaa"'s 2000 equal hard-link names have already been
                // lstat'd and matched against the old generation, well before
                // this new alias (sharing the same inode, discovered only when
                // "zzz" is listed later) can be observed. Expanding the
                // compacted residue to keep conflict resolution correct must
                // still be charged against the shared input guard.
                fs::hard_link(tree.join("content"), tree.join("zzz/alias")).unwrap();
            }
        }
    });
    let result = recrawl(&mut session, &roots, Refresh::All, &opts).unwrap();
    drop(hook);
    assert!(changed.load(Ordering::SeqCst));
    assert!(
        result.input_fallback,
        "unguarded alias-residue expansion must trip the shared input guard"
    );
    assert!(result.input_usage.exceeded);
    assert!(result.input_usage.owned_bytes <= 8192);
    oracle(&tmp, &roots, &opts, &session.view());
}
