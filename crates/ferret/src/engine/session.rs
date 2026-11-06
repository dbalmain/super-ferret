//! A pinned query session: searching its catalog view and content, the
//! derived name and document indexes it builds on demand, and grep's
//! candidate documents.

use std::ops::ControlFlow;

use ferret_index::Pinned;
use ferret_query::find::{Effects, Outcome, Plan, Unsupported};
use ferret_query::{Content, Query, Row, RunError, Stats, UNCOVERED_BOUND};

use super::*;

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
    /// Reads the posting lists now and decodes them on demand.
    pub(crate) fn grep_candidates(
        &self,
        grep: &ferret_verify::Grep,
        stopped: &dyn Fn() -> bool,
    ) -> Result<GrepCandidates<'_>, ferret_index::ReadError> {
        let live = self
            .live_until(stopped)
            .ok_or(ferret_index::ReadError::Cancelled)?;
        let pinned = Pinned::new_until(self.content.as_deref(), live, stopped)
            .ok_or(ferret_index::ReadError::Cancelled)?;
        let atom = ferret_query::GrepAtom::read_until(grep.clone(), &pinned, stopped)?;
        Ok(GrepCandidates {
            atom,
            probed: DocSet::empty(0),
            found: DocSet::empty(0),
            all: OnceLock::new(),
            pinned,
        })
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
    pub(super) fn live_until(&self, cancelled: impl Fn() -> bool) -> Option<&DocSet> {
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
            plan.indexed_catalog_source(self.catalog.clone(), self.names.get())
        };
        plan.run_parallel(source, effects, workers)
    }
}

/// Grep's candidates, decoded only as far as the walk needs them (S5).
/// [`GrepCandidates::probe`] decodes those among the documents a walk will
/// ask about, seeking past the rest; asking about any other document
/// decodes every candidate once, so membership never depends on the probe.
pub(crate) struct GrepCandidates<'s> {
    pinned: Pinned<'s>,
    atom: ferret_query::GrepAtom,
    probed: DocSet,
    found: DocSet,
    // `None` when cancelled while decoding: every document is then kept.
    all: OnceLock<Option<DocSet>>,
}

impl GrepCandidates<'_> {
    /// An upper bound on the candidates, from the dictionary.
    pub(crate) fn bound(&self) -> u64 {
        self.atom.bound(&self.pinned)
    }

    /// Decodes the candidates among `docs`.
    pub(crate) fn probe(
        &mut self,
        docs: DocSet,
        stopped: &dyn Fn() -> bool,
    ) -> Result<(), ferret_index::ReadError> {
        self.found =
            ferret_query::line_grep::candidates_among(&self.atom, &self.pinned, &docs, stopped)?;
        self.probed = docs;
        Ok(())
    }

    /// Whether `doc` may hold a match. Cancellation keeps the document.
    pub(crate) fn contains(&self, doc: u32, stopped: &dyn Fn() -> bool) -> bool {
        if self.probed.contains(doc) {
            return self.found.contains(doc);
        }
        self.all
            .get_or_init(|| {
                ferret_query::line_grep::candidates(&self.atom, &self.pinned, stopped).ok()
            })
            .as_ref()
            .is_none_or(|all| all.contains(doc))
    }
}

impl QuerySession {
    pub(super) fn doc_names(&self) -> Result<&DocNames, OpenError> {
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
        #[cfg(test)]
        DOC_NAMES_BUILDS.with(|builds| builds.set(builds.get() + 1));
        let Some(built) = DocNames::new_until(&self.catalog, cancelled)? else {
            return Ok(None);
        };
        Ok(Some(self.derived.docs.get_or_init(|| built)))
    }
}
