//! An immutable byte snapshot for paired content benchmarks. The live source
//! can change between two full builds; capturing once gives both codecs the
//! same readable population, without converting either index. Files are named
//! by DocId, and no source name or content is reported.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use ferret_catalog::Catalog;
use ferret_index::DocSet;

use crate::census::hashed_documents;

pub(crate) struct Snapshot {
    dir: PathBuf,
    docs: DocSet,
}

impl Snapshot {
    pub(crate) fn open(dir: &Path, catalog: &Catalog) -> crate::Result<Self> {
        let meta = fs::read(dir.join("population"))?;
        if meta.len() < 20 || meta[..16] != catalog.generation().incarnation
            || meta[16..20] != catalog.next_doc().0.to_le_bytes()
            || !(meta.len() - 20).is_multiple_of(4)
        {
            return Err("snapshot does not fit catalog".into());
        }
        let mut ids = Vec::new();
        for word in meta[20..].chunks_exact(4) {
            ids.push(u32::from_le_bytes(word.try_into()?));
        }
        if ids.windows(2).any(|pair| pair[0] >= pair[1]) || ids.last().is_some_and(|&doc| doc >= catalog.next_doc().0) {
            return Err("invalid snapshot population".into());
        }
        Ok(Self { dir: dir.to_owned(), docs: DocSet::new(catalog.next_doc().0, ids) })
    }

    pub(crate) fn path(&self, doc: u32) -> PathBuf { self.dir.join(doc.to_string()) }
    pub(crate) fn contains(&self, doc: u32) -> bool { self.docs.contains(doc) }
}

pub(crate) fn capture(catalog_dir: &Path, out: &Path) -> crate::Result<()> {
    crate::trigram::check_disk()?;
    // Refuse reuse: an incomplete capture is never a valid reference.
    fs::create_dir(out)?;
    fs::set_permissions(out, fs::Permissions::from_mode(0o700))?;
    let catalog = crate::open_catalog(catalog_dir)?;
    let mut meta = catalog.generation().incarnation.to_vec();
    meta.extend_from_slice(&catalog.next_doc().0.to_le_bytes());
    let mut path = Vec::new();
    let (mut count, mut bytes, mut missing) = (0u64, 0u64, 0u64);
    for document in hashed_documents(&catalog)? {
        if count.is_multiple_of(256) { crate::trigram::check_disk()?; }
        path.clear(); catalog.path(document.name, &mut path);
        match fs::read(Path::new(std::ffi::OsStr::from_bytes(&path))) {
            Ok(content) if content.len() as u64 == document.size => {
                fs::write(out.join(document.doc.0.to_string()), &content)?;
                meta.extend_from_slice(&document.doc.0.to_le_bytes());
                count += 1; bytes += content.len() as u64;
            }
            _ => missing += 1,
        }
    }
    // hashed_documents is in DocId order, just as follow's stream is.
    fs::write(out.join("population"), meta)?;
    println!("snapshot_documents: {count}\nsnapshot_bytes: {bytes}\nsnapshot_unreadable: {missing}");
    Ok(())
}
