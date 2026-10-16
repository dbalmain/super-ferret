//! Grep's line-oriented profile and argument grammar. The host supplies input
//! resolution, policy selection and output; matching and streaming are shared
//! with content search through ferret-verify and `scan`.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use ferret_verify::{Grep, GrepFlags, LineFlags};

mod context;
mod multiline;

/// Information requests skip environment, index and input resolution.
#[derive(Debug)]
pub enum Command {
    Help,
    Version,
    Search(Plan),
    Files(Names),
}

/// Selected names only: no regex or document candidate plan is constructed.
#[derive(Clone, Debug)]
pub struct Names {
    pub paths: Vec<PathBuf>,
    pub globs: Vec<String>,
    pub quiet: bool,
    pub hidden: bool,
    pub presentation: Presentation,
    pub traversal: Traversal,
}

/// Namespace freshness and depth, shared by content and names-only modes.
#[derive(Clone, Copy, Debug, Default)]
pub struct Traversal {
    pub unrestricted: bool,
    pub live: bool,
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
    pub globs: Vec<String>,
    pub line_number: bool,
    pub quiet: bool,
    pub hidden: bool,
    pub output: Output,
    pub only_matching: bool,
    pub invert: bool,
    pub stdin_patterns: bool,
    pub include_zero: bool,
    pub max_count: Option<u64>,
    pub before: u64,
    pub after: u64,
    pub presentation: Presentation,
    pub traversal: Traversal,
    pub text: bool,
    pub multiline: bool,
}

/// Name and count switches override one another in argv order; quiet is
/// independent and overrides every rendering mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
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

/// A grep argument/profile failure, before any input is opened.
#[derive(Debug)]
pub enum ParseError {
    MissingPattern,
    Value(String),
    Number(String),
    Context(String),
    Depth(String),
    Utf8(&'static str),
    Unsupported(String),
    Pattern(ferret_verify::GrepError),
    PatternFile(PathBuf, io::Error),
    StdinConflict,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingPattern => f.write_str("a pattern is required"),
            Self::Value(flag) => write!(f, "{flag} needs a value"),
            Self::Number(value) => write!(
                f,
                "invalid --max-count: {value:?}; expected a nonnegative integer"
            ),
            Self::Depth(value) => write!(
                f,
                "invalid --max-depth {value:?}; expected a nonnegative integer"
            ),
            Self::Context(value) => write!(
                f,
                "invalid context size {value:?}; expected a nonnegative integer"
            ),
            Self::Utf8(argument) => write!(f, "{argument} must be UTF-8"),
            Self::Unsupported(flag) => {
                write!(f, "{flag} is not supported; use rg for this command")
            }
            Self::Pattern(error) => error.fmt(f),
            Self::PatternFile(path, error) => write!(f, "{}: {error}", path.display()),
            Self::StdinConflict => {
                f.write_str("cannot read patterns and search input from stdin together")
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse rg spellings, clusters, attached values and `--`. Glob validation
/// follows in the policy adapter, still before input resolution.
pub fn parse(args: &[OsString]) -> Result<Command, ParseError> {
    parse_with(args, &mut |path| {
        if path == Path::new("-") {
            let mut bytes = Vec::new();
            io::stdin().lock().read_to_end(&mut bytes)?;
            Ok(bytes)
        } else {
            std::fs::read(path)
        }
    })
}

/// Parse using an input adapter for pattern files. Resolved names-only and
/// information requests never invoke it. The production wrapper supplies
/// filesystem/stdin reads; tests can observe this actual I/O boundary.
pub fn parse_with(
    args: &[OsString],
    read: &mut dyn FnMut(&Path) -> io::Result<Vec<u8>>,
) -> Result<Command, ParseError> {
    let mut positionals = Vec::new();
    let mut sources = Vec::new();
    let mut globs = Vec::new();
    let (mut line_number, mut quiet, mut done) = (false, false, false);
    let (mut fold, mut fixed, mut invert, mut only_matching) = (false, false, false, false);
    let mut smart_case = false;
    let (mut word, mut line) = (false, false);
    let mut hidden = false;
    let mut text_mode = false;
    let (mut multiline, mut dotall) = (false, false);
    let mut unrestricted_count = 0;
    let mut traversal = Traversal::default();
    let mut files = false;
    let mut include_zero = false;
    let mut max_count = None;
    let (mut before, mut after, mut around) = (None, None, 0);
    let mut presentation = Presentation::default();
    let mut output = Output::Lines;
    let (mut help, mut version) = (false, false);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let bytes = arg.as_bytes();
        if !done && bytes == b"--" {
            done = true;
            continue;
        }
        if !done && (bytes == b"--file" || bytes.starts_with(b"--file=")) {
            let path = if bytes == b"--file" {
                args.next().map(|arg| PathBuf::from(arg.as_os_str()))
            } else {
                Some(PathBuf::from(OsStr::from_bytes(&bytes[7..])))
            }
            .ok_or_else(|| ParseError::Value("--file".to_owned()))?;
            sources.push(PatternSource::File(path));
            continue;
        }
        if !done && bytes.starts_with(b"--") {
            let text = arg.to_str().ok_or(ParseError::Utf8("option"))?;
            let (flag, value) = text
                .split_once('=')
                .map_or((text, None), |(f, v)| (f, Some(v)));
            match flag {
                "--help" if value.is_none() => help = true,
                "--version" if value.is_none() => version = true,
                "--line-number" if value.is_none() => {
                    line_number = true;
                    presentation.numbers = Some(true);
                }
                "--no-line-number" if value.is_none() => {
                    line_number = false;
                    presentation.numbers = Some(false);
                }
                "--with-filename" if value.is_none() => presentation.filename = Some(true),
                "--no-filename" if value.is_none() => presentation.filename = Some(false),
                "--heading" if value.is_none() => presentation.heading = Some(true),
                "--no-heading" if value.is_none() => presentation.heading = Some(false),
                "--null" if value.is_none() => presentation.null = true,
                "--pretty" if value.is_none() => {
                    presentation.pretty();
                    line_number = true;
                }
                "--quiet" if value.is_none() => quiet = true,
                "--live" if value.is_none() => traversal.live = true,
                "--no-ignore" if value.is_none() => traversal.unrestricted = true,
                "--ignore" if value.is_none() => traversal.unrestricted = false,
                "--unrestricted" if value.is_none() => {
                    unrestricted(&mut unrestricted_count, &mut traversal, &mut hidden)?
                }
                "--multiline" if value.is_none() => multiline = true,
                "--no-multiline" if value.is_none() => multiline = false,
                "--multiline-dotall" if value.is_none() => dotall = true,
                "--no-multiline-dotall" if value.is_none() => dotall = false,
                "--text" if value.is_none() => text_mode = true,
                "--no-text" if value.is_none() => text_mode = false,
                "--hidden" if value.is_none() => hidden = true,
                "--no-hidden" if value.is_none() => hidden = false,
                "--ignore-case" if value.is_none() => {
                    fold = true;
                    smart_case = false;
                }
                "--case-sensitive" if value.is_none() => {
                    fold = false;
                    smart_case = false;
                }
                "--smart-case" if value.is_none() => {
                    fold = false;
                    smart_case = true;
                }
                "--word-regexp" if value.is_none() => {
                    word = true;
                    line = false;
                }
                "--line-regexp" if value.is_none() => {
                    line = true;
                    word = false;
                }
                "--fixed-strings" if value.is_none() => fixed = true,
                "--no-fixed-strings" if value.is_none() => fixed = false,
                "--invert-match" if value.is_none() => invert = true,
                "--no-invert-match" if value.is_none() => invert = false,
                "--only-matching" if value.is_none() => only_matching = true,
                "--files" if value.is_none() => files = true,
                "--include-zero" if value.is_none() => include_zero = true,
                "--no-include-zero" if value.is_none() => include_zero = false,
                "--count-matches" if value.is_none() => {
                    output = Output::CountMatches;
                    files = false;
                }
                "--files-without-match" if value.is_none() => {
                    output = Output::WithoutMatch;
                    files = false;
                }
                "--files-with-matches" if value.is_none() => {
                    output = Output::Names;
                    files = false;
                }
                "--count" if value.is_none() => {
                    output = Output::Count;
                    files = false;
                }
                "--scan" | "--scan-uncovered" if value.is_none() => {}
                "--glob" | "--color" | "--regexp" | "--max-count" | "--before-context"
                | "--after-context" | "--context" | "--max-depth" => {
                    let value = value
                        .map(str::to_owned)
                        .or_else(|| args.next().and_then(|s| s.to_str()).map(str::to_owned))
                        .ok_or_else(|| ParseError::Value(flag.to_owned()))?;
                    match flag {
                        "--before-context" | "--after-context" | "--context" => {
                            let number = value.parse().map_err(|_| ParseError::Context(value))?;
                            match flag {
                                "--before-context" => before = Some(number),
                                "--after-context" => after = Some(number),
                                _ => around = number,
                            }
                        }
                        "--max-depth" => {
                            traversal.max_depth =
                                Some(value.parse().map_err(|_| ParseError::Depth(value))?)
                        }
                        "--max-count" => {
                            max_count = Some(value.parse().map_err(|_| ParseError::Number(value))?)
                        }
                        "--glob" => globs.push(value),
                        "--regexp" => sources.push(PatternSource::Argument(value)),
                        _ => {
                            presentation.color = match value.as_str() {
                                "auto" => Color::Auto,
                                "never" => Color::Never,
                                "always" => Color::Always,
                                "ansi" => Color::Ansi,
                                _ => return Err(unsupported(text)),
                            }
                        }
                    }
                }
                _ => return Err(unsupported(text)),
            }
        } else if !done && bytes.starts_with(b"-") && bytes.len() > 1 {
            for (at, &flag) in bytes.iter().enumerate().skip(1) {
                match flag {
                    b'n' => {
                        line_number = true;
                        presentation.numbers = Some(true);
                    }
                    b'N' => {
                        line_number = false;
                        presentation.numbers = Some(false);
                    }
                    b'H' => presentation.filename = Some(true),
                    b'I' => presentation.filename = Some(false),
                    b'0' => presentation.null = true,
                    b'p' => {
                        presentation.pretty();
                        line_number = true;
                    }
                    b'q' => quiet = true,
                    b'u' => unrestricted(&mut unrestricted_count, &mut traversal, &mut hidden)?,
                    b'a' => text_mode = true,
                    b'U' => multiline = true,
                    b'.' => hidden = true,
                    b'i' => {
                        fold = true;
                        smart_case = false;
                    }
                    b's' => {
                        fold = false;
                        smart_case = false;
                    }
                    b'S' => {
                        fold = false;
                        smart_case = true;
                    }
                    b'w' => {
                        word = true;
                        line = false;
                    }
                    b'x' => {
                        line = true;
                        word = false;
                    }
                    b'F' => fixed = true,
                    b'v' => invert = true,
                    b'o' => only_matching = true,
                    b'l' => {
                        output = Output::Names;
                        files = false;
                    }
                    b'c' => {
                        output = Output::Count;
                        files = false;
                    }
                    b'h' => help = true,
                    b'V' => version = true,
                    b'f' => {
                        let path = if at + 1 < bytes.len() {
                            Some(PathBuf::from(OsStr::from_bytes(&bytes[at + 1..])))
                        } else {
                            args.next().map(|arg| PathBuf::from(arg.as_os_str()))
                        }
                        .ok_or_else(|| ParseError::Value("-f".to_owned()))?;
                        sources.push(PatternSource::File(path));
                        break;
                    }
                    b'g' | b'e' | b'm' | b'A' | b'B' | b'C' => {
                        let value = if at + 1 < bytes.len() {
                            std::str::from_utf8(&bytes[at + 1..]).ok()
                        } else {
                            args.next().and_then(|s| s.to_str())
                        }
                        .ok_or_else(|| ParseError::Value(format!("-{}", char::from(flag))))?;
                        if matches!(flag, b'A' | b'B' | b'C') {
                            let number = value
                                .parse()
                                .map_err(|_| ParseError::Context(value.to_owned()))?;
                            match flag {
                                b'A' => after = Some(number),
                                b'B' => before = Some(number),
                                _ => around = number,
                            }
                        } else if flag == b'm' {
                            max_count = Some(
                                value
                                    .parse()
                                    .map_err(|_| ParseError::Number(value.to_owned()))?,
                            );
                        } else if flag == b'g' {
                            globs.push(value.to_owned());
                        } else {
                            sources.push(PatternSource::Argument(value.to_owned()));
                        }
                        break;
                    }
                    _ => return Err(unsupported(&format!("-{}", char::from(flag)))),
                }
            }
        } else {
            positionals.push(arg.clone());
        }
    }
    if help {
        return Ok(Command::Help);
    }
    if version {
        return Ok(Command::Version);
    }
    if files {
        return Ok(Command::Files(Names {
            paths: positionals.into_iter().map(PathBuf::from).collect(),
            globs,
            quiet,
            hidden,
            presentation,
            traversal,
        }));
    }
    if sources.is_empty() {
        if positionals.is_empty() {
            return Err(ParseError::MissingPattern);
        }
        sources.push(PatternSource::Argument(
            positionals
                .remove(0)
                .into_string()
                .map_err(|_| ParseError::Utf8("pattern"))?,
        ));
    }
    let paths: Vec<PathBuf> = positionals.into_iter().map(PathBuf::from).collect();
    let stdin_patterns = sources
        .iter()
        .any(|source| matches!(source, PatternSource::File(path) if path == Path::new("-")));
    if stdin_patterns && paths.iter().any(|path| path == Path::new("-")) {
        return Err(ParseError::StdinConflict);
    }
    let mut patterns = Vec::new();
    for source in sources {
        match source {
            PatternSource::Argument(pattern) => patterns.push(pattern),
            PatternSource::File(path) => {
                let bytes =
                    read(&path).map_err(|error| ParseError::PatternFile(path.clone(), error))?;
                let text = std::str::from_utf8(&bytes).map_err(|error| {
                    ParseError::PatternFile(path, io::Error::new(io::ErrorKind::InvalidData, error))
                })?;
                for line in text.split_inclusive('\n') {
                    let line = line
                        .strip_suffix('\n')
                        .map_or(line, |line| line.strip_suffix('\r').unwrap_or(line));
                    patterns.push(line.to_owned());
                }
            }
        }
    }
    if max_count == Some(0) {
        patterns.clear();
    }
    let grep = GrepFlags {
        case_insensitive: fold,
    }
    .compile_line_profile(
        &patterns,
        LineFlags {
            fixed,
            smart_case,
            word,
            line,
            multiline,
            dotall,
        },
    )
    .map_err(ParseError::Pattern)?;
    Ok(Command::Search(Plan {
        grep,
        paths,
        globs,
        line_number,
        quiet,
        hidden,
        output,
        only_matching,
        invert,
        stdin_patterns,
        include_zero,
        max_count,
        before: before.unwrap_or(around),
        after: after.unwrap_or(around),
        presentation,
        traversal,
        text: text_mode,
        multiline,
    }))
}

// Each -u occurrence applies its next widening step at that argv position.
// --ignore can reverse the first without resetting the occurrence count.
fn unrestricted(
    count: &mut u8,
    traversal: &mut Traversal,
    hidden: &mut bool,
) -> Result<(), ParseError> {
    *count += 1;
    match *count {
        1 => traversal.unrestricted = true,
        2 => *hidden = true,
        _ => return Err(unsupported("-uuu")),
    }
    Ok(())
}

enum PatternSource {
    Argument(String),
    File(PathBuf),
}

fn unsupported(flag: &str) -> ParseError {
    ParseError::Unsupported(flag.to_owned())
}

/// Necessary live documents under the shared trigram plan. Uncovered
/// documents are always included and absent indexes automatically scan.
/// The result is independent of line rendering and never establishes a hit.
pub fn candidates(
    grep: &Grep,
    pinned: &ferret_index::Pinned<'_>,
    stopped: &dyn Fn() -> bool,
) -> Result<ferret_index::DocSet, ferret_index::ReadError> {
    let atom = crate::GrepAtom::read_until(grep.clone(), pinned, stopped)?;
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

/// Result of scanning one input. Binary detection discards the input's plain
/// output; a quiet match may finish before a later NUL is read.
#[derive(Clone, Copy, Debug, Default)]
pub struct InputResult {
    pub matched: bool,
    pub binary: bool,
    pub count: u64,
    pub searched: bool,
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

/// Input faults may be recoverable per file; an output failure must stop the
/// command, even when it occurs inside the checked-reader callback.
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

/// Searches LF-delimited lines without their terminators; one initial UTF-8
/// BOM is stripped. Every selected line is passed once to `emit` with its
/// 1-based number. The host buffers output until a checked read succeeds.
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
    pub number: u64,
    pub bytes: &'a [u8],
    pub kind: LineKind,
    pub fragment: bool,
    pub highlights: Option<&'a [std::ops::Range<usize>]>,
}

/// Stream selected records and bounded before/after context without a second
/// input read. All context-spool failures retain output-fault classification.
pub fn scan_lines(
    plan: &Plan,
    input: &mut dyn Read,
    size: u64,
    buffer: &mut Vec<u8>,
    stopped: &dyn Fn() -> bool,
    mut emit: impl FnMut(Line<'_>) -> io::Result<()>,
) -> Result<InputResult, ScanError> {
    if plan.multiline {
        return multiline::scan(plan, input, size, buffer, stopped, emit);
    }
    let search = plan.grep.search();
    let mut result = InputResult::empty(plan);
    if !result.searched {
        return Ok(result);
    }
    let mut selected_lines = 0;
    let mut number = 0;
    let rendering = plan.output == Output::Lines && !plan.quiet;
    let after = if rendering { plan.after } else { 0 };
    let mut before = context::Before::new(if rendering { plan.before } else { 0 }, stopped);
    let mut trailing = 0;
    let mut limited = false;
    let mut emit = |line: Line<'_>| {
        if line.kind == LineKind::Context && plan.only_matching && plan.invert {
            for span in search.spans(line.bytes) {
                if stopped() {
                    return Err(io::ErrorKind::ConnectionAborted.into());
                }
                emit(Line {
                    bytes: &line.bytes[span],
                    fragment: true,
                    ..line
                })?;
            }
            Ok(())
        } else {
            emit(line)
        }
    };
    crate::scan::runs(
        input,
        size,
        buffer,
        stopped,
        (!plan.text).then_some(0),
        |run, base, terminated| {
            let run = if base == 0 {
                run.strip_prefix(b"\xef\xbb\xbf").unwrap_or(run)
            } else {
                run
            };
            let binary = (!plan.text).then(|| memchr::memchr(0, run)).flatten();
            // M4 R1: a selected-line limit cannot hide a sentinel already
            // received, even when it follows a complete selected line.
            result.binary = binary.is_some();
            let text = &run[..binary.unwrap_or(run.len())];
            let mut lines = text.split(|&b| b == b'\n').peekable();
            while let Some(line) = lines.next() {
                // No extra line after a terminal LF or at empty EOF. A partial
                // line before NUL is searched: NUL is a binary sentinel.
                if line.is_empty() && lines.peek().is_none() && !terminated && binary.is_none() {
                    break;
                }
                number += 1;
                if stopped() {
                    return Err(ScanError::Input(io::ErrorKind::Interrupted.into()));
                }
                let selected = search.find(line).is_some() != plan.invert;
                if limited {
                    if rendering {
                        if selected && plan.only_matching && !plan.invert {
                            for span in search.spans(line) {
                                emit(Line {
                                    number,
                                    bytes: &line[span],
                                    kind: LineKind::Match,
                                    fragment: true,
                                    highlights: None,
                                })
                                .map_err(ScanError::Output)?;
                            }
                        } else {
                            emit(Line {
                                number,
                                bytes: line,
                                kind: if selected {
                                    LineKind::Match
                                } else {
                                    LineKind::Context
                                },
                                fragment: false,
                                highlights: None,
                            })
                            .map_err(ScanError::Output)?;
                        }
                    }
                    if number >= trailing {
                        return Ok(ControlFlow::Break(()));
                    }
                    continue;
                }
                if selected {
                    if plan.output == Output::WithoutMatch {
                        result.matched = false;
                        return Ok(ControlFlow::Break(()));
                    }
                    selected_lines += 1;
                    result.matched = true;
                    if plan.quiet || plan.output == Output::Names {
                        return Ok(ControlFlow::Break(()));
                    }
                    if rendering {
                        before.emit(&mut emit).map_err(ScanError::Output)?;
                    }
                    trailing = number.saturating_add(after);
                    if (plan.only_matching || plan.output == Output::CountMatches) && !plan.invert {
                        for span in search.spans(line) {
                            if stopped() {
                                return Err(ScanError::Input(io::ErrorKind::Interrupted.into()));
                            }
                            result.count += 1;
                            if plan.output == Output::Lines {
                                emit(Line {
                                    number,
                                    bytes: &line[span],
                                    kind: LineKind::Match,
                                    fragment: true,
                                    highlights: None,
                                })
                                .map_err(ScanError::Output)?;
                            }
                        }
                    } else {
                        result.count += 1;
                        if plan.output == Output::Lines {
                            emit(Line {
                                number,
                                bytes: line,
                                kind: LineKind::Match,
                                fragment: false,
                                highlights: None,
                            })
                            .map_err(ScanError::Output)?;
                        }
                    }
                    if plan.max_count.is_some_and(|limit| selected_lines >= limit) {
                        limited = true;
                        if after == 0 {
                            return Ok(ControlFlow::Break(()));
                        }
                    }
                } else if rendering && number <= trailing {
                    emit(Line {
                        number,
                        bytes: line,
                        kind: LineKind::Context,
                        fragment: false,
                        highlights: None,
                    })
                    .map_err(ScanError::Output)?;
                } else if rendering {
                    before.push(number, line).map_err(ScanError::Output)?;
                }
            }
            if binary.is_some() {
                return Ok(ControlFlow::Break(()));
            }
            Ok(ControlFlow::Continue(()))
        },
    )?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferret_verify::Trigrams;

    fn admits(query: &Trigrams, original: &[u8]) -> bool {
        match query {
            Trigrams::All => true,
            Trigrams::None => false,
            Trigrams::Key(key) => original
                .windows(3)
                .any(|w| w.iter().map(u8::to_ascii_lowercase).eq(key.iter().copied())),
            Trigrams::And(parts) => parts.iter().all(|p| admits(p, original)),
            Trigrams::Or(parts) => parts.iter().any(|p| admits(p, original)),
        }
    }

    #[test]
    fn resolved_names_only_never_reads_pattern_files_or_stdin() {
        for args in [
            vec!["--files", "-f", "missing"],
            vec!["--files", "-f", "-"],
            vec!["--help", "-f", "missing"],
        ] {
            let args: Vec<_> = args.into_iter().map(OsString::from).collect();
            parse_with(&args, &mut |_| panic!("names-only attempted pattern I/O")).unwrap();
        }
    }

    // M2: compile, matching and necessary keys see the same resolved profile.
    #[test]
    fn fixed_folded_and_repeated_profiles_are_sound_over_original_bytes() {
        let profiles: &[&[&str]] = &[
            &["-U", "red\\nviolet"],
            &["-U", "(?s:red.*violet)"],
            &["-UF", "red\nviolet"],
            &["-U", "red|\\A\\z\\n"],
            &["-U", "-e", "red", "-e", "violet\\nblue"],
            &["-Uiw", "KILO"],
            &["-Ux", "red\\nviolet"],
            &["-U", "--multiline-dotall", "red.*violet"],
            &["-U", "(?-u:\\xFF)red\\nviolet"],
            &["-i", "KILO"],
            &["-Fi", "SEA"],
            &["-F", "red.*"],
            &["-w", "red"],
            &["-x", "red"],
            &["-Sw", "red"],
            &["-SFx", "red.*"],
            &["-S", "-e", "red", "-e", "VIOLET"],
            &["-wo", "@red@"],
            &["-wo", ""],
            &["-e", "red", "-e", "(?i:violet)"],
            &["-o", "-e", "", "-e", "red"],
        ];
        let documents: &[&[u8]] = &[
            b"red.*\n",
            b"red\nviolet\n",
            b"\xef\xbb\xbfred\nviolet\n",
            b"\xffred\nviolet\n",
            b"red\r\nviolet\r\n",
            b"\xef\xbb\xbfred\n",
            b"VIOLET\n",
            b"@red@\n",
            b"x@red@x\n",
            b"RED\n",
            b"bred\n",
            "Kilo\n".as_bytes(),
            "ſea\n".as_bytes(),
            b"\xffred\n",
            b"\n",
            b"",
        ];
        let mut positives = 0;
        for args in profiles {
            let Command::Search(plan) =
                parse(&args.iter().map(OsString::from).collect::<Vec<_>>()).unwrap()
            else {
                panic!("search")
            };
            for &bytes in documents {
                let result = scan(
                    &plan,
                    &mut &bytes[..],
                    bytes.len() as u64,
                    &mut Vec::new(),
                    &|| false,
                    |_, _| Ok(()),
                )
                .unwrap();
                if result.matched {
                    positives += 1;
                    assert!(admits(plan.grep.trigrams(), bytes));
                }
            }
        }
        assert!(positives >= 12);
    }

    #[test]
    fn output_universe_follows_resolved_inversion_and_count_overrides() {
        for (args, universe, output) in [
            (vec!["-c", "red"], Universe::Candidates, Output::Count),
            (
                vec!["-c", "--include-zero", "red"],
                Universe::Searchable,
                Output::Count,
            ),
            (
                vec!["--files-without-match", "red"],
                Universe::Searchable,
                Output::WithoutMatch,
            ),
            (
                vec!["-q", "--files-without-match", "red"],
                Universe::Searchable,
                Output::WithoutMatch,
            ),
            (
                vec!["-qc", "--include-zero", "red"],
                Universe::Candidates,
                Output::Count,
            ),
            (vec!["-vc", "red"], Universe::Searchable, Output::Count),
            (vec!["-cl", "red"], Universe::Candidates, Output::Names),
            (
                vec!["-v", "--no-invert-match", "red"],
                Universe::Candidates,
                Output::Lines,
            ),
        ] {
            let Command::Search(plan) =
                parse(&args.into_iter().map(OsString::from).collect::<Vec<_>>()).unwrap()
            else {
                panic!("search")
            };
            assert_eq!(plan.universe(), universe);
            assert_eq!(plan.output, output);
        }
    }

    #[test]
    fn executed_line_profile_is_sound_over_original_bom_and_invalid_bytes() {
        let patterns = [
            "red",
            "(?i)red",
            "(?i:KELVIN)",
            r"\Ared\z",
            "^red$",
            "red|violet",
            "re[d-f]",
            "re.*",
            "",
            r"\s",
            r"[\s\S]",
            r"(?-u:\xFF)red",
            r"[a-z&&[^aeiou]]+",
            "(?s:.)",
            "(?i:[a-z_]+)red",
        ];
        let documents: &[&[u8]] = &[
            b"\xef\xbb\xbfred\n",
            b"blue\nred\n",
            b"\xffred\n",
            "Kelvin\n".as_bytes(),
            b"red\r\n",
            b"",
            b"\n",
            b"red",
            b"violet\n",
        ];
        let mut positives = 0;
        for pattern in patterns {
            let Command::Search(plan) = parse(&[pattern.into()]).unwrap() else {
                panic!("search")
            };
            for &original in documents {
                let mut selected = Vec::new();
                let result = scan(
                    &plan,
                    &mut &original[..],
                    original.len() as u64,
                    &mut Vec::new(),
                    &|| false,
                    |number, line| {
                        selected.push((number, line.to_vec()));
                        Ok(())
                    },
                )
                .unwrap();
                if result.matched {
                    positives += 1;
                    assert!(
                        admits(plan.grep.trigrams(), original),
                        "{pattern:?}, {original:?}: {:?}",
                        plan.grep.trigrams()
                    );
                }
                assert_eq!(result.matched, !selected.is_empty());
            }
        }
        assert!(positives > 40);
    }

    #[test]
    fn long_lines_tiny_reads_and_cancelled_reads_use_the_shared_stream() {
        struct Tiny<'a>(&'a [u8]);
        impl Read for Tiny<'_> {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                self.0.read(&mut bytes[..1])
            }
        }
        let Command::Search(plan) = parse(&["-n".into(), "red".into()]).unwrap() else {
            panic!("search")
        };
        let mut bytes = b"\xef\xbb\xbf".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', 100_000));
        bytes.extend_from_slice(b"red\nred");
        let mut lines = Vec::new();
        scan(
            &plan,
            &mut Tiny(&bytes),
            u64::MAX,
            &mut Vec::new(),
            &|| false,
            |n, line| {
                lines.push((n, line.len()));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(lines, [(1, 100_003), (2, 3)]);
        let result = scan(
            &plan,
            &mut &b"red\n"[..],
            4,
            &mut Vec::new(),
            &|| true,
            |_, _| panic!("cancelled"),
        );
        assert!(result.is_err());
    }
    // R1/P2 and M4 R1: an already-read NUL survives the selected-line limit,
    // including a NUL after a complete line; no mode requests another read.
    #[test]
    fn binary_sentinel_stops_before_another_read_or_buffer_growth() {
        struct Source<'a> {
            bytes: &'a [u8],
            reads: usize,
        }
        impl Read for Source<'_> {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                self.reads += 1;
                if self.reads != 1 {
                    return Err(io::Error::other("read past NUL"));
                }
                bytes[..self.bytes.len()].copy_from_slice(self.bytes);
                Ok(self.bytes.len())
            }
        }
        for bytes in [b"red\0x".as_slice(), b"red\nred\0x"] {
            for flags in [
                vec![],
                vec!["-m1"],
                vec!["-c"],
                vec!["-m1", "-c"],
                vec!["--count-matches"],
                vec!["-m1", "--count-matches"],
                vec!["-co"],
                vec!["-m1", "-co"],
            ] {
                let mut args: Vec<_> = flags.iter().map(OsString::from).collect();
                args.push("red".into());
                let Command::Search(plan) = parse(&args).unwrap() else {
                    panic!("search")
                };
                let mut source = Source { bytes, reads: 0 };
                let mut buffer = Vec::new();
                let result = scan(
                    &plan,
                    &mut source,
                    u64::MAX,
                    &mut buffer,
                    &|| false,
                    |_, _| Ok(()),
                )
                .unwrap();
                assert!(
                    result.matched && result.binary,
                    "{flags:?}: {bytes:?}: {result:?}"
                );
                let expected = if flags.contains(&"-m1") {
                    1
                } else {
                    bytes.iter().filter(|&&b| b == b'\n').count() as u64 + 1
                };
                assert_eq!(result.count, expected, "{flags:?}: {bytes:?}");
                assert_eq!(source.reads, 1);
                assert_eq!(buffer.len(), crate::scan::BUFFER);
            }
        }
    }
}
