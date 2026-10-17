//! Whole-input grep execution. The matcher owns LF classification and byte
//! iteration; this module separates search limits, line clipping, re-matching
//! and rendering. All input and variable-size metadata allocations are
//! fallible.

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

pub(super) fn scan(
    plan: &Plan,
    source: &mut dyn Read,
    size: u64,
    buffer: &mut Vec<u8>,
    stopped: &dyn Fn() -> bool,
    mut emit: impl FnMut(Line<'_>) -> io::Result<()>,
) -> Result<InputResult, ScanError> {
    let mut result = InputResult::empty(plan);
    if !result.searched {
        return Ok(result);
    }
    result.binary = read_all(source, size, buffer, plan.text, stopped)?;
    let bytes = buffer.strip_prefix(b"\xef\xbb\xbf").unwrap_or(buffer);
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
        return Ok(result);
    }

    let last_line_start = lines[lines.len() - 1].start;
    let whole = !plan.grep.excludes_lf();
    let regex = plan.grep.regex();
    let mut groups: Vec<Range<usize>> = Vec::new();
    let mut searched = 0u64;
    let mut clip = bytes.len();
    let mut terminal_context = false;
    if plan.invert {
        // rg's multiline inversion searches one positive hit per line group.
        // Its positive-search limit leaves the rest as a gap (M7-A).
        let (mut at, mut next_line) = (0, 0);
        while let Some(span) = regex.spans(&bytes[at..]).next() {
            let span = span.start + at..span.end + at;
            cancelled(stopped)?;
            if span.start >= bytes.len() && bytes.ends_with(b"\n") {
                break;
            }
            let first = line_at(&lines, span.start);
            let last = line_at(&lines, span.end.saturating_sub(1).max(span.start));
            add_group(&mut groups, next_line..first)?;
            next_line = last + 1;
            at = lines[last].end;
            searched += 1;
            if at == bytes.len() || (whole && plan.max_count.is_some_and(|limit| searched >= limit))
            {
                break;
            }
        }
        add_group(&mut groups, next_line..lines.len())?;
    } else {
        let mut previous_line = None;
        let search_spans = if whole {
            regex.rebased_spans(bytes)
        } else {
            regex.spans(bytes)
        };
        for span in search_spans {
            cancelled(stopped)?;
            if span.start == bytes.len() {
                if bytes.ends_with(b"\n") {
                    terminal_context = true;
                    break;
                }
                // LF-excluding iteration is line based: a first EOF hit on
                // this line is discovery, unlike advancing past an earlier hit.
                if !whole && previous_line == Some(lines.len() - 1) {
                    break;
                }
            }
            let first = line_at(&lines, span.start);
            let last = line_at(&lines, span.end.saturating_sub(1).max(span.start));
            add_group(&mut groups, first..last + 1)?;
            if whole || previous_line != Some(first) {
                searched += 1;
            }
            previous_line = Some(last);
            if !plan.invert && plan.max_count.is_some_and(|limit| searched >= limit) {
                clip = lines[last].end;
                break;
            }
        }
    }
    if plan.invert
        && !whole
        && let Some(mut left) = plan.max_count
    {
        let mut retained = 0;
        for group in &mut groups {
            cancelled(stopped)?;
            if left == 0 {
                break;
            }
            let keep = left.min((group.end - group.start) as u64) as usize;
            group.end = group.start + keep;
            left -= keep as u64;
            retained += 1;
        }
        groups.truncate(retained);
    }
    let selected = !groups.is_empty();
    if plan.output == Output::WithoutMatch {
        result.matched = !selected;
        return Ok(result);
    }
    result.matched = selected;
    if plan.quiet || plan.output == Output::Names {
        return Ok(result);
    }
    if !(selected || terminal_context && plan.before > 0 && plan.output == Output::Lines) {
        return Ok(result);
    }

    let match_count =
        !plan.invert && (whole || plan.only_matching || plan.output == Output::CountMatches);
    if match_count {
        let mut previous_start = None;
        for span in regex.spans(&bytes[..clip]) {
            cancelled(stopped)?;
            let first_on_line = previous_start.is_none_or(|start| start < last_line_start);
            let first_terminal = if whole {
                previous_start.is_none()
            } else {
                first_on_line
            };
            if span.start >= clip && (bytes[..clip].ends_with(b"\n") || !first_terminal) {
                break;
            }
            previous_start = Some(span.start);
            result.count += 1;
        }
    } else {
        for group in &groups {
            cancelled(stopped)?;
            result.count += (group.end - group.start) as u64;
        }
    }
    if plan.output != Output::Lines {
        return Ok(result);
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
    if terminal_context && plan.before > 0 {
        let before = usize::try_from(plan.before).unwrap_or(usize::MAX);
        add_group(
            &mut windows,
            lines.len().saturating_sub(before)..lines.len(),
        )?;
    }
    let mut spans: Vec<Range<usize>> = Vec::new();
    // Re-match the clipped prefix, then search the tail independently. A
    // cross-line match clipped by -m cannot colour or select its later suffix.
    for span in regex.spans(&bytes[..clip]) {
        cancelled(stopped)?;
        let first_on_line = spans.last().is_none_or(|span| span.start < last_line_start);
        let first_terminal = if whole {
            spans.is_empty()
        } else {
            first_on_line
        };
        if span.start >= clip && (bytes[..clip].ends_with(b"\n") || !first_terminal) {
            break;
        }
        push(&mut spans, span)?;
    }
    if clip < bytes.len() {
        // Trailing spans have independent advancement state; their discovery
        // cannot increase the clipped count computed above.
        let mut previous_start = None;
        for span in regex.spans_at(bytes, clip) {
            cancelled(stopped)?;
            let first_terminal =
                previous_start.is_none_or(|start| !whole && start < last_line_start);
            if span.start >= bytes.len() && (bytes.ends_with(b"\n") || !first_terminal) {
                break;
            }
            previous_start = Some(span.start);
            push(&mut spans, span)?;
        }
    }
    let mut tail_selection = Vec::new();
    if !plan.invert && clip < bytes.len() && plan.after > 0 {
        let mut previous_start = None;
        for span in regex.rebased_spans(&bytes[clip..]) {
            cancelled(stopped)?;
            let span = span.start + clip..span.end + clip;
            if !whole
                && span.start >= bytes.len()
                && (bytes.ends_with(b"\n")
                    || previous_start.is_some_and(|start| start >= last_line_start))
            {
                break;
            }
            previous_start = Some(span.start);
            push(&mut tail_selection, span)?;
        }
    }
    let (mut group_at, mut span_at, mut tail_at) = (0, 0, 0);
    let mut highlights: Vec<Range<usize>> = Vec::new();
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
            let range = &lines[index];
            let content_end = if bytes[range.end - 1] == b'\n' {
                range.end - 1
            } else {
                range.end
            };
            while span_at < spans.len()
                && (spans[span_at].end < range.start
                    || (!spans[span_at].is_empty() && spans[span_at].end == range.start))
            {
                cancelled(stopped)?;
                span_at += 1;
            }
            let mut end = span_at;
            while end < spans.len()
                && (spans[end].start < range.end
                    || (range.end == bytes.len()
                        && !bytes.ends_with(b"\n")
                        && spans[end].start == range.end))
            {
                cancelled(stopped)?;
                end += 1;
            }
            let relevant = &spans[span_at..end];
            while tail_at < tail_selection.len() && tail_selection[tail_at].end < range.start {
                cancelled(stopped)?;
                tail_at += 1;
            }
            let tail_match = tail_selection[tail_at..]
                .iter()
                .take_while(|span| {
                    span.start < range.end
                        || (range.end == bytes.len()
                            && (whole || !bytes.ends_with(b"\n"))
                            && span.start == range.end)
                })
                .any(|span| {
                    span.is_empty() || (span.start < content_end && span.end > range.start)
                });
            let tail_match = tail_match && (plan.replacement.is_none()
                || tail_selection[tail_at..].iter().any(|span| {
                    span.start >= range.start && span.start < range.end
                        && span.end <= lines[window.end - 1].end
                }));
            let kind = if selected || (!plan.invert && tail_match) {
                LineKind::Match
            } else {
                LineKind::Context
            };
            if plan.replacement.is_some() {
                if plan.invert {
                    replacement::line(plan, Line {
                        number: index as u64 + 1,
                        source_lines: Some((index as u64 + 1, index as u64 + 1)),
                        terminated: bytes[range.end - 1] == b'\n',
                        bytes: &bytes[range.start..content_end],
                        kind,
                        fragment: false,
                        highlights: None,
                    }, stopped, &mut emit).map_err(ScanError::Output)?;
                    index += 1;
                    continue;
                }
                let last = if whole && !plan.invert && selected {
                    groups[group_at].end.min(window.end)
                } else {
                    index + 1
                };
                let end = lines[last - 1].end;
                let range_end = if !whole && bytes[end - 1] == b'\n' { end - 1 } else { end };
                let mut local = Vec::new();
                let replacement_spans = if plan.invert {
                    for span in regex.spans(&bytes[range.clone()]) {
                        cancelled(stopped)?;
                        push(&mut local, span.start + range.start..span.end + range.start)?;
                    }
                    local.as_slice()
                } else {
                    let mut end_at = span_at;
                    while end_at < spans.len() && (spans[end_at].start < end
                        || (end == bytes.len() && !bytes.ends_with(b"\n") && spans[end_at].start == end)) {
                        cancelled(stopped)?;
                        end_at += 1;
                    }
                    if kind == LineKind::Context { &[] } else { &spans[span_at..end_at] }
                };
                replacement::region(
                    plan,
                    replacement::Region {
                        source: bytes,
                        clip: if plan.invert { bytes.len() } else { clip },
                        range: range.start..range_end,
                    },
                    replacement_spans.iter().cloned(),
                    Line {
                        number: index as u64 + 1,
                        source_lines: Some((index as u64 + 1, last as u64)),
                        terminated: bytes[end - 1] == b'\n',
                        bytes: &bytes[range.start..content_end],
                        kind,
                        fragment: false,
                        highlights: Some(&[]),
                    },
                    stopped,
                    &mut emit,
                ).map_err(ScanError::Output)?;
                index = last;
                continue;
            }
            if plan.only_matching && !plan.invert && kind == LineKind::Match {
                if !selected && relevant.is_empty() {
                    emit(Line {
                        source_lines: None,
                        terminated: true,
                        number: index as u64 + 1,
                        bytes: &bytes[range.start..content_end],
                        kind,
                        fragment: false,
                        highlights: Some(&[]),
                    })
                    .map_err(ScanError::Output)?;
                }
                for span in relevant {
                    cancelled(stopped)?;
                    if span.is_empty() {
                        if span.start == bytes.len() && !bytes.ends_with(b"\n") {
                            emit(Line {
                                source_lines: None,
                                terminated: true,
                                number: index as u64 + 1,
                                bytes: &bytes[range.start..content_end],
                                kind,
                                fragment: false,
                                highlights: Some(&[]),
                            })
                            .map_err(ScanError::Output)?;
                            continue;
                        }
                        if whole {
                            continue;
                        }
                    }
                    let from = span.start.max(range.start).min(content_end);
                    let to = span.end.min(content_end);
                    // A consuming LF alone has no printable fragment.
                    if from > to || (from == to && !span.is_empty()) {
                        continue;
                    }
                    emit(Line {
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
            } else {
                highlights.clear();
                let mut local = Vec::new();
                let relevant = if plan.invert {
                    for span in regex.spans(&bytes[range.clone()]) {
                        cancelled(stopped)?;
                        push(&mut local, span.start + range.start..span.end + range.start)?;
                    }
                    local.as_slice()
                } else {
                    relevant
                };
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
                    source_lines: None,
                    terminated: true,
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
    Ok(result)
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
        plan
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
    fn inversion_counts_selected_lines_even_when_rg_counts_positive_tail_matches() {
        // M7-A: rg reports 6 for this four-line gap. GREP.md explicitly
        // requires selected nonmatching lines, including --count-matches.
        for args in [
            vec!["-Uvcm1", "a|\\n"],
            vec!["-Uvm1", "--count-matches", "a|\\n"],
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
            assert!(result.matched);
            assert_eq!(result.count, 4);
        }
    }
}
