//! One resident catalog, shared by immutable query pins and the optional
//! writer. Output and live-tree actions run outside the publication locks.
//!
//! A writer engine may also own the content index (docs/S2.md § Manifest
//! and commit): [`Engine::attach_content`] opens it under the catalog's
//! writer lock, and a [`QuerySession`] then pins the index view published
//! with its catalog view. A query-only engine opens that index read-only
//! through [`Engine::open_content`]: no writer lock, and nothing is repaired
//! or removed.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

use ferret_catalog::{Catalog, Generation, OpenError, Section, WriterSession};
use ferret_crawl::{IndexOptions, RefreshReport, RefreshRequest};
use ferret_index::{Budget, CatalogView, DocSet, Fault, Followed, IndexWriter, Merged, Pinned};
use ferret_query::find::{Effects, Outcome, Plan, Unsupported};
use ferret_query::{Content, DocNames, NameIndex, Query, Row, RunError, Stats, UNCOVERED_BOUND};

pub(crate) mod reader;

static OPEN_COUNT: AtomicU64 = AtomicU64::new(0);

/// A checked, fully loaded query engine. A query-only engine takes no writer
/// lock; attaching a writer shares its already loaded catalog buffers.
pub struct Engine {
    current: RwLock<QuerySession>,
    writer: Mutex<Option<WriterSession>>,
    /// Content mutation is independent of catalog refresh. When both are
    /// needed, lock `writer` first; merges hold only this mutex and a pin.
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

impl Engine {
    /// Opens and validates every catalog section once. No checkpoint returns
    /// `None`, as in the catalog reader API.
    pub fn open(index: &Path) -> Result<Option<Self>, OpenError> {
        let Some(catalog) = Catalog::open(index)? else {
            return Ok(None);
        };
        let catalog = catalog.into_resident()?;
        let names = Arc::new(OnceLock::from(NameIndex::new(&catalog)));
        Ok(Some(Self::query_only(index, catalog, names)))
    }

    /// As [`Engine::open`], for a process that runs one query and exits:
    /// every section is loaded and checked as before, but the catalog keeps
    /// its raw name heap instead of interning it, and the name planner is
    /// built only if a query needs it. A content-only query such as grep
    /// never does, so it skips both builds.
    pub fn open_one_shot(index: &Path) -> Result<Option<Self>, OpenError> {
        let Some(catalog) = Catalog::open(index)? else {
            return Ok(None);
        };
        catalog.load_all()?;
        Ok(Some(Self::query_only(index, catalog, Arc::default())))
    }

    fn query_only(index: &Path, catalog: Catalog, names: Arc<OnceLock<NameIndex>>) -> Self {
        OPEN_COUNT.fetch_add(1, Ordering::Relaxed);
        Self {
            current: RwLock::new(QuerySession {
                names,
                catalog,
                content: None,
                derived: Arc::default(),
                threads: Threads::new(index),
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
                names: Arc::new(OnceLock::from(NameIndex::new(&catalog))),
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
        self.content
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    /// Published log budget of the resident writer. `None` when this engine
    /// has no writer. This waits for the writer lock, which a refresh, index
    /// or follow holds for its whole operation, so never call it while
    /// holding a lock queries need (the daemon's engine selection, say). A
    /// commit between [`Engine::pin`] and this read moves the budget ahead of
    /// that pin.
    pub(crate) fn log_budget(&self) -> Option<ferret_catalog::BudgetUsage> {
        self.writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(WriterSession::budget_usage)
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
    /// Segments open on the search thread count.
    pub fn attach_content(&self, catalog_dir: &Path) -> Result<(), Error> {
        let threads = self.pin().search_threads();
        let writer = self.writer.lock().map_err(|_| Error::WriterPanicked)?;
        let view = writer.as_ref().ok_or(Error::ReadOnly)?.view();
        let live = live_documents(&view);
        let mut content = self.content.lock().map_err(|_| Error::WriterPanicked)?;
        let opened = IndexWriter::open_with_threads(
            &catalog_dir.join(CONTENT_DIR),
            &catalog_view(&view, &live),
            threads,
        )
        .map_err(Error::Content)?;
        let published = opened.view();
        *content = Some(opened);
        self.select_content(published);
        Ok(())
    }

    /// Opens the content index in `catalog_dir`'s [`CONTENT_DIR`] for
    /// reading, for a query-only engine: no writer lock, nothing repaired
    /// or removed. An index that is absent or does not fit the catalog view
    /// leaves a fresh engine without one, which content queries treat as
    /// nothing covered. A newer published view survives a delayed open.
    /// Segments open on the search thread count.
    pub fn open_content(&self, catalog_dir: &Path) -> Result<(), Error> {
        let pin = self.pin();
        let view = ferret_index::View::open_with_threads(
            &catalog_dir.join(CONTENT_DIR),
            &catalog_view(pin.catalog(), pin.live()),
            pin.search_threads(),
        )
        .map_err(Error::Content)?;
        if let Some(view) = view {
            self.select_content(Arc::new(view));
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
        if let Some(limiter) = limiter {
            documents = documents.with_limiter(limiter);
        }
        let followed = index.follow(&catalog_view(view, live), budget, &mut |doc, bytes| {
            let Some(&name) = names.names(doc).first() else {
                return Err(Fault::Unreadable);
            };
            documents
                .read_name(view, name, bytes)
                .map_err(|_| Fault::Unreadable)
        });
        let published = index.view();
        self.select_content(published);
        followed.map_err(Error::Content)
    }

    /// One merge step of the content index, if its policy wants one. Holds
    /// only the content mutex and a catalog pin, so refreshes can publish
    /// while the merge streams its immutable inputs.
    pub fn merge_content(&self, budget: &Budget) -> Result<Option<Merged>, Error> {
        let mut content = self.content.lock().map_err(|_| Error::WriterPanicked)?;
        let index = content.as_mut().ok_or(Error::NoContentIndex)?;
        let pin = self.pin();
        self.merge_locked(index, &pin, budget)
    }

    /// Uses the same catalog pin that admitted the daemon's merge resources.
    /// A content publisher that overtook admission invalidates that attempt;
    /// never reconcile a newer index backwards against an old catalog pin.
    pub(crate) fn merge_content_at(
        &self,
        pin: &QuerySession,
        budget: &Budget,
    ) -> Result<Option<Merged>, Error> {
        let mut content = self.content.lock().map_err(|_| Error::WriterPanicked)?;
        let index = content.as_mut().ok_or(Error::NoContentIndex)?;
        let view = index.view();
        if view.manifest().incarnation != pin.generation().incarnation
            || pin
                .content()
                .is_none_or(|admitted| view.manifest().sequence != admitted.manifest().sequence)
        {
            return Ok(None);
        }
        self.merge_locked(index, pin, budget)
    }

    fn merge_locked(
        &self,
        index: &mut IndexWriter,
        pin: &QuerySession,
        budget: &Budget,
    ) -> Result<Option<Merged>, Error> {
        let merged = index.merge_if_needed(&catalog_view(pin.catalog(), pin.live()), budget);
        self.select_content(index.view());
        merged.map_err(Error::Content)
    }

    /// Publishes a content view beside the current catalog view. Publishers
    /// can finish out of order: manifest sequence must never go backwards.
    fn select_content(&self, content: Arc<ferret_index::View>) {
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let manifest = content.manifest();
        if manifest.incarnation == current.generation().incarnation
            && manifest.high_water <= current.catalog.next_doc().0
            && current
                .content
                .as_ref()
                .is_none_or(|selected| selected.manifest().sequence <= manifest.sequence)
        {
            current.content = Some(content);
        }
    }

    fn select(&self, view: Catalog) {
        let previous = self.pin();
        let names = Arc::new(OnceLock::from(NameIndex::adopt(
            &view,
            Some(previous.name_index()),
        )));
        let derived = Derived::adopt(&previous, &view);
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

fn catalog_view<'a>(view: &Catalog, live: &'a DocSet) -> CatalogView<'a> {
    CatalogView {
        incarnation: view.generation().incarnation,
        live,
    }
}

impl QuerySession {
    /// Bytes retained by the fully resident catalog and its name planner.
    pub fn resident_bytes(&self) -> u64 {
        self.catalog.bytes_read() + self.names.get().map_or(0, NameIndex::bytes) as u64
    }

    /// The view's name planner, built here on a one-shot engine's first call.
    pub fn name_index(&self) -> &NameIndex {
        self.names.get_or_init(|| NameIndex::new(&self.catalog))
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
        query.run_indexed(&self.catalog, self.name_index(), Some(scope), emit)
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

    /// Streams rows, checking host cancellation at entry and at every
    /// candidate boundary.
    /// A content query refuses an index with more than
    /// [`UNCOVERED_BOUND`] uncovered documents.
    pub fn search_until(
        &self,
        query: &Query,
        cancelled: Option<&AtomicBool>,
        emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<Stats, RunError> {
        if is_cancelled(cancelled) {
            return Ok(Stats::default());
        }
        if query.has_content() {
            return self.search_content(query, Some(UNCOVERED_BOUND), cancelled, emit);
        }
        query.run_indexed_until(&self.catalog, self.name_index(), None, cancelled, emit)
    }

    /// Dictionary-only plan for a content query, including one refused by
    /// search. This borrows the same catalog/content pin as execution.
    pub fn explain_content(&self, query: &Query) -> Result<ferret_query::ContentPlan, RunError> {
        let pinned = ferret_index::Pinned::new(self.content.as_deref(), self.live());
        query.explain_content(&self.catalog, self.name_index(), &pinned)
    }

    /// Streams a content query's rows over this pin's content view, or
    /// over none, verifying Maybe documents through the crawl's checked
    /// reader. More than `bound` uncovered documents is
    /// [`RunError::IndexIncomplete`]; `None` (`--scan-uncovered`) reads
    /// however many there are.
    ///
    /// A cancelled query returns no rows and default stats, as the checked
    /// runner does, including while the catalog view's derived state is
    /// first built: a cancelled build leaves nothing cached.
    pub fn search_content(
        &self,
        query: &Query,
        bound: Option<u32>,
        cancelled: Option<&AtomicBool>,
        emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<Stats, RunError> {
        if is_cancelled(cancelled) {
            return Ok(Stats::default());
        }
        self.search_content_threads(query, bound, self.search_threads(), cancelled, emit)
    }

    /// Threads [`QuerySession::search_content`] verifies on, from
    /// `FERRET_SEARCH_THREADS`, the index's settings file and
    /// `available_parallelism` ([`search_threads`]). Resolved once per
    /// engine, by the first call.
    pub fn search_threads(&self) -> usize {
        self.threads.get()
    }

    /// [`QuerySession::search_content`] verifying Maybe documents on
    /// `threads` workers; one verifies on the calling thread. For files
    /// that do not change during the query, and without cancellation,
    /// output is the same for any count; a document may be read before
    /// earlier rows' callbacks run
    /// (`ferret_query::Query::run_content_parallel`).
    pub fn search_content_threads(
        &self,
        query: &Query,
        bound: Option<u32>,
        threads: usize,
        cancelled: Option<&AtomicBool>,
        emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<Stats, RunError> {
        let stopped = || is_cancelled(cancelled);
        if stopped() {
            return Ok(Stats::default());
        }
        if !query.has_content() {
            return query.run_indexed_until(
                &self.catalog,
                self.name_index(),
                None,
                cancelled,
                emit,
            );
        }
        let catalog = &self.catalog;
        let Some(docs) = self.doc_names_until(stopped)? else {
            return Ok(Stats::default());
        };
        let Some(pinned) = self
            .live_until(stopped)
            .and_then(|live| Pinned::new_until(self.content.as_deref(), live, stopped))
        else {
            return Ok(Stats::default());
        };
        let content = Content {
            pinned: &pinned,
            docs,
            bound,
        };
        let documents = ferret_crawl::Documents::by_name(catalog)?;
        let fork = || -> Box<ferret_query::Reader<'_>> {
            let mut checked = reader::checked(catalog, &documents);
            Box::new(move |name, request| match checked(name, request) {
                ferret_query::ReadOutcome::Stable(version) => Some(version),
                _ => None,
            })
        };
        query.run_content_parallel(
            catalog,
            self.name_index(),
            &content,
            ferret_query::Workers {
                fork: &fork,
                threads,
            },
            cancelled,
            emit,
        )
    }

    /// Grep's necessary candidates, including uncovered documents. Missing
    /// trigram sources automatically enumerate; there is no scan refusal.
    pub(crate) fn grep_candidates(
        &self,
        grep: &ferret_verify::Grep,
        stopped: &dyn Fn() -> bool,
    ) -> Result<DocSet, ferret_index::ReadError> {
        let live = self
            .live_until(stopped)
            .ok_or(ferret_index::ReadError::Cancelled)?;
        let pinned = Pinned::new_until(self.content.as_deref(), live, stopped)
            .ok_or(ferret_index::ReadError::Cancelled)?;
        ferret_query::line_grep::candidates(grep, &pinned, stopped)
    }

    /// The catalog view's live documents, built once per view.
    pub fn live(&self) -> &DocSet {
        self.derived
            .live
            .get_or_init(|| live_documents(&self.catalog))
    }

    /// As [`QuerySession::live`], publishing only a completed build: `None`
    /// once `cancelled` answers true, leaving the next caller to build it.
    /// Two concurrent builders may both build; the first to finish wins.
    fn live_until(&self, cancelled: impl Fn() -> bool) -> Option<&DocSet> {
        if let Some(live) = self.derived.live.get() {
            return Some(live);
        }
        let built = live_documents_until(&self.catalog, cancelled)?;
        Some(self.derived.live.get_or_init(|| built))
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
            plan.indexed_catalog_source(self.catalog.clone(), self.name_index())
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
        match self.doc_names_until(|| false)? {
            Some(docs) => Ok(docs),
            None => unreachable!("a build that is never cancelled completes"),
        }
    }

    /// As `doc_names`, publishing only a completed build: `Ok(None)` once
    /// `cancelled` answers true, leaving the next caller to build it. Two
    /// concurrent builders may both build; the first to finish wins.
    pub(crate) fn doc_names_until(
        &self,
        cancelled: impl Fn() -> bool,
    ) -> Result<Option<&DocNames>, OpenError> {
        if let Some(docs) = self.derived.docs.get() {
            return Ok(Some(docs));
        }
        let Some(built) = DocNames::new_until(&self.catalog, cancelled)? else {
            return Ok(None);
        };
        Ok(Some(self.derived.docs.get_or_init(|| built)))
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
/// stored count, `available` up to [`SEARCH_THREADS`].
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
