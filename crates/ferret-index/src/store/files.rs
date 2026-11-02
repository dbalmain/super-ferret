//! The index directory's segment files: writing a pair's outputs to their
//! temporaries, opening a manifest's pairs, and removing the files no
//! manifest names.

use super::*;
use crate::regular;

/// Writes and syncs one output of a pair to its temporary name.
pub(super) fn write_output(
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
pub(super) fn open_pairs(
    dir: &Path,
    manifest: &Manifest,
    threads: usize,
) -> Result<PairedSegments, Error> {
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
pub(super) fn available_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// Unlinks merged-away segments. Open views keep their descriptors (D32);
/// a crash before this leaves orphans the next open removes.
pub(super) fn retire(dir: &Path, retired: &[SegmentEntry]) {
    for old in retired {
        let _ = fs::remove_file(dir.join(old.file_name()));
        let _ = fs::remove_file(dir.join(old.trigram_file_name()));
    }
}

/// Removes segment and temporary files `manifest` does not name. Other
/// files are left alone.
pub(super) fn remove_unnamed(dir: &Path, manifest: &Manifest) -> io::Result<()> {
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

#[cfg(test)]
thread_local! {
    /// The segment-open thread whose spawn fails.
    pub(super) static FAIL_OPEN_SPAWN_AT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    /// Segment-open threads started on this thread.
    pub(super) static OPEN_SPAWNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
