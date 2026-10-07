//! One resident catalog, shared by immutable query pins and the optional
//! writer. Output and live-tree actions run outside the publication locks.
//!
//! A writer engine may also own the content index (docs/S2.md § Manifest
//! and commit): [`Engine::attach_content`] opens it under the catalog's
//! writer lock, and a [`QuerySession`] then pins the index view published
//! with its catalog view. A query-only engine does not open it in S2 M3.

use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

use ferret_catalog::{Catalog, Generation, OpenError, WriterSession};
use ferret_crawl::{IndexOptions, RefreshReport, RefreshRequest};
use ferret_index::{Budget, CatalogView, DocSet, Fault, Followed, IndexWriter, Merged, Pinned};
use ferret_query::find::{Effects, Outcome, Plan, Unsupported};
use ferret_query::{Content, DocNames, NameIndex, Query, Row, RunError, Stats, UNCOVERED_BOUND};

static OPEN_COUNT: AtomicU64 = AtomicU64::new(0);

/// A checked, fully loaded query engine. A query-only engine takes no writer
/// lock; attaching a writer shares its already loaded catalog buffers.
pub struct Engine {
    current: RwLock<QuerySession>,
    writer: Mutex<Option<WriterSession>>,
    /// The content index, kept under `writer`'s lock: lock `writer` first.
    content: Mutex<Option<IndexWriter>>,
    retired: Mutex<Vec<(u64, Weak<NameIndex>)>>,
}

/// The content index's directory inside the catalog's (docs/S2.md §
/// Segment file).
pub const CONTENT_DIR: &str = "index";

/// A generation pinned for the whole query, including output callbacks.
#[derive(Clone)]
pub struct QuerySession {
    catalog: Catalog,
    names: Arc<NameIndex>,
    /// The content index view published with or before this catalog view,
    /// when the engine has one. Every DocId it holds is below the catalog
    /// view's `next_doc`; liveness comes from the catalog.
    content: Option<Arc<ferret_index::View>>,
    /// What content queries derive from this catalog view, built by the
    /// first that needs it and shared by every pin of the view.
    derived: Arc<Derived>,
}

/// Structures derived from one catalog view for content queries (docs/S2.md
/// § Liveness; D30 C, built when a query first needs them).
#[derive(Default)]
struct Derived {
    live: OnceLock<DocSet>,
    docs: OnceLock<DocNames>,
}

impl Engine {
    /// Opens and validates every catalog section once. No checkpoint returns
    /// `None`, as in the catalog reader API.
    pub fn open(index: &Path) -> Result<Option<Self>, OpenError> {
        let Some(catalog) = Catalog::open(index)? else {
            return Ok(None);
        };
        let catalog = catalog.into_resident()?;
        OPEN_COUNT.fetch_add(1, Ordering::Relaxed);
        Ok(Some(Self {
            current: RwLock::new(QuerySession {
                names: Arc::new(NameIndex::new(&catalog)),
                catalog,
                content: None,
                derived: Arc::default(),
            }),
            writer: Mutex::new(None),
            content: Mutex::new(None),
            retired: Mutex::new(Vec::new()),
        }))
    }

    /// Number of successful catalog engine opens in this process.
    pub fn open_count() -> u64 {
        OPEN_COUNT.load(Ordering::Relaxed)
    }

    /// Takes ownership of a resident writer and its checked, loaded view.
    pub fn from_writer(writer: WriterSession) -> Self {
        OPEN_COUNT.fetch_add(1, Ordering::Relaxed);
        let catalog = writer.view();
        Self {
            current: RwLock::new(QuerySession {
                names: Arc::new(NameIndex::new(&catalog)),
                catalog,
                content: None,
                derived: Arc::default(),
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
        self.content
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
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
            ferret_crawl::IndexError::Update(_) | ferret_crawl::IndexError::Commit(_)
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
        let mut retired = self
            .retired
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    pub fn attach_content(&self, catalog_dir: &Path) -> Result<(), Error> {
        let writer = self.writer.lock().map_err(|_| Error::WriterPanicked)?;
        let view = writer.as_ref().ok_or(Error::ReadOnly)?.view();
        let live = live_documents(&view);
        let opened = IndexWriter::open(&catalog_dir.join(CONTENT_DIR), &catalog_view(&view, &live))
            .map_err(Error::Content)?;
        let published = opened.view();
        *self.content.lock().map_err(|_| Error::WriterPanicked)? = Some(opened);
        self.select_content(published);
        Ok(())
    }

    /// Opens the content index in `catalog_dir`'s [`CONTENT_DIR`] for
    /// reading, for a query-only engine: no writer lock, nothing repaired
    /// or removed. An index that is absent or does not fit the catalog view
    /// leaves the engine without one, which content queries treat as
    /// nothing covered.
    pub fn open_content(&self, catalog_dir: &Path) -> Result<(), Error> {
        let pin = self.pin();
        let view = ferret_index::View::open(
            &catalog_dir.join(CONTENT_DIR),
            &catalog_view(pin.catalog(), pin.live()),
        )
        .map_err(Error::Content)?;
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current.generation() == pin.generation() {
            current.content = view.map(Arc::new);
        }
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
        let pin = self.pin();
        let live = pin.live();
        let names = pin.doc_names().map_err(Error::Catalog)?;
        let mut documents = ferret_crawl::Documents::by_name(&view).map_err(Error::Catalog)?;
        if let Some(limiter) = limiter {
            documents = documents.with_limiter(limiter);
        }
        let followed = index.follow(&catalog_view(&view, live), budget, &mut |doc, bytes| {
            let Some(&name) = names.names(doc).first() else {
                return Err(Fault::Unreadable);
            };
            documents
                .read_name(&view, name, bytes)
                .map_err(|_| Fault::Unreadable)
        });
        let published = index.view();
        drop(content);
        self.select_content(published);
        drop(writer);
        followed.map_err(Error::Content)
    }

    /// One merge step of the content index, if its policy wants one.
    pub fn merge_content(&self, budget: &Budget) -> Result<Option<Merged>, Error> {
        let writer = self.writer.lock().map_err(|_| Error::WriterPanicked)?;
        let view = writer.as_ref().ok_or(Error::ReadOnly)?.view();
        let mut content = self.content.lock().map_err(|_| Error::WriterPanicked)?;
        let index = content.as_mut().ok_or(Error::NoContentIndex)?;
        let pin = self.pin();
        let merged = index.merge_if_needed(&catalog_view(&view, pin.live()), budget);
        let published = index.view();
        drop(content);
        self.select_content(published);
        drop(writer);
        merged.map_err(Error::Content)
    }

    /// Publishes a content view beside the current catalog view.
    fn select_content(&self, content: Arc<ferret_index::View>) {
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let manifest = content.manifest();
        if manifest.incarnation == current.generation().incarnation
            && manifest.high_water <= current.catalog.next_doc().0
        {
            current.content = Some(content);
        }
    }

    fn select(&self, view: Catalog) {
        let previous = self.pin();
        let names = Arc::new(NameIndex::adopt(&view, Some(&previous.names)));
        let mut retired = self
            .retired
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        retired.retain(|(_, names)| names.strong_count() != 0);
        retired.push((
            previous.generation().checkpoint,
            Arc::downgrade(&previous.names),
        ));
        drop(retired);
        let incarnation = view.generation().incarnation;
        *self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = QuerySession {
            catalog: view,
            names,
            content: previous
                .content
                .filter(|content| content.manifest().incarnation == incarnation),
            derived: Arc::default(),
        };
    }
}

/// The live documents of one catalog view (docs/S2.md § Liveness).
pub fn live_documents(view: &Catalog) -> DocSet {
    DocSet::new(view.next_doc().0, view.docs().map(|(doc, _)| doc.0))
}

fn catalog_view<'a>(view: &Catalog, live: &'a DocSet) -> CatalogView<'a> {
    CatalogView {
        incarnation: view.generation().incarnation,
        live,
    }
}

impl QuerySession {
    /// Bytes retained by the fully resident catalog and its name planner.
    pub fn resident_bytes(&self) -> u64 {
        self.catalog.bytes_read() + self.names.bytes() as u64
    }

    pub fn name_index(&self) -> &NameIndex {
        &self.names
    }

    /// The pinned content index view, if the engine has a content index.
    pub fn content(&self) -> Option<&ferret_index::View> {
        self.content.as_deref()
    }

    /// A checked directory scope for hosts that already resolved a start.
    pub fn search_in(
        &self,
        scope: ferret_catalog::Handle<ferret_catalog::InoId>,
        query: &Query,
        emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<Stats, RunError> {
        query.run_indexed(&self.catalog, &self.names, Some(scope), emit)
    }

    /// Identifies the epoch and sequence in which this pin's ids are valid.
    pub fn generation(&self) -> Generation {
        self.catalog.generation()
    }

    /// Metadata and handles must be read against this pin, never a newer one.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Streams search rows against this pin, stopping when the callback breaks.
    pub fn search(
        &self,
        query: &Query,
        emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<Stats, RunError> {
        self.search_until(query, None, emit)
    }

    /// Streams rows, checking host cancellation at every candidate boundary.
    /// A content query refuses an index with more than
    /// [`UNCOVERED_BOUND`] uncovered documents.
    pub fn search_until(
        &self,
        query: &Query,
        cancelled: Option<&AtomicBool>,
        emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<Stats, RunError> {
        if query.has_content() {
            return self.search_content(query, Some(UNCOVERED_BOUND), cancelled, emit);
        }
        query.run_indexed_until(&self.catalog, &self.names, None, cancelled, emit)
    }

    /// Streams a content query's rows over this pin's content view, or
    /// over none, verifying Maybe documents through the crawl's checked
    /// reader. More than `bound` uncovered documents is
    /// [`RunError::IndexIncomplete`]; `None` (`--scan-uncovered`) reads
    /// however many there are.
    pub fn search_content(
        &self,
        query: &Query,
        bound: Option<u32>,
        cancelled: Option<&AtomicBool>,
        emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<Stats, RunError> {
        if !query.has_content() {
            return query.run_indexed_until(&self.catalog, &self.names, None, cancelled, emit);
        }
        let catalog = &self.catalog;
        let docs = self.doc_names()?;
        let pinned = Pinned::new(self.content.as_deref(), self.live());
        let content = Content {
            pinned: &pinned,
            docs,
            bound,
        };
        let mut reader = ferret_crawl::Documents::by_name(catalog)?;
        let mut read = |name, request: ferret_query::ReadRequest<'_>| {
            let observed = match request {
                ferret_query::ReadRequest::Stat => reader.stat_name(catalog, name),
                ferret_query::ReadRequest::Bytes(out) => {
                    reader.read_current_name_until(catalog, name, out, &|| {
                        cancelled.is_some_and(|flag| flag.load(Ordering::Acquire))
                    })
                }
            }
            .ok()?;
            let recorded = catalog.inode(catalog.name(name).child).stat;
            Some(if recorded.same_version(&observed) {
                ferret_query::ReadVersion::Catalogued
            } else {
                ferret_query::ReadVersion::Current
            })
        };
        query.run_content(catalog, &self.names, &content, &mut read, cancelled, emit)
    }

    /// The catalog view's live documents, built once per view.
    pub fn live(&self) -> &DocSet {
        self.derived
            .live
            .get_or_init(|| live_documents(&self.catalog))
    }

    /// Runs find with the plan's captured cwd/time and the host's effects.
    /// Live fallback and actions retain their ordinary find semantics.
    pub fn find<E: Effects + Clone + Send>(
        &self,
        plan: &Plan,
        effects: E,
        workers: usize,
    ) -> Result<Outcome, Unsupported> {
        let source = if plan.no_ignore() || plan.is_information() {
            plan.live_source()
        } else {
            plan.indexed_catalog_source(self.catalog.clone(), &self.names)
        };
        plan.run_parallel(source, effects, workers)
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
            Self::Compact(error) => error.fmt(f),
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
            Self::Compact(error) => Some(error),
        }
    }
}

impl QuerySession {
    fn doc_names(&self) -> Result<&DocNames, OpenError> {
        match self.derived.docs.get() {
            Some(docs) => Ok(docs),
            None => {
                let built = DocNames::new(&self.catalog)?;
                Ok(self.derived.docs.get_or_init(|| built))
            }
        }
    }
}
