//! JSON's source records and semantic counts over the ordinary selection
//! scanner. Presentation flags cannot change source bytes or submatch bounds.

use super::*;

pub(super) fn scan(
    plan: &Plan,
    input: &mut dyn Read,
    size: u64,
    buffer: &mut Vec<u8>,
    stopped: &dyn Fn() -> bool,
    mut emit: impl FnMut(Line<'_>) -> io::Result<()>,
) -> Result<InputResult, ScanError> {
    // JSON's quiet summary still needs the selected file's complete stats.
    // The host suppresses records and stops at its first stable matching file.
    let mut stats = EventStats::default();
    let mut record = |line: Line<'_>| {
        if stopped() {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        if line.kind == LineKind::Match {
            let mut lines = u64::from(!line.bytes.ends_with(b"\n"));
            for chunk in line.bytes.chunks(crate::scan::BUFFER) {
                if stopped() {
                    return Err(io::ErrorKind::ConnectionAborted.into());
                }
                lines += memchr::memchr_iter(b'\n', chunk).count() as u64;
            }
            stats.matched_lines += lines;
            stats.matches += if !plan.quiet {
                line.highlights.map_or(0, |spans| spans.len() as u64)
            } else if plan.invert && plan.grep.excludes_lf() {
                lines
            } else {
                // Quiet has no submatch presentation: its source-group
                // counting iterator retains a first terminal assertion,
                // while omitting EOF reached by advancing an earlier match.
                let source = if !plan.multiline && line.terminated {
                    line.bytes.strip_suffix(b"\n").unwrap_or(line.bytes)
                } else {
                    line.bytes
                };
                let mut count = 0;
                for span in plan.grep.regex().spans(source) {
                    if stopped() {
                        return Err(io::ErrorKind::ConnectionAborted.into());
                    }
                    if span.start == source.len()
                        && count > 0
                        && (plan.multiline || !line.terminated)
                    {
                        break;
                    }
                    count += 1;
                }
                // Without a submatch printer, inverted stats retain at least
                // one logical match per selected source line.
                if plan.invert { count.max(lines) } else { count }
            };
        }
        emit(line)
    };
    let search = plan.grep.search();
    let mut bytes = Vec::new();
    let mut spans = Vec::new();
    let mut result = scan_records(plan, input, size, buffer, stopped, |line| {
        if plan.multiline {
            return record(line);
        }
        bytes.clear();
        bytes
            .try_reserve(line.bytes.len().saturating_add(1))
            .map_err(io::Error::other)?;
        bytes.extend_from_slice(line.bytes);
        if line.terminated {
            bytes.push(b'\n');
        }
        spans.clear();
        for span in search.spans(line.bytes) {
            if stopped() {
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            // rg's JSON presentation excludes terminal EOF empties on an
            // unterminated line, while selection still retains discovery.
            if span.start == line.bytes.len() && !line.terminated {
                break;
            }
            spans.try_reserve(1).map_err(io::Error::other)?;
            spans.push(span);
        }
        record(Line {
            bytes: &bytes,
            highlights: Some(&spans),
            ..line
        })
    })?;
    result.stats = stats;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn json_cancellation_after_a_record_stops_before_the_next_source_record() {
        for flags in ["--json", "-U"] {
            let Command::Search(plan) =
                parse(&["--json".into(), flags.into(), "red".into()]).unwrap()
            else {
                panic!("search")
            };
            let stopped = Cell::new(false);
            let mut records = 0;
            let bytes = b"red\nred\nred\n";
            let result = scan_lines(
                &plan,
                &mut &bytes[..],
                bytes.len() as u64,
                &mut Vec::new(),
                &|| stopped.get(),
                |_| {
                    records += 1;
                    stopped.set(true);
                    Ok(())
                },
            );
            assert!(result.is_err());
            assert_eq!(records, 1);
        }
    }
}
