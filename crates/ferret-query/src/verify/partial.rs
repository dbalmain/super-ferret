use std::cell::Cell;
use std::time::{Duration, UNIX_EPOCH};

use super::{Settled, Verifier};
use crate::{Query, ReadRequest, ReadVersion};

#[test]
fn a_document_cancelled_between_atoms_counts_the_finished_atom() {
    let query = Query::parse(
        // `(?s)` lets `.` match a newline, so the document is read
        // whole: the path that checks between atoms.
        "grep:needle grep:(?s)oth.r",
        UNIX_EPOCH + Duration::from_secs(1),
    )
    .unwrap();
    let mut read = |_, request: ReadRequest<'_>| {
        if let ReadRequest::Bytes(out, _) = request {
            out.clear();
            out.extend_from_slice(b"a needle here");
        }
        Some(ReadVersion::Catalogued)
    };
    let mut verifier = Verifier::new(&query, &mut read, 1);
    // Checked before the read, before the first atom, then after it.
    let checks = Cell::new(0);
    let stopped = || {
        checks.set(checks.get() + 1);
        checks.get() > 2
    };
    let settled = verifier.here(0, ferret_catalog::NameId(0), &stopped);
    assert!(matches!(settled, Settled::Stopped));
    let tally = &verifier.tally;
    assert_eq!(tally.verified, 1);
    assert_eq!(tally.matching.regex_documents, 1);
    assert_eq!(tally.matching.regex_bytes, 13);
    assert_eq!(tally.matching.regex_rejected, 0);
    assert!(verifier.exact.is_empty());
}

/// Hands out `chunks` one per read; `stop` is set as the read after
/// the last of `before` chunks is made.
struct Chunks<'a> {
    chunks: &'a [&'a [u8]],
    before: usize,
    stop: &'a Cell<bool>,
}

impl std::io::Read for Chunks<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.before == 0 {
            self.stop.set(true);
        }
        self.before = self.before.saturating_sub(1);
        let Some((first, rest)) = self.chunks.split_first() else {
            return Ok(0);
        };
        buf[..first.len()].copy_from_slice(first);
        self.chunks = rest;
        Ok(first.len())
    }
}

/// A streamed document whose run is stopped once `before` reads have
/// been made, through the calling thread's [`Verifier::here`].
fn stream_stopped(chunks: &[&[u8]], before: usize) -> (Settled, super::Tally) {
    let query = Query::parse(
        "grep:needle grep:other",
        UNIX_EPOCH + Duration::from_secs(1),
    )
    .unwrap();
    assert!(super::settle::streams(&query.atoms));
    let stop = Cell::new(false);
    let size = chunks.iter().map(|c| c.len() as u64).sum();
    let mut read = |_, request: ReadRequest<'_>| {
        let ReadRequest::Stream(each, _) = request else {
            panic!("line-local reads stream");
        };
        let mut source = Chunks {
            chunks,
            before,
            stop: &stop,
        };
        each(&mut source, size).ok()?;
        Some(ReadVersion::Catalogued)
    };
    let mut verifier = Verifier::new(&query, &mut read, 1);
    let settled = verifier.here(0, ferret_catalog::NameId(0), &|| stop.get());
    (settled, std::mem::take(&mut verifier.tally))
}

/// Stopped as the last chunk is read: the document is not accepted, and
/// `needle`, which matched in the first, counts; `other` had not
/// finished. Not a change.
#[test]
fn a_stream_stopped_on_its_final_chunk_is_not_accepted() {
    let (settled, tally) = stream_stopped(&[b"a needle\n", b"tail\n"], 1);
    assert!(matches!(settled, Settled::Stopped));
    assert_eq!(tally.verified, 1);
    assert_eq!(tally.changed, 0);
    assert_eq!(tally.matching.regex_documents, 1);
    assert_eq!(tally.matching.regex_bytes, 14);
    assert_eq!(tally.matching.regex_rejected, 0);
}

/// Stopped after the end of the document was read and searched: both
/// atoms finished, so both count, and the document is not accepted.
/// The stop is the seventh check: one before the read, one per read
/// (two: the text, then its end), one per unanswered atom's search
/// (three), then the one before the answers are accepted.
#[test]
fn a_stream_stopped_after_its_end_counts_every_atom() {
    let query = Query::parse(
        "grep:needle grep:other",
        UNIX_EPOCH + Duration::from_secs(1),
    )
    .unwrap();
    let mut read = |_, request: ReadRequest<'_>| {
        let ReadRequest::Stream(each, _) = request else {
            panic!("line-local reads stream");
        };
        each(&mut &b"a needle\ntail\n"[..], 14).ok()?;
        Some(ReadVersion::Catalogued)
    };
    let mut verifier = Verifier::new(&query, &mut read, 1);
    let checks = Cell::new(0);
    let stopped = || {
        checks.set(checks.get() + 1);
        checks.get() >= 7
    };
    let settled = verifier.here(0, ferret_catalog::NameId(0), &stopped);
    assert!(matches!(settled, Settled::Stopped));
    let tally = &verifier.tally;
    assert_eq!(tally.changed, 0);
    assert_eq!(tally.matching.regex_documents, 2);
    assert_eq!(tally.matching.regex_bytes, 28);
    assert_eq!(tally.matching.regex_rejected, 1);
}

/// Stopped while the second chunk is wanted, after `needle` finished in
/// the first: the finished atom counts, and cancellation is no change.
#[test]
fn a_stream_stopped_after_one_atom_counts_it() {
    let (settled, tally) = stream_stopped(&[b"a needle\n", b"more\n", b"tail\n"], 1);
    assert!(matches!(settled, Settled::Stopped));
    assert_eq!(tally.verified, 1);
    assert_eq!(tally.changed, 0);
    assert_eq!(tally.matching.regex_documents, 1);
    assert_eq!(tally.matching.regex_bytes, 19);
}

/// Sol round 2: an atom that finished without a match counts when a
/// stop ends the next. The document is empty, so its end is the first
/// run: `needle` is searched and rejected, then the stop comes before
/// `other`, at the fourth check (before the read, before the first
/// read of the source, before each atom). The whole-document path
/// counts the same rejection.
#[test]
fn a_stream_stopped_after_a_rejection_at_its_end_counts_it() {
    let query = Query::parse(
        "grep:needle grep:other",
        UNIX_EPOCH + Duration::from_secs(1),
    )
    .unwrap();
    let mut read = |_, request: ReadRequest<'_>| {
        let ReadRequest::Stream(each, _) = request else {
            panic!("line-local reads stream");
        };
        each(&mut &b""[..], 0).ok()?;
        Some(ReadVersion::Catalogued)
    };
    let mut verifier = Verifier::new(&query, &mut read, 1);
    let checks = Cell::new(0);
    let stopped = || {
        checks.set(checks.get() + 1);
        checks.get() >= 4
    };
    let settled = verifier.here(0, ferret_catalog::NameId(0), &stopped);
    assert!(matches!(settled, Settled::Stopped));
    let tally = &verifier.tally;
    assert_eq!(tally.changed, 0);
    assert_eq!(tally.matching.regex_documents, 1);
    assert_eq!(tally.matching.regex_rejected, 1);
}

/// Sol round 2: the stream finishes, then the stop comes in the host's
/// own checks after it (its bracketing `fstat`, say). Whether the host
/// still returns a version or gives up, the document is stopped, not
/// accepted and not a change, and both finished atoms count.
#[test]
fn a_stream_stopped_after_it_finished_counts_its_work() {
    for host_returns in [Some(ReadVersion::Catalogued), None] {
        let query = Query::parse(
            "grep:needle grep:other",
            UNIX_EPOCH + Duration::from_secs(1),
        )
        .unwrap();
        let stop = Cell::new(false);
        let mut read = |_, request: ReadRequest<'_>| {
            let ReadRequest::Stream(each, _) = request else {
                panic!("line-local reads stream");
            };
            each(&mut &b"a needle\nother\n"[..], 15).ok()?;
            stop.set(true);
            host_returns
        };
        let mut verifier = Verifier::new(&query, &mut read, 1);
        let settled = verifier.here(0, ferret_catalog::NameId(0), &|| stop.get());
        assert!(matches!(settled, Settled::Stopped), "{host_returns:?}");
        let tally = &verifier.tally;
        assert_eq!(tally.changed, 0, "{host_returns:?}");
        assert_eq!(tally.matching.regex_documents, 2, "{host_returns:?}");
        assert_eq!(tally.matching.regex_bytes, 30, "{host_returns:?}");
        assert!(verifier.exact.is_empty());
    }
}

/// Sol round 2: a whole-document read that fails as the run is
/// cancelled is stopped, not a change.
#[test]
fn a_whole_read_failing_as_the_run_stops_is_not_a_change() {
    let query = Query::parse("grep:(?s)need.e", UNIX_EPOCH + Duration::from_secs(1)).unwrap();
    assert!(!super::settle::streams(&query.atoms));
    let stop = Cell::new(false);
    let mut read = |_, request: ReadRequest<'_>| {
        let ReadRequest::Bytes(..) = request else {
            panic!("a `(?s)` pattern reads whole");
        };
        stop.set(true);
        None
    };
    let mut verifier = Verifier::new(&query, &mut read, 1);
    let settled = verifier.here(0, ferret_catalog::NameId(0), &|| stop.get());
    assert!(matches!(settled, Settled::Stopped));
    assert_eq!(verifier.tally.verified, 1);
    assert_eq!(verifier.tally.changed, 0);
}
