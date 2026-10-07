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
//! **Commit.** A segment is written to a temporary file, synced and renamed
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

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use ferret_text::{Scratch, TOKENIZER_VERSION, cap, tokenize};

use crate::live::DocSet;
use crate::manifest::{Manifest, SegmentEntry};
use crate::merge;
use crate::segment::{Hit, Inverter, ReadAt, ReadError, Segment, WriteError, Writer};

/// S2.md's follow buffer: a pass flushes its segment once its buffered
/// postings are estimated at this many bytes.
pub const BUFFER: usize = 64 << 20;

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
    /// Cooperative pause checked between documents and merge terms.
    pub cancelled: &'a dyn Fn() -> bool,
    /// Called before each bounded index-file transfer: the shared background
    /// pacer, or a no-op for explicit commands. An error aborts the transfer.
    /// Document reads are paced separately by the host's reader.
    pub pace: &'a dyn Fn(usize) -> io::Result<()>,
}

impl Budget<'static> {
    /// No limit, no pacing: an explicit `ferret index`.
    pub fn unbounded() -> Self {
        Self {
            bytes: u64::MAX,
            buffer: BUFFER,
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
pub type Reader<'a> = dyn FnMut(u32, &mut Vec<u8>) -> Result<(), Fault> + 'a;

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
    /// The segment published, if the pass reached any document.
    pub segment: Option<SegmentEntry>,
    /// Segment bytes written.
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
            + self.segments.capacity() * std::mem::size_of::<Arc<Segment<File>>>()
            + self
                .segments
                .iter()
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
    /// due. Memory includes growable decoded lists and spilling scratch;
    /// disk includes output plus postings scratch while the committed
    /// inputs still exist.
    pub fn merge_resources(&self, live: &DocSet) -> Option<(u64, u64)> {
        let entries = self.merge_selection(live)?;
        let bytes = entries.iter().map(|s| s.bytes).sum::<u64>();
        let docs = entries.iter().map(|s| u64::from(s.docs)).sum::<u64>();
        Some((
            docs.saturating_mul(24)
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

    /// Every document whose postings hold `term`, ascending, across the
    /// segments. Dead documents are included: the caller filters by its
    /// view's liveness.
    pub fn lookup(&self, term: &[u8]) -> Result<Vec<u32>, ReadError> {
        let mut docs = Vec::new();
        for segment in &self.segments {
            match segment.lookup(term)? {
                None => {}
                Some(Hit::Single(doc)) => docs.push(doc),
                Some(Hit::Postings(list)) => list.decode(&mut docs),
            }
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
    pub fn open(dir: &Path, catalog: &CatalogView) -> Result<Option<Self>, Error> {
        for _ in 0..3 {
            let Some(manifest) = Manifest::read(dir)?.filter(|m| fits(m, catalog)) else {
                return Ok(None);
            };
            match open_segments(dir, &manifest) {
                Ok(segments) => {
                    return Ok(Some(Self {
                        manifest,
                        segments,
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
        self.manifest.segments.iter().map(|s| s.bytes).sum()
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
}

impl IndexWriter {
    /// Opens `dir`'s index for `catalog`, creating it if absent. An index
    /// that cannot be used is discarded and starts empty, to be rebuilt
    /// through coverage: one from another catalog incarnation or tokenizer
    /// version, one whose high water exceeds the catalog's `next_doc`, or one
    /// whose manifest or segments do not read back. Files no manifest names
    /// are removed.
    pub fn open(dir: &Path, catalog: &CatalogView) -> Result<Self, Error> {
        fs::create_dir_all(dir)?;
        let usable = Manifest::read(dir)?.filter(|m| fits(m, catalog));
        let opened = match usable {
            Some(manifest) => match open_segments(dir, &manifest) {
                Ok(segments) => Some(View {
                    manifest,
                    segments,
                    pending_merge: OnceLock::new(),
                }),
                Err(Error::Io(error)) if error.kind() != io::ErrorKind::NotFound => {
                    return Err(Error::Io(error));
                }
                Err(_) => None,
            },
            None => None,
        };
        let mut writer = Self {
            dir: dir.to_path_buf(),
            current: Arc::new(View {
                manifest: Manifest::empty(catalog.incarnation),
                segments: Vec::new(),
                pending_merge: OnceLock::new(),
            }),
            poisoned: false,
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
        self.guard(|writer| writer.follow_inner(catalog, budget, read))
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
        if matches!(result, Err(Error::Cancelled)) {
            if let Err(error) = remove_unnamed(&self.dir, &self.current.manifest) {
                self.poisoned = true;
                return Err(error.into());
            }
        } else {
            self.poisoned = result.is_err();
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
        read: &mut Reader<'_>,
    ) -> Result<Followed, Error> {
        self.reconcile(catalog)?;
        let live = catalog.live;
        let mut manifest = self.current.manifest.clone();
        let before = manifest.unreadable.len();
        manifest.unreadable.retain(|&doc| live.contains(doc));
        let mut followed = Followed {
            docs: 0,
            bytes: 0,
            unreadable: 0,
            pruned: (before - manifest.unreadable.len()) as u64,
            segment: None,
            written: 0,
            stopped: Stopped::Covered,
            remaining: 0,
        };

        let mut inverter = Inverter::new(budget.buffer);
        let (mut scratch, mut bytes) = (Scratch::default(), Vec::new());
        let mut range: Option<(u32, u32)> = None;
        let mut frontier = live.bound();
        for doc in live.range(manifest.frontier, live.bound()) {
            if followed.bytes >= budget.bytes || inverter.is_full() {
                followed.stopped = if inverter.is_full() {
                    Stopped::Buffer
                } else {
                    Stopped::Bytes
                };
                frontier = doc;
                break;
            }
            if (budget.cancelled)() {
                followed.stopped = Stopped::Asked;
                frontier = doc;
                break;
            }
            bytes.clear();
            match read(doc, &mut bytes) {
                Ok(()) => {
                    let mut failed = None;
                    tokenize(&bytes, &mut scratch, |token| {
                        if let Err(error) = inverter.add(doc, cap(token.bytes)) {
                            failed = Some(error);
                        }
                    });
                    if let Some(error) = failed {
                        return Err(error.into());
                    }
                    followed.docs += 1;
                    followed.bytes += bytes.len() as u64;
                }
                Err(Fault::Unreadable) => {
                    manifest.unreadable.push(doc);
                    followed.unreadable += 1;
                }
                Err(Fault::Stop) => {
                    followed.stopped = Stopped::Asked;
                    frontier = doc;
                    break;
                }
            }
            range = Some((range.map_or(doc, |(first, _)| first), doc));
        }
        if range.is_none() && frontier == manifest.frontier && followed.pruned == 0 {
            followed.remaining = live.count(frontier, live.bound()).into();
            return Ok(followed);
        }

        let mut opened = None;
        if let Some((first, last)) = range {
            let writer = inverter.drain_into(first, last)?;
            let (entry, segment) =
                self.write_segment(&mut manifest, followed.docs as u32, |file| {
                    let mut out = BufWriter::new(Paced {
                        file,
                        pace: budget.pace,
                    });
                    let sizes = writer.finish(&mut out)?;
                    out.flush()?;
                    Ok(sizes.total())
                })?;
            followed.segment = Some(entry);
            followed.written = entry.bytes;
            manifest.segments.push(entry);
            opened = Some(segment);
        }
        manifest.last_follow = Some(timestamp());
        manifest.frontier = frontier;
        manifest.high_water = manifest.high_water.max(live.bound());
        let mut segments = self.current.segments.clone();
        segments.extend(opened);
        self.publish(manifest, segments)?;
        followed.remaining = live.count(frontier, live.bound()).into();
        Ok(followed)
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
        let read: u64 = entries.iter().map(|s| s.bytes).sum();
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
        if docs == 0 {
            // Nothing survives: drop the inputs without writing. Their
            // range stays below the frontier, so it is not re-covered.
            let retired: Vec<_> = manifest.segments.drain(run.clone()).collect();
            let mut segments = self.current.segments.clone();
            segments.drain(run);
            self.publish(manifest, segments)?;
            retire(&self.dir, &retired);
            return Ok(Some(Merged {
                inputs: retired.len(),
                read,
                written: 0,
                purged,
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
            .map(|segment| {
                Segment::open(Paced {
                    file: segment.source(),
                    pace: &merge_pace,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let number = manifest.next_number;
        let scratch = self.dir.join(format!("tmp-{number}.postings"));
        let result = self.write_segment(&mut manifest, docs, |file| {
            let postings = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&scratch)?;
            let writer = Writer::spilling(first, last, file.try_clone()?, postings)?;
            let result =
                merge::stream(&inputs, catalog.live, writer, &merge_pace, budget.cancelled);
            let _ = fs::remove_file(&scratch);
            Ok(result?.total())
        });
        let (entry, segment) = result?;

        if (budget.cancelled)() {
            return Err(Error::Cancelled);
        }
        let retired: Vec<_> = manifest.segments.splice(run.clone(), [entry]).collect();
        let mut segments = self.current.segments.clone();
        segments.splice(run, [segment]);
        self.publish(manifest, segments)?;
        retire(&self.dir, &retired);
        Ok(Some(Merged {
            inputs: retired.len(),
            read,
            written: entry.bytes,
            purged,
            segment: Some(entry),
        }))
    }

    /// Writes one segment through `fill`, which writes the whole file and
    /// returns its length, then syncs it, renames it into place and syncs
    /// the directory. Takes the next segment number from `manifest`.
    fn write_segment(
        &self,
        manifest: &mut Manifest,
        docs: u32,
        fill: impl FnOnce(&File) -> Result<u64, Error>,
    ) -> Result<(SegmentEntry, Arc<Segment<File>>), Error> {
        let number = manifest.next_number;
        manifest.next_number += 1;
        let temp = self.dir.join(format!("tmp-{number}.seg"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temp)?;
        let bytes = fill(&file)?;
        file.sync_all()?;
        let segment = Segment::open(file)?;
        let info = segment.info();
        let entry = SegmentEntry {
            number,
            first: info.first,
            last: info.last,
            docs,
            terms: info.terms,
            pairs: info.pairs,
            bytes,
            digest: segment.digest(),
        };
        fs::rename(&temp, self.dir.join(entry.file_name()))?;
        hit(Point::SegmentWritten)?;
        File::open(&self.dir)?.sync_all()?;
        Ok((entry, Arc::new(segment)))
    }

    fn publish(
        &mut self,
        mut manifest: Manifest,
        segments: Vec<Arc<Segment<File>>>,
    ) -> Result<(), Error> {
        manifest.sequence += 1;
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
            pending_merge,
        });
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
        )
    }
}

/// Whether `manifest` belongs to `catalog`'s view: same incarnation and
/// tokenizer, and no DocId the view has not published.
fn fits(manifest: &Manifest, catalog: &CatalogView) -> bool {
    manifest.incarnation == catalog.incarnation
        && manifest.tokenizer_version == TOKENIZER_VERSION
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

fn open_segments(dir: &Path, manifest: &Manifest) -> Result<Vec<Arc<Segment<File>>>, Error> {
    manifest
        .segments
        .iter()
        .map(|entry| {
            let file = File::open(dir.join(entry.file_name()))?;
            let bytes = file.metadata()?.len();
            let segment = Segment::open(file)?;
            if !entry.matches(
                &segment.info(),
                &segment.digest(),
                bytes,
                manifest.tokenizer_version,
            ) {
                return Err(Error::Segment(ReadError::Head));
            }
            Ok(Arc::new(segment))
        })
        .collect()
}

/// Unlinks merged-away segments. Open views keep their descriptors (D32);
/// a crash before this leaves orphans the next open removes.
fn retire(dir: &Path, retired: &[SegmentEntry]) {
    for old in retired {
        let _ = fs::remove_file(dir.join(old.file_name()));
    }
}

/// Removes segment and temporary files `manifest` does not name. Other
/// files are left alone.
fn remove_unnamed(dir: &Path, manifest: &Manifest) -> io::Result<()> {
    let named: Vec<String> = manifest
        .segments
        .iter()
        .map(SegmentEntry::file_name)
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

/// A segment file with the host's pacer in front of every transfer.
struct Paced<'a> {
    file: &'a File,
    pace: &'a dyn Fn(usize) -> io::Result<()>,
}

impl ReadAt for Paced<'_> {
    fn size(&self) -> io::Result<u64> {
        self.file.size()
    }

    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        for (i, chunk) in buf.chunks_mut(64 << 10).enumerate() {
            (self.pace)(chunk.len())?;
            ReadAt::read_exact_at(self.file, chunk, offset + (i * (64 << 10)) as u64)?;
        }
        Ok(())
    }
}

impl Write for Paced<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let chunk = &buf[..buf.len().min(64 << 10)];
        (self.pace)(chunk.len())?;
        self.file.write(chunk)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Places a test can stop an operation, as a crash would.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Point {
    /// A segment is synced and renamed into place; no manifest names it.
    SegmentWritten,
    /// The new manifest is renamed into place, before the directory sync.
    ManifestRenamed,
    /// A merge has written part of its output.
    MergeMidway,
}

pub(crate) fn hit(point: Point) -> io::Result<()> {
    #[cfg(test)]
    if STOP.get() == Some(point) {
        STOP.set(None);
        return Err(io::Error::other(format!("injected stop at {point:?}")));
    }
    #[cfg(not(test))]
    let _ = point;
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// The next [`hit`] of this point fails, once.
    pub(crate) static STOP: std::cell::Cell<Option<Point>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
mod tests;

fn timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
