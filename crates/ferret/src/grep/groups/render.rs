//! Line rendering: a file's records written as grep or JSON lines into a
//! bounded capture that streams once it outgrows the shared budget.

use super::*;
use std::io::IoSlice;
use std::sync::Arc;

pub(in super::super) struct Render<'a> {
    pub plan: &'a Plan,
    pub prefix: Option<&'a [u8]>,
    pub stopped: &'a dyn Fn() -> bool,
    pub capture: Option<&'a Arc<capture::Budget>>,
}

#[derive(Clone)]
pub(super) struct Record {
    pub(super) offset: u64,
    pub(super) source_lines: Option<(u64, u64)>,
    pub(super) terminated: bool,
    pub(super) number: u64,
    pub(super) bytes: Vec<u8>,
    pub(super) kind: line_grep::LineKind,
    pub(super) fragment: bool,
    pub(super) highlights: Option<Vec<std::ops::Range<usize>>>,
}

pub(super) fn record_size(bytes: &[u8], highlights: Option<&[std::ops::Range<usize>]>) -> usize {
    bytes.len() + std::mem::size_of::<Record>() + highlights.map_or(0, std::mem::size_of_val)
}

pub(super) struct Capture {
    pub(super) records: Option<Vec<Record>>,
    bytes: usize,
}
impl Capture {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            records: enabled.then(Vec::new),
            bytes: 0,
        }
    }
    fn push(&mut self, line: line_grep::Line<'_>) {
        if self.records.is_none() {
            return;
        }
        self.bytes = self
            .bytes
            .saturating_add(record_size(line.bytes, line.highlights));
        if self.bytes > ENTRY_BYTES {
            self.records = None;
        } else if let Some(records) = &mut self.records {
            records.push(Record {
                offset: line.offset,
                source_lines: line.source_lines,
                terminated: line.terminated,
                number: line.number,
                bytes: line.bytes.to_vec(),
                kind: line.kind,
                fragment: line.fragment,
                highlights: line.highlights.map(<[_]>::to_vec),
            });
        }
    }
}

/// Where a file's rendered output stands. A heading belongs to the first
/// printed record. An empty `-o` fragment prints nothing but keeps its line in
/// the window, so `--` marks only lines no window covered.
#[derive(Default)]
pub(super) struct Cursor {
    line: Option<u64>,
    printed: bool,
    gap: bool,
}

/// Rendered captures never spill. Above the threshold they claim the single
/// streaming slot or keep capturing within the shared allocation budget.
pub(super) struct OutputCapture<'a> {
    pub(super) buffer: OutputBuffer,
    pub(super) held: Option<capture::Held>,
    pub(super) target: &'a mut dyn Write,
    stopped: &'a dyn Fn() -> bool,
    pub(super) streamed: bool,
}
impl<'a> OutputCapture<'a> {
    pub(super) fn new(target: &'a mut dyn Write) -> Self {
        Self {
            buffer: OutputBuffer::default().with_memory_limit(GROUP_BYTES),
            held: None,
            target,
            stopped: &|| false,
            streamed: false,
        }
    }
    pub(super) fn pooled(mut self, render: &Render<'a>) -> Self {
        self.held = render.capture.map(|budget| budget.hold());
        self.stopped = render.stopped;
        self
    }

    pub(super) fn reset(&mut self) {
        self.buffer = OutputBuffer::default().with_memory_limit(GROUP_BYTES);
        if let Some(held) = &mut self.held {
            held.clear();
        }
    }
}
impl Write for OutputCapture<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let count = bytes.len().min(CHUNK);
        if !self.streamed {
            let needed = self.buffer.len() as usize + count;
            let capacity = if let Some(held) = &mut self.held {
                if held.capacity(needed).is_some() {
                    return self.buffer.write(&bytes[..count]);
                }
                held.grow(needed, self.stopped)?
            } else {
                (needed <= GROUP_BYTES).then_some(GROUP_BYTES)
            };
            if let Some(capacity) = capacity {
                self.buffer = std::mem::take(&mut self.buffer).with_memory_limit(capacity);
                return self.buffer.write(&bytes[..count]);
            }
            self.streamed = true;
            self.buffer.write_to(self.target)?;
            self.reset();
        }
        self.target.write(&bytes[..count])
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.streamed {
            self.target.flush()
        } else {
            self.buffer.flush()
        }
    }
}

impl Render<'_> {
    pub(super) fn line(
        &self,
        output: &mut dyn Write,
        line: line_grep::Line<'_>,
        cursor: &mut Cursor,
    ) -> io::Result<()> {
        let mut output = Cancellable {
            inner: output,
            stopped: self.stopped,
        };
        if self.plan.json {
            if self.plan.quiet {
                return Ok(());
            }
            return super::json::line(
                &mut output,
                self.prefix.unwrap_or(b"<stdin>"),
                self.plan.line_number,
                line,
            );
        }
        let gap = cursor.line.is_some_and(|number| {
            line.source_lines.map_or(line.number, |(start, _)| start) > number.saturating_add(1)
        });
        cursor.line = Some(line.source_lines.map_or(line.number, |(_, end)| end));
        cursor.gap |= gap;
        if line.fragment && line.bytes.is_empty() {
            return Ok(());
        }
        if !cursor.printed
            && self.plan.presentation.heading == Some(true)
            && let Some(prefix) = self.prefix
        {
            presentation::path(&self.plan.presentation, &mut output, prefix)?;
            output.write_all(if self.plan.presentation.null {
                b"\0"
            } else {
                b"\n"
            })?;
        }
        if (self.plan.before > 0 || self.plan.after > 0) && cursor.printed && cursor.gap {
            output.write_all(b"--\n")?;
        }
        cursor.printed = true;
        cursor.gap = false;
        let omitted = self.omission(&line);
        let delimiter: &[u8] = if line.kind == line_grep::LineKind::Context {
            b"-"
        } else {
            b":"
        };
        if !self.plan.presentation.color.enabled() && !self.plan.line_number {
            let prefix = self
                .prefix
                .filter(|_| self.plan.presentation.heading != Some(true));
            let separator = prefix.map_or(b"".as_slice(), |_| {
                if self.plan.presentation.null {
                    b"\0"
                } else {
                    delimiter
                }
            });
            return write_parts(
                &mut output,
                &mut [
                    IoSlice::new(prefix.unwrap_or_default()),
                    IoSlice::new(separator),
                    IoSlice::new(omitted.as_deref().unwrap_or(line.bytes)),
                    IoSlice::new(b"\n"),
                ],
            );
        }
        if self.plan.presentation.heading != Some(true)
            && let Some(prefix) = self.prefix
        {
            presentation::path(&self.plan.presentation, &mut output, prefix)?;
            output.write_all(if self.plan.presentation.null {
                b"\0"
            } else {
                delimiter
            })?;
        }
        if self.plan.line_number {
            presentation::number(&self.plan.presentation, &mut output, line.number)?;
            output.write_all(delimiter)?;
        }
        if let Some(notice) = omitted {
            output.write_all(&notice)?;
        } else {
            presentation::text(self.plan, &mut output, line)?;
        }
        output.write_all(b"\n")
    }

    /// rg's `--max-columns` notice for a line over the limit. The width counts
    /// the line's terminator, except for an `-o` fragment. In colour, a line
    /// with matches says how many, empty ones included; highlights merge and
    /// drop those, so the line is searched again. Parsing refuses `-M` with
    /// `-r`, and with `-U` in colour, whose counts these lines cannot give.
    fn omission(&self, line: &line_grep::Line<'_>) -> Option<Vec<u8>> {
        let limit = self.plan.max_columns?;
        let width = line.bytes.len() as u64 + u64::from(line.terminated && !line.fragment);
        if width <= limit {
            return None;
        }
        let matches = if !line.fragment && self.plan.presentation.color.enabled() {
            self.plan.grep.borrowed_search().spans(line.bytes).count()
        } else {
            0
        };
        Some(match (matches, line.kind) {
            (0, line_grep::LineKind::Match) => b"[Omitted long matching line]".to_vec(),
            (0, line_grep::LineKind::Context) => b"[Omitted long context line]".to_vec(),
            (count, _) => format!("[Omitted long line with {count} matches]").into_bytes(),
        })
    }

    pub(super) fn scan(
        &self,
        input: &mut dyn Read,
        size: u64,
        buffer: &mut Vec<u8>,
        output: &mut OutputCapture<'_>,
        capture: &mut Capture,
    ) -> Result<InputResult, (InputResult, ScanError)> {
        #[cfg(test)]
        super::seam::BEFORE_SCAN.with_borrow_mut(|hook| {
            if let Some(hook) = hook {
                hook();
            }
        });
        #[cfg(any(test, debug_assertions))]
        let mut observed = super::seam::Observed::new(input);
        #[cfg(any(test, debug_assertions))]
        let input: &mut dyn Read = &mut observed;
        let mut cursor = Cursor::default();
        let result =
            line_grep::scan_lines_progress(self.plan, input, size, buffer, self.stopped, |line| {
                capture.push(line);
                self.line(output, line, &mut cursor)
            });
        let partial = match &result {
            Ok(result) | Err((result, _)) => *result,
        };
        output
            .flush()
            .map_err(|error| (partial, ScanError::Output(error)))?;
        result
    }
}

// ConnectionAborted, rather than Interrupted: Write::write_all retries an
// Interrupted error, which would turn cancellation into an infinite loop.
pub(super) struct Cancellable<'a, W: ?Sized> {
    pub(super) inner: &'a mut W,
    pub(super) stopped: &'a dyn Fn() -> bool,
}
impl<W: Write + ?Sized> Write for Cancellable<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if (self.stopped)() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "grep cancelled",
            ));
        }
        self.inner.write(&bytes[..bytes.len().min(CHUNK)])
    }
    fn write_vectored(&mut self, bytes: &[IoSlice<'_>]) -> io::Result<usize> {
        if (self.stopped)() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "grep cancelled",
            ));
        }
        // Cap both bytes and stack space. Partial vector writes preserve the
        // same between-chunk cancellation bound as ordinary writes.
        let mut parts = [IoSlice::new(&[]); 4];
        let mut left = CHUNK;
        let mut used = 0;
        for part in bytes
            .iter()
            .filter(|part| !part.is_empty())
            .take(parts.len())
        {
            let count = part.len().min(left);
            parts[used] = IoSlice::new(&part[..count]);
            used += 1;
            left -= count;
            if left == 0 {
                break;
            }
        }
        self.inner.write_vectored(&parts[..used])
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub(super) fn write_parts(mut output: impl Write, mut parts: &mut [IoSlice<'_>]) -> io::Result<()> {
    IoSlice::advance_slices(&mut parts, 0);
    while !parts.is_empty() {
        match output.write_vectored(parts) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ));
            }
            Ok(written) => IoSlice::advance_slices(&mut parts, written),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(in super::super) fn emit(
    output: &mut OutputBuffer,
    writer: &mut dyn Write,
    stopped: &dyn Fn() -> bool,
) -> io::Result<()> {
    output.write_to(&mut Cancellable {
        inner: writer,
        stopped,
    })
}
