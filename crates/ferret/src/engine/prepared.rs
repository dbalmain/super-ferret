//! Host merge setup and execution, keeping its plan, pin and content guard
//! private from the coordinating engine.

use std::sync::MutexGuard;

use ferret_index::{Budget, IndexWriter, MergePlan, Merged};

use super::{Engine, Error, QuerySession, catalog_view};

/// Exact merge inputs bound to their post-revocation pin and content ownership.
/// Constructed by `prepare`; its implementation establishes the
/// revocation/pin ordering. The low-level index primitive cannot enforce
/// catalog revocation. Reset is separate.
pub(super) struct PreparedMerge<'a> {
    engine: &'a Engine,
    content: MutexGuard<'a, Option<IndexWriter>>,
    plan: MergePlan,
    pin: QuerySession,
}

impl<'a> PreparedMerge<'a> {
    pub(super) fn prepare(
        engine: &'a Engine,
        admitted: Option<&QuerySession>,
        budget: &Budget,
        all: bool,
    ) -> Result<Option<Self>, Error> {
        let mut writer_guard = engine.writer.lock().map_err(|_| Error::WriterPanicked)?;
        let writer = writer_guard.as_mut().ok_or(Error::ReadOnly)?;
        // Routine background work must never wait behind content while
        // holding writer: refresh needs writer during the streaming phase.
        let mut content = match engine.content.try_lock() {
            Ok(content) => content,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
            Err(std::sync::TryLockError::Poisoned(_)) => return Err(Error::WriterPanicked),
        };
        let index = content.as_mut().ok_or(Error::NoContentIndex)?;
        let current = index.view();
        if let Some(admitted) = admitted
            && admitted.content().is_none_or(|view| {
                view.manifest().incarnation != current.manifest().incarnation
                    || view.manifest().sequence != current.manifest().sequence
            })
        {
            return Ok(None);
        }
        let pin = engine.pin();
        let Some(plan) = index
            .prepare_merge(&catalog_view(pin.catalog(), pin.live()), budget, all)
            .map_err(Error::Content)?
        else {
            return Ok(None);
        };
        if let Some(admitted) = admitted {
            let selection = admitted
                .content()
                .and_then(|view| view.merge_selection(admitted.live()));
            if selection.is_none_or(|entries| {
                entries
                    .iter()
                    .map(|entry| entry.number)
                    .ne(plan.entries().iter().map(|entry| entry.number))
            }) {
                return Ok(None);
            }
        }
        let view = writer
            .forget_dormant(&plan.ranges())
            .map_err(Error::CatalogWrite)?;
        if view.generation() != pin.generation() {
            engine.select(view);
        }
        let pin = engine.pin();
        drop(writer_guard);
        Ok(Some(Self {
            engine,
            content,
            plan,
            pin,
        }))
    }

    /// Consumes the prepared pair while retaining content through publication.
    pub(super) fn run(mut self, budget: &Budget) -> Result<Option<Merged>, Error> {
        let index = self.content.as_mut().ok_or(Error::NoContentIndex)?;
        let result = index.merge_selected(
            &catalog_view(self.pin.catalog(), self.pin.live()),
            budget,
            self.plan,
        );
        self.engine.select_content(index.view());
        result.map_err(Error::Content)
    }
}

/// Explicit build tail: all plans are revoked under writer→content before the
/// shared filtering pin is taken. Each plan therefore has a post-revocation
/// pin. Daemon admission remains single-plan until it can reserve an entire
/// wave.
pub(super) fn closing(engine: &Engine, budget: &Budget) -> Result<usize, Error> {
    let mut writer_guard = engine.writer.lock().map_err(|_| Error::WriterPanicked)?;
    let writer = writer_guard.as_mut().ok_or(Error::ReadOnly)?;
    let mut content = match engine.content.try_lock() {
        Ok(content) => content,
        Err(std::sync::TryLockError::WouldBlock) => return Ok(0),
        Err(std::sync::TryLockError::Poisoned(_)) => return Err(Error::WriterPanicked),
    };
    let index = content.as_mut().ok_or(Error::NoContentIndex)?;
    let pin = engine.pin();
    let plans = index
        .prepare_merges(&catalog_view(pin.catalog(), pin.live()), budget)
        .map_err(Error::Content)?;
    if plans.is_empty() {
        return Ok(0);
    }
    for plan in &plans {
        let view = writer
            .forget_dormant(&plan.ranges())
            .map_err(Error::CatalogWrite)?;
        if view.generation() != engine.pin().generation() {
            engine.select(view);
        }
    }
    let pin = engine.pin();
    drop(writer_guard);
    let result =
        index.merge_selected_batch(&catalog_view(pin.catalog(), pin.live()), budget, plans);
    engine.select_content(index.view());
    result.map_err(Error::Content)
}
