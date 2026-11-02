//! The plain scan: one input a record at a time, line by line, with its
//! context, `-m` limit and per-line rendering. Multiline plans hand off to
//! `multiline`.

use std::ops::ControlFlow;

use ferret_verify::Search;

use super::*;

/// Where a record sits in the source: what every [`Line`] the plain scan
/// emits for one source line shares.
#[derive(Clone, Copy)]
struct Position {
    offset: u64,
    terminated: bool,
    number: u64,
}
impl Position {
    /// A record at this position, with no replacement line counts and its
    /// highlights left to line-local matching.
    fn line(self, bytes: &[u8], kind: LineKind, fragment: bool) -> Line<'_> {
        Line {
            offset: self.offset,
            source_lines: None,
            terminated: self.terminated,
            number: self.number,
            bytes,
            kind,
            fragment,
            highlights: None,
        }
    }
}

pub(super) fn scan_records(
    plan: &Plan,
    input: &mut dyn Read,
    size: u64,
    buffer: &mut Vec<u8>,
    stopped: &dyn Fn() -> bool,
    result: &mut InputResult,
    mut emit: impl FnMut(Line<'_>) -> io::Result<()>,
) -> Result<(), ScanError> {
    if plan.multiline {
        return multiline::scan_progress(plan, input, size, buffer, stopped, result, emit);
    }
    let search = plan.grep.borrowed_search();
    // LF exclusion alone is insufficient: absolute and CRLF anchors see a
    // different boundary on a detached line (D79). The shared HIR proof also
    // rules those out. Context and inversion still need every intervening line.
    let fast = !plan.invert && plan.before == 0 && plan.after == 0 && plan.grep.is_line_local();
    // Context still needs the lines around each selection, but not a search
    // of each one: the same proof finds the next matching line, and only the
    // trailing window and the last `-B` lines before it are visited. Under
    // inversion the lines before it are the selected ones, so none is jumped.
    let skip = !fast && plan.grep.is_line_local();
    #[cfg(test)]
    let (fast, skip) = (fast && !plan.force_per_line, skip && !plan.force_per_line);
    // Counting every LF costs about 15% of a dense scan; skip it unless
    // something reads the number. A jump then adds one, which still leaves
    // a gap for context separators to see.
    let count_lines = plan.line_number || plan.json;
    if !result.searched {
        return Ok(());
    }
    let mut selected_lines = 0;
    let mut bom = 0_u64;
    let mut number = 0;
    let rendering = plan.output == Output::Lines && (!plan.quiet || plan.json);
    let after = if rendering { plan.after } else { 0 };
    let before_lines = if rendering { plan.before } else { 0 };
    let mut before = context::Before::new(before_lines, stopped, plan.spill_dir.clone());
    let mut trailing = 0;
    let mut limited = false;
    let mut emit = |line: Line<'_>| render(plan, &search, stopped, line, &mut emit);
    crate::scan::runs(
        input,
        size,
        buffer,
        stopped,
        (!plan.text).then_some(0),
        |run, base, terminated| {
            let run = if base == 0 {
                let stripped = run.strip_prefix(b"\xef\xbb\xbf").unwrap_or(run);
                bom = (run.len() - stripped.len()) as u64;
                stripped
            } else {
                run
            };
            // `runs` ends a run just after the sentinel, and every byte before
            // it was searched for one, so a NUL can only be the last byte.
            let binary = (!plan.text && run.last() == Some(&0)).then(|| run.len() - 1);
            // M4 R1: a selected-line limit cannot hide a sentinel already
            // received, even when it follows a complete selected line.
            result.binary = binary.is_some();
            if let Some(at) = binary {
                result.binary_offset =
                    Some(base as u64 + at as u64 - if base == 0 { 0 } else { bom });
            }
            let mut offset = base as u64 - if base == 0 { 0 } else { bom };
            let text = &run[..binary.unwrap_or(run.len())];
            let mut next = Some(0);
            // The start of the run's next selected line in skip mode, or
            // `usize::MAX` when the rest of the run selects nothing.
            let mut hit_line = None;
            let mut hit_boundary = None;
            // The first LF at or after a line start, when `next_hit` found
            // it: `(line start, LF offset in the rest)`.
            let mut first_lf = None;
            let mut line_start = 0;
            // How often hits fall on their search's first line: a
            // saturating counter, trusted from 2 of 3.
            let mut dense = 3u8;
            loop {
                let line = if fast {
                    let Some(start) = next.take() else { break };
                    let remaining = &text[start..];
                    if remaining.is_empty() && !terminated && binary.is_none() {
                        break;
                    }
                    let Some(hit) = search.matching_line(remaining) else {
                        // InputResult exposes bytes searched even without JSON.
                        // A later run's callbacks also observe skipped line
                        // numbers.
                        offset += remaining.len() as u64 + u64::from(terminated);
                        result.bytes_searched = offset;
                        if count_lines {
                            number += count_lf(remaining)
                                + u64::from(
                                    terminated
                                        || binary.is_some()
                                        || (!remaining.is_empty() && !remaining.ends_with(b"\n")),
                                );
                        }
                        break;
                    };
                    // While hits keep landing on the first remaining line,
                    // that line's end bounds the hit in one search.
                    let first_end = (dense >= 2)
                        .then(|| memchr::memchr(b'\n', remaining).unwrap_or(remaining.len()))
                        .filter(|&first_end| hit <= first_end);
                    let (first, end) = match first_end {
                        Some(first_end) => {
                            dense = 3;
                            (0, first_end)
                        }
                        None => {
                            let first =
                                memchr::memrchr(b'\n', &remaining[..hit]).map_or(0, |at| at + 1);
                            dense = if first == 0 {
                                (dense + 1).min(3)
                            } else {
                                dense.saturating_sub(1)
                            };
                            let end = memchr::memchr(b'\n', &remaining[hit..])
                                .map_or(remaining.len(), |at| hit + at);
                            (first, end)
                        }
                    };
                    if count_lines {
                        number += count_lf(&remaining[..first]);
                    }
                    offset += first as u64;
                    if end < remaining.len() {
                        next = Some(start + end + 1);
                    }
                    &remaining[first..end]
                } else {
                    let Some(start) = next.take() else { break };
                    if skip && !limited {
                        let hit = match hit_line {
                            Some(hit) if hit >= start => hit,
                            _ => {
                                let open = !terminated && binary.is_none();
                                let (hit, boundary, lf) =
                                    next_hit(&search, text, start, open, before_lines, plan.invert);
                                first_lf = lf.map(|lf| (start, lf));
                                hit_boundary = Some(boundary);
                                hit_line = Some(hit);
                                hit
                            }
                        };
                        // Jump over whole unselected lines that are neither
                        // trailing context nor among the last `-B` lines
                        // before the next selected line or the run's end.
                        let jump = if plan.invert || number < trailing || hit == start {
                            start
                        } else {
                            hit_boundary.unwrap_or(start)
                        };
                        if jump > start {
                            if stopped() {
                                return Err(ScanError::Input(io::ErrorKind::Interrupted.into()));
                            }
                            number += if count_lines {
                                count_lf(&text[start..jump])
                            } else {
                                1
                            };
                            offset += (jump - start) as u64;
                            result.bytes_searched = offset;
                            next = Some(jump);
                            continue;
                        }
                    }
                    line_start = start;
                    let rest = &text[start..];
                    let lf = match first_lf {
                        Some((at, lf)) if at == start => lf,
                        _ => memchr::memchr(b'\n', rest),
                    };
                    match lf {
                        Some(at) => {
                            next = Some(start + at + 1);
                            &rest[..at]
                        }
                        None => rest,
                    }
                };
                // No extra line after a terminal LF or at empty EOF. A partial
                // line before NUL is searched: NUL is a binary sentinel.
                if !fast && line.is_empty() && next.is_none() && !terminated && binary.is_none() {
                    break;
                }
                let line_terminated = next.is_some() || terminated;
                let line_offset = offset;
                offset += line.len() as u64 + u64::from(line_terminated);
                result.bytes_searched = offset;
                number += 1;
                let at = Position {
                    offset: line_offset,
                    terminated: line_terminated,
                    number,
                };
                if stopped() {
                    return Err(ScanError::Input(io::ErrorKind::Interrupted.into()));
                }
                if limited {
                    // Lines after `-m` are context and are never searched.
                    if rendering {
                        emit(Line {
                            highlights: Some(&[]),
                            ..at.line(line, LineKind::Context, false)
                        })
                        .map_err(ScanError::Output)?;
                    }
                    if number >= trailing {
                        return Ok(ControlFlow::Break(()));
                    }
                    continue;
                }
                let selected = if fast {
                    true
                } else if skip {
                    (hit_line == Some(line_start)) != plan.invert
                } else {
                    search.find(line).is_some() != plan.invert
                };
                if selected {
                    if plan.output == Output::WithoutMatch {
                        result.matched = false;
                        return Ok(ControlFlow::Break(()));
                    }
                    selected_lines += 1;
                    result.matched = true;
                    if (plan.quiet && !plan.json) || plan.output == Output::Names {
                        return Ok(ControlFlow::Break(()));
                    }
                    if rendering {
                        before.emit(&mut emit).map_err(ScanError::Output)?;
                    }
                    trailing = number.saturating_add(after);
                    emit_selected(
                        plan,
                        &search,
                        stopped,
                        at,
                        line,
                        &mut result.count,
                        &mut emit,
                    )?;
                    if plan.max_count.is_some_and(|limit| selected_lines >= limit) {
                        limited = true;
                        if after == 0 {
                            return Ok(ControlFlow::Break(()));
                        }
                    }
                } else if rendering && number <= trailing {
                    emit(at.line(line, LineKind::Context, false)).map_err(ScanError::Output)?;
                } else if rendering {
                    before
                        .push_at(number, line_offset, line)
                        .map_err(ScanError::Output)?;
                }
            }
            if binary.is_some() {
                return Ok(ControlFlow::Break(()));
            }
            Ok(ControlFlow::Continue(()))
        },
    )?;
    Ok(())
}

/// Passes one scanned line on as it renders: context lines unhighlighted,
/// `-r` through [`replacement`], and inverted `-o` context split into its
/// spans.
fn render(
    plan: &Plan,
    search: &Search<'_>,
    stopped: &dyn Fn() -> bool,
    line: Line<'_>,
    emit: &mut impl FnMut(Line<'_>) -> io::Result<()>,
) -> io::Result<()> {
    // A context line matches only under inversion; otherwise it is never
    // searched. Lines after `-m` arrive with their highlights resolved
    // (none): they are context and never searched, also under `-v` (D98).
    if line.kind == LineKind::Context && (!plan.invert || line.highlights.is_some()) {
        return emit(Line {
            highlights: Some(&[]),
            ..line
        });
    }
    if plan.replacement.is_some() {
        return replacement::line(plan, line, stopped, emit);
    }
    if line.kind == LineKind::Context && plan.only_matching && plan.invert {
        let mut printed = false;
        for span in search.spans(line.bytes) {
            if stopped() {
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            // D79 D: `-o` never prints an empty record.
            if !span.is_empty() {
                printed = true;
                emit(Line {
                    bytes: &line.bytes[span],
                    fragment: true,
                    ..line
                })?;
            }
        }
        if printed {
            return Ok(());
        }
        // An empty fragment prints nothing; it keeps the line in the
        // window.
        emit(Line {
            bytes: &line.bytes[..0],
            fragment: true,
            ..line
        })
    } else {
        emit(line)
    }
}

/// The start of the next selected line in `text` from `start`, or
/// `usize::MAX` when none is, with where the lines worth visiting before it
/// begin: the last `before` lines, or the run's last line when nothing more is
/// selected. `open` says a final LF may still be followed by more input.
/// With a hit and `visit_first`, also the first LF in the rest (`None`
/// inside when there is none): a hit on the first line then needs no search
/// back for its start, and the caller, which visits that line (as `-v`
/// does), splits it without searching again.
fn next_hit(
    search: &Search<'_>,
    text: &[u8],
    start: usize,
    open: bool,
    before: u64,
    visit_first: bool,
) -> (usize, usize, Option<Option<usize>>) {
    let rest = &text[start..];
    let mut first_lf = None;
    let hit = search.matching_line(rest).map_or(usize::MAX, |hit| {
        let lf = visit_first.then(|| memchr::memchr(b'\n', rest));
        first_lf = lf;
        if lf.is_some_and(|lf| lf.is_none_or(|lf| hit <= lf)) {
            start
        } else {
            start + memchr::memrchr(b'\n', &rest[..hit]).map_or(0, |at| at + 1)
        }
    });
    let boundary = if hit == usize::MAX {
        let end = if open && text.ends_with(b"\n") {
            text.len() - 1
        } else {
            text.len()
        };
        start + context::tail_start(&text[start..end.max(start)], before.max(1))
    } else if hit == start {
        start
    } else {
        start + context::tail_start(&text[start..hit - 1], before)
    };
    (hit, boundary, first_lf)
}

/// Counts and emits a selected line: its spans under `-o` or a match count,
/// else the whole line.
fn emit_selected(
    plan: &Plan,
    search: &Search<'_>,
    stopped: &dyn Fn() -> bool,
    at: Position,
    line: &[u8],
    count: &mut u64,
    emit: &mut impl FnMut(Line<'_>) -> io::Result<()>,
) -> Result<(), ScanError> {
    if (plan.only_matching || plan.output == Output::CountMatches) && !plan.invert {
        let fragments = plan.output == Output::Lines && plan.replacement.is_none();
        let mut printed = false;
        for span in search.spans(line) {
            if stopped() {
                return Err(ScanError::Input(io::ErrorKind::Interrupted.into()));
            }
            *count += 1;
            // D79 D: `-o` never prints an empty record.
            if fragments && !span.is_empty() {
                printed = true;
                emit(at.line(&line[span], LineKind::Match, true)).map_err(ScanError::Output)?;
            }
        }
        if fragments && !printed {
            // An empty fragment prints nothing; it keeps this
            // selected line in the context window.
            emit(at.line(&line[..0], LineKind::Match, true)).map_err(ScanError::Output)?;
        }
        if plan.output == Output::Lines && plan.replacement.is_some() {
            emit(at.line(line, LineKind::Match, false)).map_err(ScanError::Output)?;
        }
    } else {
        *count += 1;
        if plan.output == Output::Lines {
            emit(at.line(line, LineKind::Match, false)).map_err(ScanError::Output)?;
        }
    }
    Ok(())
}
