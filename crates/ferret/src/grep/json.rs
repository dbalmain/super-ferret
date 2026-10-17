//! rg JSON Lines, separate from ferret's envelope schema. Byte fields choose
//! text or padded base64 independently. Encoding streams into the file spill;
//! even a multiline event needs no second whole-group buffer.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use ferret_query::line_grep::{InputResult, Line, LineKind};

#[derive(Default)]
pub(super) struct Stats {
    searches: u64,
    searches_with_match: u64,
    matched_lines: u64,
    matches: u64,
    bytes_searched: u64,
    bytes_printed: u64,
    elapsed: Duration,
}

impl Stats {
    pub fn add(&mut self, result: InputResult, printed: u64) {
        self.searches += u64::from(result.searched);
        self.searches_with_match += u64::from(result.matched);
        self.matched_lines += result.stats.matched_lines;
        self.matches += result.stats.matches;
        self.bytes_searched += result.bytes_searched;
        self.bytes_printed += printed;
        self.elapsed += result.elapsed;
    }

    pub fn merge(&mut self, other: Self) {
        self.searches += other.searches;
        self.searches_with_match += other.searches_with_match;
        self.matched_lines += other.matched_lines;
        self.matches += other.matches;
        self.bytes_searched += other.bytes_searched;
        self.bytes_printed += other.bytes_printed;
        self.elapsed += other.elapsed;
    }

    fn write(&self, out: &mut dyn Write) -> io::Result<()> {
        out.write_all(b"{\"elapsed\":")?;
        duration(out, self.elapsed)?;
        write!(
            out,
            ",\"searches\":{},\"searches_with_match\":{},\"bytes_searched\":{},\"bytes_printed\":{},\"matched_lines\":{},\"matches\":{}}}",
            self.searches,
            self.searches_with_match,
            self.bytes_searched,
            self.bytes_printed,
            self.matched_lines,
            self.matches
        )
    }
}

fn duration(out: &mut dyn Write, elapsed: Duration) -> io::Result<()> {
    write!(
        out,
        "{{\"secs\":{},\"nanos\":{},\"human\":\"{:.6}s\"}}",
        elapsed.as_secs(),
        elapsed.subsec_nanos(),
        elapsed.as_secs_f64()
    )
}

fn field(out: &mut dyn Write, bytes: &[u8]) -> io::Result<()> {
    let mut encoded = Vec::new();
    if std::str::from_utf8(bytes).is_ok() {
        out.write_all(b"{\"text\":\"")?;
        for chunk in bytes.chunks(8192) {
            encoded.clear();
            for &byte in chunk {
                match byte {
                    b'"' => encoded.extend_from_slice(b"\\\""),
                    b'\\' => encoded.extend_from_slice(b"\\\\"),
                    b'\n' => encoded.extend_from_slice(b"\\n"),
                    b'\r' => encoded.extend_from_slice(b"\\r"),
                    b'\t' => encoded.extend_from_slice(b"\\t"),
                    0..0x20 => write!(encoded, "\\u{byte:04x}")?,
                    _ => encoded.push(byte),
                }
            }
            out.write_all(&encoded)?;
        }
    } else {
        out.write_all(b"{\"bytes\":\"")?;
        // All nonfinal chunks end at a base64 quantum boundary.
        for chunk in bytes.chunks(3 * 8192) {
            encoded.clear();
            crate::json::encode_base64(&mut encoded, chunk);
            out.write_all(&encoded)?;
        }
    }
    out.write_all(b"\"}")
}

pub(super) fn begin(out: &mut dyn Write, path: &[u8]) -> io::Result<()> {
    out.write_all(b"{\"type\":\"begin\",\"data\":{\"path\":")?;
    field(out, path)?;
    out.write_all(b"}}\n")
}

pub(super) fn line(
    out: &mut dyn Write,
    path: &[u8],
    numbers: bool,
    line: Line<'_>,
) -> io::Result<()> {
    let kind = if line.kind == LineKind::Match {
        "match"
    } else {
        "context"
    };
    write!(out, "{{\"type\":\"{kind}\",\"data\":{{\"path\":")?;
    field(out, path)?;
    out.write_all(b",\"lines\":")?;
    field(out, line.bytes)?;
    out.write_all(b",\"line_number\":")?;
    if numbers {
        write!(out, "{}", line.number)?;
    } else {
        out.write_all(b"null")?;
    }
    write!(out, ",\"absolute_offset\":{},\"submatches\":[", line.offset)?;
    for (at, span) in line.highlights.unwrap_or_default().iter().enumerate() {
        if at > 0 {
            out.write_all(b",")?;
        }
        out.write_all(b"{\"match\":")?;
        field(out, &line.bytes[span.clone()])?;
        write!(out, ",\"start\":{},\"end\":{}}}", span.start, span.end)?;
    }
    out.write_all(b"]}}\n")
}

pub(super) fn end(
    out: &mut dyn Write,
    path: &[u8],
    result: InputResult,
    printed: u64,
) -> io::Result<()> {
    out.write_all(b"{\"type\":\"end\",\"data\":{\"path\":")?;
    field(out, path)?;
    out.write_all(b",\"binary_offset\":")?;
    if let Some(offset) = result.binary_offset {
        write!(out, "{offset}")?;
    } else {
        out.write_all(b"null")?;
    }
    out.write_all(b",\"stats\":")?;
    let mut stats = Stats::default();
    stats.add(result, printed);
    stats.write(out)?;
    out.write_all(b"}}\n")
}

pub(super) fn summary(out: &mut dyn Write, stats: &Stats, start: Instant) -> io::Result<()> {
    out.write_all(b"{\"type\":\"summary\",\"data\":{\"elapsed_total\":")?;
    duration(out, start.elapsed())?;
    out.write_all(b",\"stats\":")?;
    stats.write(out)?;
    out.write_all(b"}}\n")
}
