//! Settling one document, sequentially: reading it through a [`Hand`],
//! verifying it whole or streamed, and the [`Settled`] answer.

use super::*;

/// One document read and matched.
pub(super) enum Outcome {
    /// The reader failed: vanished, unreadable, refused or cancelled.
    Unread(Duration),
    /// Stopped before the read, or between atoms: the work the atoms that
    /// finished did, which the sequential order counted.
    Stopped(Matched),
    Read {
        version: ReadVersion,
        answers: Vec<Option<usize>>,
        work: Matched,
    },
}

/// The time and matcher work spent on one document.
#[derive(Default)]
pub(super) struct Matched {
    time: Duration,
    regex: Duration,
    matching: MatchStats,
}

pub(super) enum Settled {
    Unread,
    Stopped,
    /// The run's cached answers for the document.
    Cached,
    /// Answers for this path alone: its current version differs.
    Fresh(Vec<Option<usize>>),
}

/// A thread's reading tools: its reader, matcher and buffer.
pub(super) struct Hand<'r, 'a> {
    pub(super) read: &'r mut Reader<'a>,
    matcher: TextMatcher,
    /// A whole document, for atoms that need one.
    pub(super) bytes: Vec<u8>,
    /// A run of lines at a time, for line-local patterns ([`stream`]).
    pub(super) lines: Vec<u8>,
    /// This thread's own copy of each grep atom's search, by atom: a regex
    /// shared between threads hands each a match cache from a pool, which
    /// many threads at once contend on.
    regexes: Vec<Option<Search<'static>>>,
}

impl<'r, 'a> Hand<'r, 'a> {
    pub(super) fn new(read: &'r mut Reader<'a>, atoms: &[ContentAtom]) -> Self {
        let regexes = atoms
            .iter()
            .map(|atom| match atom {
                ContentAtom::Grep(grep) => Some(grep.search()),
                ContentAtom::Text(_) => None,
            })
            .collect();
        Self {
            read,
            matcher: TextMatcher::new(),
            bytes: Vec::new(),
            lines: Vec::new(),
            regexes,
        }
    }

    /// Reads `name`'s document and runs every atom over it, unless `stopped`
    /// says to stop first; the reader is given `stopped` too.
    pub(super) fn verify(
        &mut self,
        atoms: &[ContentAtom],
        name: NameId,
        stopped: &dyn Fn() -> bool,
    ) -> Outcome {
        if streams(atoms) {
            return stream(
                &self.regexes,
                &mut *self.read,
                &mut self.lines,
                name,
                stopped,
            );
        }
        verify(
            atoms,
            &self.regexes,
            &mut *self.read,
            &mut self.matcher,
            &mut self.bytes,
            name,
            stopped,
        )
    }
}

/// The calling thread's side: the cache of catalogued answers, the counts,
/// and its reading tools, for later names of a document.
pub(super) struct Verifier<'q, 'r, 'a> {
    atoms: &'q [ContentAtom],
    pub(super) hand: Hand<'r, 'a>,
    /// Only content facts established by a bracketed catalog-version read.
    /// Path failures never enter this map; every path has its own stat.
    pub(super) exact: HashMap<u32, Vec<Option<usize>>>,
    pub(super) tally: Tally,
}

impl<'q, 'r, 'a> Verifier<'q, 'r, 'a> {
    pub(super) fn new(query: &'q Query, read: &'r mut Reader<'a>, threads: usize) -> Self {
        Self {
            atoms: &query.atoms,
            hand: Hand::new(read, &query.atoms),
            exact: HashMap::new(),
            tally: Tally {
                threads,
                ..Tally::default()
            },
        }
    }

    /// Settles `name`'s row on this thread: reuse the document's cached
    /// answers if a fresh stat says this path is still the catalogued
    /// version, else read it.
    pub(super) fn here(&mut self, doc: u32, name: NameId, stopped: &dyn Fn() -> bool) -> Settled {
        if self.exact.contains_key(&doc) {
            match (self.hand.read)(name, ReadRequest::Stat) {
                Some(ReadVersion::Catalogued) => return Settled::Cached,
                Some(ReadVersion::Current) => {}
                None => {
                    self.tally.changed += 1;
                    return Settled::Unread;
                }
            }
        }
        let outcome = self.hand.verify(self.atoms, name, stopped);
        self.settle(doc, outcome, true)
    }

    /// Counts a read, wherever it ran, and caches catalogued answers if
    /// `keep`: only a document with another name in the run is asked again.
    pub(super) fn settle(&mut self, doc: u32, outcome: Outcome, keep: bool) -> Settled {
        let tally = &mut self.tally;
        tally.verified += 1;
        let count = |tally: &mut Tally, work: Matched| {
            tally.time += work.time;
            tally.regex += work.regex;
            add(&mut tally.matching, &work.matching);
        };
        match outcome {
            Outcome::Unread(time) => {
                tally.time += time;
                tally.changed += 1;
                Settled::Unread
            }
            Outcome::Stopped(work) => {
                count(tally, work);
                Settled::Stopped
            }
            Outcome::Read {
                version,
                answers,
                work,
            } => {
                count(tally, work);
                match version {
                    ReadVersion::Catalogued => {
                        if keep {
                            self.exact.entry(doc).or_insert(answers);
                        }
                        Settled::Cached
                    }
                    ReadVersion::Current => {
                        tally.changed += 1;
                        Settled::Fresh(answers)
                    }
                }
            }
        }
    }
}

/// Reads `name`'s document into `bytes` and runs every atom over it,
/// unless `stopped` says to stop first; the reader is given `stopped` too.
fn verify(
    atoms: &[ContentAtom],
    regexes: &[Option<Search<'_>>],
    read: &mut Reader<'_>,
    matcher: &mut TextMatcher,
    bytes: &mut Vec<u8>,
    name: NameId,
    stopped: &dyn Fn() -> bool,
) -> Outcome {
    let start = Instant::now();
    if stopped() {
        return Outcome::Stopped(Matched::default());
    }
    let Some(version) = read(name, ReadRequest::Bytes(bytes, stopped)) else {
        return unread(start, stopped);
    };
    let before = matcher.stats();
    let mut work = Matched::default();
    let mut answers = Vec::with_capacity(atoms.len());
    let mut halted = false;
    for (atom, regex) in atoms.iter().zip(regexes) {
        if stopped() {
            halted = true;
            break;
        }
        let engine_start = Instant::now();
        let answer = match regex {
            Some(search) => search.find(bytes),
            None => atom.find(matcher, bytes),
        };
        if matches!(atom, ContentAtom::Grep(_)) {
            work.regex += engine_start.elapsed();
            work.matching.regex_documents += 1;
            work.matching.regex_bytes += bytes.len() as u64;
            work.matching.regex_rejected += u64::from(answer.is_none());
        }
        if stopped() {
            halted = true;
            break;
        }
        answers.push(answer);
    }
    let after = matcher.stats();
    work.matching.documents = after.documents - before.documents;
    work.matching.rejected = after.rejected - before.rejected;
    work.matching.whole = after.whole - before.whole;
    work.matching.tokenized = after.tokenized - before.tokenized;
    work.time = start.elapsed();
    if halted {
        return Outcome::Stopped(work);
    }
    Outcome::Read {
        version,
        answers,
        work,
    }
}

/// A read that failed: stopped, if the run was, which is not a change.
fn unread(start: Instant, stopped: &dyn Fn() -> bool) -> Outcome {
    match stopped() {
        true => Outcome::Stopped(Matched {
            time: start.elapsed(),
            ..Matched::default()
        }),
        false => Outcome::Unread(start.elapsed()),
    }
}

/// Whether every atom is a pattern [`stream`] can search a run of lines at
/// a time: then documents are streamed, not read whole.
pub(super) fn streams(atoms: &[ContentAtom]) -> bool {
    !atoms.is_empty()
        && atoms
            .iter()
            .all(|atom| matches!(atom, ContentAtom::Grep(grep) if grep.is_line_local()))
}

/// [`verify`] for line-local patterns ([`ferret_verify::Grep::is_line_local`]):
/// the document streams through `buffer`, and each read is searched up to
/// its last `\n`, not including it, with the partial line after it carried
/// to the next. A line longer than the buffer grows it, by doubling, up to
/// the rest of the document's declared length: a document's buffer is never
/// larger than 64 KiB or its length. The run stops early once every atom
/// has matched.
///
/// The answers are the whole-document search's. The first match of a
/// line-local pattern in a document is the first match in the first run of
/// whole lines, cut short of a `\n`, that has one, and its offset is the
/// run's offset plus the match's. The counts are too: each atom counts the
/// document's whole size, matched or not.
///
/// `stopped` is asked before each read and each atom's search, before the
/// answers are accepted, and again once the host's read returns, as
/// [`verify`] asks it between atoms. A stopped document, whatever the host
/// returned, counts the atoms that had finished: those that matched, and
/// those the last run was searched for without a match.
fn stream(
    regexes: &[Option<Search<'_>>],
    read: &mut Reader<'_>,
    buffer: &mut Vec<u8>,
    name: NameId,
    stopped: &dyn Fn() -> bool,
) -> Outcome {
    let start = Instant::now();
    if stopped() {
        return Outcome::Stopped(Matched::default());
    }
    // Sized before the read, in case the host never calls `each`.
    let mut progress = Progress::new(regexes.len());
    // Set when `stopped` ends the read.
    let mut halted = false;
    let mut each = |source: &mut dyn Read, len: u64| {
        progress.restart(len);
        crate::scan::runs(
            source,
            len,
            buffer,
            stopped,
            None,
            |run, base, terminated| {
                progress
                    .search(regexes, run, base, terminated, stopped)
                    .ok_or_else(|| {
                        halted = true;
                        io::Error::other("stopped")
                    })
            },
        )
    };
    let version = read(name, ReadRequest::Stream(&mut each, stopped));
    // A stop after `each` returned, in the host's own checks, is a stop
    // too: the host may have succeeded or given up because of it.
    if halted || stopped() {
        return Outcome::Stopped(progress.finished(start));
    }
    match version {
        None => Outcome::Unread(start.elapsed()),
        Some(version) => Outcome::Read {
            version,
            work: progress.finished(start),
            answers: progress.answers,
        },
    }
}

/// One read's search, run by run: each atom's first match.
struct Progress {
    answers: Vec<Option<usize>>,
    /// Which atoms' answers are final: matched, or searched to the end.
    done: Vec<bool>,
    /// Atoms not yet matched.
    open: usize,
    regex: Duration,
    size: u64,
}

impl Progress {
    fn new(atoms: usize) -> Self {
        Self {
            answers: vec![None; atoms],
            done: vec![false; atoms],
            open: atoms,
            regex: Duration::ZERO,
            size: 0,
        }
    }

    /// Starts over for a read of `len` bytes.
    fn restart(&mut self, len: u64) {
        let atoms = self.answers.len();
        self.size = len;
        self.regex = Duration::ZERO;
        self.answers.clear();
        self.answers.resize(atoms, None);
        self.done.clear();
        self.done.resize(atoms, false);
        self.open = atoms;
    }

    /// Searches `run`, at `base`, for every atom still open; `Break` once
    /// none is. `None` when `stopped` ends the read.
    fn search(
        &mut self,
        regexes: &[Option<Search<'_>>],
        run: &[u8],
        base: usize,
        terminated: bool,
        stopped: &dyn Fn() -> bool,
    ) -> Option<ControlFlow<()>> {
        let engine = Instant::now();
        let atoms = regexes.iter().zip(&mut self.answers).zip(&mut self.done);
        for ((search, answer), done) in atoms {
            let (false, Some(search)) = (*done, search) else {
                continue;
            };
            if stopped() {
                self.regex += engine.elapsed();
                return None;
            }
            if let Some(found) = search.find(run) {
                *answer = Some(base + found);
                self.open -= 1;
            }
            *done = answer.is_some() || !terminated;
        }
        self.regex += engine.elapsed();
        if (self.open == 0 || !terminated) && stopped() {
            return None;
        }
        Some(if self.open == 0 {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        })
    }

    /// The work of the atoms that finished, matched or not: every one after
    /// a full read.
    fn finished(&self, start: Instant) -> Matched {
        let settled = || {
            self.answers
                .iter()
                .zip(&self.done)
                .filter(|(_, done)| **done)
        };
        let counted = settled().count() as u64;
        let mut work = Matched {
            regex: self.regex,
            ..Matched::default()
        };
        work.matching.regex_documents = counted;
        work.matching.regex_bytes = counted * self.size;
        work.matching.regex_rejected = settled().filter(|(a, _)| a.is_none()).count() as u64;
        work.time = start.elapsed();
        work
    }
}

fn add(sum: &mut MatchStats, more: &MatchStats) {
    sum.regex_documents += more.regex_documents;
    sum.regex_bytes += more.regex_bytes;
    sum.regex_rejected += more.regex_rejected;
    sum.documents += more.documents;
    sum.rejected += more.rejected;
    sum.whole += more.whole;
    sum.tokenized += more.tokenized;
}

/// [`stream`] against [`verify`], the whole-document search, on the same
/// bytes: every answer and count agree for line-local patterns, with the
/// bytes arriving a few at a time, so runs end at every kind of place, and
/// with a line longer than the buffer.
#[cfg(test)]
mod streamed;
