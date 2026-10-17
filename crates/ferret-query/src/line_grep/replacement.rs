//! Capture expansion before byte records reach the host. Matching stays in
//! ferret-verify; records retain replacement highlights for cache replay.
//! Whole-input expansion retains one displayed line, not an expanded group.

use std::ops::Range;

use super::*;

fn cancelled(stopped: &dyn Fn() -> bool) -> io::Result<()> {
    if stopped() {
        Err(io::ErrorKind::ConnectionAborted.into())
    } else {
        Ok(())
    }
}

pub(super) fn line(
    plan: &Plan,
    line: Line<'_>,
    stopped: &dyn Fn() -> bool,
    emit: &mut impl FnMut(Line<'_>) -> io::Result<()>,
) -> io::Result<()> {
    region(
        plan,
        Region {
            source: line.bytes,
            clip: line.bytes.len(),
            range: 0..line.bytes.len(),
        },
        plan.grep.regex().spans(line.bytes),
        line,
        stopped,
        emit,
    )
}

/// A cross-line discovery in an -m tail cannot publish the following line
/// twice. Classify and capture this context line within its own bounds (M8-C).
pub(super) fn trailing(
    plan: &Plan,
    source: &[u8],
    line: Line<'_>,
    stopped: &dyn Fn() -> bool,
    emit: &mut impl FnMut(Line<'_>) -> io::Result<()>,
) -> io::Result<()> {
    let regex = plan.grep.regex();
    let end = if plan.grep.excludes_lf() {
        line.bytes.len()
    } else {
        source.len()
    };
    region(
        plan,
        Region {
            source,
            clip: source.len(),
            range: 0..end,
        },
        regex.spans(source),
        Line {
            kind: if regex.find(source).is_some() {
                LineKind::Match
            } else {
                LineKind::Context
            },
            ..line
        },
        stopped,
        emit,
    )
}

/// Original match bounds survive clipping and line-group projection. Captures
/// must never be re-matched on a detached fragment or on expanded output.
pub(super) struct Region<'a> {
    pub source: &'a [u8],
    pub clip: usize,
    pub range: Range<usize>,
}

pub(super) fn region(
    plan: &Plan,
    region: Region<'_>,
    spans: impl Iterator<Item = Range<usize>>,
    line: Line<'_>,
    stopped: &dyn Fn() -> bool,
    emit: &mut impl FnMut(Line<'_>) -> io::Result<()>,
) -> io::Result<()> {
    let Some(replacement) = &plan.replacement else {
        return emit(line);
    };
    let whole = plan.multiline && !plan.invert && !plan.grep.excludes_lf();
    let only = plan.only_matching
        && if plan.invert {
            line.kind == LineKind::Context
        } else {
            line.kind == LineKind::Match
        };
    let mut output = Records::new(
        Line {
            fragment: only,
            ..line
        },
        whole,
    );
    let mut at = region.range.start;
    let mut expanded = false;
    let mut had_spans = false;
    for span in spans {
        had_spans = true;
        cancelled(stopped)?;
        if span.start < region.range.start
            || span.start > region.range.end
            || (span.start == region.range.end && !(span.is_empty() && line.terminated && !whole))
        {
            continue;
        }
        let haystack = if span.start < region.clip {
            &region.source[..region.clip]
        } else {
            region.source
        };
        let Some(captures) = plan.grep.regex().captures_at(haystack, span.start) else {
            continue;
        };
        expanded = true;
        if !only {
            output.push(&region.source[at..span.start], false, stopped, emit)?;
        } else if whole {
            output.line.number += line_feeds(&region.source[at..span.start], stopped)?;
        }
        output.merge = whole;
        replacement.expand(&captures, |piece| output.push(piece, true, stopped, emit))?;
        if only {
            output.flush(!whole, stopped, emit)?;
        }
        at = span.end.min(region.range.end);
    }
    if only && expanded {
        return Ok(());
    }
    if only {
        // A discovered terminal assertion can select a line without a
        // printable span. Keep the existing whole-line fallback.
        return if had_spans || (plan.multiline && plan.invert) {
            emit(line)
        } else {
            Ok(())
        };
    }
    output.push(&region.source[at..region.range.end], false, stopped, emit)?;
    output.flush(!whole, stopped, emit)
}

struct Records<'a> {
    line: Line<'a>,
    split: bool,
    merge: bool,
    bytes: Vec<u8>,
    highlights: Vec<Range<usize>>,
}

impl<'a> Records<'a> {
    fn new(line: Line<'a>, split: bool) -> Self {
        Self {
            line,
            split,
            merge: false,
            bytes: Vec::new(),
            highlights: Vec::new(),
        }
    }

    fn push(
        &mut self,
        bytes: &[u8],
        highlighted: bool,
        stopped: &dyn Fn() -> bool,
        emit: &mut impl FnMut(Line<'_>) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut at = 0;
        while at < bytes.len() {
            cancelled(stopped)?;
            let chunk = &bytes[at..bytes.len().min(at.saturating_add(crate::scan::BUFFER))];
            let newline = self.split.then(|| memchr::memchr(b'\n', chunk)).flatten();
            let piece = &chunk[..newline.unwrap_or(chunk.len())];
            let start = self.bytes.len();
            self.bytes
                .try_reserve(piece.len())
                .map_err(io::Error::other)?;
            self.bytes.extend_from_slice(piece);
            if highlighted && !piece.is_empty() && !self.line.fragment {
                let span = start..self.bytes.len();
                if self.merge
                    && let Some(last) = self.highlights.last_mut()
                    && last.end == start
                {
                    last.end = span.end;
                } else {
                    self.highlights.try_reserve(1).map_err(io::Error::other)?;
                    self.highlights.push(span);
                }
            }
            self.merge = highlighted;
            at += piece.len();
            if newline.is_some() {
                self.flush(true, stopped, emit)?;
                self.line.number += 1;
                at += 1;
            }
        }
        Ok(())
    }

    fn flush(
        &mut self,
        force: bool,
        stopped: &dyn Fn() -> bool,
        emit: &mut impl FnMut(Line<'_>) -> io::Result<()>,
    ) -> io::Result<()> {
        if force || !self.bytes.is_empty() {
            cancelled(stopped)?;
            emit(Line {
                bytes: &self.bytes,
                highlights: (!self.line.fragment).then_some(&self.highlights),
                ..self.line
            })?;
            self.bytes.clear();
            self.highlights.clear();
        }
        Ok(())
    }
}

fn line_feeds(bytes: &[u8], stopped: &dyn Fn() -> bool) -> io::Result<u64> {
    let mut count = 0;
    for chunk in bytes.chunks(crate::scan::BUFFER) {
        cancelled(stopped)?;
        count += memchr::memchr_iter(b'\n', chunk).count() as u64;
    }
    Ok(count)
}
