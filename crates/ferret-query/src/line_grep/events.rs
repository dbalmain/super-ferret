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
            // Quiet reports the same stats as the events it suppresses (D91 A).
            stats.matches += line.highlights.map_or(0, |spans| spans.len() as u64);
        }
        emit(line)
    };
    let search = plan.grep.borrowed_search();
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
        // A record that already carries its spans (a never-searched context
        // line) keeps them.
        if let Some(highlights) = line.highlights {
            return record(Line {
                bytes: &bytes,
                highlights: Some(highlights),
                ..line
            });
        }
        for span in search.spans(line.bytes) {
            if stopped() {
                return Err(io::ErrorKind::ConnectionAborted.into());
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
