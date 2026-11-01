//! [`IndexWriter`]: the index's own directory of segments under a
//! [`Manifest`], kept in step with the catalog by [`IndexWriter::follow`]
//! and compacted by [`IndexWriter::merge_if_needed`] (docs/S2.md § Manifest
//! and commit, § The content flow, § What S5 needs). [`View`] is one
//! published manifest with its segments open, which a query pins beside a
//! catalog view.
//!
//! **Seam.** This crate never opens a document. A follow pass asks the
//! host's `read` callback for each document's bytes by DocId; the host
//! opens it through the crawl's checked path and catalog key (D21, D55) and
//! paces it. Liveness and the catalog's identity arrive as a
//! [`CatalogView`], since this crate cannot see the catalog. The files this
//! module does open are the index's own: segments and the manifest, all
//! inside the directory the host names.
//!
//! **Commit.** Both files of a pair are written to temporaries, synced,
//! checked against their intended heads and renamed
//! into place, and only then named by a new manifest (write, fsync, rename,
//! directory fsync). A crash at any point leaves the old manifest or the
//! new one, never a manifest naming a missing file; files no manifest names
//! are removed when the next writer opens. Old [`View`]s keep their
//! descriptors, so a merge may unlink its inputs under a reader (D32).
//!
//! **Invariant.** A segment holds only DocIds below the `next_doc` of the
//! catalog view its pass was based on, and only documents live in that
//! view. DocIds are never reused (D36 B, D60 B), so a committed segment is
//! never wrong about a document; at worst it holds a dead one, which
//! liveness hides and a merge purges.

use std::cell::RefCell;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::thread::ScopedJoinHandle;

use ferret_text::{TERM_VERSION, TRIGRAM_VERSION};

use crate::extract::Extracted;
use crate::live::DocSet;
use crate::manifest::{Manifest, SegmentEntry};
use crate::merge;
use crate::regular;
use crate::segment::{
    CHUNK, Hit, Inverter, ReadAt, ReadError, Segment, Sizes, TrigramInverter, WriteError, Writer,
};

mod paced;
use paced::{Paced, PacedInput};
#[cfg(test)]
use paced::{RECENT_PAGES, input_trace};

/// Follow flush charge for terms plus 16 bytes per buffered trigram pair.
/// The fixed trigram allocation is separate from this charge (D70).
pub const BUFFER: usize = 64 << 20;

/// D70 revision 5 B. This seam owns admission and the fixed reservation.
struct PairBuffer;
impl PairBuffer {
    const CAPACITY: usize = 3_145_728;

    fn full(terms: &Inverter, trigrams: &TrigramInverter, bound: usize) -> bool {
        terms
            .memory()
            .saturating_add(trigrams.len().saturating_mul(16))
            >= bound
    }

    fn fits(buffered: usize, distinct: usize) -> bool {
        buffered.saturating_add(distinct) <= Self::CAPACITY
    }

    fn admit(
        trigrams: &mut TrigramInverter,
        doc: u32,
        keys: &[u32],
    ) -> Result<Option<usize>, WriteError> {
        let buffered = trigrams.len();
        let remaining = Self::CAPACITY.saturating_sub(buffered);
        let admitted = trigrams.add_keys(doc, keys, remaining, |_| {})?;
        if let Some(distinct) = admitted {
            debug_assert!(Self::fits(buffered, distinct));
        }
        Ok(admitted)
    }
}

/// What a pass needs from one published catalog view.
#[derive(Clone, Copy, Debug)]
pub struct CatalogView<'a> {
    /// The catalog incarnation; a new one (an explicit reset) discards the
    /// index.
    pub incarnation: [u8; 16],
    /// The view's live documents; its bound is the view's `next_doc`.
    pub live: &'a DocSet,
}

/// How much one call may do.
#[derive(Clone, Copy)]
pub struct Budget<'a> {
    /// Follow: document bytes a pass may read before it stops. Merge: the
    /// most segment bytes one merge may take as input; a larger merge waits.
    pub bytes: u64,
    /// Follow's postings buffer, [`BUFFER`] by default.
    pub buffer: usize,
    /// Follow: the most segments one pass may write, 1 by default. When a
    /// pass may write more, it hands each full buffer's files to a second
    /// thread and reads on, publishing each segment as it completes.
    pub segments: usize,
    /// Cooperative pause checked between documents and merge terms.
    pub cancelled: &'a (dyn Fn() -> bool + Sync),
    /// Called before each bounded index-file transfer: the shared background
    /// pacer, or a no-op for explicit commands. An error aborts the transfer.
    /// A merge charges an input page once per run of nearby reads of it.
    /// Document reads are paced separately by the host's reader.
    pub pace: &'a (dyn Fn(usize) -> io::Result<()> + Sync),
}

impl Budget<'static> {
    /// No limit, no pacing: an explicit `ferret index`.
    pub fn unbounded() -> Self {
        Self {
            bytes: u64::MAX,
            buffer: BUFFER,
            segments: 1,
            pace: &|_| Ok(()),
            cancelled: &|| false,
        }
    }
}

impl std::fmt::Debug for Budget<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Budget")
            .field("bytes", &self.bytes)
            .field("buffer", &self.buffer)
            .field("segments", &self.segments)
            .finish_non_exhaustive()
    }
}

/// Why the host's `read` callback did not return a document's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The document could not be read with its catalog key intact: the file
    /// changed, vanished or failed. It joins the unreadable set and is never
    /// retried.
    Unreadable,
    /// Stop the pass here, before this document: a pause request. The pass
    /// still publishes what it covered.
    Stop,
}

/// The host's document reader: fills the buffer with document `doc`'s
/// bytes, or says why it cannot.
pub type Reader<'a> = dyn FnMut(u32, &mut Vec<u8>) -> Result<(), Fault> + Send + 'a;

/// Why a follow pass ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stopped {
    /// Every live document is covered.
    Covered,
    /// The postings buffer filled.
    Buffer,
    /// The byte budget was spent.
    Bytes,
    /// The callback asked to stop.
    Asked,
}

/// What a follow pass did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Followed {
    /// Documents read and tokenized.
    pub docs: u64,
    /// Their bytes.
    pub bytes: u64,
    /// Documents that joined the unreadable set.
    pub unreadable: u64,
    /// Unreadable documents dropped because the catalog dropped them.
    pub pruned: u64,
    /// The last segment published, if the pass reached any document.
    pub segment: Option<SegmentEntry>,
    /// Segment bytes written, over every segment the pass published.
    pub written: u64,
    pub stopped: Stopped,
    /// Live documents still at or above the frontier.
    pub remaining: u64,
}

/// What a merge did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Merged {
    pub inputs: usize,
    /// Input segment bytes.
    pub read: u64,
    /// Output segment bytes.
    pub written: u64,
    /// Indexed documents dropped because they are dead.
    pub purged: u64,
    /// Unique fingerprint records introduced by dictionary demotion.
    pub records_added: u64,
    /// Fingerprint records removed because their documents died.
    pub records_removed: u64,
    /// The merged segment; `None` when every input document was dead and
    /// the inputs were simply dropped.
    pub segment: Option<SegmentEntry>,
}

/// Why an index operation failed. Except for cooperative cancellation, an
/// error poisons the writer: reopen to remove the failed operation's files.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// A segment the manifest names failed to read.
    Segment(ReadError),
    /// A previous operation failed; reopen the writer.
    Poisoned,
    /// A cooperative merge pause; the old manifest remains selected.
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "index I/O failed: {error}"),
            Self::Segment(error) => error.fmt(f),
            Self::Cancelled => f.write_str("content merge paused"),
            Self::Poisoned => f.write_str("an earlier index write failed; reopen the index"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Segment(error) => Some(error),
            Self::Poisoned | Self::Cancelled => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ReadError> for Error {
    fn from(error: ReadError) -> Self {
        Self::Segment(error)
    }
}

impl From<WriteError> for Error {
    // Follow and merge feed the writer DocIds in order by construction; a
    // contract break here is a bug, reported rather than panicking.
    fn from(error: WriteError) -> Self {
        Self::Io(io::Error::other(error))
    }
}

/// One published manifest with every segment it names open.
pub struct View {
    manifest: Manifest,
    segments: Vec<Arc<Segment<File>>>,
    trigrams: Vec<Arc<Segment<File>>>,
    /// Transient retry selection; no partial output is retained. Append-only
    /// follow work must not let another policy choice reset a large retry.
    pending_merge: OnceLock<Vec<u64>>,
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

    fn merge_run(&self, live: &DocSet) -> Option<std::ops::Range<usize>> {
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

/// Keeps one index directory. The caller must hold the catalog's writer lock
/// for as long as this lives (docs/S2.md § Manifest and commit): the index
/// has one writer, the catalog's.
#[derive(Debug)]
pub struct IndexWriter {
    dir: PathBuf,
    current: Arc<View>,
    poisoned: bool,
    codec: u32,
    trigrams: TrigramInverter,
    publishing: bool,
}

impl IndexWriter {
    /// Opens `dir`'s index for `catalog`, creating it if absent. An index
    /// that cannot be used is discarded and starts empty, to be rebuilt
    /// through coverage: one from another catalog incarnation or tokenizer
    /// version, one whose high water exceeds the catalog's `next_doc`, or one
    /// whose manifest or segments do not read back. Files no manifest names
    /// are removed.
    ///
    /// Segments open as [`View::open`] opens them.
    pub fn open(dir: &Path, catalog: &CatalogView) -> Result<Self, Error> {
        Self::open_with_codec(dir, catalog, crate::segment::DICTIONARY_CODEC)
    }

    /// [`IndexWriter::open`] with segments opened as
    /// [`View::open_with_threads`] opens them.
    pub fn open_with_threads(
        dir: &Path,
        catalog: &CatalogView,
        threads: usize,
    ) -> Result<Self, Error> {
        Self::open_with(dir, catalog, crate::segment::DICTIONARY_CODEC, threads)
    }

    /// Opens an index with the requested term dictionary codec. Existing
    /// segments of another codec are rebuilt through coverage.
    pub fn open_with_codec(dir: &Path, catalog: &CatalogView, codec: u32) -> Result<Self, Error> {
        Self::open_with(dir, catalog, codec, available_threads())
    }

    fn open_with(
        dir: &Path,
        catalog: &CatalogView,
        codec: u32,
        threads: usize,
    ) -> Result<Self, Error> {
        Writer::with_codec(0, 0, codec)?;
        fs::create_dir_all(dir)?;
        let usable = Manifest::read(dir)?.filter(|m| fits(m, catalog));
        let opened = match usable {
            Some(manifest) => match open_pairs(dir, &manifest, threads) {
                Ok((segments, trigrams)) if segments.iter().all(|s| s.codec() == codec) => {
                    Some(View {
                        manifest,
                        segments,
                        trigrams,
                        pending_merge: OnceLock::new(),
                    })
                }
                Err(Error::Io(error)) if error.kind() != io::ErrorKind::NotFound => {
                    return Err(Error::Io(error));
                }
                Ok(_) | Err(_) => None,
            },
            None => None,
        };
        let mut writer = Self {
            dir: dir.to_path_buf(),
            current: Arc::new(View {
                manifest: Manifest::empty(catalog.incarnation),
                segments: Vec::new(),
                trigrams: Vec::new(),
                pending_merge: OnceLock::new(),
            }),
            poisoned: false,
            codec,
            trigrams: TrigramInverter::with_capacity(PairBuffer::CAPACITY),
            publishing: false,
        };
        match opened {
            Some(view) => writer.current = Arc::new(view),
            None => writer.manifest_only(Manifest::empty(catalog.incarnation))?,
        }
        remove_unnamed(dir, &writer.current.manifest)?;
        Ok(writer)
    }

    /// The published state a query pins with its catalog view.
    pub fn view(&self) -> Arc<View> {
        self.current.clone()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Covers uncovered live documents of `catalog` in ascending DocId
    /// order, up to `budget`, publishing at most one segment. `read` fills
    /// its buffer with a document's bytes or says why it cannot.
    pub fn follow(
        &mut self,
        catalog: &CatalogView,
        budget: &Budget,
        read: &mut Reader<'_>,
    ) -> Result<Followed, Error> {
        self.follow_with(catalog, budget, &mut [read])
    }

    /// [`IndexWriter::follow`] with a thread per reader reading and
    /// tokenizing documents ahead of this one, which writes the same segment
    /// as one reader would. Readers past a stop have read for nothing.
    ///
    /// # Panics
    ///
    /// If `readers` is empty.
    pub fn follow_with(
        &mut self,
        catalog: &CatalogView,
        budget: &Budget,
        readers: &mut [&mut Reader<'_>],
    ) -> Result<Followed, Error> {
        assert!(!readers.is_empty(), "a follow pass needs a reader");
        self.guard(|writer| writer.follow_inner(catalog, budget, readers))
    }

    /// One merge step if the policy wants one (adjacent levels, or a segment
    /// past the dead-fraction trigger) and it fits `budget.bytes`.
    pub fn merge_if_needed(
        &mut self,
        catalog: &CatalogView,
        budget: &Budget,
    ) -> Result<Option<Merged>, Error> {
        self.guard(|writer| {
            writer.reconcile(catalog)?;
            let Some(run) = writer.current.merge_run(catalog.live) else {
                return Ok(None);
            };
            writer.merge_run(catalog, budget, run)
        })
    }

    /// Merges every segment into one, purging dead documents: a forced full
    /// merge, for measurement and for an explicit optimise.
    pub fn merge_all(
        &mut self,
        catalog: &CatalogView,
        budget: &Budget,
    ) -> Result<Option<Merged>, Error> {
        self.guard(|writer| {
            writer.reconcile(catalog)?;
            let count = writer.current.manifest.segments.len();
            if count == 0 {
                return Ok(None);
            }
            writer.merge_run(catalog, budget, 0..count)
        })
    }

    fn guard<T>(&mut self, run: impl FnOnce(&mut Self) -> Result<T, Error>) -> Result<T, Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        self.publishing = false;
        let result = match run(self) {
            Err(Error::Io(ref error) | Error::Segment(ReadError::Io(ref error)))
                if matches!(
                    error.get_ref().and_then(|e| e.downcast_ref::<Error>()),
                    Some(Error::Cancelled)
                ) =>
            {
                Err(Error::Cancelled)
            }
            result => result,
        };
        if result.is_err()
            && !self.publishing
            && !result.as_ref().err().is_some_and(injected_crash)
            && let Err(error) = remove_unnamed(&self.dir, &self.current.manifest)
        {
            self.poisoned = true;
            return Err(error.into());
        }
        self.poisoned = result.is_err() && !matches!(result, Err(Error::Cancelled));
        self.trigrams.clear();
        if self.trigrams.capacity() > PairBuffer::CAPACITY {
            self.trigrams = TrigramInverter::with_capacity(PairBuffer::CAPACITY);
        }
        result
    }

    /// Discards the index when `catalog` is not the one it was built
    /// against: a new incarnation, or a `next_doc` below the high water.
    fn reconcile(&mut self, catalog: &CatalogView) -> Result<(), Error> {
        if !fits(&self.current.manifest, catalog) {
            self.manifest_only(Manifest::empty(catalog.incarnation))?;
            remove_unnamed(&self.dir, &self.current.manifest)?;
        }
        Ok(())
    }

    fn follow_inner(
        &mut self,
        catalog: &CatalogView,
        budget: &Budget,
        readers: &mut [&mut Reader<'_>],
    ) -> Result<Followed, Error> {
        self.reconcile(catalog)?;
        let live = catalog.live;
        let mut pass = Pass::new(self.current.manifest.clone(), live, budget.buffer);
        // The pass holds the trigram buffer, so that it can publish each
        // segment it completes while it reads on.
        let mut trigrams = std::mem::take(&mut self.trigrams);
        let dir = self.dir.clone();
        let codec = self.codec;
        let result = std::thread::scope(|scope| -> Result<(), Error> {
            // A full buffer's files, being written on their own thread, and
            // the frontier to publish with them.
            let mut writing: Option<(ScopedJoinHandle<'_, Result<WrittenPair, Error>>, u32)> = None;
            let trigrams = &mut trigrams;
            let order = live.range(pass.manifest.frontier, live.bound());
            crate::extract::in_order(order, readers, |doc, extracted| -> Result<bool, Error> {
                match pass.next(&extracted, trigrams, budget, writing.is_some()) {
                    Next::Stop(stopped) => return Ok(pass.stop(stopped, doc)),
                    Next::HandOff => {
                        if let Some(range) = pass.range.take() {
                            if let Some(written) = writing.take() {
                                self.publish_written(&mut pass, live, written)?;
                            }
                            let held = pass.inverter.terms();
                            let drained = pass.seal(range, codec, trigrams, budget.buffer)?;
                            // The next segment is likely the size of this
                            // one, whose table is gone now that it has drained.
                            pass.inverter.reserve(held);
                            let (dir, pace) = (&dir, budget.pace);
                            #[cfg(test)]
                            let inherit = seams();
                            let handle = scope.spawn(move || {
                                #[cfg(test)]
                                inherit();
                                drained.write(dir, pace)
                            });
                            writing = Some((handle, doc));
                        }
                    }
                    Next::Read => {}
                }
                pass.add(doc, extracted, trigrams)
            })?;
            if let Some(written) = writing.take() {
                self.publish_written(&mut pass, live, written)?;
            }
            Ok(())
        });
        self.trigrams = trigrams;
        result?;
        self.publish_pass(pass, live, budget)
    }

    /// Publishes what a follow pass read since its last published segment,
    /// unless it read, skipped and pruned nothing.
    fn publish_pass(
        &mut self,
        mut pass: Pass,
        live: &DocSet,
        budget: &Budget,
    ) -> Result<Followed, Error> {
        let frontier = pass.frontier;
        if pass.range.is_none() && frontier == pass.manifest.frontier && pass.followed.pruned == 0 {
            pass.followed.remaining = live.count(frontier, live.bound()).into();
            return Ok(pass.followed);
        }

        let mut opened = None;
        if let Some(range) = pass.range {
            let drained = pass.seal(range, self.codec, &mut self.trigrams, budget.buffer)?;
            let (entry, segment, trigram) = drained.write(&self.dir, budget.pace)?;
            pass.followed.segment = Some(entry);
            pass.followed.written += entry.total_bytes();
            pass.manifest.segments.push(entry);
            opened = Some((segment, trigram));
        }
        if pass.oversized {
            self.trigrams = TrigramInverter::with_capacity(PairBuffer::CAPACITY);
        }
        let mut manifest = pass.manifest;
        manifest.last_follow = Some(timestamp());
        manifest.frontier = frontier;
        manifest.high_water = manifest.high_water.max(live.bound());
        let mut segments = self.current.segments.clone();
        let mut trigrams = self.current.trigrams.clone();
        if let Some((segment, trigram)) = opened {
            segments.push(segment);
            trigrams.push(trigram);
        }
        self.publish(manifest, segments, trigrams)?;
        pass.followed.remaining = live.count(frontier, live.bound()).into();
        Ok(pass.followed)
    }

    /// Publishes a segment that a follow pass wrote on a second thread, with
    /// the frontier the pass had reached when it handed the segment over:
    /// what a pass that had stopped there would have published.
    fn publish_written(
        &mut self,
        pass: &mut Pass,
        live: &DocSet,
        (writing, frontier): (ScopedJoinHandle<'_, Result<WrittenPair, Error>>, u32),
    ) -> Result<(), Error> {
        let (entry, segment, trigram) = writing
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))?;
        pass.manifest.segments.push(entry);
        pass.manifest.frontier = frontier;
        pass.manifest.high_water = pass.manifest.high_water.max(live.bound());
        let mut stopped = pass.manifest.clone();
        stopped.last_follow = Some(timestamp());
        // Documents found unreadable since are past this frontier.
        stopped.unreadable.retain(|&doc| doc < frontier);
        let mut segments = self.current.segments.clone();
        segments.push(segment);
        let mut trigrams = self.current.trigrams.clone();
        trigrams.push(trigram);
        self.publish(stopped, segments, trigrams)?;
        pass.manifest.sequence = self.current.manifest.sequence;
        pass.followed.segment = Some(entry);
        pass.followed.written += entry.total_bytes();
        pass.published += 1;
        Ok(())
    }

    /// Merges the segments at `run` into one, if their bytes fit the budget.
    fn merge_run(
        &mut self,
        catalog: &CatalogView,
        budget: &Budget,
        run: std::ops::Range<usize>,
    ) -> Result<Option<Merged>, Error> {
        // Remember identities without replacing the published content view:
        // cancellation preserves its manifest, descriptors and pin identity.
        self.current.pending_merge.get_or_init(|| {
            self.current.manifest.segments[run.clone()]
                .iter()
                .map(|s| s.number)
                .collect()
        });
        if (budget.cancelled)() {
            return Err(Error::Cancelled);
        }
        let mut manifest = self.current.manifest.clone();
        manifest.last_merge = Some(timestamp());
        manifest
            .unreadable
            .retain(|&doc| catalog.live.contains(doc));
        let entries = &self.current.manifest.segments[run.clone()];
        let read: u64 = entries.iter().map(SegmentEntry::total_bytes).sum();
        if read > budget.bytes {
            return Ok(None);
        }
        let (Some(first), Some(last)) = (entries.first(), entries.last()) else {
            return Ok(None);
        };
        let (first, last) = (first.first, last.last);
        let docs: u32 = entries
            .iter()
            .map(|s| alive(s, catalog.live, &manifest.unreadable))
            .sum();
        let purged = entries.iter().map(|s| u64::from(s.docs)).sum::<u64>() - u64::from(docs);
        let input_records: u64 = self.current.segments[run.clone()]
            .iter()
            .map(|s| s.fingerprint_records())
            .sum();
        if docs == 0 {
            // Nothing survives: drop the inputs without writing. Their
            // range stays below the frontier, so it is not re-covered.
            let retired = self.replace_run(manifest, run, None)?;
            return Ok(Some(Merged {
                inputs: retired.len(),
                read,
                written: 0,
                purged,
                records_added: 0,
                records_removed: input_records,
                segment: None,
            }));
        }
        let merge_pace = |bytes| {
            if (budget.cancelled)() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, Error::Cancelled));
            }
            (budget.pace)(bytes)
        };
        let inputs = self.current.segments[run.clone()]
            .iter()
            .map(|segment| PacedInput::open(segment.source(), &merge_pace))
            .collect::<Result<Vec<_>, _>>()?;
        let trigram_inputs = self.current.trigrams[run.clone()]
            .iter()
            .map(|segment| PacedInput::open(segment.source(), &merge_pace))
            .collect::<Result<Vec<_>, _>>()?;
        let number = manifest.next_number;
        manifest.next_number += 1;
        let codec = self.codec;
        let dir = &self.dir;
        // Each output spills its postings to its own scratch file.
        let fill = |inputs: &[Segment<PacedInput<'_>>], file: &File, tri: bool| {
            let extension = if tri { "tri-postings" } else { "postings" };
            let scratch = dir.join(format!("tmp-{number}.{extension}"));
            let postings = regular::replace(&scratch)?;
            let writer = if tri {
                Writer::keyed_spilling(first, last, TRIGRAM_VERSION, file.try_clone()?, postings)?
            } else {
                Writer::spilling_with_codec(first, last, codec, file.try_clone()?, postings)?
            };
            let result = merge::stream(inputs, catalog.live, writer, &merge_pace, budget.cancelled);
            let _ = fs::remove_file(&scratch);
            result
        };
        let mut changes = (0, 0);
        let result = Self::write_pair(
            &self.dir,
            number,
            docs,
            first,
            last,
            budget.cancelled,
            (
                |file| {
                    let sizes = fill(&inputs, file, false)?;
                    changes = (sizes.records_added, sizes.records_removed);
                    Ok(sizes)
                },
                move |file| fill(&trigram_inputs, file, true),
            ),
        );
        let written = result?;
        let entry = written.0;

        if (budget.cancelled)() {
            return Err(Error::Cancelled);
        }
        let retired = self.replace_run(manifest, run, Some(written))?;
        Ok(Some(Merged {
            inputs: retired.len(),
            read,
            written: entry.total_bytes(),
            purged,
            records_added: changes.0,
            records_removed: changes.1,
            segment: Some(entry),
        }))
    }

    /// Publishes `manifest` with the segments at `run` replaced by `merged`,
    /// or dropped when there is none, then retires the replaced files.
    /// Returns their entries.
    fn replace_run(
        &mut self,
        mut manifest: Manifest,
        run: std::ops::Range<usize>,
        merged: Option<WrittenPair>,
    ) -> Result<Vec<SegmentEntry>, Error> {
        let (entry, segment, trigram) = match merged {
            Some((entry, segment, trigram)) => (Some(entry), Some(segment), Some(trigram)),
            None => (None, None, None),
        };
        let retired: Vec<_> = manifest.segments.splice(run.clone(), entry).collect();
        let mut segments = self.current.segments.clone();
        segments.splice(run.clone(), segment);
        let mut trigrams = self.current.trigrams.clone();
        trigrams.splice(run, trigram);
        self.publish(manifest, segments, trigrams)?;
        retire(&self.dir, &retired);
        Ok(retired)
    }

    /// Writes, checks and installs both outputs before manifest publication.
    /// The term file is written here while the trigram file is written on a
    /// second thread: the two are independent until both are renamed.
    fn write_pair(
        dir: &Path,
        number: u64,
        docs: u32,
        first: u32,
        last: u32,
        cancelled: &(dyn Fn() -> bool + Sync),
        (terms, trigrams): (
            impl FnOnce(&File) -> Result<Sizes, Error>,
            impl FnOnce(&File) -> Result<Sizes, Error> + Send,
        ),
    ) -> Result<WrittenPair, Error> {
        let open = |tri: bool, file: File, sizes: Sizes| -> Result<_, Error> {
            let segment = Segment::open(file)?;
            let info = segment.info();
            let version = if tri { TRIGRAM_VERSION } else { TERM_VERSION };
            if (
                info.first,
                info.last,
                info.terms,
                info.pairs,
                info.tokenizer_version,
            ) != (first, last, sizes.terms, sizes.pairs, version)
                || segment.source().metadata()?.len() != sizes.total()
            {
                return Err(ReadError::Head.into());
            }
            Ok(Arc::new(segment))
        };
        let (term_written, trigram_written) = std::thread::scope(|scope| {
            #[cfg(test)]
            let inherit = (
                crate::segment::inherit_fingerprint(),
                input_trace::inherit(),
            );
            let trigram = scope.spawn(|| {
                #[cfg(test)]
                {
                    inherit.0();
                    inherit.1();
                }
                write_output(dir, number, true, cancelled, trigrams)
            });
            let term = write_output(dir, number, false, cancelled, terms);
            let trigram = trigram
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            (term, trigram)
        });
        let (temp, file, sizes) = term_written?;
        hit(Point::TermSynced)?;
        let (trigram_temp, trigram_file, trigram_sizes) = trigram_written?;
        let segment = open(false, file, sizes)?;
        let trigram = open(true, trigram_file, trigram_sizes)?;
        let entry = SegmentEntry {
            number,
            first,
            last,
            docs,
            terms: sizes.terms,
            pairs: sizes.pairs,
            bytes: sizes.total(),
            digest: segment.digest(),
            trigram_bytes: trigram_sizes.total(),
            trigram_keys: trigram_sizes.terms,
            trigram_pairs: trigram_sizes.pairs,
            trigram_digest: trigram.digest(),
        };
        if cancelled() {
            return Err(Error::Cancelled);
        }
        fs::rename(temp, dir.join(entry.file_name()))?;
        hit(Point::TermRenamed)?;
        fs::rename(trigram_temp, dir.join(entry.trigram_file_name()))?;
        hit(Point::PairRenamed)?;
        File::open(dir)?.sync_all()?;
        hit(Point::SegmentWritten)?;
        if cancelled() {
            return Err(Error::Cancelled);
        }
        Ok((entry, segment, trigram))
    }

    fn publish(
        &mut self,
        mut manifest: Manifest,
        segments: Vec<Arc<Segment<File>>>,
        trigrams: Vec<Arc<Segment<File>>>,
    ) -> Result<(), Error> {
        manifest.sequence += 1;
        self.publishing = true;
        manifest.publish(&self.dir)?;
        let pending_merge = self
            .current
            .pending_merge
            .get()
            .filter(|selection| {
                manifest.incarnation == self.current.manifest.incarnation
                    && selection.iter().all(|number| {
                        manifest
                            .segments
                            .iter()
                            .any(|entry| entry.number == *number)
                    })
            })
            .cloned()
            .map_or_else(OnceLock::new, OnceLock::from);
        self.current = Arc::new(View {
            manifest,
            segments,
            trigrams,
            pending_merge,
        });
        self.publishing = false;
        Ok(())
    }

    /// Publishes a manifest with no segments.
    fn manifest_only(&mut self, manifest: Manifest) -> Result<(), Error> {
        let sequence = self.current.manifest.sequence;
        self.publish(
            Manifest {
                sequence,
                ..manifest
            },
            Vec::new(),
            Vec::new(),
        )
    }
}

/// Whether `manifest` belongs to `catalog`'s view: same incarnation, term
/// and trigram versions, and no DocId the view has not published.
fn fits(manifest: &Manifest, catalog: &CatalogView) -> bool {
    manifest.incarnation == catalog.incarnation
        && manifest.tokenizer_version == TERM_VERSION
        && manifest.trigram_version == TRIGRAM_VERSION
        && manifest.high_water <= catalog.live.bound()
}

/// Indexed documents of `segment` still live: the live documents in its
/// range less the unreadable ones, which sit in the range with no postings.
/// No document in the range can have become live since (D60 B).
fn alive(segment: &SegmentEntry, live: &DocSet, unreadable: &[u32]) -> u32 {
    let end = segment.last.saturating_add(1);
    let skipped = unreadable
        .iter()
        .filter(|&&doc| doc >= segment.first && doc < end && live.contains(doc))
        .count() as u32;
    live.count(segment.first, end) - skipped
}

/// A follow pass's progress: the manifest it will publish, what it has
/// done, and the segment it is filling.
struct Pass {
    manifest: Manifest,
    followed: Followed,
    inverter: Inverter,
    /// The current segment's range and document count.
    range: Option<(u32, u32)>,
    docs: u32,
    frontier: u32,
    /// Whether the current segment holds one document too big for the
    /// trigram buffer.
    oversized: bool,
    /// Segments this pass has published from a second thread.
    published: usize,
}

/// What a follow pass does before reading its next document.
enum Next {
    Read,
    /// Hand the full buffers to a second thread to write, then read on.
    HandOff,
    Stop(Stopped),
}

impl Pass {
    /// A pass over `live` from `manifest`, whose unreadable documents the
    /// catalog has since dropped are pruned.
    fn new(mut manifest: Manifest, live: &DocSet, buffer: usize) -> Self {
        let before = manifest.unreadable.len();
        manifest.unreadable.retain(|&doc| live.contains(doc));
        let followed = Followed {
            docs: 0,
            bytes: 0,
            unreadable: 0,
            pruned: (before - manifest.unreadable.len()) as u64,
            segment: None,
            written: 0,
            stopped: Stopped::Covered,
            remaining: 0,
        };
        Self {
            manifest,
            followed,
            inverter: Inverter::new(buffer),
            range: None,
            docs: 0,
            frontier: live.bound(),
            oversized: false,
            published: 0,
        }
    }

    /// Whether to read `extracted`, hand off first, or stop before it.
    fn next(
        &self,
        extracted: &Result<&Extracted, Fault>,
        trigrams: &TrigramInverter,
        budget: &Budget,
        writing: bool,
    ) -> Next {
        // The first document is read before the pause is consulted, so a pass
        // that keeps outlasting its time still makes progress. The pause comes
        // before a full buffer's stop: a caller that goes straight on after a
        // full buffer must still see it.
        if self.followed.docs + self.followed.unreadable != 0 && (budget.cancelled)() {
            return Next::Stop(Stopped::Asked);
        }
        // A document whose trigrams the buffer has no room left for fills it
        // as surely as the bound does.
        let overflows = extracted
            .as_ref()
            .is_ok_and(|e| !PairBuffer::fits(trigrams.len(), e.keys.len()));
        let full = self.docs != 0
            && (overflows || PairBuffer::full(&self.inverter, trigrams, budget.buffer));
        let hand_off = full
            && self.followed.bytes < budget.bytes
            && self.published + usize::from(writing) + 1 < budget.segments;
        if self.docs != 0 && self.followed.bytes >= budget.bytes || full && !hand_off {
            Next::Stop(if full {
                Stopped::Buffer
            } else {
                Stopped::Bytes
            })
        } else if hand_off {
            Next::HandOff
        } else {
            Next::Read
        }
    }

    /// Stops the pass with `frontier` as the first document left unread.
    /// Returns false, for the extraction loop.
    fn stop(&mut self, stopped: Stopped, frontier: u32) -> bool {
        self.followed.stopped = stopped;
        self.frontier = frontier;
        false
    }

    /// Adds `doc` to the current segment, or to the unreadable set. Returns
    /// whether to read on.
    fn add(
        &mut self,
        doc: u32,
        extracted: Result<&Extracted, Fault>,
        trigrams: &mut TrigramInverter,
    ) -> Result<bool, Error> {
        match extracted {
            Ok(extracted) => {
                match PairBuffer::admit(trigrams, doc, &extracted.keys)? {
                    Some(_) => {}
                    None => {
                        // A document that overflows a buffer holding others
                        // stopped or handed off above.
                        debug_assert_eq!(self.docs, 0);
                        trigrams.add_keys(doc, &extracted.keys, usize::MAX, |_| {})?;
                        self.oversized = true;
                    }
                }
                for (hash, term) in extracted.terms() {
                    self.inverter.add_hashed(doc, hash, term)?;
                }
                self.followed.docs += 1;
                self.docs += 1;
                self.followed.bytes += extracted.len;
            }
            Err(Fault::Unreadable) => {
                self.manifest.unreadable.push(doc);
                self.followed.unreadable += 1;
            }
            Err(Fault::Stop) => return Ok(self.stop(Stopped::Asked, doc)),
        }
        self.range = Some((self.range.map_or(doc, |(first, _)| first), doc));
        if self.oversized {
            return Ok(self.stop(Stopped::Buffer, doc + 1));
        }
        Ok(true)
    }

    /// Drains the current segment's buffers over `range` as the next segment
    /// number's writers, leaving both empty.
    fn seal(
        &mut self,
        range: (u32, u32),
        codec: u32,
        trigrams: &mut TrigramInverter,
        buffer: usize,
    ) -> Result<Drained, Error> {
        let number = self.manifest.next_number;
        self.manifest.next_number += 1;
        let full = std::mem::replace(&mut self.inverter, Inverter::new(buffer));
        let (terms, trigrams) = drain_buffers(range, codec, full, trigrams)?;
        let docs = std::mem::take(&mut self.docs);
        Ok(Drained {
            number,
            docs,
            range,
            terms,
            trigrams,
        })
    }
}

/// A sealed segment's writers, ready to write as its pair.
struct Drained {
    number: u64,
    docs: u32,
    range: (u32, u32),
    terms: Writer,
    trigrams: Writer,
}

impl Drained {
    fn write(
        self,
        dir: &Path,
        pace: &(dyn Fn(usize) -> io::Result<()> + Sync),
    ) -> Result<WrittenPair, Error> {
        write_drained(
            dir,
            self.number,
            self.docs,
            self.range,
            pace,
            self.terms,
            self.trigrams,
        )
    }
}

/// Drains a follow's buffers into a segment pair's writers, leaving
/// `trigrams` empty with its capacity.
fn drain_buffers(
    (first, last): (u32, u32),
    codec: u32,
    mut inverter: Inverter,
    trigrams: &mut TrigramInverter,
) -> Result<(Writer, Writer), Error> {
    // The two drains share nothing: terms sort on a second thread while the
    // trigram pairs radix-sort here.
    let (terms, trigrams) = std::thread::scope(|scope| {
        #[cfg(test)]
        let inherit = crate::segment::inherit_fingerprint();
        let terms = scope.spawn(|| {
            #[cfg(test)]
            inherit();
            inverter.drain_into_codec(first, last, codec)
        });
        let trigrams = trigrams.drain_into(first, last);
        let terms = terms
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        (terms, trigrams)
    });
    Ok((terms?, trigrams?))
}

/// Writes drained buffers as segment pair `number`.
fn write_drained(
    dir: &Path,
    number: u64,
    docs: u32,
    (first, last): (u32, u32),
    pace: &(dyn Fn(usize) -> io::Result<()> + Sync),
    terms: Writer,
    trigrams: Writer,
) -> Result<WrittenPair, Error> {
    let finish = |writer: Writer, file: &File| -> Result<Sizes, Error> {
        let mut out = BufWriter::new(Paced { file, pace });
        let sizes = writer.finish(&mut out)?;
        out.flush()?;
        Ok(sizes)
    };
    IndexWriter::write_pair(
        dir,
        number,
        docs,
        first,
        last,
        // Follow checks pause between documents and publishes the completed
        // range, as its Stopped::Asked contract requires. Merge checks
        // cancellation while writing its output pair.
        &|| false,
        (|file| finish(terms, file), |file| finish(trigrams, file)),
    )
}

/// This thread's test seams, to set on a thread that writes for it.
#[cfg(test)]
fn seams() -> impl FnOnce() + Send {
    let inherit = (
        crate::segment::inherit_fingerprint(),
        input_trace::inherit(),
        STOP.with_borrow(Arc::clone),
    );
    move || {
        inherit.0();
        inherit.1();
        STOP.set(inherit.2);
    }
}

/// Writes and syncs one output of a pair to its temporary name.
fn write_output(
    dir: &Path,
    number: u64,
    tri: bool,
    cancelled: &(dyn Fn() -> bool + Sync),
    fill: impl FnOnce(&File) -> Result<Sizes, Error>,
) -> Result<(PathBuf, File, Sizes), Error> {
    if cancelled() {
        return Err(Error::Cancelled);
    }
    let extension = if tri { "tri" } else { "seg" };
    let temp = dir.join(format!("tmp-{number}.{extension}"));
    let file = regular::replace(&temp)?;
    let sizes = fill(&file)?;
    file.sync_all()?;
    Ok((temp, file, sizes))
}

type OpenPair = (Arc<Segment<File>>, Arc<Segment<File>>);
type WrittenPair = (SegmentEntry, Arc<Segment<File>>, Arc<Segment<File>>);
type PairedSegments = (Vec<Arc<Segment<File>>>, Vec<Arc<Segment<File>>>);

/// Opens `entry`'s term segment, or its trigram segment when `tri`, checking
/// it is the one `manifest` describes.
fn open_segment(
    dir: &Path,
    manifest: &Manifest,
    entry: &SegmentEntry,
    tri: bool,
) -> Result<Arc<Segment<File>>, Error> {
    let name = if tri {
        entry.trigram_file_name()
    } else {
        entry.file_name()
    };
    // A special file under a segment's name is refused, as a
    // regular file of the wrong shape is, without waiting on it.
    let file = regular::open(&dir.join(name)).map_err(|e| {
        if regular::is_not_regular(&e) {
            Error::Segment(ReadError::NotASegment)
        } else {
            Error::Io(e)
        }
    })?;
    let bytes = file.metadata()?.len();
    let segment = Segment::open(file)?;
    let matches = if tri {
        entry.matches_trigrams(
            &segment.info(),
            &segment.digest(),
            bytes,
            manifest.trigram_version,
        ) && segment.codec() == crate::segment::DICTIONARY_CODEC
    } else {
        entry.matches(
            &segment.info(),
            &segment.digest(),
            bytes,
            manifest.tokenizer_version,
        )
    };
    if !matches {
        return Err(ReadError::Head.into());
    }
    Ok(Arc::new(segment))
}

/// Opens every segment pair in manifest order, on at most `threads` threads.
/// The error is the first in manifest order, as a serial open returns it: a
/// NotFound there makes [`View::open`] retry from a newer manifest.
fn open_pairs(dir: &Path, manifest: &Manifest, threads: usize) -> Result<PairedSegments, Error> {
    let one = |entry: &SegmentEntry| -> Result<OpenPair, Error> {
        Ok((
            open_segment(dir, manifest, entry, false)?,
            open_segment(dir, manifest, entry, true)?,
        ))
    };
    // Parsing each segment's block index is most of opening a view, so
    // segments open in parallel (grep-why P7), on the host's thread count
    // (D77 C). The calling thread opens whatever a refused spawn leaves.
    let segments = &manifest.segments;
    let threads = threads.clamp(1, segments.len().max(1));
    let pairs: Result<Vec<_>, Error> = if threads == 1 {
        segments.iter().map(one).collect()
    } else {
        let chunk = segments.len().div_ceil(threads);
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            let mut rest: &[SegmentEntry] = &[];
            for (i, entries) in segments.chunks(chunk).enumerate() {
                let work = move || entries.iter().map(one).collect::<Vec<_>>();
                match spawn_open(scope, i, work) {
                    Ok(handle) => handles.push(handle),
                    Err(_) => {
                        rest = &segments[i * chunk..];
                        break;
                    }
                }
            }
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
                .chain(rest.iter().map(one))
                .collect()
        })
    };
    pairs.map(|pairs| pairs.into_iter().unzip())
}

/// Starts segment-open thread `i` on the scope; `Err` when the OS refuses.
fn spawn_open<'scope, T: Send + 'scope>(
    scope: &'scope std::thread::Scope<'scope, '_>,
    i: usize,
    work: impl FnOnce() -> T + Send + 'scope,
) -> io::Result<std::thread::ScopedJoinHandle<'scope, T>> {
    #[cfg(test)]
    {
        if FAIL_OPEN_SPAWN_AT.get() == Some(i) {
            return Err(io::Error::other("injected spawn failure"));
        }
        OPEN_SPAWNS.set(OPEN_SPAWNS.get() + 1);
    }
    #[cfg(not(test))]
    let _ = i;
    std::thread::Builder::new()
        .name("ferret-open".into())
        .spawn_scoped(scope, work)
}

/// One thread per available core, for opens without a configured count.
fn available_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// Unlinks merged-away segments. Open views keep their descriptors (D32);
/// a crash before this leaves orphans the next open removes.
fn retire(dir: &Path, retired: &[SegmentEntry]) {
    for old in retired {
        let _ = fs::remove_file(dir.join(old.file_name()));
        let _ = fs::remove_file(dir.join(old.trigram_file_name()));
    }
}

/// Removes segment and temporary files `manifest` does not name. Other
/// files are left alone.
fn remove_unnamed(dir: &Path, manifest: &Manifest) -> io::Result<()> {
    let named: Vec<String> = manifest
        .segments
        .iter()
        .flat_map(|entry| [entry.file_name(), entry.trigram_file_name()])
        .collect();
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let ours = name.starts_with("seg-") || name.starts_with("tmp-") || name == "manifest.tmp";
        if ours && !named.iter().any(|n| n == name) {
            fs::remove_file(dir.join(name))?;
        }
    }
    Ok(())
}

/// Places a test can stop an operation, as a crash would.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Point {
    /// A segment is synced and renamed into place; no manifest names it.
    SegmentWritten,
    TermSynced,
    TermRenamed,
    PairRenamed,
    /// The new manifest is renamed into place, before the directory sync.
    ManifestRenamed,
    /// Publication has synced its temporary but has not renamed it yet.
    ManifestSynced,
    /// A merge has written part of its output.
    MergeMidway,
}

pub(crate) fn hit(point: Point) -> io::Result<()> {
    #[cfg(test)]
    if STOP.with_borrow(|stop| {
        let mut stop = stop.lock().unwrap_or_else(|e| e.into_inner());
        stop.take_if(|stop| *stop == point).is_some()
    }) {
        return Err(io::Error::other(InjectedStop(point)));
    }
    #[cfg(test)]
    if CANCEL_AT.get() == Some(point) {
        CANCEL_AT.set(None);
        CANCELLED.set(true);
    }
    #[cfg(not(test))]
    let _ = point;
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// The next [`hit`] of this point fails, once.
    static CANCEL_AT: std::cell::Cell<Option<Point>> = const { std::cell::Cell::new(None) };
    static CANCELLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Shared with a thread that writes for this one, so that the point
    /// fails once, on whichever thread reaches it first.
    pub(crate) static STOP: RefCell<Arc<std::sync::Mutex<Option<Point>>>> =
        RefCell::new(Arc::default());
    /// The segment-open thread whose spawn fails.
    static FAIL_OPEN_SPAWN_AT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    /// Segment-open threads started on this thread.
    static OPEN_SPAWNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests;

fn timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// Crash injection leaves files as a terminated process would. Ordinary errors
// still exercise eager cleanup through the same operation and publication path.
#[cfg(test)]
#[derive(Debug)]
struct InjectedStop(Point);
#[cfg(test)]
impl std::fmt::Display for InjectedStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "injected crash at {:?}", self.0)
    }
}
#[cfg(test)]
impl std::error::Error for InjectedStop {}

fn injected_crash(error: &Error) -> bool {
    #[cfg(test)]
    if let Error::Io(io) = error {
        return io.get_ref().is_some_and(|e| e.is::<InjectedStop>());
    }
    let _ = error;
    false
}
