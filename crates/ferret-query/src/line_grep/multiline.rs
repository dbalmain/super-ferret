//! Whole-input grep execution. The matcher owns LF classification and byte
//! iteration; this module takes one pass of whole matches over the original
//! input, applies the `-m` limit to their line groups and renders from those
//! same spans. All input and variable-size metadata allocations are fallible.

use std::ops::Range;

use super::*;

fn push<T>(items: &mut Vec<T>, item: T) -> io::Result<()> {
    #[cfg(test)]
    tests::metadata_reservation()?;
    items.try_reserve(1).map_err(io::Error::other)?;
    items.push(item);
    Ok(())
}

fn cancelled(stopped: &dyn Fn() -> bool) -> io::Result<()> {
    if stopped() {
        Err(io::ErrorKind::Interrupted.into())
    } else {
        Ok(())
    }
}

fn read_all(
    source: &mut dyn Read,
    size: u64,
    buffer: &mut Vec<u8>,
    text: bool,
    stopped: &dyn Fn() -> bool,
) -> io::Result<bool> {
    read_all_with(
        source,
        size,
        buffer,
        text,
        stopped,
        &mut |buffer, additional| buffer.try_reserve(additional).map_err(io::Error::other),
    )
}

fn read_all_with(
    source: &mut dyn Read,
    size: u64,
    buffer: &mut Vec<u8>,
    text: bool,
    stopped: &dyn Fn() -> bool,
    reserve: &mut dyn FnMut(&mut Vec<u8>, usize) -> io::Result<()>,
) -> io::Result<bool> {
    buffer.clear();
    if size != u64::MAX && size > isize::MAX as u64 {
        return Err(io::ErrorKind::FileTooLarge.into());
    }
    loop {
        cancelled(stopped)?;
        let start = buffer.len();
        let left = size.saturating_sub(start as u64);
        if left == 0 {
            return Ok(false);
        }
        let additional = left.min(crate::scan::BUFFER as u64) as usize;
        let end = start
            .checked_add(additional)
            .filter(|&end| end <= isize::MAX as usize)
            .ok_or(io::ErrorKind::FileTooLarge)?;
        reserve(buffer, additional)?;
        buffer.resize(end, 0);
        let n = match source.read(&mut buffer[start..]) {
            Ok(n) => n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                buffer.truncate(start);
                continue;
            }
            Err(error) => return Err(error),
        };
        buffer.truncate(start + n);
        if !text && let Some(at) = memchr::memchr(0, &buffer[start..]) {
            buffer.truncate(start + at);
            return Ok(true);
        }
        if n == 0 {
            return Ok(false);
        }
    }
}

fn add_group(groups: &mut Vec<Range<usize>>, range: Range<usize>) -> io::Result<()> {
    if range.is_empty() {
        return Ok(());
    }
    if let Some(last) = groups.last_mut()
        && range.start <= last.end
    {
        last.end = last.end.max(range.end);
        return Ok(());
    }
    push(groups, range)
}

fn line_at(lines: &[Range<usize>], at: usize) -> usize {
    lines
        .partition_point(|line| line.end <= at)
        .min(lines.len() - 1)
}

#[cfg(test)]
fn scan(
    plan: &Plan,
    source: &mut dyn Read,
    size: u64,
    buffer: &mut Vec<u8>,
    stopped: &dyn Fn() -> bool,
    emit: impl FnMut(Line<'_>) -> io::Result<()>,
) -> Result<InputResult, ScanError> {
    let mut result = InputResult::empty(plan);
    scan_progress(plan, source, size, buffer, stopped, &mut result, emit)?;
    Ok(result)
}

pub(super) fn scan_progress(
    plan: &Plan,
    source: &mut dyn Read,
    size: u64,
    buffer: &mut Vec<u8>,
    stopped: &dyn Fn() -> bool,
    result: &mut InputResult,
    mut emit: impl FnMut(Line<'_>) -> io::Result<()>,
) -> Result<(), ScanError> {
    if !result.searched {
        return Ok(());
    }
    result.binary = read_all(source, size, buffer, plan.text, stopped)?;
    if result.binary {
        result.binary_offset =
            Some(buffer.len() as u64 - u64::from(buffer.starts_with(b"\xef\xbb\xbf")) * 3);
    }
    let bytes = buffer.strip_prefix(b"\xef\xbb\xbf").unwrap_or(buffer);
    result.bytes_searched = bytes.len() as u64;
    let mut lines = Vec::new();
    let mut start = 0;
    for (chunk_number, chunk) in bytes.chunks(crate::scan::BUFFER).enumerate() {
        cancelled(stopped)?;
        let base = chunk_number * crate::scan::BUFFER;
        for offset in memchr::memchr_iter(b'\n', chunk) {
            let end = base + offset + 1;
            push(&mut lines, start..end)?;
            start = end;
        }
    }
    if start < bytes.len() {
        push(&mut lines, start..bytes.len())?;
    }
    if lines.is_empty() {
        return Ok(());
    }

    let whole = !plan.grep.excludes_lf();
    let regex = plan.grep.regex();
    // One pass over the original input (D81 C): assertions see the whole
    // input and every match is taken whole. A line group holds every match
    // that starts on its lines, so a group can extend downward; `-m N` stops at
    // the first match that would start group N + 1. Nothing is clipped or
    // re-matched: counts, rendering, `-o`, `-r` and JSON all use these spans.
    // Inversion needs only the matched lines, so it keeps no spans.
    let mut spans: Vec<Range<usize>> = Vec::new();
    let mut groups: Vec<Range<usize>> = Vec::new();
    let (mut taken, mut group_end) = (0, 0);
    // Lines from here on are past the `-m` limit: context, never searched.
    let mut searched_end = lines.len();
    for span in regex.spans(bytes) {
        cancelled(stopped)?;
        if span.start == bytes.len() && bytes.ends_with(b"\n") {
            // The position after a final LF is on no line and never matches,
            // so it draws no before-context either (D98).
            break;
        }
        let first = line_at(&lines, span.start);
        let last = line_at(&lines, span.end.saturating_sub(1).max(span.start));
        if first >= group_end {
            if !plan.invert && plan.max_count.is_some_and(|limit| taken >= limit) {
                break;
            }
            taken += 1;
        }
        group_end = group_end.max(last + 1);
        add_group(&mut groups, first..last + 1)?;
        if !plan.invert {
            push(&mut spans, span)?;
        }
    }
    if plan.invert {
        // D80 A: select the lines no match touches, then limit those.
        let mut left = plan.max_count.unwrap_or(u64::MAX);
        let mut selected = Vec::new();
        let mut next = 0;
        for matched in groups
            .iter()
            .chain(std::iter::once(&(lines.len()..lines.len())))
        {
            cancelled(stopped)?;
            let keep = left.min((matched.start - next) as u64) as usize;
            add_group(&mut selected, next..next + keep)?;
            left -= keep as u64;
            next = matched.end;
        }
        if plan.max_count.is_some() && left == 0 {
            searched_end = selected.last().map_or(0, |group| group.end);
        }
        groups = selected;
    }
    let selected = !groups.is_empty();
    if plan.output == Output::WithoutMatch {
        result.matched = !selected;
        return Ok(());
    }
    result.matched = selected;
    if (plan.quiet && !plan.json) || plan.output == Output::Names {
        return Ok(());
    }
    if !selected {
        return Ok(());
    }
    result.count =
        if !plan.invert && (whole || plan.only_matching || plan.output == Output::CountMatches) {
            spans.len() as u64
        } else {
            groups
                .iter()
                .map(|group| (group.end - group.start) as u64)
                .sum()
        };
    if plan.output != Output::Lines {
        return Ok(());
    }

    let mut windows = Vec::new();
    for group in &groups {
        cancelled(stopped)?;
        let before = usize::try_from(plan.before).unwrap_or(usize::MAX);
        let after = usize::try_from(plan.after).unwrap_or(usize::MAX);
        add_group(
            &mut windows,
            group.start.saturating_sub(before)..group.end.saturating_add(after).min(lines.len()),
        )?;
    }
    // JSON events and replacement cover a whole selected group; plain and
    // `-o` output render one source line at a time.
    let split = whole && !plan.invert;
    let (mut group_at, mut span_at) = (0, 0);
    let mut highlights: Vec<Range<usize>> = Vec::new();
    let mut local = Vec::new();
    for window in windows {
        let mut index = window.start;
        while index < window.end {
            cancelled(stopped)?;
            while group_at < groups.len() && groups[group_at].end <= index {
                group_at += 1;
            }
            let selected = groups
                .get(group_at)
                .is_some_and(|group| group.contains(&index));
            let kind = if selected {
                LineKind::Match
            } else {
                LineKind::Context
            };
            let range = lines[index].clone();
            let content_end = if bytes[range.end - 1] == b'\n' {
                range.end - 1
            } else {
                range.end
            };
            let last = if split && selected {
                groups[group_at].end
            } else {
                index + 1
            };
            let end = lines[last - 1].end;
            while span_at < spans.len()
                && (spans[span_at].end < range.start
                    || (!spans[span_at].is_empty() && spans[span_at].end == range.start))
            {
                cancelled(stopped)?;
                span_at += 1;
            }
            // Spans that start before `to`; an unterminated last line also
            // owns the empty span at its end.
            let upto = |to: usize| {
                let mut at = span_at;
                while at < spans.len()
                    && (spans[at].start < to
                        || (to == bytes.len() && !bytes.ends_with(b"\n") && spans[at].start == to))
                {
                    at += 1;
                }
                at
            };
            // Inverted context lines before the limit are the matching ones.
            // Their captures and highlights come from the complete line,
            // including its LF.
            let inverted_context = plan.invert && !selected && index < searched_end;
            if plan.json {
                let region = range.start..end;
                highlights.clear();
                if inverted_context {
                    for span in regex.spans(&bytes[region.clone()]) {
                        cancelled(stopped)?;
                        if span.start < region.len() {
                            push(&mut highlights, span)?;
                        }
                    }
                } else {
                    for span in &spans[span_at..upto(end)] {
                        cancelled(stopped)?;
                        if span.start >= region.start && span.end <= region.end {
                            push(
                                &mut highlights,
                                span.start - region.start..span.end - region.start,
                            )?;
                        }
                    }
                }
                emit(Line {
                    offset: region.start as u64,
                    source_lines: Some((index as u64 + 1, last as u64)),
                    terminated: bytes[region.end - 1] == b'\n',
                    number: index as u64 + 1,
                    bytes: &bytes[region],
                    kind,
                    fragment: false,
                    highlights: Some(&highlights),
                })
                .map_err(ScanError::Output)?;
                index = last;
                continue;
            }
            if plan.replacement.is_some() {
                let line = Line {
                    offset: range.start as u64,
                    number: index as u64 + 1,
                    source_lines: Some((index as u64 + 1, last as u64)),
                    terminated: bytes[end - 1] == b'\n',
                    bytes: &bytes[range.start..content_end],
                    kind,
                    fragment: false,
                    highlights: Some(&[]),
                };
                if inverted_context {
                    let source = &bytes[range.clone()];
                    replacement::region(
                        plan,
                        replacement::Region {
                            source,
                            range: 0..line.bytes.len(),
                        },
                        regex.spans(source),
                        line,
                        stopped,
                        &mut emit,
                    )
                } else {
                    let range_end = if !split && bytes[end - 1] == b'\n' {
                        end - 1
                    } else {
                        end
                    };
                    replacement::region(
                        plan,
                        replacement::Region {
                            source: bytes,
                            range: range.start..range_end,
                        },
                        spans[span_at..upto(end)].iter().cloned(),
                        line,
                        stopped,
                        &mut emit,
                    )
                }
                .map_err(ScanError::Output)?;
                index = last;
                continue;
            }
            let relevant = &spans[span_at..upto(range.end)];
            if plan.only_matching && selected && !plan.invert {
                let mut printed = false;
                for span in relevant {
                    cancelled(stopped)?;
                    let from = span.start.max(range.start);
                    let to = span.end.min(content_end);
                    // D79 D: `-o` never prints an empty record.
                    if from >= to {
                        continue;
                    }
                    printed = true;
                    emit(Line {
                        offset: range.start as u64,
                        source_lines: None,
                        terminated: true,
                        number: index as u64 + 1,
                        bytes: &bytes[from..to],
                        kind,
                        fragment: true,
                        highlights: None,
                    })
                    .map_err(ScanError::Output)?;
                }
                if !printed {
                    // An empty fragment prints nothing; it keeps this selected
                    // line in the context window.
                    emit(Line {
                        offset: range.start as u64,
                        source_lines: None,
                        terminated: true,
                        number: index as u64 + 1,
                        bytes: &[],
                        kind,
                        fragment: true,
                        highlights: None,
                    })
                    .map_err(ScanError::Output)?;
                }
            } else {
                let relevant = if inverted_context {
                    local.clear();
                    for span in regex.spans(&bytes[range.clone()]) {
                        cancelled(stopped)?;
                        push(&mut local, span.start + range.start..span.end + range.start)?;
                    }
                    local.as_slice()
                } else {
                    relevant
                };
                highlights.clear();
                for span in relevant {
                    cancelled(stopped)?;
                    let from = span.start.max(range.start);
                    let to = span.end.min(content_end);
                    if from < to {
                        let highlight = from - range.start..to - range.start;
                        if let Some(last) = highlights.last_mut()
                            && last.end == highlight.start
                        {
                            last.end = highlight.end;
                        } else {
                            push(&mut highlights, highlight)?;
                        }
                    }
                }
                emit(Line {
                    offset: range.start as u64,
                    source_lines: None,
                    terminated: content_end < range.end,
                    number: index as u64 + 1,
                    bytes: &bytes[range.start..content_end],
                    kind,
                    fragment: false,
                    highlights: Some(&highlights),
                })
                .map_err(ScanError::Output)?;
            }
            index += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        static METADATA_FAILURE_AFTER: Cell<Option<usize>> = const { Cell::new(None) };
    }

    pub(super) fn metadata_reservation() -> io::Result<()> {
        METADATA_FAILURE_AFTER.with(|remaining| match remaining.get() {
            None => Ok(()),
            Some(0) => {
                remaining.set(None);
                Err(io::ErrorKind::OutOfMemory.into())
            }
            Some(left) => {
                remaining.set(Some(left - 1));
                Ok(())
            }
        })
    }

    struct MetadataFailure;
    impl Drop for MetadataFailure {
        fn drop(&mut self) {
            METADATA_FAILURE_AFTER.set(None);
        }
    }

    fn plan(args: &[&str]) -> Plan {
        let Command::Search(plan) =
            parse(&args.iter().map(OsString::from).collect::<Vec<_>>()).unwrap()
        else {
            panic!("search")
        };
        *plan
    }

    #[test]
    fn oversize_and_allocation_failure_are_input_errors_before_matching() {
        struct NoRead;
        impl Read for NoRead {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("must not read")
            }
        }
        let plan = plan(&["-U", "red\\nblue"]);
        let result = scan(
            &plan,
            &mut NoRead,
            isize::MAX as u64 + 1,
            &mut Vec::new(),
            &|| false,
            |_| panic!("must not emit"),
        );
        assert!(
            matches!(result, Err(ScanError::Input(error)) if error.kind() == io::ErrorKind::FileTooLarge)
        );
        let mut buffer = Vec::new();
        let result = read_all_with(
            &mut NoRead,
            10,
            &mut buffer,
            true,
            &|| false,
            &mut |_, _| Err(io::ErrorKind::OutOfMemory.into()),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::OutOfMemory);
        assert!(buffer.is_empty());
    }

    #[test]
    fn metadata_allocation_faults_propagate_through_the_real_scanner() {
        // The existing push/reservation seam serves both line offsets and
        // spans. Fail each vector's first allocation, without a global
        // allocator.
        for successful_reservations in [0, 4] {
            let plan = plan(&["-U", "red\\nblue"]);
            METADATA_FAILURE_AFTER.set(Some(successful_reservations));
            let _reset = MetadataFailure;
            let result = scan(
                &plan,
                &mut &b"red\nblue\n"[..],
                9,
                &mut Vec::new(),
                &|| false,
                |_| panic!("failed metadata must not emit"),
            );
            assert!(matches!(result, Err(ScanError::Input(error))
                if error.kind() == io::ErrorKind::OutOfMemory));
            assert_eq!(METADATA_FAILURE_AFTER.get(), None);
        }
    }

    #[test]
    fn cancelled_buffering_stops_before_the_next_read() {
        let stop = Cell::new(false);
        let reads = Cell::new(0);
        struct Source<'a> {
            stop: &'a Cell<bool>,
            reads: &'a Cell<usize>,
        }
        impl Read for Source<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                self.reads.set(self.reads.get() + 1);
                buffer[..4].copy_from_slice(b"red\n");
                self.stop.set(true);
                Ok(4)
            }
        }
        let result = scan(
            &plan(&["-U", "red"]),
            &mut Source {
                stop: &stop,
                reads: &reads,
            },
            u64::MAX,
            &mut Vec::new(),
            &|| stop.get(),
            |_| panic!("must buffer first"),
        );
        assert!(
            matches!(result, Err(ScanError::Input(error)) if error.kind() == io::ErrorKind::Interrupted)
        );
        assert_eq!(reads.get(), 1);
    }

    #[test]
    fn long_line_metadata_polls_between_input_sized_chunks() {
        // Input reads use polls 1 and 2; the extent check uses poll 3.
        // Cancel at the second metadata chunk, before the absent regex runs.
        let bytes = vec![b'x'; crate::scan::BUFFER * 2];
        let polls = Cell::new(0);
        let result = scan(
            &plan(&["-U", "absent\\n"]),
            &mut &bytes[..],
            bytes.len() as u64,
            &mut Vec::new(),
            &|| {
                polls.set(polls.get() + 1);
                polls.get() >= 5
            },
            |_| panic!("cancelled metadata must not emit"),
        );
        assert!(
            matches!(result, Err(ScanError::Input(error)) if error.kind() == io::ErrorKind::Interrupted)
        );
        assert_eq!(polls.get(), 5);
    }

    #[test]
    fn rendering_cancellation_after_a_successful_emit_stops_before_the_next_record() {
        let stop = Cell::new(false);
        let mut emitted = 0;
        let result = scan(
            &plan(&["-Uo", "(?s:.)"]),
            &mut &b"red\nblue\n"[..],
            9,
            &mut Vec::new(),
            &|| stop.get(),
            |_| {
                emitted += 1;
                stop.set(true);
                Ok(())
            },
        );
        assert!(
            matches!(result, Err(ScanError::Input(error)) if error.kind() == io::ErrorKind::Interrupted)
        );
        assert_eq!(emitted, 1);
    }

    #[test]
    fn a_late_input_fault_never_publishes_a_buffered_match_or_zero() {
        struct Source(bool);
        impl Read for Source {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if self.0 {
                    return Err(io::ErrorKind::PermissionDenied.into());
                }
                self.0 = true;
                buffer[..9].copy_from_slice(b"red\nblue\n");
                Ok(9)
            }
        }
        for args in [
            vec!["-U", "red\\nblue"],
            vec!["-Uc", "--include-zero", "absent"],
            vec!["-U", "--files-without-match", "absent"],
        ] {
            let result = scan(
                &plan(&args),
                &mut Source(false),
                u64::MAX,
                &mut Vec::new(),
                &|| false,
                |_| panic!("failed input must not emit"),
            );
            assert!(
                matches!(result, Err(ScanError::Input(error)) if error.kind() == io::ErrorKind::PermissionDenied)
            );
        }
    }

    #[test]
    fn cancelled_rendering_stops_with_the_original_output_fault() {
        let stop = Cell::new(false);
        let mut emitted = 0;
        let error = scan(
            &plan(&["-Uo", "(?s:.)"]),
            &mut &b"red\nblue\n"[..],
            9,
            &mut Vec::new(),
            &|| stop.get(),
            |_| {
                emitted += 1;
                stop.set(true);
                Err(io::Error::other("owned output fault"))
            },
        )
        .unwrap_err();
        assert!(
            matches!(error, ScanError::Output(error) if error.to_string() == "owned output fault")
        );
        assert_eq!(emitted, 1);
    }

    #[test]
    fn tiny_reads_bom_binary_and_bounded_extent_share_whole_input_buffering() {
        struct Tiny<'a>(&'a [u8]);
        impl Read for Tiny<'_> {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                self.0.read(&mut bytes[..1])
            }
        }
        for size in [14, u64::MAX] {
            let mut output = Vec::new();
            let result = scan(
                &plan(&["-Uo", "red\\nblue"]),
                &mut Tiny(b"\xef\xbb\xbfred\nblue\0tail"),
                size,
                &mut Vec::new(),
                &|| false,
                |line| {
                    output.push(line.bytes.to_vec());
                    Ok(())
                },
            )
            .unwrap();
            assert!(result.matched && result.binary);
            assert_eq!(output, [b"red".to_vec(), b"blue".to_vec()]);
        }
        let mut buffer = Vec::new();
        assert!(
            !read_all(&mut &b"red\nblue\nextra"[..], 9, &mut buffer, true, &|| {
                false
            })
            .unwrap()
        );
        assert_eq!(buffer, b"red\nblue\n");
    }

    #[test]
    fn inversion_counts_lines_no_match_touches_up_to_the_limit() {
        // D80 A: -U -v selects the lines no match intersects, -m limits them,
        // and both count modes report selected lines.
        for (args, count) in [
            (vec!["-Uvcm2", "a"], 2),
            (vec!["-Uvm2", "--count-matches", "a"], 2),
            (vec!["-Uvc", "a"], 3),
            (vec!["-Uvcm1", "a|\\n"], 0),
        ] {
            let result = scan(
                &plan(&args),
                &mut &b"x\na\ny\na\nz\n"[..],
                10,
                &mut Vec::new(),
                &|| false,
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(result.matched, count > 0, "{args:?}");
            assert_eq!(result.count, count, "{args:?}");
        }
    }
}
