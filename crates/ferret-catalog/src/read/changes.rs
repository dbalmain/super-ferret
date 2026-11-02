//! What a view's log overlay changed since its checkpoint, and advancing a
//! view by one checked transaction.

use super::*;

impl Catalog {
    /// Directory coverage/count rows replaced in the selected log prefix.
    pub fn changed_directory_scopes(&self) -> impl Iterator<Item = InoId> + '_ {
        self.overlay
            .iter()
            .flat_map(|overlay| overlay.projection(Family::Namespace).records(overlay::DIR))
            .filter_map(|record| match record {
                Record::DirPut { id, .. } => Some(InoId(*id)),
                _ => None,
            })
    }

    /// Name rows the selected overlay replaced since the checkpoint, each
    /// with whether it is live in this view: tombstones included, also for
    /// names born earlier in the epoch. Bounded by the overlay's name rows.
    /// Requires Names.
    pub fn changed_names(&self) -> impl Iterator<Item = (NameId, bool)> + '_ {
        self.overlay
            .iter()
            .flat_map(|overlay| overlay.projection(Family::Namespace).records(overlay::NAME))
            .filter_map(|record| match record {
                Record::NamePut { id, .. } => Some((NameId(*id), true)),
                Record::NameDelete { id } => Some((NameId(*id), false)),
                _ => None,
            })
    }

    /// Inodes whose stat row, document included, the selected overlay
    /// replaced since the checkpoint. Bounded by the overlay's inode rows.
    /// Requires [`Section::Doc`].
    pub fn changed_inodes(&self) -> impl Iterator<Item = InoId> + '_ {
        self.overlay
            .iter()
            .flat_map(|overlay| overlay.projection(Family::Inodes).records(overlay::INODE))
            .filter_map(|record| match record {
                Record::InodePut { id, .. } => Some(InoId(*id)),
                _ => None,
            })
    }

    /// Base edges overwritten or removed by the selected generation.
    pub fn suppressed_base_names(&self) -> impl Iterator<Item = NameId> + '_ {
        self.overlay
            .iter()
            .flat_map(|overlay| overlay.namespace().suppressed.iter().copied().map(NameId))
    }

    /// Applies one checked same-epoch transaction without copying base buffers.
    /// A mismatched generation is rejected before any supplied id is read.
    pub fn advance(
        &self,
        expected: Generation,
        changes: &crate::log::ChangeSet,
    ) -> Result<Self, crate::log::Error> {
        self.advance_with_sniffer(expected, changes, self.sniffer_version())
    }
    pub(crate) fn advance_with_sniffer(
        &self,
        expected: Generation,
        changes: &crate::log::ChangeSet,
        sniffer: u32,
    ) -> Result<Self, crate::log::Error> {
        use crate::log::Error;
        self.generation().check(expected).map_err(Error::Stale)?;
        self.load_all().map_err(Error::Previous)?;
        let mut manifest = self
            .overlay
            .as_ref()
            .map_or_else(|| self.manifest(), |o| o.manifest.clone());
        if changes.records.is_empty() {
            if changes.counters != manifest.counters || changes.counts != manifest.counts {
                return Err(Error::Invalid(DecodeError::Corrupt(
                    "empty change set counters",
                )));
            }
            return Ok(self.clone());
        }
        manifest.generation.sequence = expected
            .sequence
            .checked_add(1)
            .filter(|&n| n != u64::MAX)
            .ok_or(Error::Invalid(DecodeError::Corrupt("sequence exhausted")))?;
        if changes
            .counters
            .iter()
            .zip(manifest.counters)
            .any(|(&a, b)| a < b)
        {
            return Err(Error::Invalid(DecodeError::Corrupt(
                "allocation counters decreased",
            )));
        }
        manifest.sniffer = sniffer;
        manifest.counters = changes.counters;
        manifest.counts = changes.counts;
        Manifest::decode(&manifest.encode()).map_err(Error::Invalid)?;
        crate::log::checked_size(changes, manifest.generation.sequence).map_err(Error::Invalid)?;
        let mut view = self.clone();
        let parent = self
            .overlay
            .clone()
            .unwrap_or_else(|| Arc::new(Overlay::empty(self.clone(), self.manifest())));
        view.overlay = Some(Arc::new(parent.advance(
            parent.clone(),
            manifest,
            &changes.records,
        )));
        view.load_all().map_err(Error::Previous)?;
        if let Some(overlay) = view.overlay.as_mut().and_then(Arc::get_mut) {
            overlay.detach();
        } else {
            unreachable!("new view has an unshared overlay");
        }
        Ok(view)
    }
    /// Number of immutable replacement runs, for carry measurements.
    /// Requires all families loaded.
    pub fn overlay_run_count(&self) -> usize {
        self.overlay.as_ref().map_or(0, |o| {
            [Family::Namespace, Family::Inodes, Family::Aux, Family::Docs]
                .iter()
                .map(|&f| o.projection(f).run_count())
                .sum()
        })
    }
    pub(super) fn overlay_inode(&self, id: InoId) -> Option<&Record> {
        self.overlay.as_ref().and_then(|o| o.inode(id.0))
    }
}
