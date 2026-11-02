//! Grep's execution profiles; the argument grammar is in `args`. The host
//! supplies input resolution, policy selection and output; matching and
//! streaming are shared with content search through ferret-verify and `scan`.

use std::io::{self, Read};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ferret_verify::{Grep, Search};

mod args;
mod context;
mod descriptors;
mod events;
mod multiline;
mod replacement;

pub use args::{ParseError, own_flags, parse, parse_with};
pub use descriptors::{IoBudget, IoPermit};

/// Information requests skip environment, index and input resolution.
#[derive(Debug)]
pub enum Command {
    Help,
    Version,
    Search(Box<Plan>),
    Files(Names),
}

/// One path filter from argv, in argv order: `-g`, or `-t`/`-T` by type name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Filter {
    Glob(String),
    Type { name: String, include: bool },
}

/// Selected names only: no regex or document candidate plan is constructed.
#[derive(Clone, Debug)]
pub struct Names {
    pub paths: Vec<PathBuf>,
    pub filters: Vec<Filter>,
    pub quiet: bool,
    pub hidden: bool,
    pub presentation: Presentation,
    pub traversal: Traversal,
    pub sort: Option<Sort>,
}

/// rg's `--sort=modified` and `--sortr=modified`, for names only: every
/// listed name in one order by modification time. Equal times keep walk
/// order, which differs from rg's directory read order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    Oldest,
    Newest,
}

/// Namespace freshness and depth, shared by content and names-only modes.
#[derive(Clone, Copy, Debug, Default)]
pub struct Traversal {
    pub unrestricted: bool,
    pub live: bool,
    /// `--rg`: rg's own visibility rules, where ferret's are the default
    /// (D128): hidden names stay hidden, and an uncovered directory walks
    /// under rg's ignore files from `/` rather than ferret's policy.
    pub rg: bool,
    pub follow: bool,
    pub max_depth: Option<usize>,
}

/// Output choices whose defaults the host resolves from its actual stdout.
#[derive(Clone, Debug, Default)]
pub struct Presentation {
    pub filename: Option<bool>,
    pub heading: Option<bool>,
    pub numbers: Option<bool>,
    pub null: bool,
    pub color: Color,
}

/// The host resolves Auto from its real terminal and environment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Auto,
    Never,
    Always,
    Ansi,
}

impl Color {
    pub fn enabled(self) -> bool {
        matches!(self, Self::Always | Self::Ansi)
    }
}

impl Presentation {
    fn pretty(&mut self) {
        self.heading = Some(true);
        self.numbers = Some(true);
        self.color = Color::Always;
    }
}

/// The resolved line-output profile. Unsupported flags are rejected while
/// parsing, including later slices, before any input is opened.
#[derive(Clone, Debug)]
pub struct Plan {
    pub grep: Grep,
    pub paths: Vec<PathBuf>,
    pub filters: Vec<Filter>,
    pub line_number: bool,
    pub quiet: bool,
    pub hidden: bool,
    pub output: Output,
    pub only_matching: bool,
    pub invert: bool,
    pub stdin_patterns: bool,
    pub include_zero: bool,
    pub max_count: Option<u64>,
    /// rg's `--max-columns`: a rendered line longer than this many bytes,
    /// counting its terminator, prints as an omission notice. `None` for 0.
    pub max_columns: Option<u64>,
    pub before: u64,
    pub after: u64,
    pub presentation: Presentation,
    pub traversal: Traversal,
    pub text: bool,
    pub multiline: bool,
    pub json: bool,
    pub replacement: Option<Box<ferret_verify::Replacement>>,
    /// Where before-context windows spill. `None`, as parsed, reads the
    /// process's temporary directory when a spool is created; a resident host
    /// serving another process sets that caller's absolute directory.
    pub spill_dir: Option<Arc<Path>>,
    #[cfg(test)]
    force_per_line: bool,
}

/// Name and count switches override one another in argv order; quiet is
/// independent and overrides every rendering mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Output {
    #[default]
    Lines,
    Names,
    Count,
    CountMatches,
    WithoutMatch,
}

/// Names required after resolving output and selection switches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Universe {
    Candidates,
    Searchable,
}

impl Plan {
    /// Inversion, zero counts and no-match names enumerate searchable names.
    /// Only non-inverted snapshot output may use posting absence; it never
    /// proves a selected inverted line. Quiet positive searches need no zeros.
    pub fn universe(&self) -> Universe {
        if self.invert
            || self.output == Output::WithoutMatch
            || (!self.quiet
                && self.include_zero
                && matches!(self.output, Output::Count | Output::CountMatches))
        {
            Universe::Searchable
        } else {
            Universe::Candidates
        }
    }
}

/// Whether directory operand `path`, resolved against this process's cwd,
/// names the same directory from any other process given that cwd: its
/// resolution jumps through no procfs magic link (`/proc/self/cwd`,
/// `/proc/self/fd/N`, their `/dev/fd` aliases and symlinks to them) and ends
/// outside procfs, whose `self` entries differ by reader. A host that hands
/// an operand's spelling to another process keeps any other operand in its
/// own. Kernels without `openat2` answer false.
pub fn same_in_every_process(path: &Path) -> bool {
    use rustix::fs::{CWD, Mode, OFlags, PROC_SUPER_MAGIC, ResolveFlags};
    rustix::fs::openat2(
        CWD,
        path,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_MAGICLINKS,
    )
    .and_then(rustix::fs::fstatfs)
    .is_ok_and(|filesystem| filesystem.f_type != PROC_SUPER_MAGIC)
}

/// Necessary live documents under the shared trigram plan. Uncovered
/// documents are always included and absent indexes automatically scan.
/// The result is independent of line rendering and never establishes a hit.
pub fn candidates(
    atom: &crate::GrepAtom,
    pinned: &ferret_index::Pinned<'_>,
    stopped: &dyn Fn() -> bool,
) -> Result<ferret_index::DocSet, ferret_index::ReadError> {
    let mut cursor = atom.cursor_until(pinned, stopped)?;
    let mut docs = ferret_index::DocSet::empty(pinned.live().bound());
    let mut at = 0;
    while let Some((doc, _)) = cursor.next_geq(at) {
        if stopped() {
            return Err(ferret_index::ReadError::Cancelled);
        }
        docs.insert(doc);
        let Some(next) = doc.checked_add(1) else {
            break;
        };
        at = next;
    }
    Ok(docs)
}

/// The members of `among` that [`candidates`] holds. The cursor seeks to
/// each, so postings far from them are skipped rather than decoded: a small
/// subtree of a large index costs its own documents, not the index's (S5).
pub fn candidates_among(
    atom: &crate::GrepAtom,
    pinned: &ferret_index::Pinned<'_>,
    among: &ferret_index::DocSet,
    stopped: &dyn Fn() -> bool,
) -> Result<ferret_index::DocSet, ferret_index::ReadError> {
    let mut cursor = atom.cursor_until(pinned, stopped)?;
    let mut docs = ferret_index::DocSet::empty(pinned.live().bound());
    let mut target = among.next_geq(0);
    while let Some(at) = target {
        if stopped() {
            return Err(ferret_index::ReadError::Cancelled);
        }
        let Some((doc, _)) = cursor.next_geq(at) else {
            break;
        };
        target = if doc == at {
            docs.insert(doc);
            doc.checked_add(1).and_then(|next| among.next_geq(next))
        } else {
            among.next_geq(doc)
        };
    }
    Ok(docs)
}

/// Result of scanning one input. Binary detection replaces staged plain
/// output with a warning; an already-streamed prefix cannot be withdrawn.
/// A quiet match may finish before a later NUL is read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputResult {
    pub matched: bool,
    pub binary: bool,
    pub count: u64,
    pub searched: bool,
    /// First detected NUL, relative to bytes after initial BOM removal.
    pub binary_offset: Option<u64>,
    /// Semantic JSON counts describe published match records and their
    /// submatches; quiet counts the same (D91 A).
    pub stats: EventStats,
    pub bytes_searched: u64,
    pub elapsed: std::time::Duration,
}

/// Publication counts, independent of selection limits and plain count output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EventStats {
    pub matched_lines: u64,
    pub matches: u64,
}

impl InputResult {
    /// Successful empty input or snapshot posting absence, never an I/O fault.
    pub fn empty(plan: &Plan) -> Self {
        let searched = plan.max_count != Some(0);
        Self {
            searched,
            matched: searched && plan.output == Output::WithoutMatch,
            ..Self::default()
        }
    }
}

/// Input faults belong to the file. Output includes context staging faults;
/// the host distinguishes these from destination faults before publication.
#[derive(Debug)]
pub enum ScanError {
    Input(io::Error),
    Output(io::Error),
}

impl From<io::Error> for ScanError {
    fn from(error: io::Error) -> Self {
        Self::Input(error)
    }
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Input(error) | Self::Output(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ScanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Input(error) | Self::Output(error) => Some(error),
        }
    }
}

/// Emit selected records with 1-based line numbers after stripping one
/// initial UTF-8 BOM. Line mode searches without LF terminators; multiline
/// mode buffers the checked input. The host owns publication and retry policy.
pub fn scan(
    plan: &Plan,
    input: &mut dyn Read,
    size: u64,
    buffer: &mut Vec<u8>,
    stopped: &dyn Fn() -> bool,
    mut emit: impl FnMut(u64, &[u8]) -> io::Result<()>,
) -> Result<InputResult, ScanError> {
    scan_lines(plan, input, size, buffer, stopped, |line| {
        emit(line.number, line.bytes)
    })
}

/// A rendered line is either selected data (possibly one -o fragment) or
/// context from the same checked input attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Match,
    Context,
}

#[derive(Clone, Copy, Debug)]
pub struct Line<'a> {
    /// Original source line/group start after initial BOM removal. JSON
    /// records retain LF in `bytes`; ordinary rendered records omit it.
    pub offset: u64,
    /// Inclusive original source lines when replacement changes displayed
    /// line counts. Group separators use these boundaries, not display numbers.
    pub source_lines: Option<(u64, u64)>,
    /// Whether the original line had LF, which JSON restores to `bytes`.
    pub terminated: bool,
    /// One-based line number. The line-local paths count lines only when the
    /// plan prints numbers or JSON. Otherwise it is only ordered: it rises by
    /// one between adjacent lines and by more across skipped ones, which is
    /// all context separators read.
    pub number: u64,
    pub bytes: &'a [u8],
    pub kind: LineKind,
    pub fragment: bool,
    /// Resolved ranges relative to `bytes`; None permits line-local matching.
    pub highlights: Option<&'a [std::ops::Range<usize>]>,
}

/// The LFs in `bytes`, counted into 64 byte-wide lanes the compiler
/// vectorises, one 64-byte block at a time. A lane sees one byte a block, so
/// 255 blocks fill it; a run ends with its lanes summed and its last partial
/// block counted bytewise. `memchr_iter` pays per hit on dense text.
fn count_lf(bytes: &[u8]) -> u64 {
    const LANES: usize = 64;
    let mut total = 0;
    for run in bytes.chunks(255 * LANES) {
        let mut lanes = [0u8; LANES];
        let blocks = run.chunks_exact(LANES);
        let tail = blocks.remainder();
        for block in blocks {
            for (lane, &byte) in lanes.iter_mut().zip(block) {
                *lane += u8::from(byte == b'\n');
            }
        }
        total += lanes.iter().map(|&lane| u64::from(lane)).sum::<u64>();
        total += tail.iter().filter(|&&byte| byte == b'\n').count() as u64;
    }
    total
}

/// Rg's binary test for a recursive file's first window: any NUL byte.
pub fn binary_window(head: &[u8]) -> bool {
    memchr::memchr(0, head).is_some()
}

/// Stream selected records and bounded before/after context without a second
/// input read. Multiline buffers the checked input before producing records.
/// Context-spool failures are Output errors, which the host treats per file.
pub fn scan_lines(
    plan: &Plan,
    input: &mut dyn Read,
    size: u64,
    buffer: &mut Vec<u8>,
    stopped: &dyn Fn() -> bool,
    emit: impl FnMut(Line<'_>) -> io::Result<()>,
) -> Result<InputResult, ScanError> {
    scan_lines_progress(plan, input, size, buffer, stopped, emit).map_err(|(_, error)| error)
}

/// As [`scan_lines`], retaining observed statistics on failure. A streaming
/// host uses these to close a JSON group whose prefix is already published.
pub fn scan_lines_progress(
    plan: &Plan,
    input: &mut dyn Read,
    size: u64,
    buffer: &mut Vec<u8>,
    stopped: &dyn Fn() -> bool,
    emit: impl FnMut(Line<'_>) -> io::Result<()>,
) -> Result<InputResult, (InputResult, ScanError)> {
    let start = std::time::Instant::now();
    let mut result = InputResult::empty(plan);
    let outcome = if plan.json {
        events::scan(plan, input, size, buffer, stopped, &mut result, emit)
    } else {
        scan_records(plan, input, size, buffer, stopped, &mut result, emit)
    };
    result.elapsed = start.elapsed();
    outcome.map(|()| result).map_err(|error| (result, error))
}

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

fn scan_records(
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
                                let (hit, boundary) =
                                    next_hit(&search, text, start, open, before_lines);
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
                    match memchr::memchr(b'\n', rest) {
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
fn next_hit(
    search: &Search<'_>,
    text: &[u8],
    start: usize,
    open: bool,
    before: u64,
) -> (usize, usize) {
    let rest = &text[start..];
    let hit = search.matching_line(rest).map_or(usize::MAX, |hit| {
        start + memchr::memrchr(b'\n', &rest[..hit]).map_or(0, |at| at + 1)
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
    (hit, boundary)
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

#[cfg(test)]
mod tests;
