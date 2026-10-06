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
use std::sync::Arc;

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
    /// Called with each index-file transfer's size before it happens: the
    /// host's shared byte pacer for background work, a no-op for an explicit
    /// `ferret index`. Document reads are paced by the host's callback.
    pub pace: &'a dyn Fn(usize),
}

impl Budget<'static> {
    /// No limit, no pacing: an explicit `ferret index`.
    pub fn unbounded() -> Self {
        Self {
            bytes: u64::MAX,
            buffer: BUFFER,
            pace: &|_| {},
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
    pub segment: SegmentEntry,
}

/// Why an index operation failed. After any error the writer is poisoned:
/// reopen it, which removes whatever the failed operation left behind.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// A segment the manifest names failed to read.
    Segment(ReadError),
    /// A previous operation failed; reopen the writer.
    Poisoned,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "index I/O failed: {error}"),
            Self::Segment(error) => error.fmt(f),
            Self::Poisoned => f.write_str("an earlier index write failed; reopen the index"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Segment(error) => Some(error),
            Self::Poisoned => None,
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
}

impl std::fmt::Debug for View {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("View")
            .field("manifest", &self.manifest)
            .finish_non_exhaustive()
    }
}

impl View {
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
                Ok(segments) => Some(View { manifest, segments }),
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
        read: &mut dyn FnMut(u32, &mut Vec<u8>) -> Result<(), Fault>,
    ) -> Result<Followed, Error> {
        self.guard(|writer| writer.follow_inner(catalog, budget, read))
    }

    /// One merge step if the policy wants one (adjacent tiers, or a segment
    /// past the dead-fraction trigger) and it fits `budget.bytes`.
    pub fn merge_if_needed(
        &mut self,
        catalog: &CatalogView,
        budget: &Budget,
    ) -> Result<Option<Merged>, Error> {
        self.guard(|writer| {
            writer.reconcile(catalog)?;
            let unreadable = &writer.current.manifest.unreadable;
            let alive = |s: &SegmentEntry| alive(s, catalog.live, unreadable);
            let Some(run) = merge::choose(&writer.current.manifest.segments, &alive) else {
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
        let result = run(self);
        self.poisoned = result.is_err();
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
        read: &mut dyn FnMut(u32, &mut Vec<u8>) -> Result<(), Fault>,
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
        let mut manifest = self.current.manifest.clone();
        manifest.unreadable.retain(|&doc| catalog.live.contains(doc));
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
        let inputs = self.current.segments[run.clone()]
            .iter()
            .map(|segment| {
                Segment::open(Paced {
                    file: segment.source(),
                    pace: budget.pace,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let number = manifest.next_number;
        let scratch = self.dir.join(format!("tmp-{number}.postings"));
        let (entry, segment) = self.write_segment(&mut manifest, docs, |file| {
            let postings = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&scratch)?;
            let writer = Writer::spilling(first, last, file.try_clone()?, postings)?;
            let result = merge::stream(&inputs, catalog.live, writer, budget.pace);
            let _ = fs::remove_file(&scratch);
            Ok(result?.total())
        })?;

        let retired: Vec<_> = manifest.segments.splice(run.clone(), [entry]).collect();
        let mut segments = self.current.segments.clone();
        segments.splice(run, [segment]);
        self.publish(manifest, segments)?;
        for old in &retired {
            // A crash before this leaves orphans the next open removes.
            let _ = fs::remove_file(self.dir.join(old.file_name()));
        }
        Ok(Some(Merged {
            inputs: retired.len(),
            read,
            written: entry.bytes,
            purged,
            segment: entry,
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
        self.current = Arc::new(View { manifest, segments });
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

/// Removes segment and temporary files `manifest` does not name. Other
/// files are left alone.
fn remove_unnamed(dir: &Path, manifest: &Manifest) -> io::Result<()> {
    let named: Vec<String> = manifest.segments.iter().map(SegmentEntry::file_name).collect();
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
    pace: &'a dyn Fn(usize),
}

impl ReadAt for Paced<'_> {
    fn size(&self) -> io::Result<u64> {
        self.file.size()
    }

    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        (self.pace)(buf.len());
        ReadAt::read_exact_at(self.file, buf, offset)
    }
}

impl Write for Paced<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (self.pace)(buf.len());
        self.file.write(buf)
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
