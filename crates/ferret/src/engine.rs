//! One resident catalog, shared by immutable query pins and the optional
//! writer. Output and live-tree actions run outside the publication locks.

use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

use ferret_catalog::{Catalog, Generation, OpenError, WriterSession};
use ferret_crawl::{IndexOptions, RefreshReport, RefreshRequest};
use ferret_query::find::{Effects, Outcome, Plan, Unsupported};
use ferret_query::{NameIndex, Query, Row, RunError, Stats};

static OPEN_COUNT: AtomicU64 = AtomicU64::new(0);

/// A checked, fully loaded query engine. A query-only engine takes no writer
/// lock; attaching a writer shares its already loaded catalog buffers.
pub struct Engine {
    current: RwLock<QuerySession>,
    writer: Mutex<Option<WriterSession>>,
    retired: Mutex<Vec<(u64, Weak<NameIndex>)>>,
}

/// A generation pinned for the whole query, including output callbacks.
#[derive(Clone)]
pub struct QuerySession {
    catalog: Catalog,
    names: Arc<NameIndex>,
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
            }),
            writer: Mutex::new(None),
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
            }),
            writer: Mutex::new(Some(writer)),
            retired: Mutex::new(Vec::new()),
        }
    }

    /// The daemon calls this after its writer queue stops, before releasing
    /// endpoint ownership. Query pins never retain this lock; closing it here
    /// prevents a replacement host from winning the endpoint but losing the
    /// still-live old engine's writer lock.
    pub(crate) fn close_writer(&self) {
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
        self.select(report.view.clone());
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
        *self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = QuerySession {
            catalog: view,
            names,
        };
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
    pub fn search_until(
        &self,
        query: &Query,
        cancelled: Option<&AtomicBool>,
        emit: impl FnMut(&Row<'_>) -> ControlFlow<()>,
    ) -> Result<Stats, RunError> {
        query.run_indexed_until(&self.catalog, &self.names, None, cancelled, emit)
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
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadOnly => f.write_str("the engine has no writer"),
            Self::WriterPanicked => f.write_str("the engine writer panicked"),
            Self::Refresh(error) => error.fmt(f),
            Self::Compact(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ReadOnly | Self::WriterPanicked => None,
            Self::Refresh(error) => Some(error),
            Self::Compact(error) => Some(error),
        }
    }
}
