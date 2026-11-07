//! One resident catalog, shared by immutable query pins and the optional
//! writer. Output and live-tree actions run outside the publication locks.
//!
//! A writer engine may also own the content index (docs/S2.md § Manifest
//! and commit): [`Engine::attach_content`] opens it under the catalog's
//! writer lock, and a [`QuerySession`] then pins the index view published
//! with its catalog view. A query-only engine opens that index read-only
//! through [`Engine::open_with_content`], pinning content before the catalog:
//! no writer lock, and nothing is repaired or removed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

use ferret_catalog::{Catalog, Generation, OpenError, Section, WriterSession};
use ferret_crawl::{IndexOptions, RefreshReport, RefreshRequest};
use ferret_index::{Budget, CatalogView, DocSet, Fault, Followed, IndexWriter, Merged};
use ferret_query::{DocNames, NameIndex};

use crate::lock;

mod prepared;
pub(crate) mod reader;
mod session;
use prepared::PreparedMerge;
pub(crate) use session::GrepCandidates;

static OPEN_COUNT: AtomicU64 = AtomicU64::new(0);

/// A checked, fully loaded query engine. A query-only engine takes no writer
/// lock; attaching a writer shares its already loaded catalog buffers.
pub struct Engine {
    current: RwLock<QuerySession>,
    writer: Mutex<Option<WriterSession>>,
    /// Content mutation is independent of catalog refresh. When both are
    /// needed, lock `writer` first. Merge setup revokes permissions there,
    /// then streams with only this mutex and the resulting liveness pin.
    content: Mutex<Option<IndexWriter>>,
    retired: Mutex<Vec<(u64, Weak<OnceLock<NameIndex>>)>>,
}

/// The content index's directory inside the catalog's (docs/S2.md §
/// Segment file).
pub const CONTENT_DIR: &str = "index";

/// A generation pinned for the whole query, including output callbacks.
#[derive(Clone)]
pub struct QuerySession {
    catalog: Catalog,
    /// The name planner, shared by every pin of the view. Built at open and
    /// publication, except on a one-shot engine, which builds it when a
    /// query first needs it ([`Engine::open_one_shot`]).
    names: Arc<OnceLock<NameIndex>>,
    /// The content index view published with or before this catalog view,
    /// when the engine has one. Every DocId it holds is below the catalog
    /// view's `next_doc`; liveness comes from the catalog.
    content: Option<Arc<ferret_index::View>>,
    /// What content queries derive from this catalog view, built by the
    /// first that needs it and shared by every pin of the view.
    derived: Arc<Derived>,
    /// Shared by every pin of the engine.
    threads: Arc<Threads>,
}

/// How many threads verify content, resolved by the first content query
/// that needs it: one settings read per engine, no sysfs (D77).
struct Threads {
    index: PathBuf,
    count: OnceLock<usize>,
}

/// Structures derived from one catalog view for content queries (docs/S2.md
/// § Liveness; D30 C, built when a query first needs them, or adopted from
/// the previous view).
#[derive(Default)]
struct Derived {
    live: OnceLock<DocSet>,
    docs: OnceLock<DocNames>,
}

impl Derived {
    /// `previous`'s built structures carried forward to `view`, at a cost
    /// bounded by `view`'s overlay rather than its catalog. Only within one
    /// checkpoint epoch, where `view` descends from `previous`; a
    /// structure `previous` never built, or one that cannot be carried,
    /// stays unbuilt for the first query to build, as across epochs.
    fn adopt(previous: &QuerySession, view: &Catalog) -> Self {
        let derived = Self::default();
        let (from, to) = (previous.generation(), view.generation());
        if from.incarnation != to.incarnation
            || from.checkpoint != to.checkpoint
            || from.sequence > to.sequence
        {
            return derived;
        }
        if let Some(live) = previous.derived.live.get()
            && view.load(&[Section::Docs]).is_ok()
        {
            let mut live = live.clone();
            live.grow_to(view.next_doc().0);
            for (doc, is_live) in view.changed_docs() {
                if is_live {
                    live.insert(doc.0);
                } else {
                    live.remove(doc.0);
                }
            }
            let _ = derived.live.set(live);
        }
        if let Some(docs) = previous.derived.docs.get()
            && let Ok(Some(docs)) = DocNames::adopt(view, &previous.catalog, docs)
        {
            let _ = derived.docs.set(docs);
        }
        derived
    }
}

/// A writer-backed engine's name planner, its scope counts built at
/// publication so that no query waits for them.
fn resident_index(catalog: &Catalog, previous: Option<&NameIndex>) -> NameIndex {
    let index = NameIndex::adopt(catalog, previous);
    let Ok(()) = index.warm(catalog) else {
        unreachable!("adopted from this catalog");
    };
    index
}

impl Engine {
    /// Opens and validates every catalog section once. No checkpoint returns
    /// `None`, as in the catalog reader API.
    pub fn open(index: &Path) -> Result<Option<Self>, OpenError> {
        Self::open_query(index, false, Threads::new(index))
    }

    /// As [`Engine::open`], for a process that runs one query and exits:
    /// every section is loaded and checked as before, but the catalog keeps
    /// its raw name heap instead of interning it, and the name planner is
    /// built only if a query needs it. A content-only query such as grep
    /// never does, so it skips both builds.
    pub fn open_one_shot(index: &Path) -> Result<Option<Self>, OpenError> {
        Self::open_query(index, true, Threads::new(index))
    }

    /// Opens content descriptors before the catalog, then checks their
    /// compatibility with that later catalog. Under D60 A′ an id purged
    /// before this pin cannot revive; new ids are uncovered. Benign catalog
    /// publications therefore do not discard coverage. No writer lock,
    /// generation retry or second catalog load is needed.
    ///
    /// `one_shot` keeps raw names and defers the name planner as in
    /// [`Self::open_one_shot`]. An absent or incompatible index leaves all
    /// documents uncovered; no catalog returns `None`.
    pub fn open_with_content(index: &Path, one_shot: bool) -> Result<Option<Self>, Error> {
        Self::open_pair(index, one_shot, None)
    }

    /// Refreshes a query-only pair, reusing this catalog when its generation
    /// still matches a manifest read after the new content pin. `None` means
    /// no replacement is needed or no catalog exists; safe old pins survive.
    pub(crate) fn reload_with_content(&self, index: &Path) -> Result<Option<Self>, Error> {
        Self::open_pair(index, false, Some(self))
    }

    fn open_pair(
        index: &Path,
        one_shot: bool,
        previous: Option<&Self>,
    ) -> Result<Option<Self>, Error> {
        let pin = previous.map(Self::pin);
        let threads = pin
            .as_ref()
            .map_or_else(|| Threads::new(index), |pin| pin.threads.clone());
        let content = ferret_index::View::open_unpaired(&index.join(CONTENT_DIR), threads.get())
            .map_err(Error::Content)?;
        #[cfg(test)]
        tests::content_open_checkpoint();
        if let Some(pin) = pin
            && Catalog::published_generation(index).map_err(Error::Catalog)?
                == Some(pin.generation())
        {
            if let Some(content) = content
                && let Some(previous) = previous
            {
                previous.select_content(Arc::new(content));
            }
            return Ok(None);
        }
        let Some(engine) = Self::open_query(index, one_shot, threads).map_err(Error::Catalog)?
        else {
            return Ok(None);
        };
        if let Some(content) = content {
            engine.select_content(Arc::new(content));
        }
        Ok(Some(engine))
    }

    fn open_query(
        index: &Path,
        one_shot: bool,
        threads: Arc<Threads>,
    ) -> Result<Option<Self>, OpenError> {
        let Some(catalog) = Catalog::open(index)? else {
            return Ok(None);
        };
        #[cfg(test)]
        tests::catalog_load_checkpoint();
        let (catalog, names) = if one_shot {
            catalog.load_all()?;
            (catalog, Arc::default())
        } else {
            let catalog = catalog.into_resident()?;
            let names = Arc::new(OnceLock::from(NameIndex::new(&catalog)));
            (catalog, names)
        };
        Ok(Some(Self::query_only(catalog, names, threads)))
    }

    fn query_only(
        catalog: Catalog,
        names: Arc<OnceLock<NameIndex>>,
        threads: Arc<Threads>,
    ) -> Self {
        OPEN_COUNT.fetch_add(1, Ordering::Relaxed);
        Self {
            current: RwLock::new(QuerySession {
                names,
                catalog,
                content: None,
                derived: Arc::default(),
                threads,
            }),
            writer: Mutex::new(None),
            content: Mutex::new(None),
            retired: Mutex::new(Vec::new()),
        }
    }

    /// Number of successful catalog engine opens in this process.
    pub fn open_count() -> u64 {
        OPEN_COUNT.load(Ordering::Relaxed)
    }

    /// Takes ownership of a resident writer and its checked, loaded view.
    pub fn from_writer(writer: WriterSession) -> Self {
        OPEN_COUNT.fetch_add(1, Ordering::Relaxed);
        let catalog = writer.view();
        let threads = Threads::new(writer.dir());
        Self {
            current: RwLock::new(QuerySession {
                names: Arc::new(OnceLock::from(resident_index(&catalog, None))),
                catalog,
                content: None,
                derived: Arc::default(),
                threads,
            }),
            writer: Mutex::new(Some(writer)),
            content: Mutex::new(None),
            retired: Mutex::new(Vec::new()),
        }
    }

    /// The daemon calls this after its writer queue stops, before releasing
    /// endpoint ownership. Query pins never retain this lock; closing it here
    /// prevents a replacement host from winning the endpoint but losing the
    /// still-live old engine's writer lock.
    pub(crate) fn close_writer(&self) {
        lock(&self.content).take();
        lock(&self.writer).take();
    }

    /// Published log budget of the resident writer. `None` when this engine
    /// has no writer. This waits for the writer lock, which a refresh, index
    /// or follow holds for its whole operation, so never call it while
    /// holding a lock queries need (the daemon's engine selection, say). A
    /// commit between [`Engine::pin`] and this read moves the budget ahead of
    /// that pin.
    pub(crate) fn log_budget(&self) -> Option<ferret_catalog::BudgetUsage> {
        lock(&self.writer).as_ref().map(WriterSession::budget_usage)
    }

    /// Pins the selected generation without holding a lock during execution.
    pub fn pin(&self) -> QuerySession {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Generation currently selected by this resident engine.
    pub fn generation(&self) -> Generation {
        self.pin().generation()
    }

    /// Bytes retained by the fully resident catalog and its name planner.
    pub fn resident_bytes(&self) -> u64 {
        self.pin().resident_bytes()
    }

    /// Observes final disk state under the writer lock and adopts the checked
    /// successor directly. Stale requests are checked by crawl before ids.
    pub fn refresh(
        &self,
        request: RefreshRequest,
        options: &IndexOptions,
    ) -> Result<RefreshReport, Error> {
        let mut writer = self.writer.lock().map_err(|_| Error::WriterPanicked)?;
        let writer = writer.as_mut().ok_or(Error::ReadOnly)?;
        self.authorize_content(writer);
        let report = match ferret_crawl::refresh(writer, request, options) {
            Ok(report) => report,
            Err(error) => {
                self.recover_failed_write(writer, &error)
                    .map_err(Error::Refresh)?;
                return Err(Error::Refresh(error));
            }
        };
        if !matches!(
            report.outcome,
            ferret_crawl::RefreshOutcome::DeferredBulk(_)
        ) {
            self.select(report.view.clone());
        }
        Ok(report)
    }

    /// Runs an explicit root/index command under the retained writer lock and
    /// publishes its checked view before returning the ordinary producer
    /// report.
    pub fn index_change(
        &self,
        change: ferret_crawl::RootChange<'_>,
        refresh: ferret_crawl::Refresh<'_>,
        options: &IndexOptions,
    ) -> Result<ferret_crawl::Report, ferret_crawl::IndexError> {
        let mut guard = self
            .writer
            .lock()
            .map_err(|_| ferret_crawl::IndexError::Begin(ferret_catalog::BeginError::Locked))?;
        let writer = guard.as_mut().ok_or(ferret_crawl::IndexError::Begin(
            ferret_catalog::BeginError::Locked,
        ))?;
        self.authorize_content(writer);
        let report = match ferret_crawl::session_change(writer, change, refresh, options) {
            Ok(report) => report,
            Err(error) => {
                self.recover_failed_write(writer, &error)?;
                return Err(error);
            }
        };
        self.select(writer.view());
        Ok(report)
    }

    fn recover_failed_write(
        &self,
        writer: &mut WriterSession,
        error: &ferret_crawl::IndexError,
    ) -> Result<(), ferret_crawl::IndexError> {
        if matches!(
            error,
            ferret_crawl::IndexError::Update(_)
                | ferret_crawl::IndexError::Commit(_)
                | ferret_crawl::IndexError::Undurable { .. }
        ) {
            let view = writer.recover().map_err(ferret_crawl::IndexError::Update)?;
            self.select(view);
        }
        Ok(())
    }

    /// Services an explicit idle-boundary compaction. Old query pins continue
    /// to own the old buffers and descriptors after retired files are unlinked.
    pub fn compact(&self) -> Result<Generation, Error> {
        let mut writer = self.writer.lock().map_err(|_| Error::WriterPanicked)?;
        let view = writer
            .as_mut()
            .ok_or(Error::ReadOnly)?
            .compact()
            .map_err(Error::Compact)?;
        let generation = view.generation();
        self.select(view);
        Ok(generation)
    }

    /// Checkpoint epochs still owned by the current view or an internal pin.
    pub fn pinned_epochs(&self) -> Vec<u64> {
        let current = self.pin();
        let mut retired = lock(&self.retired);
        retired.retain(|(_, names)| names.strong_count() != 0);
        let mut epochs = retired.iter().map(|(epoch, _)| *epoch).collect::<Vec<_>>();
        epochs.push(current.generation().checkpoint);
        epochs.sort_unstable();
        epochs.dedup();
        epochs
    }

    /// Opens the content index in `catalog_dir`'s [`CONTENT_DIR`] under the
    /// writer lock this engine holds. An index that does not fit the current
    /// catalog view is discarded, to be rebuilt by [`Engine::follow_content`].
    /// Segments open on the search thread count.
    pub fn attach_content(&self, catalog_dir: &Path) -> Result<(), Error> {
        let threads = self.pin().search_threads();
        let mut writer = self.writer.lock().map_err(|_| Error::WriterPanicked)?;
        let writer = writer.as_mut().ok_or(Error::ReadOnly)?;
        let view = writer.view();
        let live = live_documents(&view);
        let mut content = self.content.lock().map_err(|_| Error::WriterPanicked)?;
        // Open may reset files before reporting failure. Retire the previous
        // writer and selection first so an error cannot leave revival or
        // follow enabled against descriptors whose names open just removed.
        content.take();
        self.current
            .write()
            .map_err(|_| Error::WriterPanicked)?
            .content = None;
        writer.set_coverage(None);
        let opened = IndexWriter::open_with_threads(
            &catalog_dir.join(CONTENT_DIR),
            &catalog_view(&view, &live),
            threads,
        )
        .map_err(Error::Content)?;
        if opened.reset_at_open() || opened.view().manifest().segments.is_empty() {
            let view = writer.clear_dormant().map_err(Error::CatalogWrite)?;
            self.select(view);
        }
        let published = opened.view();
        *content = Some(opened);
        self.select_content(published);
        Ok(())
    }

    /// One follow pass over the current catalog view, reading documents
    /// through the crawl's checked reader: paced by `limiter` when given,
    /// unpaced for an explicit `ferret index`. Publishes the new index view
    /// to later pins.
    pub fn follow_content(
        &self,
        budget: &Budget,
        limiter: Option<Arc<ferret_catalog::bulk::Limiter>>,
    ) -> Result<Followed, Error> {
        let writer = self.writer.lock().map_err(|_| Error::WriterPanicked)?;
        let view = writer.as_ref().ok_or(Error::ReadOnly)?.view();
        let mut content = self.content.lock().map_err(|_| Error::WriterPanicked)?;
        let index = content.as_mut().ok_or(Error::NoContentIndex)?;
        self.follow_locked(&view, index, budget, limiter)
    }

    /// Like [`Engine::follow_content`], but skips without waiting while a
    /// merge owns the content index. The caller must retry after the merge;
    /// catalog refreshes still publish uncovered documents for verification.
    pub fn try_follow_content(
        &self,
        budget: &Budget,
        limiter: Option<Arc<ferret_catalog::bulk::Limiter>>,
    ) -> Result<Option<Followed>, Error> {
        let writer = self.writer.lock().map_err(|_| Error::WriterPanicked)?;
        let view = writer.as_ref().ok_or(Error::ReadOnly)?.view();
        let mut content = match self.content.try_lock() {
            Ok(content) => content,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
            Err(std::sync::TryLockError::Poisoned(_)) => return Err(Error::WriterPanicked),
        };
        let index = content.as_mut().ok_or(Error::NoContentIndex)?;
        self.follow_locked(&view, index, budget, limiter).map(Some)
    }

    fn follow_locked(
        &self,
        view: &Catalog,
        index: &mut IndexWriter,
        budget: &Budget,
        limiter: Option<Arc<ferret_catalog::bulk::Limiter>>,
    ) -> Result<Followed, Error> {
        let pin = self.pin();
        let live = pin.live();
        let names = pin.doc_names().map_err(Error::Catalog)?;
        let mut documents = ferret_crawl::Documents::by_name(view)
            .map_err(Error::Catalog)?
            .holding_directories();
        // Paced background work reads one document at a time. An explicit
        // `ferret index` reads and tokenizes with a reader per walk worker.
        let readers = match limiter {
            Some(limiter) => {
                documents = documents.with_limiter(limiter);
                1
            }
            None => ferret_crawl::default_workers(),
        };
        let followed = follow_reading(
            index,
            &catalog_view(view, live),
            budget,
            &documents,
            readers,
            |documents, doc, bytes| {
                let Some(&name) = names.names(doc).first() else {
                    return Err(ferret_crawl::ContentFault::Changed);
                };
                documents.read_name(view, name, bytes)
            },
        );
        let published = index.view();
        self.select_content(published);
        followed.map_err(Error::Content)
    }

    /// Grants coverage only from the selected committed view under writer.
    /// Reading it needs no content mutex, so refresh can progress beside merge.
    fn authorize_content(&self, writer: &mut WriterSession) {
        let pin = self.pin();
        let view = writer.view();
        let coverage = pin
            .content()
            .filter(|content| {
                let manifest = content.manifest();
                manifest.incarnation == view.generation().incarnation
                    && manifest.high_water <= view.next_doc().0
            })
            .map(|content| {
                let manifest = content.manifest();
                ferret_catalog::Coverage::new(manifest.frontier, manifest.unreadable.clone())
            });
        writer.set_coverage(coverage);
    }

    /// Clears dormant permissions durably before resetting the content index.
    /// Writer→content ownership waits for any streaming merge to finish.
    pub fn reset_content(&self) -> Result<(), Error> {
        let mut writer = self.writer.lock().map_err(|_| Error::WriterPanicked)?;
        let writer = writer.as_mut().ok_or(Error::ReadOnly)?;
        let mut content = self.content.lock().map_err(|_| Error::WriterPanicked)?;
        let index = content.as_mut().ok_or(Error::NoContentIndex)?;
        let view = writer.clear_dormant().map_err(Error::CatalogWrite)?;
        self.select(view.clone());
        writer.set_coverage(None);
        let live = live_documents(&view);
        match index.reset(&catalog_view(&view, &live)) {
            Ok(()) => {
                self.select_content(index.view());
                Ok(())
            }
            Err(error) => {
                // The empty manifest may already be visible. Disable the old
                // writer and coverage until a checked reattach completes.
                content.take();
                self.current
                    .write()
                    .map_err(|_| Error::WriterPanicked)?
                    .content = None;
                Err(Error::Content(error))
            }
        }
    }

    /// One policy merge, revoking permissions before pinning current liveness.
    /// Streaming retains content ownership while refresh can take writer.
    pub fn merge_content(&self, budget: &Budget) -> Result<Option<Merged>, Error> {
        self.merge_content_inner(None, budget, false)
    }

    /// One bounded wave of explicit-build closing merges, with D60 revocation
    /// before the filtering pin and content ownership through publication.
    pub(crate) fn merge_closing_content(&self, budget: &Budget) -> Result<usize, Error> {
        prepared::closing(self, budget)
    }

    /// Forced full merge under the same permission protocol as policy merges.
    pub fn merge_all_content(&self, budget: &Budget) -> Result<Option<Merged>, Error> {
        self.merge_content_inner(None, budget, true)
    }

    /// Revalidates the daemon's advisory resource admission against the exact
    /// current inputs. Its older catalog pin must never filter the output.
    pub(crate) fn merge_content_at(
        &self,
        admitted: &QuerySession,
        budget: &Budget,
    ) -> Result<Option<Merged>, Error> {
        self.merge_content_inner(Some(admitted), budget, false)
    }

    fn merge_content_inner(
        &self,
        admitted: Option<&QuerySession>,
        budget: &Budget,
        all: bool,
    ) -> Result<Option<Merged>, Error> {
        let Some(prepared) = PreparedMerge::prepare(self, admitted, budget, all)? else {
            return Ok(None);
        };
        prepared.run(budget)
    }

    /// Publishes a content view beside the current catalog view. Publishers
    /// can finish out of order: manifest sequence must never go backwards.
    /// Inputs must be pinned before the catalog read, or selected under the
    /// writer's liveness/revocation protocol; compatibility alone is not a
    /// bracket against a purge of ids still live in an older catalog.
    fn select_content(&self, content: Arc<ferret_index::View>) {
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let manifest = content.manifest();
        if content.fits_catalog(
            current.generation().incarnation,
            current.catalog.next_doc().0,
        ) && current
            .content
            .as_ref()
            .is_none_or(|selected| selected.manifest().sequence <= manifest.sequence)
        {
            current.content = Some(content);
        }
    }

    fn select(&self, view: Catalog) {
        let previous = self.pin();
        let names = Arc::new(OnceLock::from(resident_index(
            &view,
            Some(previous.name_index()),
        )));
        let derived = Derived::adopt(&previous, &view);
        let mut retired = lock(&self.retired);
        retired.retain(|(_, names)| names.strong_count() != 0);
        retired.push((
            previous.generation().checkpoint,
            Arc::downgrade(&previous.names),
        ));
        drop(retired);
        let incarnation = view.generation().incarnation;
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The content view is read under the lock, not from `previous`: a
        // content publication since the pin is newer and must survive.
        let content = current
            .content
            .take()
            .filter(|content| content.manifest().incarnation == incarnation);
        *current = QuerySession {
            catalog: view,
            names,
            content,
            derived: Arc::new(derived),
            threads: previous.threads,
        };
    }
}

/// The live documents of one catalog view (docs/S2.md § Liveness).
pub fn live_documents(view: &Catalog) -> DocSet {
    DocSet::new(view.next_doc().0, view.docs().map(|(doc, _)| doc.0))
}

/// As [`live_documents`], `None` once `cancelled` answers true. It is asked
/// before allocating and after every [`DocSet::CHECK_EVERY`] document rows
/// and overlay records scanned, dead ones included.
fn live_documents_until(view: &Catalog, cancelled: impl Fn() -> bool) -> Option<DocSet> {
    if cancelled() {
        return None;
    }
    let mut live = DocSet::empty(view.next_doc().0);
    let mut scanned = 0usize;
    let finished = view.for_each_doc_until(
        || {
            scanned += 1;
            scanned.is_multiple_of(DocSet::CHECK_EVERY) && cancelled()
        },
        |doc| live.insert(doc.0),
    );
    finished.then_some(live)
}

/// One follow pass with `readers` forks of `documents`, each on a thread
/// of its own when there are several, reading a document through `read`.
/// A document `read` cannot return is unreadable.
pub(crate) fn follow_reading(
    index: &mut IndexWriter,
    view: &CatalogView<'_>,
    budget: &Budget,
    documents: &ferret_crawl::Documents,
    readers: usize,
    read: impl Fn(
        &mut ferret_crawl::Documents,
        u32,
        &mut Vec<u8>,
    ) -> Result<(), ferret_crawl::ContentFault>
    + Sync,
) -> Result<Followed, ferret_index::Error> {
    let mut forks: Vec<_> = (0..readers.max(1)).map(|_| documents.fork()).collect();
    let read = &read;
    let mut reads: Vec<_> = forks
        .iter_mut()
        .map(|documents| {
            move |doc, bytes: &mut Vec<u8>| {
                read(documents, doc, bytes).map_err(|_| Fault::Unreadable)
            }
        })
        .collect();
    let mut readers: Vec<&mut ferret_index::Reader<'_>> = reads
        .iter_mut()
        .map(|read| read as &mut ferret_index::Reader<'_>)
        .collect();
    index.follow_with(view, budget, &mut readers)
}

fn catalog_view<'a>(view: &Catalog, live: &'a DocSet) -> CatalogView<'a> {
    CatalogView {
        incarnation: view.generation().incarnation,
        live,
    }
}

/// Failure to refresh or compact. Query pins remain independently readable.
#[derive(Debug)]
pub enum Error {
    /// A query-only engine has no writer lock or writer caches.
    ReadOnly,
    /// A writer panic may have interrupted mutation; reopen before writing.
    WriterPanicked,
    Refresh(ferret_crawl::IndexError),
    Compact(ferret_catalog::log::Error),
    /// Permission publication failed; content mutation must not proceed.
    CatalogWrite(ferret_catalog::log::Error),
    /// The engine has no content index attached.
    NoContentIndex,
    Content(ferret_index::Error),
    Catalog(OpenError),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadOnly => f.write_str("the engine has no writer"),
            Self::WriterPanicked => f.write_str("the engine writer panicked"),
            Self::Refresh(error) => error.fmt(f),
            Self::Compact(error) | Self::CatalogWrite(error) => error.fmt(f),
            Self::NoContentIndex => f.write_str("the engine has no content index"),
            Self::Content(error) => error.fmt(f),
            Self::Catalog(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ReadOnly | Self::WriterPanicked | Self::NoContentIndex => None,
            Self::Content(error) => Some(error),
            Self::Catalog(error) => Some(error),
            Self::Refresh(error) => Some(error),
            Self::Compact(error) | Self::CatalogWrite(error) => Some(error),
        }
    }
}

/// The most threads [`search_threads`] uses when the index stores no count
/// (made before D77 C, or its cores could not be counted). Measured on 16
/// cores with 32 threads, a warm scan is fastest at 16 threads on every
/// class, and 32 uses 60% more CPU for 10–20 ms more wall; a cold scan is
/// 2–12% faster at 32 (docs/SCAN.md § Threads, D77).
const SEARCH_THREADS: usize = 16;

/// Threads that verify a content query's Maybe documents. `env` is
/// `FERRET_SEARCH_THREADS` parsed by [`env_threads`] and wins outright.
/// Otherwise `stored`, the index's `search_threads` setting (the physical
/// cores counted when it was created), capped by `available` so CPU
/// affinity, cgroup quotas and a smaller machine still apply. Without a
/// stored count, `available` up to `SEARCH_THREADS`.
pub fn search_threads(env: Option<usize>, stored: Option<usize>, available: usize) -> usize {
    env.unwrap_or_else(|| available.min(stored.unwrap_or(SEARCH_THREADS)))
}

/// `FERRET_SEARCH_THREADS`'s value as a thread count: a positive number, or
/// nothing.
pub fn env_threads(value: Option<&str>) -> Option<usize> {
    value
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&threads| threads > 0)
}

/// Resolve index settings without opening the resident content engine.
pub(crate) fn search_threads_for(index: &Path) -> usize {
    Threads::new(index).get()
}

impl Threads {
    fn new(index: &Path) -> Arc<Self> {
        Arc::new(Self {
            index: index.to_path_buf(),
            count: OnceLock::new(),
        })
    }

    /// The count, reading the settings file only when the environment does
    /// not decide it. An unusable file is warned about once per process and
    /// treated as absent; it never fails the search.
    fn get(&self) -> usize {
        *self.count.get_or_init(|| {
            static WARNED: AtomicBool = AtomicBool::new(false);
            let env = env_threads(std::env::var("FERRET_SEARCH_THREADS").ok().as_deref());
            let stored = match env {
                Some(_) => None,
                None => match crate::settings::Settings::read(&self.index) {
                    Ok(settings) => settings.and_then(|settings| settings.search_threads),
                    Err(e) => {
                        if !WARNED.swap(true, Ordering::Relaxed) {
                            crate::cli::warn(&format!(
                                "{}: {e}; searching on at most {SEARCH_THREADS} threads",
                                self.index.join(crate::settings::FILE).display()
                            ));
                        }
                        None
                    }
                },
            };
            let available = std::thread::available_parallelism().map_or(1, |n| n.get());
            search_threads(env, stored, available)
        })
    }
}

fn is_cancelled(cancelled: Option<&AtomicBool>) -> bool {
    cancelled.is_some_and(|flag| flag.load(Ordering::Acquire))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
thread_local! {
    pub(crate) static DOC_NAMES_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
