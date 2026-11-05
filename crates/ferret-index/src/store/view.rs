//! [`View`]: one published manifest with its segments open, the
//! read side a query pins beside a catalog view. The writer builds and
//! replaces views, so their fields are visible to the rest of `store`.

use super::*;
use crate::segment::Hit;

/// One published manifest with every segment it names open.
pub struct View {
    pub(super) manifest: Manifest,
    pub(super) segments: Vec<Arc<Segment<File>>>,
    pub(super) trigrams: Vec<Arc<Segment<File>>>,
    /// Transient retry selection; no partial output is retained. Append-only
    /// follow work must not let another policy choice reset a large retry.
    pub(super) pending_merge: OnceLock<Vec<u64>>,
}

impl std::fmt::Debug for View {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("View")
            .field("manifest", &self.manifest)
            .finish_non_exhaustive()
    }
}

impl View {
    /// Resident manifest and segment metadata; postings remain on disk.
    /// Shared segments are counted once in this view, also shared by older
    /// pins.
    pub fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.pending_merge.get().map_or(0, |selection| {
                selection.capacity() * std::mem::size_of::<u64>()
            })
            + self.manifest.segments.capacity() * std::mem::size_of::<SegmentEntry>()
            + self.manifest.unreadable.capacity() * std::mem::size_of::<u32>()
            + self.trigrams.capacity() * std::mem::size_of::<Arc<Segment<File>>>()
            + self.segments.capacity() * std::mem::size_of::<Arc<Segment<File>>>()
            + self
                .segments
                .iter()
                .chain(&self.trigrams)
                .map(|s| s.resident_bytes() + std::mem::size_of::<Segment<File>>())
                .sum::<usize>()
    }

    /// Immutable input identities for the next policy-selected merge. The
    /// host can retain these across cancelled attempts even as unrelated
    /// follow publications append segments to the manifest.
    pub fn merge_selection(&self, live: &DocSet) -> Option<&[SegmentEntry]> {
        Some(&self.manifest.segments[self.merge_run(live)?])
    }

    /// Headroom for the next policy-selected merge, absent when no merge is
    /// due. Memory includes growable decoded lists and spilling scratch.
    /// The term and trigram outputs merge at once, and each stream holds
    /// three document lists (decoded, live, and the writer's relative ids)
    /// whose capacity can reach 8 bytes a document. Disk includes output
    /// plus postings scratch while the committed inputs still exist.
    pub fn merge_resources(&self, live: &DocSet) -> Option<(u64, u64)> {
        let entries = self.merge_selection(live)?;
        let bytes = entries.iter().map(SegmentEntry::total_bytes).sum::<u64>();
        let docs = entries.iter().map(|s| u64::from(s.docs)).sum::<u64>();
        Some((
            docs.saturating_mul(2 * 3 * 8)
                .saturating_add(64 << 20)
                .saturating_add(bytes / 8),
            bytes.saturating_mul(3).saturating_add(16 << 20),
        ))
    }

    pub(super) fn merge_run(&self, live: &DocSet) -> Option<std::ops::Range<usize>> {
        if let Some(selection) = self.pending_merge.get()
            && let Some(&first) = selection.first()
            && let Some(start) = self
                .manifest
                .segments
                .iter()
                .position(|s| s.number == first)
            && let Some(entries) = self.manifest.segments.get(start..start + selection.len())
            && entries
                .iter()
                .map(|s| s.number)
                .eq(selection.iter().copied())
            && (entries.len() > 1
                || merge::choose(entries, &|s| alive(s, live, &self.manifest.unreadable)).is_some())
        {
            return Some(start..start + selection.len());
        }
        merge::choose(&self.manifest.segments, &|s| {
            alive(s, live, &self.manifest.unreadable)
        })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// In DocId order, matching `manifest().segments`.
    pub fn segments(&self) -> &[Arc<Segment<File>>] {
        &self.segments
    }

    /// Trigram segments in the same order and ranges as `segments()`.
    pub fn trigrams(&self) -> &[Arc<Segment<File>>] {
        &self.trigrams
    }

    /// Every document whose postings hold `term`, ascending, across the
    /// segments. Dead documents are included: the caller filters by its
    /// view's liveness.
    pub fn lookup(&self, term: &[u8]) -> Result<Vec<u32>, ReadError> {
        let mut docs = Vec::new();
        for segment in &self.segments {
            let mut part = segment.fingerprint_docs(term)?;
            match segment.lookup(term)? {
                None => {}
                Some(Hit::Single(doc)) => part.push(doc),
                Some(Hit::Postings(list)) => list.decode(&mut part),
            }
            part.sort_unstable();
            part.dedup();
            docs.extend(part);
        }
        Ok(docs)
    }

    /// Opens `dir`'s published index for reading only, without the writer
    /// lock: a query host that does not own the writer. `None` when there
    /// is no index, or it does not fit `catalog` (another incarnation or
    /// tokenizer, or a high water past its `next_doc`), which a query
    /// treats as nothing covered. Segments are immutable and a merge
    /// unlinks a retired one only after publishing the manifest that drops
    /// it, so a segment missing here means a newer manifest exists: the
    /// open is retried, a few times, from the manifest.
    ///
    /// Segments open on up to one thread per available core; a host with a
    /// configured thread count uses [`View::open_with_threads`].
    pub fn open(dir: &Path, catalog: &CatalogView) -> Result<Option<Self>, Error> {
        Self::open_with_threads(dir, catalog, available_threads())
    }

    /// [`View::open`] with segments opened on at most `threads` threads,
    /// capped by the segment count; one opens them on the calling thread.
    pub fn open_with_threads(
        dir: &Path,
        catalog: &CatalogView,
        threads: usize,
    ) -> Result<Option<Self>, Error> {
        for _ in 0..3 {
            let Some(manifest) = Manifest::read(dir)?.filter(|m| fits(m, catalog)) else {
                return Ok(None);
            };
            match open_pairs(dir, &manifest, threads) {
                Ok((segments, trigrams)) => {
                    return Ok(Some(Self {
                        manifest,
                        segments,
                        trigrams,
                        pending_merge: OnceLock::new(),
                    }));
                }
                Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    /// [`View::uncovered`] as a set, built without an intermediate list:
    /// during a first build it holds every live document.
    pub fn uncovered_set(&self, live: &DocSet) -> DocSet {
        match self.uncovered_set_until(live, || false) {
            Some(set) => set,
            None => unreachable!("a build that is never cancelled completes"),
        }
    }

    /// As [`View::uncovered_set`], `None` once `cancelled` answers true
    /// (asked as [`DocSet::tail_until`] asks it).
    pub fn uncovered_set_until(
        &self,
        live: &DocSet,
        cancelled: impl Fn() -> bool,
    ) -> Option<DocSet> {
        live.tail_until(
            self.manifest.frontier,
            self.manifest.unreadable.iter().copied(),
            cancelled,
        )
    }

    /// The live documents this view does not cover, ascending: the
    /// unreadable ones, and every live one at or above the frontier.
    pub fn uncovered(&self, live: &DocSet) -> Vec<u32> {
        let unreadable = self.manifest.unreadable.iter().copied();
        unreadable
            .filter(|&doc| live.contains(doc))
            .chain(live.range(self.manifest.frontier, live.bound()))
            .collect()
    }

    /// Bytes of every segment file.
    pub fn bytes(&self) -> u64 {
        self.manifest
            .segments
            .iter()
            .map(SegmentEntry::total_bytes)
            .sum()
    }
}
