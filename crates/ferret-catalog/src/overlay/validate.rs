//! Checks run after a family loads: auxiliary rows and inode kinds against
//! the base, every new inode's fields, and documents' reference counts and
//! hashes, either across the whole overlay or only a checked parent's delta.

use super::*;

impl Overlay {
    pub(crate) fn check_aux(&self) -> Result<(), OpenError> {
        self.base.load(&[Section::Links])?;
        if self.aux_checked.get().is_none() {
            let rows = self.unproven(|| {
                self.projection(Family::Namespace)
                    .records(LIFE)
                    .chain(self.projection(Family::Aux).records(LINK))
                    .chain(self.projection(Family::Aux).records(WORKTREE))
            });
            rows.into_iter()
                .try_for_each(|r| self.check_aux_record(r))?;
            let _ = self.aux_checked.set(());
        }
        if self.kinds_checked.get().is_none()
            && let Some(fields) = self.families[Family::Inodes as usize].get()
        {
            let rows = self.unproven(|| fields.records(INODE));
            rows.into_iter()
                .try_for_each(|r| self.check_inode_kind(r))?;
            let _ = self.kinds_checked.set(());
        }
        Ok(())
    }
    /// The rows still to validate. A checked predecessor proves the inherited
    /// rows, so only current changes; disk replay has no predecessor proof,
    /// so every row `all` yields.
    fn unproven<'a, I: Iterator<Item = &'a Record>>(
        &'a self,
        all: impl FnOnce() -> I,
    ) -> Vec<&'a Record> {
        if self
            .parent
            .as_ref()
            .is_some_and(|p| p.checked.get().is_some())
        {
            self.delta.iter().map(Arc::as_ref).collect()
        } else {
            all().collect()
        }
    }
    fn check_aux_record(&self, r: &Record) -> Result<(), OpenError> {
        match r {
            Record::LifePut { id, kind, .. }
                if *id < self.base.base_inode_count() && self.base.kind(InoId(*id)) != *kind =>
            {
                Err(bad("base inode lifetime type changed"))
            }
            Record::LinkPut { id, .. }
                if self.live_inode(*id)
                    && self.kind(*id).unwrap_or_else(|| self.base.kind(InoId(*id)))
                        != Kind::Symlink =>
            {
                Err(bad("link target on non-symlink"))
            }
            Record::WorkTreePut { id, .. }
                if self.live_inode(*id)
                    && !self
                        .kind(*id)
                        .map_or(*id < self.base.base_dir_count(), |kind| kind == Kind::Dir) =>
            {
                Err(bad("work tree on non-directory inode"))
            }
            _ => Ok(()),
        }
    }
    fn check_inode_kind(&self, r: &Record) -> Result<(), OpenError> {
        if let Record::InodePut { id, kind, .. } = r
            && self.live_inode(*id)
        {
            let actual = self.kind(*id).unwrap_or_else(|| self.base.kind(InoId(*id)));
            if *kind != actual {
                return Err(bad("inode/life kind mismatch"));
            }
        }
        Ok(())
    }
    pub(crate) fn check_fields(&self) -> Result<(), OpenError> {
        if self.fields_checked.get().is_some() {
            return Ok(());
        }
        let rows = self.projection(Family::Inodes);
        for r in self.projection(Family::Namespace).records(LIFE) {
            if let Record::LifePut { id, .. } = r
                && *id >= self.base.base_inode_count()
                && rows.get(INODE, *id).is_none()
            {
                return Err(bad("new inode has no fields"));
            }
        }
        if self.families[Family::Aux as usize].get().is_some() {
            self.check_aux()?;
        }
        let _ = self.fields_checked.set(());
        Ok(())
    }
    pub(crate) fn check_documents(&self, view: &Catalog) -> Result<(), OpenError> {
        if self.docs_checked.get().is_some()
            || self.families[Family::Inodes as usize].get().is_none()
            || self.families[Family::Docs as usize].get().is_none()
        {
            return Ok(());
        }
        if let Some(parent) = &self.parent
            && parent.checked.get().is_some()
            && self.families[Family::Aux as usize].get().is_some()
        {
            self.validate_delta(view, parent)?;
            let _ = self.docs_checked.set(());
            return Ok(());
        }
        self.base.load(&[Section::DocRefs])?;
        let doc_delta = self.base_reference_deltas(view)?;
        let mut affected: BTreeSet<_> = doc_delta.keys().copied().collect();
        self.check_doc_records(&mut affected)?;
        for id in affected {
            let actual = i64::from(self.base.doc_references(crate::DocId(id)).unwrap_or(0))
                + doc_delta.get(&id).copied().unwrap_or(0);
            let declared = i64::from(view.doc_references(crate::DocId(id)).unwrap_or(0));
            if actual != declared {
                return Err(bad("document reference count"));
            }
        }
        let _ = self.docs_checked.set(());
        Ok(())
    }
    /// The reference each changed inode moved from its base document to its
    /// document in `view`, per document; rejects an inode naming an absent
    /// document.
    fn base_reference_deltas(&self, view: &Catalog) -> Result<BTreeMap<u32, i64>, OpenError> {
        let mut doc_delta = BTreeMap::<u32, i64>::new();
        let mut changed = BTreeSet::new();
        for r in self.projection(Family::Inodes).records(INODE) {
            if let Record::InodePut { id, .. } = r {
                changed.insert(*id);
            }
        }
        for r in self.projection(Family::Namespace).records(LIFE) {
            if let Record::InodeDelete { id } = r {
                changed.insert(*id);
            }
        }
        for id in changed {
            if id < self.base.base_inode_count()
                && let Some(doc) = self.base.doc(InoId(id))
            {
                *doc_delta.entry(doc.0).or_default() -= 1;
            }
            if self.live_inode(id)
                && let Some(doc) = view.doc(InoId(id))
            {
                if view.doc_hash(doc).is_none() {
                    return Err(bad("inode names absent document"));
                }
                *doc_delta.entry(doc.0).or_default() += 1;
            }
        }
        Ok(doc_delta)
    }
    /// Checks the document records against the base: no hash changes, no
    /// retired id reused, and the live document count. Adds each document a
    /// record names to `affected`.
    fn check_doc_records(&self, affected: &mut BTreeSet<u32>) -> Result<(), OpenError> {
        let mut live_docs = i64::from(self.base.doc_count());
        for r in self.projection(Family::Docs).records(DOC) {
            match r {
                Record::DocPut { id, hash, .. } => {
                    if let Some(old) = self.base.doc_hash(crate::DocId(*id)) {
                        if old != *hash {
                            return Err(bad("document hash changed"));
                        }
                    } else {
                        live_docs += 1;
                    }
                    affected.insert(*id);
                }
                Record::DocDelete { id }
                | Record::DocDormant { id, .. }
                | Record::DocForget { id } => {
                    if self.base.doc_hash(crate::DocId(*id)).is_some() {
                        live_docs -= 1;
                    }
                    affected.insert(*id);
                }
                _ => {}
            }
        }
        if live_docs != i64::from(self.manifest.counts[3]) {
            return Err(bad("live document count"));
        }
        Ok(())
    }
    pub(crate) fn validate_all(&self, view: &Catalog) -> Result<(), OpenError> {
        if self.checked.get().is_some() {
            return Ok(());
        }
        self.check_fields()?;
        // Removing or consuming permissions cannot violate hash uniqueness
        // or the cap. Only a new dormant row needs a complete retained scan.
        let proven = self
            .parent
            .as_ref()
            .filter(|parent| parent.checked.get().is_some());
        if proven.is_none()
            || self
                .delta
                .iter()
                .any(|r| matches!(r.as_ref(), Record::DocDormant { .. }))
        {
            let hashes: Vec<_> = view.dormant_docs().map(|(_, hash)| hash).collect();
            crate::dormant::check_hashes(hashes).map_err(OpenError::Decode)?;
        }
        if let Some(parent) = proven {
            self.validate_delta(view, parent)?;
            let _ = self.docs_checked.set(());
            let _ = self.checked.set(());
            return Ok(());
        }
        let ns = self.namespace();
        let needs_refs = ns.references.values().any(|&d| d != 0)
            || self
                .projection(Family::Namespace)
                .records(LIFE)
                .next()
                .is_some();
        let refs = if needs_refs {
            self.base.name_references()
        } else {
            &[]
        };
        for r in self.projection(Family::Namespace).records(LIFE) {
            let (id, expected) = match r {
                Record::LifePut { id, names, .. } => (*id, i64::from(*names)),
                Record::InodeDelete { id } => (*id, 0),
                _ => continue,
            };
            let actual = i64::from(refs.get(id as usize).copied().unwrap_or(0))
                + ns.references.get(&id).copied().unwrap_or(0);
            if actual != expected {
                return Err(bad("inode name reference count"));
            }
        }
        for (&id, &delta) in &ns.references {
            let actual = i64::from(refs.get(id as usize).copied().unwrap_or(0)) + delta;
            if actual < 0
                || (!self.live_inode(id) && actual != 0)
                || (self.live_inode(id) && !view.is_directory(InoId(id)) && actual == 0)
            {
                return Err(bad("dangling name/inode reference"));
            }
        }
        self.check_documents(view)?;
        for r in self.projection(Family::Aux).records(LINK) {
            if let Record::LinkPut { id, .. } = r
                && self.live_inode(*id)
                && view.kind(InoId(*id)) != Kind::Symlink
            {
                return Err(bad("link target on non-symlink"));
            }
        }
        for r in self.projection(Family::Aux).records(WORKTREE) {
            if let Record::WorkTreePut { id, .. } = r
                && self.live_inode(*id)
                && !view.is_directory(InoId(*id))
            {
                return Err(bad("work tree on absent/non-directory inode"));
            }
        }
        let _ = self.checked.set(());
        Ok(())
    }
    fn validate_delta(&self, view: &Catalog, parent: &Overlay) -> Result<(), OpenError> {
        let (mut docs, changed) = self.delta_touches(view)?;
        self.count_references(view, parent, changed, &mut docs)?;
        self.check_documents_moved(view, parent, docs)
    }
    /// The documents this delta's records name, each with a zero reference
    /// delta, and the inodes it changes; rejects a link or work tree on an
    /// inode of the wrong kind.
    fn delta_touches(
        &self,
        view: &Catalog,
    ) -> Result<(BTreeMap<u32, i64>, BTreeSet<u32>), OpenError> {
        let mut docs = BTreeMap::<u32, i64>::new();
        let mut changed = BTreeSet::new();
        for r in &self.delta {
            match r.as_ref() {
                Record::InodePut { id, .. } | Record::InodeDelete { id } => {
                    changed.insert(*id);
                }
                Record::DocPut { id, .. }
                | Record::DocDelete { id }
                | Record::DocDormant { id, .. }
                | Record::DocForget { id } => {
                    docs.entry(*id).or_default();
                }
                Record::LinkPut { id, .. }
                    if self.live_inode(*id) && view.kind(InoId(*id)) != Kind::Symlink =>
                {
                    return Err(bad("link target on non-symlink"));
                }
                Record::WorkTreePut { id, .. }
                    if self.live_inode(*id) && !view.is_directory(InoId(*id)) =>
                {
                    return Err(bad("work tree on absent/non-directory inode"));
                }
                _ => {}
            }
        }
        Ok((docs, changed))
    }
    /// Adds to `docs` the reference each `changed` inode moved from its
    /// parent document to its document in `view`.
    fn count_references(
        &self,
        view: &Catalog,
        parent: &Overlay,
        changed: BTreeSet<u32>,
        docs: &mut BTreeMap<u32, i64>,
    ) -> Result<(), OpenError> {
        for id in changed {
            let old = parent
                .inode(id)
                .map(|r| match r {
                    Record::InodePut { doc, .. } => *doc,
                    _ => None,
                })
                .unwrap_or_else(|| {
                    (id < self.base.base_inode_count())
                        .then(|| self.base.doc(InoId(id)).map(|d| d.0))
                        .flatten()
                });
            if parent.live_inode(id)
                && let Some(doc) = old
            {
                *docs.entry(doc).or_default() -= 1;
            }
            if self.live_inode(id) {
                if let Some(Record::InodePut { kind, .. }) = self.inode(id)
                    && *kind != view.kind(InoId(id))
                {
                    return Err(bad("inode/life kind mismatch"));
                }
                if let Some(doc) = view.doc(InoId(id)) {
                    *docs.entry(doc.0).or_default() += 1;
                }
            }
        }
        Ok(())
    }
    /// Checks each document's references in `view` against its parent's plus
    /// `docs`, that no retired document returns or changes hash, and the live
    /// document count.
    fn check_documents_moved(
        &self,
        view: &Catalog,
        parent: &Overlay,
        docs: BTreeMap<u32, i64>,
    ) -> Result<(), OpenError> {
        let mut count = i64::from(parent.manifest.counts[3]);
        for (id, delta) in docs {
            let prior = match parent.projection(Family::Docs).get(DOC, id) {
                Some(Record::DocPut { references, .. }) => Some(*references),
                Some(
                    Record::DocDelete { .. } | Record::DocDormant { .. } | Record::DocForget { .. },
                ) => None,
                _ => self.base.doc_references(crate::DocId(id)),
            };
            let now = view.doc_references(crate::DocId(id));
            if i64::from(prior.unwrap_or(0)) + delta != i64::from(now.unwrap_or(0)) {
                return Err(bad("document reference count"));
            }
            if prior.is_none() && now.is_some() {
                count += 1;
            }
            if prior.is_some() && now.is_none() {
                count -= 1;
            }
            let old_hash = match parent.projection(Family::Docs).get(DOC, id) {
                Some(Record::DocPut { hash, .. }) => Some(*hash),
                Some(
                    Record::DocDelete { .. } | Record::DocDormant { .. } | Record::DocForget { .. },
                ) => None,
                _ => self.base.doc_hash(crate::DocId(id)),
            };
            if let (Some(a), Some(b)) = (old_hash, view.doc_hash(crate::DocId(id)))
                && a != b
            {
                return Err(bad("document hash changed"));
            }
        }
        if count != i64::from(self.manifest.counts[3]) {
            return Err(bad("live document count"));
        }
        Ok(())
    }
}
