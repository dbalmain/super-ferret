//! A follow pass's per-document work, off the writer's thread: reading a
//! document and finding its distinct trigram keys and terms. The writer
//! then only appends each document's postings, in DocId order, so a pass
//! with several readers writes the same segment as a pass with one.

use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex, mpsc};

use ferret_text::{Scratch, cap, tokenize_terms};

use crate::segment::{KeySet, TermTable, term_hash};
use crate::store::{Fault, Reader};

/// Documents a reader thread may run ahead of the writer, per reader. The
/// window is shared, so one slow document holds back the rest: at four a
/// reader, the writer of a fresh 48k-file index waited on readers for 1.3 s
/// of its 3.5 s consuming; sixteen took the index from 6.9 s to 6.0 s, and
/// 64 bought nothing more (2026-10-31). Once the writer drained and wrote
/// while it read, 64 cut its waiting from 1.0 s to 0.4 s and the index from
/// 3.7 s to 3.2 s, for 25 MB more at peak.
const AHEAD: usize = 64;

/// Bytes of extractions ahead of the writer past which readers claim no
/// more documents, however few they are: a document's trigram keys alone
/// take up to four times its bytes, and the window is thousands of
/// documents. Extractions already claimed may still finish past it.
const HELD: usize = 64 << 20;

/// Distinct terms a reader remembers across documents before it forgets
/// them all. A short term's key costs no allocation to file again, so
/// remembering more buys nothing: 65,536 a reader took 60 MB more of a
/// fresh 48k-file index's peak, at the same speed (2026-10-31).
const REMEMBERED: usize = 1 << 13;

/// One document's postings: its length, distinct trigram keys and distinct
/// terms.
#[derive(Debug, Default)]
pub(crate) struct Extracted {
    pub len: u64,
    pub keys: Vec<u32>,
    text: Vec<u8>,
    ends: Vec<usize>,
    hashes: Vec<u64>,
}

impl Extracted {
    /// Bytes allocated for this extraction.
    fn bytes(&self) -> usize {
        self.keys.capacity() * size_of::<u32>()
            + self.text.capacity()
            + self.ends.capacity() * size_of::<usize>()
            + self.hashes.capacity() * size_of::<u64>()
    }

    /// The document's distinct terms, capped as the term index stores them,
    /// each with its [`term_hash`].
    pub fn terms(&self) -> impl Iterator<Item = (u64, &[u8])> {
        let mut start = 0;
        self.ends
            .iter()
            .zip(&self.hashes)
            .map(move |(&end, &hash)| {
                let term = &self.text[start..end];
                start = end;
                (hash, term)
            })
    }
}

/// One reader's state, reused from document to document.
struct Extractor {
    keys: KeySet,
    scratch: Scratch,
    bytes: Vec<u8>,
    /// The last document each remembered term was found in.
    last: TermTable<u32>,
}

impl Extractor {
    fn new() -> Self {
        Self {
            keys: KeySet::new(),
            scratch: Scratch::default(),
            bytes: Vec::new(),
            last: TermTable::default(),
        }
    }

    fn extract(
        &mut self,
        read: &mut Reader<'_>,
        doc: u32,
        out: &mut Extracted,
    ) -> Result<(), Fault> {
        self.bytes.clear();
        read(doc, &mut self.bytes)?;
        out.len = self.bytes.len() as u64;
        self.keys.distinct(&self.bytes, &mut out.keys);
        out.text.clear();
        out.ends.clear();
        out.hashes.clear();
        if self.last.len() > REMEMBERED {
            self.last.clear();
        }
        let last = &mut self.last;
        tokenize_terms(&self.bytes, &mut self.scratch, |token| {
            let term = cap(token.bytes);
            let hash = term_hash(term);
            let fresh = match last.get_mut(hash, term) {
                Some(seen) if *seen == doc => false,
                Some(seen) => {
                    *seen = doc;
                    true
                }
                None => {
                    last.insert(hash, term, doc);
                    true
                }
            };
            if fresh {
                out.text.extend_from_slice(term);
                out.ends.push(out.text.len());
                out.hashes.push(hash);
            }
        });
        Ok(())
    }
}

/// Calls `consume` with each of `docs` and its extraction, in order, until
/// it returns false. One reader works on this thread; more each get a
/// thread of their own, running at most [`AHEAD`] documents each, and
/// [`HELD`] bytes in all, ahead of `consume`, whose reads are wasted if it
/// stops early.
pub(crate) fn in_order<'a, E>(
    docs: impl Iterator<Item = u32> + Send,
    readers: &mut [&mut Reader<'a>],
    consume: impl FnMut(u32, Result<&Extracted, Fault>) -> Result<bool, E>,
) -> Result<(), E> {
    holding(docs, readers, HELD, consume)
}

/// [`in_order`], with readers holding at most `bound` bytes of extractions
/// ahead of `consume`.
fn holding<'a, E>(
    docs: impl Iterator<Item = u32> + Send,
    readers: &mut [&mut Reader<'a>],
    bound: usize,
    mut consume: impl FnMut(u32, Result<&Extracted, Fault>) -> Result<bool, E>,
) -> Result<(), E> {
    if let [read] = readers {
        let mut extractor = Extractor::new();
        let mut extracted = Extracted::default();
        for doc in docs {
            let result = extractor.extract(*read, doc, &mut extracted);
            if !consume(doc, result.map(|()| &extracted))? {
                break;
            }
        }
        return Ok(());
    }
    let claims = Mutex::new(Claims {
        docs,
        window: AHEAD * readers.len(),
        bound,
        next: 0,
        consumed: 0,
        held: 0,
        stop: false,
    });
    let moved = Condvar::new();
    let (sender, receiver) = mpsc::channel();
    std::thread::scope(|scope| {
        // Releases waiting readers however the writer leaves, a failed spawn
        // or a panic included, so the scope can join them.
        let _stop = Stop(&claims, &moved);
        for read in readers.iter_mut() {
            let sender = sender.clone();
            let (claims, moved) = (&claims, &moved);
            scope.spawn(move || read_claims(read, claims, moved, &sender));
        }
        drop(sender);
        consume_in_order(&receiver, &claims, &moved, consume)
    })
}

/// One extraction, sent with its claim's sequence number.
type Sent = (usize, u32, Result<Extracted, Fault>);

/// A reader thread: extracts each document it claims and sends it on.
fn read_claims<I: Iterator<Item = u32>>(
    read: &mut Reader<'_>,
    claims: &Mutex<Claims<I>>,
    moved: &Condvar,
    sender: &mpsc::Sender<Sent>,
) {
    // Ends every claim when this reader finishes, or unwinds: the writer
    // then sees the channel close, and the scope re-raises a panic.
    let _stop = Stop(claims, moved);
    let mut extractor = Extractor::new();
    while let Some((seq, doc)) = Claims::claim(claims, moved) {
        let mut extracted = Extracted::default();
        let result = extractor
            .extract(read, doc, &mut extracted)
            .map(|()| extracted);
        // A reader that asks to stop reads no further; the writer stops
        // when it reaches this document.
        let stop = matches!(result, Err(Fault::Stop));
        if let Ok(extracted) = &result {
            claims.lock().unwrap_or_else(|e| e.into_inner()).held += extracted.bytes();
        }
        if sender.send((seq, doc, result)).is_err() || stop {
            break;
        }
    }
}

/// The writer: passes extractions to `consume` in claim order, releasing
/// each one's bytes and window slot as it goes.
fn consume_in_order<I, E>(
    receiver: &mpsc::Receiver<Sent>,
    claims: &Mutex<Claims<I>>,
    moved: &Condvar,
    mut consume: impl FnMut(u32, Result<&Extracted, Fault>) -> Result<bool, E>,
) -> Result<(), E> {
    let mut ready = BTreeMap::new();
    let mut next = 0;
    loop {
        let Some((doc, result)) = ready.remove(&next) else {
            match receiver.recv() {
                Ok((seq, doc, result)) => {
                    ready.insert(seq, (doc, result));
                    continue;
                }
                // Every reader has stopped: the documents ran out.
                Err(_) => return Ok(()),
            }
        };
        next += 1;
        {
            let mut claims = claims.lock().unwrap_or_else(|e| e.into_inner());
            claims.consumed = next;
            claims.held -= result.as_ref().map_or(0, Extracted::bytes);
            moved.notify_all();
        }
        if !consume(doc, result.as_ref().map_err(|&fault| fault))? {
            return Ok(());
        }
    }
}

/// Stops all claims when dropped.
struct Stop<'a, I>(&'a Mutex<Claims<I>>, &'a Condvar);

impl<I> Drop for Stop<'_, I> {
    fn drop(&mut self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        self.1.notify_all();
    }
}

/// The documents readers have yet to claim, how far the writer has consumed
/// their results, and the bytes of results it has yet to consume.
struct Claims<I> {
    docs: I,
    /// How many documents past the writer's a reader may claim.
    window: usize,
    /// How many bytes of results a reader may hold ahead of the writer
    /// before it claims another.
    bound: usize,
    next: usize,
    consumed: usize,
    held: usize,
    stop: bool,
}

impl<I: Iterator<Item = u32>> Claims<I> {
    /// The next document and its sequence number, once it is within the
    /// window and the results ahead of the writer hold less than the bound;
    /// `None` when there are no more or the writer stopped. Every claimed
    /// document is delivered, so the writer never waits on one this refuses.
    fn claim(claims: &Mutex<Self>, moved: &Condvar) -> Option<(usize, u32)> {
        let mut claims = claims.lock().unwrap_or_else(|e| e.into_inner());
        while !claims.stop
            && (claims.next >= claims.consumed + claims.window || claims.held >= claims.bound)
        {
            claims = moved.wait(claims).unwrap_or_else(|e| e.into_inner());
        }
        if claims.stop {
            return None;
        }
        let doc = claims.docs.next()?;
        let seq = claims.next;
        claims.next += 1;
        Some((seq, doc))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    /// Readers given a slow writer run ahead to the byte bound, not the
    /// document window: with every extraction over the bound, each reader
    /// holds at most the one it claimed before the bound filled.
    #[test]
    fn readers_stop_at_the_byte_bound_before_the_window() {
        const READERS: usize = 2;
        const DOCS: u32 = 200;
        let started = AtomicUsize::new(0);
        let read = |_doc: u32, bytes: &mut Vec<u8>| {
            started.fetch_add(1, Ordering::SeqCst);
            bytes.extend_from_slice(b"the quick brown fox");
            Ok(())
        };
        let (mut first, mut second) = (read, read);
        let mut readers: [&mut Reader<'_>; READERS] = [&mut first, &mut second];
        let mut consumed = 0;
        let mut furthest = 0;
        holding(0..DOCS, &mut readers, 1, |doc, extracted| {
            assert_eq!(doc, consumed);
            assert!(extracted.is_ok_and(|e| e.bytes() > 1));
            std::thread::sleep(Duration::from_micros(200));
            consumed += 1;
            furthest = furthest.max(started.load(Ordering::SeqCst) - consumed as usize);
            Ok::<_, ()>(true)
        })
        .unwrap();
        assert_eq!(consumed, DOCS);
        assert!(
            furthest <= READERS,
            "readers ran {furthest} documents ahead"
        );
    }
}
