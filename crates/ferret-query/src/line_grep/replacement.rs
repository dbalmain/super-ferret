//! Capture expansion before byte records reach the host. Matching stays in
//! ferret-verify; records retain replacement highlights for cache replay.

use std::ops::Range;

use super::*;

fn append(buffer: &mut Vec<u8>, bytes: &[u8], stopped: &dyn Fn() -> bool) -> io::Result<()> {
    for chunk in bytes.chunks(crate::scan::BUFFER) {
        if stopped() {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        buffer.try_reserve(chunk.len()).map_err(io::Error::other)?;
        buffer.extend_from_slice(chunk);
    }
    Ok(())
}

pub(super) fn line(
    plan: &Plan,
    line: Line<'_>,
    stopped: &dyn Fn() -> bool,
    emit: &mut impl FnMut(Line<'_>) -> io::Result<()>,
) -> io::Result<()> {
    region(
        plan,
        Region { source: line.bytes, clip: line.bytes.len(), range: 0..line.bytes.len() },
        plan.grep.regex().spans(line.bytes),
        line,
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
    let only = plan.only_matching && if plan.invert {
        line.kind == LineKind::Context
    } else {
        line.kind == LineKind::Match
    };
    let mut bytes = Vec::new();
    let mut highlights = Vec::new();
    let mut at = region.range.start;
    let mut expanded = false;
    let mut number = line.number;
    let mut had_spans = false;
    for span in spans {
        had_spans = true;
        if stopped() {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        if span.start < region.range.start || span.start > region.range.end
            || (span.start == region.range.end && !(span.is_empty() && line.terminated && !whole)) {
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
            append(&mut bytes, &region.source[at..span.start], stopped)?;
        }
        let start = bytes.len();
        replacement.expand(&captures, |piece| append(&mut bytes, piece, stopped))?;
        if only {
            if whole {
                number += region.source[at..span.start]
                    .iter().filter(|&&byte| byte == b'\n').count() as u64;
            }
            emit_bytes(&bytes, &[], Line { number, fragment: true, ..line }, whole, stopped, emit)?;
            if whole {
                number += bytes.iter().filter(|&&byte| byte == b'\n').count() as u64;
            }
            bytes.clear();
        } else {
            highlights.try_reserve(1).map_err(io::Error::other)?;
            highlights.push(start..bytes.len());
        }
        at = span.end.min(region.range.end);
    }
    if only && expanded {
        return Ok(());
    }
    if only {
        // A discovered terminal assertion can select a line without a
        // printable span. Keep the existing whole-line fallback.
        return if had_spans || (plan.multiline && plan.invert) { emit(line) } else { Ok(()) };
    }
    append(&mut bytes, &region.source[at..region.range.end], stopped)?;
    emit_bytes(&bytes, &highlights, line, whole, stopped, emit)
}

fn emit_bytes(
    bytes: &[u8],
    spans: &[Range<usize>],
    line: Line<'_>,
    whole: bool,
    stopped: &dyn Fn() -> bool,
    emit: &mut impl FnMut(Line<'_>) -> io::Result<()>,
) -> io::Result<()> {
    if !whole {
        return emit(Line { bytes, highlights: (!line.fragment).then_some(spans), ..line });
    }
    let mut at = 0;
    let mut number = line.number;
    let mut highlights: Vec<Range<usize>> = Vec::new();
    for chunk in bytes.split_inclusive(|&byte| byte == b'\n') {
        if stopped() {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        let content = chunk.strip_suffix(b"\n").unwrap_or(chunk);
        highlights.clear();
        for span in spans {
            let start = span.start.max(at);
            let end = span.end.min(at + content.len());
            if start < end {
                let range = start - at..end - at;
                if let Some(last) = highlights.last_mut() && last.end == range.start {
                    last.end = range.end;
                } else {
                    highlights.try_reserve(1).map_err(io::Error::other)?;
                    highlights.push(range);
                }
            }
        }
        emit(Line {
            number,
            bytes: content,
            highlights: (!line.fragment).then_some(&highlights),
            ..line
        })?;
        at += chunk.len();
        number += 1;
    }
    Ok(())
}
