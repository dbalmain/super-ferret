//! File jobs: one input read into a rendered group, retried while unpublished,
//! or replayed from the bounded duplicate cache.

use super::read::{read_catalogued, read_file, read_file_policy};
use super::render::{Capture, Cursor, OutputCapture, Record, Render, record_size};
use super::*;
use ferret_query::ReadVersion;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub(in super::super) struct Group {
    pub result: InputResult,
    pub output: OutputBuffer,
    _capture: Option<capture::Held>,
    version: ReadVersion,
    records: Option<Vec<Record>>,
}

pub(in super::super) enum Observed {
    Ready(Group),
    Skipped,
    Failed(ReadFault),
    Spool(io::Error),
    Streamed(InputResult, Option<String>, Option<capture::Held>),
}

/// Test scans still use the production threshold, with a discard destination.
#[cfg(test)]
pub(in super::super) fn observe(
    render: &Render<'_>,
    buffer: &mut Vec<u8>,
    read: &mut dyn FnMut(ferret_query::Consumer<'_>) -> ReadOutcome,
    cache: bool,
) -> io::Result<Observed> {
    observe_streaming(render, buffer, read, cache, &mut io::sink())
}

/// Retry an unpublished changed attempt once. Once streaming has started,
/// retain its prefix and report file faults without retrying. Only actual
/// destination failures escape as errors that stop the whole run.
pub(in super::super) fn observe_streaming(
    render: &Render<'_>,
    buffer: &mut Vec<u8>,
    read: &mut dyn FnMut(ferret_query::Consumer<'_>) -> ReadOutcome,
    cache: bool,
    target: &mut dyn Write,
) -> io::Result<Observed> {
    let mut output = OutputCapture::new(target).pooled(render);
    let mut result = InputResult::default();
    let mut capture = Capture::new(cache);
    let mut output_failure = None;
    let mut changed_after_publication = false;
    let outcome = ferret_query::retry_read(
        || {
            output.reset();
            result = InputResult::default();
            capture = Capture::new(cache);
            let mut each = |input: &mut dyn Read, size| match render.scan(
                input,
                size,
                buffer,
                &mut output,
                &mut capture,
            ) {
                Ok(scanned) => {
                    result = scanned;
                    Ok(())
                }
                Err((partial, error)) => {
                    result = partial;
                    match error {
                        ScanError::Input(error) => Err(error),
                        ScanError::Output(error) => {
                            output_failure = Some(error);
                            Err(io::Error::other("grep rendering failed"))
                        }
                    }
                }
            };
            let outcome = read(&mut each);
            if output_failure.is_some() {
                ReadOutcome::Cancelled
            } else if output.streamed && matches!(outcome, ReadOutcome::RetryChanged) {
                changed_after_publication = true;
                // Do not let retry_read repeat an irreversible observation.
                ReadOutcome::Failed(ReadFault::Unstable)
            } else {
                outcome
            }
        },
        render.stopped,
    );
    let spool_failure = if let Some(error) = output_failure {
        if ferret_query::find::spill_path(&error).is_none() {
            return Err(error);
        }
        Some(error)
    } else {
        None
    };
    if output.streamed {
        let error = if let Some(error) = spool_failure {
            Some(error.to_string())
        } else if changed_after_publication {
            Some("changed during read after output began; output is partial (not retried)".into())
        } else {
            match outcome {
                ReadOutcome::Failed(error) => Some(error.to_string()),
                ReadOutcome::Vanished => {
                    Some("vanished after output began; output is partial".into())
                }
                _ => None,
            }
        };
        return Ok(Observed::Streamed(result, error, output.held));
    }
    if let Some(error) = spool_failure {
        return Ok(Observed::Spool(error));
    }
    Ok(match outcome {
        ReadOutcome::Stable(version) => Observed::Ready(Group {
            result,
            output: output.buffer,
            _capture: output.held,
            version,
            records: capture.records,
        }),
        ReadOutcome::Failed(error) => Observed::Failed(error),
        ReadOutcome::RetryChanged => Observed::Failed(ReadFault::Unstable),
        ReadOutcome::Cancelled | ReadOutcome::Vanished => Observed::Skipped,
    })
}

pub(in super::super) enum Job {
    /// An explicit file: display spelling and absolute lookup path.
    Direct(PathBuf, PathBuf),
    /// A walked file and the directory it was listed in, which it is opened
    /// beneath, never through a symlink; then whether to probe binary as rg
    /// does, and whether to read it once without the changed-during-read
    /// check, as an uncovered scan does (D123).
    Listed(PathBuf, PathBuf, engine::reader::Parent, bool, bool),
    Checked(PathBuf, NameId, u32),
    /// A catalogued file omitted from content indexing: oversized, or binary
    /// under -a. Read through the checked catalog walk, uncached.
    Unindexed(PathBuf, NameId),
    Failed(PathBuf, String),
    // Catalog posting absence only: explicit/live inputs always read bytes.
    Absent(PathBuf),
}
impl Job {
    pub(super) fn path(&self) -> &Path {
        match self {
            Self::Direct(p, _)
            | Self::Listed(p, _, _, _, _)
            | Self::Checked(p, _, _)
            | Self::Unindexed(p, _)
            | Self::Failed(p, _)
            | Self::Absent(p) => p,
        }
    }

    /// What publication keeps of a finished job: no open directory. Finished
    /// groups can wait in held batches behind a blocked writer, and each
    /// would otherwise keep its directory's descriptor open.
    pub(super) fn finished(self) -> Self {
        match self {
            Self::Listed(path, actual, _, _, _) => Self::Direct(path, actual),
            job => job,
        }
    }
}

pub(super) struct Cached {
    result: InputResult,
    records: Vec<Record>,
}
#[derive(Default)]
pub(super) struct Cache {
    pub(super) entries: HashMap<u32, Arc<Cached>>,
    pub(super) bytes: usize,
}
impl Cache {
    fn get(&self, doc: u32) -> Option<Arc<Cached>> {
        self.entries.get(&doc).cloned()
    }
    fn remember(&mut self, doc: u32, group: &mut Group) {
        if group.version != ReadVersion::Catalogued || self.entries.contains_key(&doc) {
            return;
        }
        let Some(records) = group.records.take() else {
            return;
        };
        let bytes = records
            .iter()
            .map(|r| record_size(&r.bytes, r.highlights.as_deref()))
            .sum::<usize>()
            + std::mem::size_of::<Cached>();
        if self.bytes + bytes > CACHE_BYTES {
            self.entries.clear();
            self.bytes = 0;
        }
        self.bytes += bytes;
        self.entries.insert(
            doc,
            Arc::new(Cached {
                result: group.result,
                records,
            }),
        );
    }
}

/// A feed worker: [`Scratch`] plus the checked reader an indexed run reads
/// catalogued files through. The reader is not `Send`, so live walkers, which
/// read no catalogued file, hold only the scratch.
pub(super) struct Worker<'a> {
    scratch: Scratch,
    read: Option<Box<CheckedReader<'a>>>,
}
impl<'a> Worker<'a> {
    pub(super) fn new(plan: Plan, read: Option<Box<CheckedReader<'a>>>) -> Self {
        Self {
            scratch: Scratch::new(plan),
            read,
        }
    }

    pub(super) fn prepare(
        &mut self,
        job: &Job,
        filename: bool,
        cache: &Mutex<Cache>,
        eligible: bool,
        destination: (
            &dyn Fn() -> bool,
            &mut dyn Write,
            Option<&Arc<capture::Budget>>,
        ),
    ) -> io::Result<Observed> {
        self.scratch.prepare(
            self.read.as_deref_mut(),
            job,
            filename,
            cache,
            eligible,
            destination,
        )
    }
}

/// One search thread's own plan clone and reusable read buffer, for the feed
/// and the live walk alike.
pub(super) struct Scratch {
    plan: Plan,
    buffer: Vec<u8>,
}
impl Scratch {
    pub(super) fn new(plan: Plan) -> Self {
        Self {
            plan,
            buffer: Vec::new(),
        }
    }

    /// Searches one job's bytes into a group, or replays a cached
    /// duplicate's, then drops a buffer an outsized file grew.
    pub(super) fn prepare(
        &mut self,
        read: Option<&mut CheckedReader<'_>>,
        job: &Job,
        filename: bool,
        cache: &Mutex<Cache>,
        eligible: bool,
        destination: (
            &dyn Fn() -> bool,
            &mut dyn Write,
            Option<&Arc<capture::Budget>>,
        ),
    ) -> io::Result<Observed> {
        let observed = search_job(
            &self.plan,
            &mut self.buffer,
            read,
            job,
            (filename, cache, eligible),
            destination,
        );
        if self.buffer.capacity() > ferret_query::KEPT_BUFFER {
            self.buffer = Vec::new();
        }
        observed
    }
}

fn search_job(
    plan: &Plan,
    buffer: &mut Vec<u8>,
    read: Option<&mut CheckedReader<'_>>,
    job: &Job,
    (filename, cache, eligible): (bool, &Mutex<Cache>, bool),
    destination: (
        &dyn Fn() -> bool,
        &mut dyn Write,
        Option<&Arc<capture::Budget>>,
    ),
) -> io::Result<Observed> {
    let (stopped, target, capture) = destination;
    let render = Render {
        plan,
        prefix: (filename || plan.json).then(|| label(job.path(), plan.paths.is_empty())),
        stopped,
        capture,
    };
    let observed = match job {
        Job::Checked(_, name, doc) => {
            let Some(read) = read else {
                return Err(io::Error::other("catalogued input has no checked reader"));
            };
            let cached = eligible.then(|| lock(cache).get(*doc)).flatten();
            if let Some(cached) = cached {
                match read(*name, ReadRequest::Stat) {
                    ReadOutcome::Stable(ReadVersion::Catalogued) => {
                        let mut output = OutputCapture::new(target).pooled(&render);
                        let mut cursor = Cursor::default();
                        for record in &cached.records {
                            render.line(
                                &mut output,
                                line_grep::Line {
                                    offset: record.offset,
                                    source_lines: record.source_lines,
                                    terminated: record.terminated,
                                    number: record.number,
                                    bytes: &record.bytes,
                                    kind: record.kind,
                                    fragment: record.fragment,
                                    highlights: record.highlights.as_deref(),
                                },
                                &mut cursor,
                            )?;
                        }
                        output.flush()?;
                        if output.streamed {
                            return Ok(Observed::Streamed(cached.result, None, output.held));
                        }
                        return Ok(Observed::Ready(Group {
                            result: cached.result,
                            output: output.buffer,
                            _capture: output.held,
                            version: ReadVersion::Catalogued,
                            records: None,
                        }));
                    }
                    ReadOutcome::Failed(error) => return Ok(Observed::Failed(error)),
                    ReadOutcome::Cancelled | ReadOutcome::Vanished => {
                        return Ok(Observed::Skipped);
                    }
                    ReadOutcome::Stable(ReadVersion::Current) | ReadOutcome::RetryChanged => {}
                }
            }
            let observed = observe_streaming(
                &render,
                buffer,
                &mut |each| read_catalogued(read, *name, plan, each, stopped, false),
                eligible,
                target,
            )?;
            if let Observed::Ready(mut group) = observed {
                if eligible {
                    lock(cache).remember(*doc, &mut group);
                }
                Observed::Ready(group)
            } else {
                observed
            }
        }
        Job::Unindexed(_, name) => {
            let Some(read) = read else {
                return Err(io::Error::other("catalogued input has no checked reader"));
            };
            observe_streaming(
                &render,
                buffer,
                &mut |each| read_catalogued(read, *name, plan, each, stopped, true),
                false,
                target,
            )?
        }
        Job::Direct(_, actual) => observe_streaming(
            &render,
            buffer,
            &mut |each| read_file(actual, None, plan, each, stopped),
            false,
            target,
        )?,
        Job::Listed(_, actual, directory, rg, once) => observe_streaming(
            &render,
            buffer,
            &mut |each| read_file_policy(actual, Some(directory), plan, each, stopped, *rg, *once),
            false,
            target,
        )?,
        Job::Absent(_) => Observed::Ready(Group {
            result: InputResult::empty(plan),
            output: OutputBuffer::default().with_memory_limit(GROUP_BYTES),
            _capture: None,
            version: ReadVersion::Catalogued,
            records: None,
        }),
        Job::Failed(_, _) => Observed::Skipped,
    };
    Ok(observed)
}
