//! A follow pass: reading new documents into a segment's buffers, sealing
//! and writing each segment as the buffers fill, and publishing the pass.

use super::*;

impl IndexWriter {
    pub(super) fn follow_inner(
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
