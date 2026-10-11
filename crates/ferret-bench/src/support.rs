//! Test support: a temp directory holding a flat tree of real files and a
//! catalog of it, committed through a public [`Transaction`] batch. Content
//! hashes are real BLAKE3-128, so equal bytes make one document, and the
//! files exist, so the sampler can copy them. `ferret-catalog`'s own fixture
//! helpers are private to its tests, and `ferret-bench` does not depend on
//! `ferret-crawl`.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
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

    /// The catalog directory [`Fixture::commit`] writes.
    pub(crate) fn catalog_dir(&self) -> PathBuf {
        self.base.join("cat")
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
        let root = batch.root(
            tree.as_os_str().as_bytes(),
            stat(&fs::metadata(&tree).unwrap()),
        );
        for &(name, bytes) in files {
            fs::write(tree.join(name), bytes).unwrap();
            let state = content
                .iter()
                .find(|(other, _)| *other == name)
                .map_or_else(
                    || Content::Hashed(ferret_catalog::checkpoint_checksum(bytes)),
                    |&(_, state)| state,
                );
            let stat = stat(&fs::metadata(tree.join(name)).unwrap());
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

fn stat(meta: &fs::Metadata) -> Stat {
    Stat {
        dev: meta.dev(),
        ino: meta.ino(),
        size: meta.len(),
        mode: meta.mode(),
        nlink: meta.nlink(),
        uid: meta.uid(),
        gid: meta.gid(),
        mtime_sec: meta.mtime(),
        mtime_nsec: meta.mtime_nsec() as u32,
        ctime_sec: meta.ctime(),
        ctime_nsec: meta.ctime_nsec() as u32,
    }
}
