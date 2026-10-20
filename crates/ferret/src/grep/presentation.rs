//! Terminal defaults and rg's default ANSI palette. All byte emission still
//! passes through the existing cancellable group/spill writer.

use std::io::{self, IsTerminal, Write};

use ferret_query::line_grep::{Color, Line, Plan, Presentation};

const RESET: &[u8] = b"\x1b[0m";
const PATH: &[u8] = b"\x1b[0m\x1b[35m";
const NUMBER: &[u8] = b"\x1b[0m\x1b[32m";
const MATCH: &[u8] = b"\x1b[0m\x1b[1m\x1b[31m";

pub(super) fn resolve(presentation: &mut Presentation) -> bool {
    let tty = io::stdout().is_terminal();
    presentation.heading = Some(presentation.heading.unwrap_or(tty));
    if presentation.color == Color::Auto {
        presentation.color = if tty
            && std::env::var_os("NO_COLOR").is_none()
            && std::env::var_os("TERM").is_none_or(|term| term != "dumb")
        {
            Color::Always
        } else {
            Color::Never
        };
    }
    tty
}

fn paint(writer: &mut impl Write, bytes: &[u8], style: &[u8], enabled: bool) -> io::Result<()> {
    if enabled && !bytes.is_empty() {
        writer.write_all(style)?;
        writer.write_all(bytes)?;
        writer.write_all(RESET)
    } else {
        writer.write_all(bytes)
    }
}

pub(super) fn path(
    presentation: &Presentation,
    writer: &mut impl Write,
    bytes: &[u8],
) -> io::Result<()> {
    paint(writer, bytes, PATH, presentation.color.enabled())
}

pub(super) fn number(
    presentation: &Presentation,
    writer: &mut impl Write,
    number: u64,
) -> io::Result<()> {
    if presentation.color.enabled() {
        writer.write_all(NUMBER)?;
    }
    write!(writer, "{number}")?;
    if presentation.color.enabled() {
        writer.write_all(RESET)?;
    }
    Ok(())
}

pub(super) fn text(plan: &Plan, writer: &mut impl Write, line: Line<'_>) -> io::Result<()> {
    if !plan.presentation.color.enabled() {
        return writer.write_all(line.bytes);
    }
    // -o fragments were matched against their original complete line. Do not
    // re-run anchors or word assertions against a detached span.
    if line.fragment {
        return paint(writer, line.bytes, MATCH, true);
    }
    if let Some(spans) = line.highlights {
        highlighted(writer, line.bytes, spans.iter().cloned())
    } else {
        highlighted(
            writer,
            line.bytes,
            plan.grep.borrowed_search().spans(line.bytes),
        )
    }
}

fn highlighted(
    writer: &mut impl Write,
    bytes: &[u8],
    spans: impl Iterator<Item = std::ops::Range<usize>>,
) -> io::Result<()> {
    let mut at = 0;
    for span in spans {
        if span.is_empty() {
            continue;
        }
        writer.write_all(&bytes[at..span.start])?;
        paint(writer, &bytes[span.clone()], MATCH, true)?;
        at = span.end;
    }
    writer.write_all(&bytes[at..])
}
