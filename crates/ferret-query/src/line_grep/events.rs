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
    let mut profile = plan.clone();
    profile.quiet = false;
    let mut stats = EventStats::default();
    let mut record = |line: Line<'_>| {
        if stopped() {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        if line.kind == LineKind::Match {
            let lines = memchr::memchr_iter(b'\n', line.bytes).count() as u64
                + u64::from(!line.bytes.ends_with(b"\n"));
            stats.matched_lines += lines;
            stats.matches += if plan.quiet && plan.invert {
                lines
            } else {
                line.highlights.map_or(0, |spans| spans.len() as u64)
            };
        }
        emit(line)
    };
    let search = plan.grep.search();
    let mut bytes = Vec::new();
    let mut spans = Vec::new();
    let mut result = scan_records(&profile, input, size, buffer, stopped, |line| {
        if plan.multiline {
            return record(line);
        }
        bytes.clear();
        bytes.try_reserve(line.bytes.len().saturating_add(1)).map_err(io::Error::other)?;
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
