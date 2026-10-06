//! Test support: a temp directory holding a flat tree of real files and a
//! catalog of it, committed through a public [`Transaction`] batch. Content
//! hashes are real BLAKE3-128, so equal bytes make one document, and the
//! files exist, so the sampler can copy them. `ferret-catalog`'s own fixture
//! helpers are private to its tests, and `ferret-bench` does not depend on
//! `ferret-crawl`.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use ferret_catalog::{Catalog, Content, Stat, Transaction};

/// A fixture directory: `tree/` for files, `cat/` for the catalog, and
/// output directories beside them. Removed on drop.
pub(crate) struct Fixture {
    base: PathBuf,
}

impl Fixture {
    pub(crate) fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!("ferret-bench-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("tree")).unwrap();
        Self { base }
    }

    /// A fresh output directory under the fixture.
    pub(crate) fn out(&self, name: &str) -> PathBuf {
        let path = self.base.join(name);
        let _ = fs::remove_dir_all(&path);
        path
    }

    /// Writes `files` into the tree and commits a catalog of them: each file
    /// `Hashed` unless `content` names it with another state.
    pub(crate) fn commit(&self, files: &[(&str, &[u8])], content: &[(&str, Content)]) -> Catalog {
        let tree = self.base.join("tree");
        let mut transaction = Transaction::begin(&self.base.join("cat"), 1).unwrap();
        let mut batch = transaction.batch();
        let root = batch.root(tree.as_os_str().as_bytes(), stat(1, 0o040_755, 0));
        for (ino, &(name, bytes)) in (2..).zip(files) {
            fs::write(tree.join(name), bytes).unwrap();
            let state = content
                .iter()
                .find(|(other, _)| *other == name)
                .map_or_else(
                    || Content::Hashed(ferret_catalog::checkpoint_checksum(bytes)),
                    |&(_, state)| state,
                );
            let stat = stat(ino, 0o100_644, bytes.len() as u64);
            batch.file(root, name.as_bytes(), stat, state);
        }
        transaction.add(batch);
        transaction.commit().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn stat(ino: u64, mode: u32, size: u64) -> Stat {
    Stat {
        dev: 1,
        ino,
        size,
        mode,
        nlink: 1,
        ..Stat::default()
    }
}
