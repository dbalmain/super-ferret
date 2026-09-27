//! Races the real [`walk`](crate::walk) against a directory replaced mid-walk.
//!
//! The visitor swaps a name during a callback that the path-based walker
//! handles afterwards by path. A directory handle opened with `O_NOFOLLOW`
//! must not follow that swap out of the root.

use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use ferret_policy::{Config, Decision};

use super::{Scratch, Walked, write};
use crate::{Decided, Event, walk};

fn record(out: &mut Walked, event: Event<'_, ()>) {
    match event {
        Event::Decided(Decided {
            path,
            decision,
            stat,
            ..
        }) => {
            out.rows.insert(
                path.to_path_buf(),
                super::Row {
                    decision,
                    stat: stat.map(super::OwnedStat::from),
                },
            );
        }
        Event::Io { path, error, .. } => out.io.push((path.to_path_buf(), error.kind())),
        Event::Pattern(error) => out.patterns.push(error),
        Event::Entered { .. } | Event::Boundary { .. } => {}
    }
}

fn fresh() -> Walked {
    Walked {
        rows: Default::default(),
        io: Vec::new(),
        patterns: Vec::new(),
    }
}

/// `dir` is replaced by a symlink to a directory outside the root during its
/// `Descend` callback. The file out there must not be reported, and `dir`
/// must fault.
#[test]
fn a_directory_replaced_by_a_symlink_is_not_followed() {
    let scratch = Scratch::new("swap-dir");
    let root = scratch.join("root");
    let outside = scratch.join("outside");
    fs::create_dir_all(root.join("dir")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    write(&root.join("dir/inside.txt"), "in");
    write(&outside.join("leaked.txt"), "out");

    let mut swapped = false;
    let mut out = fresh();
    walk(&root, None, Config::default(), |event| {
        if let Event::Decided(decided) = &event
            && decided.path == Path::new("dir")
            && decided.decision == Decision::Descend
        {
            let victim = root.join("dir");
            fs::rename(&victim, root.join("dir.was")).unwrap();
            symlink(&outside, &victim).unwrap();
            swapped = true;
        }
        record(&mut out, event);
    });

    assert!(swapped, "the descend hook did not run");
    assert!(
        !out.rows.contains_key(Path::new("dir/leaked.txt")),
        "followed the swapped symlink: {:?}",
        out.rows.keys()
    );
    assert!(
        out.io.iter().any(|(path, _)| path == Path::new("dir")),
        "expected an Io event for dir, got {:?}",
        out.io
    );
}

/// `.git` is swapped for a symlink to an outside git dir after it has been
/// classified and before `info/exclude` is read. The callback is the `Io`
/// from an unreadable `.gitignore`, which sits in that window. The outside
/// exclude must not apply.
#[test]
fn a_swapped_git_dir_does_not_apply_an_outside_exclude() {
    let scratch = Scratch::new("swap-git");
    let root = scratch.join("root");
    let outside = scratch.join("outside.git");
    fs::create_dir_all(root.join(".git/info")).unwrap();
    fs::create_dir_all(outside.join("info")).unwrap();
    write(&root.join(".git/info/exclude"), "# in tree\n");
    write(&outside.join("info/exclude"), "secret.txt\n");
    write(&root.join(".gitignore"), "*.o\n");
    fs::set_permissions(root.join(".gitignore"), fs::Permissions::from_mode(0o000)).unwrap();
    write(&root.join("secret.txt"), "x");
    write(&root.join("kept.txt"), "y");

    let mut swapped = false;
    let mut out = fresh();
    walk(&root, None, Config::default(), |event| {
        if let Event::Io { path, error, .. } = &event
            && *path == Path::new(".gitignore")
            && error.kind() == io::ErrorKind::PermissionDenied
        {
            let victim = root.join(".git");
            fs::rename(&victim, root.join(".git.real")).unwrap();
            symlink(&outside, &victim).unwrap();
            swapped = true;
        }
        record(&mut out, event);
    });

    assert!(swapped, "the gitignore Io hook did not run");
    let secret = out
        .rows
        .get(Path::new("secret.txt"))
        .map(|row| row.decision);
    assert_eq!(
        secret,
        Some(Decision::Index),
        "outside exclude applied: {secret:?}; io {:?}",
        out.io
    );
    assert_eq!(
        out.rows.get(Path::new("kept.txt")).map(|row| row.decision),
        Some(Decision::Index)
    );
    // The rename lands inside the root after the listing, so it is not a row.
    assert!(!out.rows.contains_key(Path::new(".git.real")));
}

/// A gitfile's relative gitdir is resolved from the already-open work
/// directory, even when that directory's pathname is replaced in a callback.
#[test]
fn a_swapped_gitfile_parent_does_not_apply_an_outside_exclude() {
    let scratch = Scratch::new("swap-gitfile-parent");
    let root = scratch.join("root");
    let work = root.join("work");
    let outside = scratch.join("outside");
    write(&work.join(".git"), "gitdir: meta\n");
    write(&work.join("meta/info/exclude"), "# in tree\n");
    write(&outside.join("meta/info/exclude"), "secret.txt\n");
    write(&work.join(".gitignore"), "*.o\n");
    fs::set_permissions(work.join(".gitignore"), fs::Permissions::from_mode(0o000)).unwrap();
    write(&work.join("secret.txt"), "x");

    let mut swapped = false;
    let mut out = fresh();
    walk(&root, None, Config::default(), |event| {
        if let Event::Io { path, error, .. } = &event
            && *path == Path::new("work/.gitignore")
            && error.kind() == io::ErrorKind::PermissionDenied
        {
            fs::rename(&work, root.join("work.saved")).unwrap();
            symlink(&outside, &work).unwrap();
            swapped = true;
        }
        record(&mut out, event);
    });

    assert!(swapped, "the gitignore Io hook did not run");
    assert_eq!(
        out.rows
            .get(Path::new("work/secret.txt"))
            .map(|row| row.decision),
        Some(Decision::Index),
        "outside exclude applied: {:?}",
        out.io
    );
}

/// `O_NOFOLLOW` does not catch a directory replaced by a different directory.
/// The open must `fstat` and refuse a `(dev, ino)` other than the one `decide`
/// was given.
#[test]
fn a_directory_replaced_by_another_directory_is_not_listed() {
    let scratch = Scratch::new("swap-ino");
    let root = scratch.join("root");
    fs::create_dir_all(root.join("dir")).unwrap();
    write(&root.join("dir/inside.txt"), "in");

    let mut swapped = false;
    let mut out = fresh();
    walk(&root, None, Config::default(), |event| {
        if let Event::Decided(decided) = &event
            && decided.path == Path::new("dir")
            && decided.decision == Decision::Descend
        {
            let victim = root.join("dir");
            fs::rename(&victim, root.join("dir.was")).unwrap();
            fs::create_dir(&victim).unwrap();
            write(&victim.join("leaked.txt"), "out");
            swapped = true;
        }
        record(&mut out, event);
    });

    assert!(swapped, "the descend hook did not run");
    assert!(
        !out.rows.contains_key(Path::new("dir/leaked.txt")),
        "listed the replacement directory: {:?}",
        out.rows.keys()
    );
    assert!(
        out.io.iter().any(|(path, _)| path == Path::new("dir")),
        "expected an Io event for dir, got {:?}",
        out.io
    );
}
