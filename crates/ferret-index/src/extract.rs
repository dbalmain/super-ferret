//! A follow pass's per-document work, off the writer's thread: reading a
//! document and finding its distinct trigram keys and terms. The writer
//! then only appends each document's postings, in DocId order, so a pass
//! with several readers writes the same segment as a pass with one.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Condvar, Mutex, mpsc};

use ferret_text::{Scratch, cap, tokenize_terms};

use crate::segment::KeySet;
use crate::store::{Fault, Reader};

/// Documents a reader thread may run ahead of the writer, per reader.
const AHEAD: usize = 4;

/// Distinct terms a reader remembers across documents before it forgets
/// them all: enough for a project's vocabulary, small beside the document
/// it is reading.
const REMEMBERED: usize = 1 << 16;

/// One document's postings: its length, distinct trigram keys and distinct
/// terms.
#[derive(Debug, Default)]
pub(crate) struct Extracted {
    pub len: u64,
    pub keys: Vec<u32>,
    text: Vec<u8>,
    ends: Vec<usize>,
}

impl Extracted {
    /// The document's distinct terms, capped as the term index stores them.
    pub fn terms(&self) -> impl Iterator<Item = &[u8]> {
        let mut start = 0;
        self.ends.iter().map(move |&end| {
            let term = &self.text[start..end];
            start = end;
            term
        })
    }
}

/// One reader's state, reused from document to document.
struct Extractor {
    keys: KeySet,
    scratch: Scratch,
    bytes: Vec<u8>,
    /// The last document each remembered term was found in.
    last: HashMap<Box<[u8]>, u32>,
}

impl Extractor {
    fn new() -> Self {
        Self {
            keys: KeySet::new(),
            scratch: Scratch::default(),
            bytes: Vec::new(),
            last: HashMap::new(),
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
        if self.last.len() > REMEMBERED {
            self.last.clear();
        }
        let last = &mut self.last;
        tokenize_terms(&self.bytes, &mut self.scratch, |token| {
            let term = cap(token.bytes);
            let fresh = match last.get_mut(term) {
                Some(seen) if *seen == doc => false,
                Some(seen) => {
                    *seen = doc;
                    true
                }
                None => {
                    last.insert(term.into(), doc);
                    true
                }
            };
            if fresh {
                out.text.extend_from_slice(term);
                out.ends.push(out.text.len());
            }
        });
        Ok(())
    }
}

/// Calls `consume` with each of `docs` and its extraction, in order, until
/// it returns false. One reader works on this thread; more each get a
/// thread of their own, running at most [`AHEAD`] documents each ahead of
/// `consume`, whose reads are wasted if it stops early.
pub(crate) fn in_order<'a, E>(
    docs: impl Iterator<Item = u32> + Send,
    readers: &mut [&mut Reader<'a>],
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
    let window = AHEAD * readers.len();
    let claims = Mutex::new(Claims {
        docs,
        next: 0,
        consumed: 0,
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
            scope.spawn(move || {
                // Ends every claim when this reader finishes, or unwinds:
                // the writer then sees the channel close, and the scope
                // re-raises a panic.
                let _stop = Stop(claims, moved);
                let mut extractor = Extractor::new();
                while let Some((seq, doc)) = Claims::claim(claims, moved, window) {
                    let mut extracted = Extracted::default();
                    let result = extractor
                        .extract(*read, doc, &mut extracted)
                        .map(|()| extracted);
                    // A reader that asks to stop reads no further; the
                    // writer stops when it reaches this document.
                    let stop = matches!(result, Err(Fault::Stop));
                    if sender.send((seq, doc, result)).is_err() || stop {
                        break;
                    }
                }
            });
        }
        drop(sender);
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
                    Err(_) => break Ok(()),
                }
            };
            next += 1;
            {
                claims.lock().unwrap_or_else(|e| e.into_inner()).consumed = next;
                moved.notify_all();
            }
            match consume(doc, result.as_ref().map_err(|&fault| fault)) {
                Ok(true) => {}
                Ok(false) => break Ok(()),
                Err(error) => break Err(error),
            }
        }
    })
}

/// Stops all claims when dropped.
struct Stop<'a, I>(&'a Mutex<Claims<I>>, &'a Condvar);

impl<I> Drop for Stop<'_, I> {
    fn drop(&mut self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        self.1.notify_all();
    }
}

/// The documents readers have yet to claim, and how far the writer has
/// consumed their results.
struct Claims<I> {
    docs: I,
    next: usize,
    consumed: usize,
    stop: bool,
}

impl<I: Iterator<Item = u32>> Claims<I> {
    /// The next document and its sequence number, once it is within
    /// `window` of the writer; `None` when there are no more or the writer
    /// stopped.
    fn claim(claims: &Mutex<Self>, moved: &Condvar, window: usize) -> Option<(usize, u32)> {
        let mut claims = claims.lock().unwrap_or_else(|e| e.into_inner());
        while !claims.stop && claims.next >= claims.consumed + window {
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
