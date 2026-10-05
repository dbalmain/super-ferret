//! Tests use the production intake seam, real kernel watches and real crawl
//! publication. Shared FD admission respects the existing descriptor tests.
use std::fs;
use std::sync::RwLockReadGuard;

use super::*;
use crate::{IndexOptions, Refresh, index};
use ferret_catalog::WriterSession;

struct Tree {
    _fds: RwLockReadGuard<'static, ()>,
    path: PathBuf,
    watch: Arc<Watch>,
    writer: WriterSession,
}
impl Tree {
    fn new(name: &str, scopes: usize, bytes: usize) -> Self {
        let fds = crate::tests::FDS
            .read()
            .unwrap_or_else(|error| panic!("FD admission: {error:?}"));
        let path =
            std::env::temp_dir().join(format!("ferret-intake-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("root/a")).unwrap_or_else(|error| panic!("root: {error:?}"));
        fs::create_dir_all(path.join("root/b")).unwrap_or_else(|error| panic!("root: {error:?}"));
        fs::write(path.join("root/a/old"), b"old")
            .unwrap_or_else(|error| panic!("file: {error:?}"));
        let watch = Arc::new(
            Watch::new(Config {
                watch_cap: 100,
                scopes,
                bytes,
            })
            .unwrap_or_else(|error| panic!("inotify: {error:?}")),
        );
        let options = IndexOptions {
            watch: Some(watch.clone()),
            workers: 1,
            ..IndexOptions::default()
        };
        index(
            &path.join("index"),
            &[path.join("root")],
            Refresh::All,
            &options,
        )
        .unwrap_or_else(|error| panic!("index: {error:?}"));
        let writer = WriterSession::open(&path.join("index"))
            .unwrap_or_else(|error| panic!("writer: {error:?}"));
        Self {
            _fds: fds,
            path,
            watch,
            writer,
        }
    }
    fn descriptor(&self, suffix: &str) -> i32 {
        self.watch
            .state
            .lock()
            .unwrap_or_else(|error| panic!("state: {error:?}"))
            .descriptors
            .iter()
            .find(|(_, d)| {
                d.iter()
                    .any(|d| d.path() == self.path.join("root").join(suffix))
            })
            .map(|(&wd, _)| wd)
            .unwrap_or_else(|| panic!("installed descriptor"))
    }
    fn due(&self) -> Burst {
        self.watch
            .state
            .lock()
            .unwrap_or_else(|error| panic!("state: {error:?}"))
            .first = Some(Instant::now() - MAX_AGE);
        self.watch.take().unwrap_or_else(|| panic!("due burst"))
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn crawl_arms_all_directories_before_observation_and_ignores_its_reads() {
    let tree = Tree::new("arm-and-mask", 100, 1 << 20);
    assert_eq!(tree.watch.status().installed, 4);
    fs::read(tree.path.join("root/a/old")).unwrap_or_else(|error| panic!("own read: {error:?}"));
    tree.watch
        .drain()
        .unwrap_or_else(|error| panic!("drain: {error:?}"));
    assert_eq!(
        tree.watch.status().pending,
        0,
        "access/open/read-close must not enqueue work"
    );
    fs::write(tree.path.join("root/a/new"), b"new")
        .unwrap_or_else(|error| panic!("create: {error:?}"));
    tree.watch
        .drain()
        .unwrap_or_else(|error| panic!("real kernel events: {error:?}"));
    let request = tree.due().request(&tree.writer.view());
    assert!(
        request
            .scopes
            .iter()
            .any(|s| matches!(s, RefreshScope::Entry { basename, .. } if basename == b"new"))
    );
}

#[test]
fn debounce_is_trailing_but_maximum_age_cannot_be_postponed() {
    let tree = Tree::new("debounce", 100, 1 << 20);
    let wd = tree.descriptor("a");
    tree.watch.event(wd, ReadFlags::MODIFY, 0, b"old");
    assert!(tree.watch.take().is_none());
    {
        let mut state = tree
            .watch
            .state
            .lock()
            .unwrap_or_else(|error| panic!("state: {error:?}"));
        state.last = Some(Instant::now() - Duration::from_millis(201));
    }
    let burst = tree
        .watch
        .take()
        .unwrap_or_else(|| panic!("trailing deadline"));
    tree.watch.finish(burst, true);
    tree.watch.event(wd, ReadFlags::MODIFY, 0, b"old");
    let burst = tree.due();
    assert_eq!(burst.reason(), RefreshReason::Burst);
}

#[test]
fn cookie_pair_survives_create_and_close_write_coalescing() {
    let tree = Tree::new("cookie-pair", 100, 1 << 20);
    let a = tree.descriptor("a");
    let b = tree.descriptor("b");
    tree.watch.event(a, ReadFlags::CREATE, 0, b"old");
    tree.watch.event(a, ReadFlags::MOVED_FROM, 8, b"old");
    tree.watch.event(b, ReadFlags::MOVED_TO, 8, b"new");
    tree.watch.event(b, ReadFlags::CLOSE_WRITE, 0, b"new");
    let request = tree.due().request(&tree.writer.view());
    assert_eq!(request.rename_hints.len(), 1);
    assert_eq!(request.rename_hints[0].old_name, b"old");
    assert_eq!(request.rename_hints[0].new_name, b"new");
}

#[test]
fn wrong_duplicate_and_unpaired_cookies_keep_all_observations() {
    let tree = Tree::new("wrong-cookies", 100, 1 << 20);
    let wd = tree.descriptor("a");
    tree.watch.event(wd, ReadFlags::MOVED_FROM, 7, b"old");
    tree.watch.event(wd, ReadFlags::MOVED_FROM, 8, b"old");
    tree.watch.event(wd, ReadFlags::MOVED_TO, 8, b"new");
    tree.watch.event(wd, ReadFlags::MOVED_TO, 9, b"unpaired");
    let request = tree.due().request(&tree.writer.view());
    assert_eq!(request.scopes.len(), 3);
    assert!(request.rename_hints.is_empty());
}

#[test]
fn both_pending_bounds_and_kernel_loss_clear_hints() {
    for (name, scopes, bytes) in [("scope-bound", 1, 1 << 20), ("byte-bound", 100, 1)] {
        let tree = Tree::new(name, scopes, bytes);
        let wd = tree.descriptor("a");
        tree.watch.event(wd, ReadFlags::MOVED_FROM, 3, b"old");
        tree.watch.event(wd, ReadFlags::MOVED_TO, 3, b"new");
        assert_eq!(tree.watch.status().backstop, Some(RefreshReason::Overflow));
        let request = tree
            .watch
            .take()
            .unwrap_or_else(|| panic!("overflow due immediately"))
            .request(&tree.writer.view());
        assert_eq!(request.reason, RefreshReason::Overflow);
        assert!(request.rename_hints.is_empty());
    }
    let tree = Tree::new("kernel-loss", 100, 1 << 20);
    tree.watch.event(-1, ReadFlags::QUEUE_OVERFLOW, 0, &[]);
    assert_eq!(tree.watch.status().pending, 1);
}

#[test]
fn loss_marker_survives_failure_and_new_loss_during_success() {
    let tree = Tree::new("loss-watermark", 100, 1 << 20);
    tree.watch.event(-1, ReadFlags::QUEUE_OVERFLOW, 0, &[]);
    let first = tree.watch.take().unwrap_or_else(|| panic!("loss"));
    tree.watch.finish(first, false);
    assert_eq!(tree.watch.status().backstop, Some(RefreshReason::Overflow));
    let second = tree.watch.take().unwrap_or_else(|| panic!("retry"));
    tree.watch.event(-1, ReadFlags::QUEUE_OVERFLOW, 0, &[]);
    tree.watch.finish(second, true);
    assert_eq!(tree.watch.status().backstop, Some(RefreshReason::Overflow));
    let third = tree.watch.take().unwrap_or_else(|| panic!("new loss"));
    tree.watch.finish(third, true);
    assert_eq!(tree.watch.status().backstop, None);
}

#[test]
fn arrivals_during_backstop_are_kept_for_the_next_burst() {
    let tree = Tree::new("concurrent-arrival", 100, 1 << 20);
    tree.watch.backstop(RefreshReason::Backstop);
    let burst = tree.watch.take().unwrap_or_else(|| panic!("backstop"));
    tree.watch
        .event(tree.descriptor("a"), ReadFlags::CREATE, 0, b"late");
    tree.watch.finish(burst, true);
    let request = tree.due().request(&tree.writer.view());
    assert_eq!(request.reason, RefreshReason::Burst);
    assert!(
        matches!(&request.scopes[0], RefreshScope::Entry { basename, .. } if basename == b"late")
    );
}

#[test]
fn ambiguous_ignored_or_descriptor_reuse_becomes_complete_loss() {
    let tree = Tree::new("descriptor-lifetime", 100, 1 << 20);
    let wd = tree.descriptor("a");
    tree.watch.event(wd, ReadFlags::MODIFY, 0, b"old");
    tree.watch.event(wd, ReadFlags::IGNORED, 0, &[]);
    assert_eq!(tree.watch.status().backstop, Some(RefreshReason::Overflow));
    tree.watch.event(wd, ReadFlags::CREATE, 0, b"reuse");
    let request = tree
        .watch
        .take()
        .unwrap_or_else(|| panic!("loss"))
        .request(&tree.writer.view());
    assert_eq!(request.reason, RefreshReason::Overflow);
    assert!(request.scopes.is_empty());
}

#[test]
fn queued_locators_survive_compaction_without_saved_numeric_ids() {
    let mut tree = Tree::new("epoch-locators", 100, 1 << 20);
    tree.watch
        .event(tree.descriptor("a"), ReadFlags::MODIFY, 0, b"old");
    let burst = tree.due();
    let before = tree.writer.view().generation();
    tree.writer
        .compact()
        .unwrap_or_else(|error| panic!("compact: {error:?}"));
    let request = burst.request(&tree.writer.view());
    assert_ne!(request.expected_generation, before);
    crate::refresh(&mut tree.writer, request, &IndexOptions::default())
        .unwrap_or_else(|error| panic!("real producer accepts re-resolved scope: {error:?}"));
}

#[test]
fn proven_occurrences_share_one_descriptor_without_polling() {
    let tree = Tree::new("alias-fallback", 100, 1 << 20);
    let root = tree.path.join("root");
    let alias = tree.path.join("alias");
    std::os::unix::fs::symlink(&root, &alias).unwrap_or_else(|error| panic!("alias: {error:?}"));
    let directory =
        fs::File::open(&alias).unwrap_or_else(|error| panic!("alias handle: {error:?}"));
    use std::os::fd::AsFd;
    tree.watch.arm(&alias, Path::new(""), directory.as_fd());
    tree.watch.arm(&root, Path::new(""), directory.as_fd());
    assert_eq!(tree.watch.status().backstop, None);
    assert!(!tree.watch.status().uncovered);
    let wd = tree.descriptor("");
    let state = tree
        .watch
        .state
        .lock()
        .unwrap_or_else(|error| panic!("state: {error:?}"));
    let occurrences = &state.descriptors[&wd];
    assert_eq!(occurrences.len(), 2);
}

#[test]
fn an_external_policy_parent_attributes_event_refreshes_its_root() {
    use std::os::unix::fs::PermissionsExt;
    let tree = Tree::new("policy-parent-attributes", 100, 1 << 20);
    let outside = tree.path.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("ignore"), "").unwrap();
    tree.watch
        .policy_path(&tree.path.join("root"), &outside.join("ignore"));
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o000)).unwrap();
    tree.watch.drain().unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();
    let burst = tree
        .watch
        .take()
        .unwrap_or_else(|| panic!("policy parent attrib must schedule work"));
    assert!(matches!(
        &burst.request(&tree.writer.view()).scopes[0],
        RefreshScope::Root(_)
    ));
}

#[test]
fn unreliable_filesystem_magic_keeps_successfully_watched_roots_in_the_poll_set() {
    let tree = Tree::new("network-poll", 100, 1 << 20);
    for magic in [
        0x6969, 0xff534d42, 0x517b, 0xfe534d42, 0x01021997, 0x65735546,
    ] {
        assert!(policy::unreliable(magic));
    }
    for magic in [0xef53, 0x9123683e, 0x58465342, 0x01021994, 0x794c7630] {
        assert!(!policy::unreliable(magic));
    }
    tree.watch
        .state
        .lock()
        .unwrap()
        .unreliable
        .insert(tree.path.join("root"));
    tree.watch.reconcile(&tree.writer.view());
    assert_eq!(
        tree.watch.polling_roots(&tree.writer.view()),
        [tree.path.join("root")]
    );
}
