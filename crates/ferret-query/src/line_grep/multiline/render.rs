//! Rendering a whole-input scan: each window line's records, from the
//! spans and groups the scan selected.

use super::*;

/// Renders the windows' lines from the scan's spans, reusing its highlight
/// buffers across lines.
pub(super) struct Render<'a> {
    pub(super) plan: &'a Plan,
    pub(super) regex: &'a ferret_verify::Regex,
    pub(super) bytes: &'a [u8],
    pub(super) lines: &'a [Range<usize>],
    pub(super) spans: &'a [Range<usize>],
    pub(super) groups: &'a [Range<usize>],
    pub(super) searched_end: usize,
    pub(super) split: bool,
    pub(super) stopped: &'a dyn Fn() -> bool,
    pub(super) highlights: Vec<Range<usize>>,
    pub(super) local: Vec<Range<usize>>,
}

/// The source line being rendered.
struct At {
    index: usize,
    range: Range<usize>,
    content_end: usize,
    /// One past the last line the record covers.
    last: usize,
    /// The end of the record's bytes.
    end: usize,
    kind: LineKind,
    selected: bool,
    /// An inverted context line before the limit: a matching one. Its
    /// captures and highlights come from the complete line, including its
    /// LF.
    inverted_context: bool,
    /// The first span that may touch the line.
    span_at: usize,
}

impl Render<'_> {
    /// Emits line `index`'s records; returns the next line to render.
    pub(super) fn line(
        &mut self,
        index: usize,
        group_at: usize,
        span_at: &mut usize,
        emit: &mut impl FnMut(Line<'_>) -> io::Result<()>,
    ) -> Result<usize, ScanError> {
        let (bytes, spans) = (self.bytes, self.spans);
        let selected = self
            .groups
            .get(group_at)
            .is_some_and(|group| group.contains(&index));
        let kind = if selected {
            LineKind::Match
        } else {
            LineKind::Context
        };
        let range = self.lines[index].clone();
        let content_end = if bytes[range.end - 1] == b'\n' {
            range.end - 1
        } else {
            range.end
        };
        let last = if self.split && selected {
            self.groups[group_at].end
        } else {
            index + 1
        };
        let end = self.lines[last - 1].end;
        while *span_at < spans.len()
            && (spans[*span_at].end < range.start
                || (!spans[*span_at].is_empty() && spans[*span_at].end == range.start))
        {
            cancelled(self.stopped)?;
            *span_at += 1;
        }
        let at = At {
            index,
            range,
            content_end,
            last,
            end,
            kind,
            selected,
            inverted_context: self.plan.invert && !selected && index < self.searched_end,
            span_at: *span_at,
        };
        if self.plan.json {
            self.json(&at, emit)?;
            return Ok(at.last);
        }
        if self.plan.replacement.is_some() {
            self.replace(&at, emit).map_err(ScanError::Output)?;
            return Ok(at.last);
        }
        if self.plan.only_matching && at.selected && !self.plan.invert {
            self.fragments(&at, emit)?;
        } else {
            self.plain(&at, emit)?;
        }
        Ok(at.index + 1)
    }

    /// Spans from `from` that start before `to`; an unterminated last line
    /// also owns the empty span at its end.
    fn upto(&self, from: usize, to: usize) -> usize {
        let (bytes, spans) = (self.bytes, self.spans);
        let mut at = from;
        while at < spans.len()
            && (spans[at].start < to
                || (to == bytes.len() && !bytes.ends_with(b"\n") && spans[at].start == to))
        {
            at += 1;
        }
        at
    }

    fn json(
        &mut self,
        at: &At,
        emit: &mut impl FnMut(Line<'_>) -> io::Result<()>,
    ) -> Result<(), ScanError> {
        let (bytes, stopped) = (self.bytes, self.stopped);
        let region = at.range.start..at.end;
        self.highlights.clear();
        if at.inverted_context {
            for span in self.regex.spans(&bytes[region.clone()]) {
                cancelled(stopped)?;
                if span.start < region.len() {
                    push(&mut self.highlights, span)?;
                }
            }
        } else {
            for span in &self.spans[at.span_at..self.upto(at.span_at, at.end)] {
                cancelled(stopped)?;
                if span.start >= region.start && span.end <= region.end {
                    push(
                        &mut self.highlights,
                        span.start - region.start..span.end - region.start,
                    )?;
                }
            }
        }
        emit(Line {
            offset: region.start as u64,
            source_lines: Some((at.index as u64 + 1, at.last as u64)),
            terminated: bytes[region.end - 1] == b'\n',
            number: at.index as u64 + 1,
            bytes: &bytes[region],
            kind: at.kind,
            fragment: false,
            highlights: Some(&self.highlights),
        })
        .map_err(ScanError::Output)
    }

    fn replace(
        &self,
        at: &At,
        emit: &mut impl FnMut(Line<'_>) -> io::Result<()>,
    ) -> io::Result<()> {
        let (plan, bytes, end) = (self.plan, self.bytes, at.end);
        let line = Line {
            offset: at.range.start as u64,
            number: at.index as u64 + 1,
            source_lines: Some((at.index as u64 + 1, at.last as u64)),
            terminated: bytes[end - 1] == b'\n',
            bytes: &bytes[at.range.start..at.content_end],
            kind: at.kind,
            fragment: false,
            highlights: Some(&[]),
        };
        if at.inverted_context {
            let source = &bytes[at.range.clone()];
            replacement::region(
                plan,
                replacement::Region {
                    source,
                    range: 0..line.bytes.len(),
                },
                self.regex.spans(source),
                line,
                self.stopped,
                emit,
            )
        } else {
            let range_end = if !self.split && bytes[end - 1] == b'\n' {
                end - 1
            } else {
                end
            };
            replacement::region(
                plan,
                replacement::Region {
                    source: bytes,
                    range: at.range.start..range_end,
                },
                self.spans[at.span_at..self.upto(at.span_at, end)]
                    .iter()
                    .cloned(),
                line,
                self.stopped,
                emit,
            )
        }
    }

    /// `-o`: each match's part of a selected line.
    fn fragments(
        &self,
        at: &At,
        emit: &mut impl FnMut(Line<'_>) -> io::Result<()>,
    ) -> Result<(), ScanError> {
        let bytes = self.bytes;
        let mut printed = false;
        for span in &self.spans[at.span_at..self.upto(at.span_at, at.range.end)] {
            cancelled(self.stopped)?;
            let from = span.start.max(at.range.start);
            let to = span.end.min(at.content_end);
            // D79 D: `-o` never prints an empty record.
            if from >= to {
                continue;
            }
            printed = true;
            emit(Line {
                offset: at.range.start as u64,
                source_lines: None,
                terminated: true,
                number: at.index as u64 + 1,
                bytes: &bytes[from..to],
                kind: at.kind,
                fragment: true,
                highlights: None,
            })
            .map_err(ScanError::Output)?;
        }
        if !printed {
            // An empty fragment prints nothing; it keeps this selected line
            // in the context window.
            emit(Line {
                offset: at.range.start as u64,
                source_lines: None,
                terminated: true,
                number: at.index as u64 + 1,
                bytes: &[],
                kind: at.kind,
                fragment: true,
                highlights: None,
            })
            .map_err(ScanError::Output)?;
        }
        Ok(())
    }

    fn plain(
        &mut self,
        at: &At,
        emit: &mut impl FnMut(Line<'_>) -> io::Result<()>,
    ) -> Result<(), ScanError> {
        let (bytes, stopped, range) = (self.bytes, self.stopped, &at.range);
        let relevant = if at.inverted_context {
            self.local.clear();
            for span in self.regex.spans(&bytes[range.clone()]) {
                cancelled(stopped)?;
                push(
                    &mut self.local,
                    span.start + range.start..span.end + range.start,
                )?;
            }
            self.local.as_slice()
        } else {
            &self.spans[at.span_at..self.upto(at.span_at, range.end)]
        };
        self.highlights.clear();
        for span in relevant {
            cancelled(stopped)?;
            let from = span.start.max(range.start);
            let to = span.end.min(at.content_end);
            if from < to {
                let highlight = from - range.start..to - range.start;
                if let Some(last) = self.highlights.last_mut()
                    && last.end == highlight.start
                {
                    last.end = highlight.end;
                } else {
                    push(&mut self.highlights, highlight)?;
                }
            }
        }
        emit(Line {
            offset: range.start as u64,
            source_lines: None,
            terminated: at.content_end < range.end,
            number: at.index as u64 + 1,
            bytes: &bytes[range.start..at.content_end],
            kind: at.kind,
            fragment: false,
            highlights: Some(&self.highlights),
        })
        .map_err(ScanError::Output)
    }
}
