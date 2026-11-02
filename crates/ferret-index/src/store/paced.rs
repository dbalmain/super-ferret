//! Segment files behind the host's pacer: [`Paced`] for a segment being
//! written, [`PacedInput`] for a merge input being read.

use std::io::Write;

use super::*;
use crate::segment::{CHUNK, ReadAt};

/// A segment file being written, with the host's pacer in front of every
/// transfer.
pub(super) struct Paced<'a> {
    pub(super) file: &'a File,
    pub(super) pace: &'a dyn Fn(usize) -> io::Result<()>,
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

/// Distinct pages per merge input that count as recently read, so a repeat
/// read of them is not charged again.
///
/// A merge reads each input through three interleaved cursors (dictionary
/// blocks, postings, fingerprints), and a checked chunk read spans two file
/// pages because sections start at the end of the head. In a trace of the
/// merges on a 120k-file index, more than 99.9% of repeat touches of a page
/// came with at most 7 other pages touched in between. A window of 8 charged
/// 1.034× the distinct pages read, as did 16; 3 charged 6.6×, and charging
/// every read 47×. 16 is twice the observed reach, and costs 128 bytes per
/// input whatever its size.
pub(super) const RECENT_PAGES: usize = 16;

/// A merge input with the host's pacer in front of its reads. A read charges
/// the whole [`CHUNK`]-byte pages it touches, except those among the input's
/// [`RECENT_PAGES`] most recently touched: checked reads widen to whole
/// chunks, so the merge's cursors re-read the same few pages many times in
/// quick succession, and charging each re-read made pacing tens of times the
/// real I/O.
///
/// The guarantee is that a page leaving the window is charged again on its
/// next read, whether the page cache kept it or not. A re-read inside the
/// window is free even if the kernel evicted the page in between, so that
/// real I/O goes unpaced; it needs pressure severe enough to evict one of the
/// last 64 KiB read from that input.
pub(super) struct PacedInput<'a> {
    file: &'a File,
    pace: &'a (dyn Fn(usize) -> io::Result<()> + Sync),
    size: u64,
    /// The most recently touched pages, newest first; [`u64::MAX`] is empty.
    recent: RefCell<[u64; RECENT_PAGES]>,
}

impl<'a> PacedInput<'a> {
    pub(super) fn new(
        file: &'a File,
        pace: &'a (dyn Fn(usize) -> io::Result<()> + Sync),
    ) -> io::Result<Self> {
        Ok(Self {
            file,
            pace,
            size: file.size()?,
            recent: RefCell::new([u64::MAX; RECENT_PAGES]),
        })
    }

    pub(super) fn open(
        file: &'a File,
        pace: &'a (dyn Fn(usize) -> io::Result<()> + Sync),
    ) -> Result<Segment<Self>, ReadError> {
        Segment::open(Self::new(file, pace)?)
    }

    /// Bytes of the pages `[start, end)` touches that are not among the
    /// recently touched, all of which become the most recent.
    fn uncharged(&self, start: u64, end: u64) -> u64 {
        let page = CHUNK as u64;
        let mut recent = self.recent.borrow_mut();
        let mut bytes = 0;
        // Past the end the read itself fails.
        for p in start / page..end.min(self.size).div_ceil(page) {
            match recent.iter().position(|&r| r == p) {
                Some(i) => recent[..=i].rotate_right(1),
                None => {
                    recent.rotate_right(1);
                    recent[0] = p;
                    bytes += self.size.min((p + 1) * page) - p * page;
                }
            }
        }
        bytes
    }
}

impl ReadAt for PacedInput<'_> {
    fn size(&self) -> io::Result<u64> {
        Ok(self.size)
    }

    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        for (i, piece) in buf.chunks_mut(64 << 10).enumerate() {
            let at = offset + (i * (64 << 10)) as u64;
            #[cfg(test)]
            input_trace::record(self.file, self.size, at, piece.len());
            #[cfg(test)]
            let _charging = input_trace::Charging::start();
            // Called even when nothing is new: the pacer is also the
            // merge's cancellation check.
            (self.pace)(self.uncharged(at, at + piece.len() as u64) as usize)?;
            ReadAt::read_exact_at(self.file, piece, at)?;
        }
        Ok(())
    }
}

/// What merge inputs read, and which pacer calls charge them, for tests
/// that check the charges against the reads.
#[cfg(test)]
pub(crate) mod input_trace {
    use std::cell::{Cell, RefCell};
    use std::fs::File;
    use std::sync::{Arc, Mutex};

    /// One input read: the file (by address and size) and the span.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct Read {
        pub file: usize,
        pub size: u64,
        pub offset: u64,
        pub len: usize,
    }

    thread_local! {
        static READS: RefCell<Option<Arc<Mutex<Vec<Read>>>>> = const { RefCell::new(None) };
        static CHARGING: Cell<bool> = const { Cell::new(false) };
    }

    pub(super) fn record(file: &File, size: u64, offset: u64, len: usize) {
        READS.with_borrow(|reads| {
            if let Some(reads) = reads {
                let file = std::ptr::from_ref(file) as usize;
                reads.lock().unwrap().push(Read {
                    file,
                    size,
                    offset,
                    len,
                });
            }
        });
    }

    /// Marks the pacer calls made while it lives as input charges.
    pub(super) struct Charging;

    impl Charging {
        pub(super) fn start() -> Self {
            CHARGING.set(true);
            Self
        }
    }

    impl Drop for Charging {
        fn drop(&mut self) {
            CHARGING.set(false);
        }
    }

    /// Whether the pacer is being called for a merge input's read.
    pub(crate) fn charging() -> bool {
        CHARGING.get()
    }

    /// Runs `f`, returning its result and every input read on this thread
    /// or a thread that inherited its trace.
    pub(crate) fn during<T>(f: impl FnOnce() -> T) -> (T, Vec<Read>) {
        let reads = Arc::new(Mutex::new(Vec::new()));
        READS.set(Some(reads.clone()));
        let result = f();
        READS.set(None);
        (result, std::mem::take(&mut *reads.lock().unwrap()))
    }

    /// This thread's trace, to set on a thread that reads for it.
    pub(crate) fn inherit() -> impl FnOnce() + Send {
        let reads = READS.with_borrow(Clone::clone);
        move || READS.set(reads)
    }
}
