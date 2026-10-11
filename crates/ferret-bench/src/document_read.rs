//! Version-bracketed whole/window reads for D65 builds, graduation and
//! query-time offset hints. Frozen snapshots are immutable benchmark inputs;
//! ordinary paths must match the catalog key before and after each read.

use std::fs::File;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;

use ferret_catalog::{Catalog, NameId};
use ferret_index::segment::Window;

pub(crate) fn read(
    catalog: &Catalog,
    name: NameId,
    doc: u32,
    snapshot: Option<&crate::content_snapshot::Snapshot>,
    offset: Option<u32>,
    bytes: &mut Vec<u8>,
) -> Option<Window> {
    let mut path = Vec::new();
    let frozen = snapshot.map(|s| s.path(doc));
    if snapshot.is_some_and(|s| !s.contains(doc)) {
        return None;
    }
    let file = if let Some(path) = &frozen {
        File::open(path).ok()?
    } else {
        catalog.path(name, &mut path);
        File::open(Path::new(std::ffi::OsStr::from_bytes(&path))).ok()?
    };
    let before = file.metadata().ok()?;
    if !before.is_file()
        || (frozen.is_none() && !crate::content_query::same_version(catalog, name, &before))
    {
        return None;
    }
    bytes.clear();
    let (start, complete) = if let Some(offset) = offset {
        let start = u64::from(offset).saturating_sub(256).min(before.len());
        let len = (before.len() - start).min(1280) as usize;
        bytes.resize(len, 0);
        file.read_exact_at(bytes, start).ok()?;
        (start, start == 0 && len as u64 == before.len())
    } else {
        (&file).read_to_end(bytes).ok()?;
        (0, true)
    };
    let after = file.metadata().ok()?;
    let key = |m: &std::fs::Metadata| {
        (
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    };
    if key(&before) != key(&after) {
        return None;
    }
    Some(Window { start, complete })
}
