//! A merge step: planning which segments of a run survive, writing their
//! merged pair, and publishing the manifest that replaces the run.

use super::files::retire;
use super::paced::PacedInput;
use super::*;
use crate::regular;

impl IndexWriter {
    /// Merges the segments at `run` into one, if their bytes fit the budget.
    pub(super) fn merge_run(
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
        let Some(mut plan) = self.plan_run(catalog, budget, run.clone()) else {
            return Ok(None);
        };
        if plan.docs == 0 {
            // Nothing survives: drop the inputs without writing. Their
            // range stays below the frontier, so it is not re-covered.
            let retired = self.replace_run(plan.manifest, run, None)?;
            return Ok(Some(Merged {
                inputs: retired.len(),
                read: plan.read,
                written: 0,
                purged: plan.purged,
                records_added: 0,
                records_removed: plan.input_records,
                segment: None,
            }));
        }
        let (written, changes) = self.write_run(&mut plan, catalog, budget, run.clone())?;
        let entry = written.0;
        if (budget.cancelled)() {
            return Err(Error::Cancelled);
        }
        let retired = self.replace_run(plan.manifest, run, Some(written))?;
        Ok(Some(Merged {
            inputs: retired.len(),
            read: plan.read,
            written: entry.total_bytes(),
            purged: plan.purged,
            records_added: changes.0,
            records_removed: changes.1,
            segment: Some(entry),
        }))
    }

    /// What merging the segments at `run` reads and keeps, or `None` when
    /// their bytes exceed the budget.
    fn plan_run(
        &self,
        catalog: &CatalogView,
        budget: &Budget,
        run: std::ops::Range<usize>,
    ) -> Option<RunPlan> {
        let mut manifest = self.current.manifest.clone();
        manifest.last_merge = Some(timestamp());
        manifest
            .unreadable
            .retain(|&doc| catalog.live.contains(doc));
        let entries = &self.current.manifest.segments[run.clone()];
        let read: u64 = entries.iter().map(SegmentEntry::total_bytes).sum();
        if read > budget.bytes {
            return None;
        }
        let (first, last) = (entries.first()?.first, entries.last()?.last);
        let docs: u32 = entries
            .iter()
            .map(|s| alive(s, catalog.live, &manifest.unreadable))
            .sum();
        let purged = entries.iter().map(|s| u64::from(s.docs)).sum::<u64>() - u64::from(docs);
        let input_records = self.current.segments[run]
            .iter()
            .map(|s| s.fingerprint_records())
            .sum();
        Some(RunPlan {
            manifest,
            read,
            first,
            last,
            docs,
            purged,
            input_records,
        })
    }

    /// Writes the merged segment pair for `run` under the plan's next
    /// number, returning it with the records it added and removed.
    fn write_run(
        &self,
        plan: &mut RunPlan,
        catalog: &CatalogView,
        budget: &Budget,
        run: std::ops::Range<usize>,
    ) -> Result<(WrittenPair, (u64, u64)), Error> {
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
        let trigram_inputs = self.current.trigrams[run]
            .iter()
            .map(|segment| PacedInput::open(segment.source(), &merge_pace))
            .collect::<Result<Vec<_>, _>>()?;
        let number = plan.manifest.next_number;
        plan.manifest.next_number += 1;
        let (first, last) = (plan.first, plan.last);
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
        let written = Self::write_pair(
            &self.dir,
            number,
            plan.docs,
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
        )?;
        Ok((written, changes))
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
        hit(Point::Retired)?;
        Ok(retired)
    }
}

/// What merging a run of segments reads and keeps, decided before any
/// input is opened.
struct RunPlan {
    /// The manifest to publish, with dead unreadable documents dropped.
    manifest: Manifest,
    /// Input segment bytes.
    read: u64,
    first: u32,
    last: u32,
    /// Live documents that survive the merge.
    docs: u32,
    purged: u64,
    input_records: u64,
}
