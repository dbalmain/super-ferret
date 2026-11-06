//! Catalog/content pairing for diagnostic readers, including historical term
//! and comparison controls. All inputs share one later resident catalog.

use std::path::Path;

use ferret_catalog::Catalog;
use ferret_index::View;

pub(crate) struct Inputs {
    pub(crate) catalog: Catalog,
    pub(crate) view: View,
    pub(crate) terms: Option<View>,
    pub(crate) compare: Option<View>,
}

/// Opens all requested content inputs and one compatible resident catalog.
/// Pins every index before reading the catalog: A′ forbids revival of an
/// already-purged id, and later ids stay uncovered. An older compatible
/// control is valid; no generation equality or writer exclusion is needed.
pub(crate) fn open(
    catalog_dir: &Path,
    index_dir: &Path,
    terms_dir: Option<&Path>,
    compare_dir: Option<&Path>,
) -> crate::Result<Inputs> {
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    let pin = |dir: &Path| -> crate::Result<View> {
        View::open_unpaired(dir, threads)?
            .ok_or_else(|| format!("no usable content index in {}", dir.display()).into())
    };
    let view = pin(index_dir)?;
    let terms = terms_dir.map(pin).transpose()?;
    let compare = compare_dir.map(pin).transpose()?;
    #[cfg(test)]
    checkpoint();
    let catalog = crate::open_catalog(catalog_dir)?.into_resident()?;
    for view in std::iter::once(&view)
        .chain(terms.iter())
        .chain(compare.iter())
    {
        if !view.fits_catalog(catalog.generation().incarnation, catalog.next_doc().0) {
            return Err("content index does not fit catalog".into());
        }
    }
    Ok(Inputs {
        catalog,
        view,
        terms,
        compare,
    })
}

#[cfg(test)]
thread_local! {
    static OPEN_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn checkpoint() {
    let hook = OPEN_HOOK.take();
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(test)]
mod tests {
    use std::ops::ControlFlow;
    use std::time::SystemTime;

    use ferret_catalog::{Content, Coverage, WriterSession};
    use ferret_index::{Budget, CatalogView, DocSet, IndexWriter, Pinned};
    use ferret_query::{Content as QueryContent, DocNames, NameIndex, Query, ReadVersion};

    use super::*;
    use crate::support::Fixture;

    /// D60: diagnostic readers must pin every input before a death/merge
    /// can remove postings that an older catalog would still consider live.
    #[test]
    fn a_death_and_merge_between_pair_reads_cannot_lose_candidates() {
        let fixture = Fixture::new("paired-death-merge");
        let catalog = fixture
            .commit(&[("a", b"alpha one"), ("b", b"alpha two")], &[])
            .into_resident()
            .unwrap();
        let dir = fixture.catalog_dir();
        let index_dir = fixture.out("index");
        let mut writer = WriterSession::open(&dir).unwrap();
        let live = DocSet::new(catalog.next_doc().0, catalog.docs().map(|(id, _)| id.0));
        let catalog_view = CatalogView {
            incarnation: catalog.generation().incarnation,
            live: &live,
        };
        let mut index = IndexWriter::open(&index_dir, &catalog_view).unwrap();
        index
            .follow(&catalog_view, &Budget::unbounded(), &mut |doc, out| {
                let hash = catalog.doc_hash(ferret_catalog::DocId(doc)).unwrap();
                let bytes = [b"alpha one".as_slice(), b"alpha two".as_slice()]
                    .into_iter()
                    .find(|bytes| ferret_catalog::checkpoint_checksum(bytes) == hash)
                    .unwrap();
                out.extend_from_slice(bytes);
                Ok(())
            })
            .unwrap();
        let manifest = index.view().manifest().clone();
        writer.set_coverage(Some(Coverage::new(
            manifest.frontier,
            manifest.unreadable.clone(),
        )));
        let moved = catalog.clone();
        OPEN_HOOK.set(Some(Box::new(move || {
            let (root, path) = moved.roots().next().unwrap();
            let mut batch = writer.checkpoint_batch(1);
            let parent = batch.root(path, moved.inode(root).stat);
            let (name, _) = moved.names().find(|(_, name)| *name == b"b").unwrap();
            let ferret_catalog::Target::Inode(inode) = moved.name(name).target() else {
                panic!("fixture file");
            };
            let hash = moved.doc_hash(moved.doc(inode).unwrap()).unwrap();
            batch.file(parent, b"b", moved.inode(inode).stat, Content::Hashed(hash));
            let view = writer
                .rebuild_checkpoint(vec![batch], moved.sniffer_version(), moved.policy())
                .unwrap();
            assert_eq!(view.doc_count(), 1);
            let live = DocSet::new(view.next_doc().0, view.docs().map(|(id, _)| id.0));
            let filter = CatalogView {
                incarnation: view.generation().incarnation,
                live: &live,
            };
            let plan = index
                .prepare_merge(&filter, &Budget::unbounded(), true)
                .unwrap()
                .unwrap();
            let view = writer.forget_dormant(&plan.ranges()).unwrap();
            let filter = CatalogView {
                incarnation: view.generation().incarnation,
                live: &live,
            };
            let merged = index
                .merge_selected(&filter, &Budget::unbounded(), plan)
                .unwrap()
                .unwrap();
            assert_eq!(merged.purged, 1);
        })));
        let inputs = open(&dir, &index_dir, Some(&index_dir), Some(&index_dir)).unwrap();
        assert!(OPEN_HOOK.with_borrow(Option::is_none));
        let catalog = &inputs.catalog;
        let live = DocSet::new(catalog.next_doc().0, catalog.docs().map(|(id, _)| id.0));
        let names = NameIndex::new(catalog);
        let docs = DocNames::new(catalog).unwrap();
        let query = Query::from_args([b"text:alpha".as_slice()], SystemTime::now()).unwrap();
        for view in [
            &inputs.view,
            inputs.terms.as_ref().unwrap(),
            inputs.compare.as_ref().unwrap(),
        ] {
            let pinned = Pinned::new(Some(view), &live);
            let content = QueryContent {
                pinned: &pinned,
                docs: &docs,
                bound: None,
            };
            let rows = query
                .run_content(
                    catalog,
                    &names,
                    &content,
                    &mut |_, _| Some(ReadVersion::Catalogued),
                    None,
                    |_| ControlFlow::Continue(()),
                )
                .unwrap()
                .rows;
            assert_eq!(
                rows,
                u64::from(catalog.doc_count()),
                "a catalog-live document lost its candidate"
            );
            assert_eq!(
                view.lookup(b"alpha").unwrap().len(),
                2,
                "every index must be pinned before the purge"
            );
        }
        assert_eq!(catalog.doc_count(), 1);
    }
}
