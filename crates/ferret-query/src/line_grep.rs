//! Grep's execution profiles and argument grammar. The host supplies input
//! resolution, policy selection and output; matching and streaming are shared
//! with content search through ferret-verify and `scan`.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ferret_verify::{Grep, GrepFlags, LineFlags};

mod context;
mod descriptors;
mod events;
mod multiline;
mod replacement;

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
}

/// Namespace freshness and depth, shared by content and names-only modes.
#[derive(Clone, Copy, Debug, Default)]
pub struct Traversal {
    pub unrestricted: bool,
    pub live: bool,
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
    Columns(String),
    Utf8(&'static str),
    Unsupported(String),
    Pattern(ferret_verify::GrepError),
    PatternFile(PathBuf, io::Error),
    StdinConflict,
    JsonMode,
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
            Self::Columns(value) => write!(
                f,
                "invalid --max-columns {value:?}; expected a nonnegative integer"
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
            Self::JsonMode => f.write_str("--json cannot be combined with names or count output"),
        }
    }
}

impl std::error::Error for ParseError {}

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
    let mut filters = Vec::new();
    let (mut line_number, mut quiet, mut done) = (false, false, false);
    let (mut fold, mut fixed, mut invert, mut only_matching) = (false, false, false, false);
    let mut smart_case = false;
    let mut replacement = None;
    let mut json = false;
    let (mut word, mut line) = (false, false);
    let mut hidden = false;
    let mut text_mode = false;
    let (mut multiline, mut dotall) = (false, false);
    let mut unrestricted_count = 0;
    let mut traversal = Traversal::default();
    let mut files = false;
    let mut include_zero = false;
    let mut max_count = None;
    let mut max_columns = None;
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
        if !done && (bytes == b"--replace" || bytes.starts_with(b"--replace=")) {
            let value = if bytes == b"--replace" {
                args.next().map(|arg| arg.as_bytes())
            } else {
                Some(&bytes[10..])
            }
            .ok_or_else(|| ParseError::Value("--replace".to_owned()))?;
            replacement = Some(
                std::str::from_utf8(value)
                    .map_err(|_| ParseError::Utf8("replacement"))?
                    .to_owned(),
            );
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
                "--json" if value.is_none() => json = true,
                "--no-json" if value.is_none() => json = false,
                "--live" if value.is_none() => traversal.live = true,
                "--follow" if value.is_none() => traversal.follow = true,
                "--no-follow" if value.is_none() => traversal.follow = false,
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
                "--glob" | "--type" | "--type-not" | "--color" | "--regexp" | "--max-count"
                | "--before-context" | "--after-context" | "--context" | "--max-depth"
                | "--max-columns" => {
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
                        "--max-columns" => max_columns = columns(&value)?,
                        "--glob" => filters.push(Filter::Glob(value)),
                        "--type" | "--type-not" => filters.push(Filter::Type {
                            name: value,
                            include: flag == "--type",
                        }),
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
                    b'L' => traversal.follow = true,
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
                    b'r' => {
                        let value = if at + 1 < bytes.len() {
                            Some(&bytes[at + 1..])
                        } else {
                            args.next().map(|arg| arg.as_bytes())
                        }
                        .ok_or_else(|| ParseError::Value("-r".to_owned()))?;
                        replacement = Some(
                            std::str::from_utf8(value)
                                .map_err(|_| ParseError::Utf8("replacement"))?
                                .to_owned(),
                        );
                        break;
                    }
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
                    b'g' | b't' | b'T' | b'e' | b'm' | b'M' | b'A' | b'B' | b'C' => {
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
                        } else if flag == b'M' {
                            max_columns = columns(value)?;
                        } else if flag == b'g' {
                            filters.push(Filter::Glob(value.to_owned()));
                        } else if matches!(flag, b't' | b'T') {
                            filters.push(Filter::Type {
                                name: value.to_owned(),
                                include: flag == b't',
                            });
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
    if json && (files || output != Output::Lines) {
        return Err(ParseError::JsonMode);
    }
    if json {
        only_matching = false;
        replacement = None;
    }
    if files {
        return Ok(Command::Files(Names {
            paths: positionals.into_iter().map(PathBuf::from).collect(),
            filters,
            quiet,
            hidden,
            presentation,
            traversal,
        }));
    }
    // rg's notice counts source matches, which expansion does not keep. Only
    // printed lines carry the notice.
    if max_columns.is_some() && replacement.is_some() && output == Output::Lines && !quiet {
        return Err(unsupported("-M with -r"));
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
    let settings = GrepFlags {
        case_insensitive: fold,
    };
    let flags = LineFlags {
        fixed,
        smart_case,
        word,
        line,
        multiline,
        dotall,
    };
    let (grep, replacement) = if let Some(template) = replacement {
        let (grep, replacement) = settings
            .compile_line_replacement(&patterns, flags, &template)
            .map_err(ParseError::Pattern)?;
        (grep, Some(Box::new(replacement)))
    } else {
        (
            settings
                .compile_line_profile(&patterns, flags)
                .map_err(ParseError::Pattern)?,
            None,
        )
    };
    Ok(Command::Search(Box::new(Plan {
        #[cfg(test)]
        force_per_line: false,
        grep,
        replacement,
        paths,
        filters,
        line_number,
        quiet,
        hidden,
        output,
        only_matching,
        invert,
        stdin_patterns,
        include_zero,
        max_count,
        max_columns,
        before: before.unwrap_or(around),
        after: after.unwrap_or(around),
        presentation,
        traversal,
        text: text_mode,
        multiline,
        json,
        spill_dir: None,
    })))
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

fn columns(value: &str) -> Result<Option<u64>, ParseError> {
    let limit: u64 = value
        .parse()
        .map_err(|_| ParseError::Columns(value.to_owned()))?;
    Ok((limit > 0).then_some(limit))
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
    /// One-based line number. The line-local fast path counts lines only
    /// when the plan prints numbers or JSON, so it is unspecified otherwise.
    pub number: u64,
    pub bytes: &'a [u8],
    pub kind: LineKind,
    pub fragment: bool,
    /// Resolved ranges relative to `bytes`; None permits line-local matching.
    pub highlights: Option<&'a [std::ops::Range<usize>]>,
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
    // of each one: the same proof finds the next selected line, and only the
    // trailing window and the last `-B` lines before it are visited.
    let skip = !fast && !plan.invert && plan.grep.is_line_local();
    #[cfg(test)]
    let (fast, skip) = (fast && !plan.force_per_line, skip && !plan.force_per_line);
    // Counting every LF costs about 15% of a dense scan; skip it unless
    // something reads the number.
    let count_lines = !fast || plan.line_number || plan.json;
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
    let mut emit = |line: Line<'_>| {
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
            return replacement::line(plan, line, stopped, &mut emit);
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
    };
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
            let binary = (!plan.text).then(|| memchr::memchr(0, run)).flatten();
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
            loop {
                let line = if fast {
                    let Some(start) = next.take() else { break };
                    let remaining = &text[start..];
                    if stopped() {
                        return Err(ScanError::Input(io::ErrorKind::Interrupted.into()));
                    }
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
                            number += memchr::memchr_iter(b'\n', remaining).count() as u64
                                + u64::from(
                                    terminated
                                        || binary.is_some()
                                        || (!remaining.is_empty() && !remaining.ends_with(b"\n")),
                                );
                        }
                        break;
                    };
                    let first = memchr::memrchr(b'\n', &remaining[..hit]).map_or(0, |at| at + 1);
                    let end = memchr::memchr(b'\n', &remaining[hit..])
                        .map_or(remaining.len(), |at| hit + at);
                    if count_lines {
                        number += memchr::memchr_iter(b'\n', &remaining[..first]).count() as u64;
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
                                let rest = &text[start..];
                                let hit = search.matching_line(rest).map_or(usize::MAX, |hit| {
                                    start
                                        + memchr::memrchr(b'\n', &rest[..hit])
                                            .map_or(0, |at| at + 1)
                                });
                                hit_boundary = Some(if hit == usize::MAX {
                                    let end =
                                        if !terminated && binary.is_none() && text.ends_with(b"\n")
                                        {
                                            text.len() - 1
                                        } else {
                                            text.len()
                                        };
                                    start
                                        + context::tail_start(
                                            &text[start..end.max(start)],
                                            before_lines.max(1),
                                        )
                                } else if hit == start {
                                    start
                                } else {
                                    start + context::tail_start(&text[start..hit - 1], before_lines)
                                });
                                hit_line = Some(hit);
                                hit
                            }
                        };
                        // Jump over whole unselected lines that are neither
                        // trailing context nor among the last `-B` lines
                        // before the next selected line or the run's end.
                        let jump = if number < trailing || hit == start {
                            start
                        } else {
                            hit_boundary.unwrap_or(start)
                        };
                        if jump > start {
                            if stopped() {
                                return Err(ScanError::Input(io::ErrorKind::Interrupted.into()));
                            }
                            number += memchr::memchr_iter(b'\n', &text[start..jump]).count() as u64;
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
                if stopped() {
                    return Err(ScanError::Input(io::ErrorKind::Interrupted.into()));
                }
                if limited {
                    // Lines after `-m` are context and are never searched.
                    if rendering {
                        emit(Line {
                            offset: line_offset,
                            source_lines: None,
                            terminated: line_terminated,
                            number,
                            bytes: line,
                            kind: LineKind::Context,
                            fragment: false,
                            highlights: Some(&[]),
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
                    hit_line == Some(line_start)
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
                    if (plan.only_matching || plan.output == Output::CountMatches) && !plan.invert {
                        let fragments = plan.output == Output::Lines && plan.replacement.is_none();
                        let mut printed = false;
                        for span in search.spans(line) {
                            if stopped() {
                                return Err(ScanError::Input(io::ErrorKind::Interrupted.into()));
                            }
                            result.count += 1;
                            // D79 D: `-o` never prints an empty record.
                            if fragments && !span.is_empty() {
                                printed = true;
                                emit(Line {
                                    offset: line_offset,
                                    source_lines: None,
                                    terminated: line_terminated,
                                    number,
                                    bytes: &line[span],
                                    kind: LineKind::Match,
                                    fragment: true,
                                    highlights: None,
                                })
                                .map_err(ScanError::Output)?;
                            }
                        }
                        if fragments && !printed {
                            // An empty fragment prints nothing; it keeps this
                            // selected line in the context window.
                            emit(Line {
                                offset: line_offset,
                                source_lines: None,
                                terminated: line_terminated,
                                number,
                                bytes: &line[..0],
                                kind: LineKind::Match,
                                fragment: true,
                                highlights: None,
                            })
                            .map_err(ScanError::Output)?;
                        }
                        if plan.output == Output::Lines && plan.replacement.is_some() {
                            emit(Line {
                                offset: line_offset,
                                source_lines: None,
                                terminated: line_terminated,
                                number,
                                bytes: line,
                                kind: LineKind::Match,
                                fragment: false,
                                highlights: None,
                            })
                            .map_err(ScanError::Output)?;
                        }
                    } else {
                        result.count += 1;
                        if plan.output == Output::Lines {
                            emit(Line {
                                offset: line_offset,
                                source_lines: None,
                                terminated: line_terminated,
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
                        offset: line_offset,
                        source_lines: None,
                        terminated: line_terminated,
                        number,
                        bytes: line,
                        kind: LineKind::Context,
                        fragment: false,
                        highlights: None,
                    })
                    .map_err(ScanError::Output)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use ferret_verify::Trigrams;

    #[test]
    fn process_relative_directories_are_not_the_same_in_every_process() {
        let here = std::env::current_dir().unwrap();
        assert!(same_in_every_process(Path::new(".")));
        assert!(same_in_every_process(&here));
        let symlinked =
            std::env::temp_dir().join(format!("ferret-cwd-link-{}", std::process::id()));
        std::os::unix::fs::symlink(&here, &symlinked).unwrap();
        assert!(same_in_every_process(&symlinked));
        std::fs::remove_file(&symlinked).unwrap();
        let descriptor = std::fs::File::open(&here).unwrap();
        let fd = std::os::fd::AsRawFd::as_raw_fd(&descriptor);
        for path in [
            "/proc/self/cwd".to_owned(),
            format!("/proc/self/fd/{fd}"),
            format!("/dev/fd/{fd}"),
            "/proc/self".to_owned(),
            "/proc/self/fd".to_owned(),
        ] {
            assert!(Path::new(&path).is_dir(), "{path}");
            assert!(!same_in_every_process(Path::new(&path)), "{path}");
        }
        assert!(!same_in_every_process(Path::new("Cargo.toml")));
    }

    // Compare the actual scanner with its old path, including the source
    // metadata consumed by plain rendering, replacement and JSON encoding.
    fn compare_scanners(plan: &Plan, bytes: &[u8]) {
        let scan = |plan: &Plan| {
            let mut output = Vec::new();
            let mut result = scan_lines(
                plan,
                &mut &bytes[..],
                bytes.len() as u64,
                &mut Vec::new(),
                &|| false,
                |line| {
                    output.push((
                        line.offset,
                        line.source_lines,
                        line.terminated,
                        line.number,
                        line.bytes.to_vec(),
                        line.kind,
                        line.fragment,
                        line.highlights.map(<[_]>::to_vec),
                    ));
                    Ok(())
                },
            )
            .unwrap();
            result.elapsed = std::time::Duration::ZERO;
            (output, result)
        };
        // The fast path counts lines only when something prints them, so an
        // unnumbered plan compares everything else; a numbered copy compares
        // the numbers too.
        let mut numbered = plan.clone();
        numbered.line_number = true;
        for (plan, masked) in [(plan, !plan.line_number && !plan.json), (&numbered, false)] {
            let mask = |(mut output, result): (Vec<_>, _)| {
                if masked {
                    output
                        .iter_mut()
                        .for_each(|line: &mut (_, _, _, u64, _, _, _, _)| line.3 = 0);
                }
                (output, result)
            };
            let actual = mask(scan(plan));
            let mut slow = plan.clone();
            slow.force_per_line = true;
            let expected = mask(scan(&slow));
            assert_eq!(
                actual,
                expected,
                "pattern {:?}, {} input bytes",
                plan.grep.pattern(),
                bytes.len()
            );
        }
    }

    #[test]
    fn run_search_and_per_line_search_emit_identical_records_and_results() {
        let documents: &[&[u8]] = &[
            // Pins the bounded-context skip path at a distant match.
            b"x\nx\nx\nx\nx\nx\nx\nx\nx\nx\nx\nx\nx\nx\nx\nx\nx\nx\nx\nx\nred\n",
            b"red\ngap\nred\n",
            b"red\ngap\nred",
            b"gap\nred\n",
            b"gap\nred",
            b"red red\r\ngap\r\nred\r\n",
            b"\nred\n\n",
            b"\xef\xbb\xbfred\ngap\nred\n",
            b"red\0red\n",
            b"gap\0red\n",
            b"red\n\0red\n",
            b"\0red\n",
            b"\n\0",
            b"sing\nwalking\nred\nsinging\n",
            b"a b z\nb\nred gap red\n",
            b"\x90\x80\x81\xf0\x90\x80\x81X\n",
            "singing界no\nwalking\nx singing𐀀 end\nsinging🙂\n".as_bytes(),
            b"gap\n",
            b"gap",
            b"",
            b"\n",
            b"\n\n",
            b"red\nred\nred\n",
            b"\xffred\n\xff\nred\xff\n",
            "Kilo\nred\nſea\n".as_bytes(),
            // Gaps longer and shorter than the context windows.
            b"a\nb\nc\nd\nred\ne\nf\ng\nh\ni\nred\nj\nred\nk\nl\nm\nn\n",
            b"a\nb\nc\nd\ne\nf\n\n\nred\n\n\n\n\ng",
            b"a\nb\nc\nd\ne\nf\ng\n",
            b"a\nb\nc\nd\ne\nf\n\0g\nred\n",
        ];
        let patterns = [
            "red",
            "absent",
            "",
            "^",
            "$",
            "^$",
            "^red$",
            r"\bred\b",
            r"\b",
            r"\B",
            r"\b{start-half}",
            r"\b{end-half}",
            r"\w+ing\b",
            "red|$",
            "red.*red|gap",
            "a.*z|b",
            r"a*(?-u:\x90\x80\x81)$",
            r"(?-u:[\x00-\x09\x0B-\xEF]*\x90\x80\x81)$",
            r"[a-z]+ing\b",
            "red?",
            "red*",
            "(?i:red)",
            "[a-z]+red",
            "[a-z]*red",
            // These must retain per-line assertion/consumption bounds (D79).
            r"\Ared",
            r"red\z",
            r"\A|\z",
            "(?-m:^red$)",
            "(?R:^red$)",
            r"\s",
            r"[^x]",
            "(?s:.)",
            r"[\s\S]",
            r"(?-u:\xFF)",
        ];
        let profiles: &[&[&str]] = &[
            &[],
            &["-n"],
            &["-m1"],
            &["-m2"],
            &["-m0"],
            &["-c"],
            &["-co"],
            &["--count-matches"],
            &["-l"],
            &["-q"],
            &["-o"],
            &["-om1"],
            &["-cm1"],
            &["--json"],
            &["--json", "-m1"],
            &["--json", "-q"],
            &["--json", "-N"],
            &["-r", "<$0>"],
            &["-or", "<$0>"],
            &["--files-without-match"],
            // Unchanged fallbacks share the same switch and public entry point.
            &["-v"],
            &["-C1"],
            &["-B2"],
            &["-A2"],
            &["-C3"],
            &["-A1", "-B3"],
            &["-B2", "-m1"],
            &["-C2", "-m2"],
            &["-C1", "-o"],
            &["-c", "-C2"],
            &["-B2", "-r", "<$0>"],
            &["--json", "-A1"],
            &["--json", "-B2"],
            &["-U"],
        ];
        for pattern in patterns {
            for &profile in profiles {
                for text in [false, true] {
                    let mut args: Vec<_> = profile.iter().map(OsString::from).collect();
                    if text {
                        args.push("-a".into());
                    }
                    args.push(pattern.into());
                    let Command::Search(plan) = parse(&args).unwrap() else {
                        panic!("search")
                    };
                    for &bytes in documents {
                        compare_scanners(&plan, bytes);
                    }
                }
            }
        }
        // Context windows that straddle 64 KiB read runs, around lines that
        // are long, short and empty.
        let mut runs = Vec::new();
        for line in 0..12_000 {
            match line % 4_001 {
                0 | 4_000 => runs.extend_from_slice(b"red\n"),
                7 => runs.extend(std::iter::repeat_n(b'x', 70_000)),
                _ if line % 3 == 0 => {}
                _ => runs.extend_from_slice(format!("gap {line}").as_bytes()),
            }
            runs.push(b'\n');
        }
        for profile in [
            &["-C3"][..],
            &["-B5"],
            &["-A2"],
            &["-B2", "-m2"],
            &["-c", "-C1"],
        ] {
            let mut args: Vec<_> = profile.iter().map(OsString::from).collect();
            args.push("red".into());
            let Command::Search(plan) = parse(&args).unwrap() else {
                panic!("search")
            };
            compare_scanners(&plan, &runs);
            compare_scanners(&plan, &runs[..runs.len() - 1]);
        }
        for flags in [
            vec!["-F"],
            vec!["-Fi"],
            vec!["-w"],
            vec!["-x"],
            vec!["-e", "", "-e"],
        ] {
            let mut args: Vec<_> = flags.into_iter().map(OsString::from).collect();
            args.push("red".into());
            let Command::Search(plan) = parse(&args).unwrap() else {
                panic!("search")
            };
            for &bytes in documents {
                compare_scanners(&plan, bytes);
            }
        }
    }

    #[test]
    fn before_context_boundary_work_is_linear() {
        let count = 256;
        let mut bytes = b"x\n".repeat(count);
        bytes.extend_from_slice(b"red\n");
        let args = ["-n", "-B256", "red"];
        let Command::Search(plan) = parse(&args.map(OsString::from)).unwrap() else {
            panic!("search")
        };
        compare_scanners(&plan, &bytes);
        context::reset_tail_start_scanned();
        scan_lines(
            &plan,
            &mut &bytes[..],
            bytes.len() as u64,
            &mut Vec::new(),
            &|| false,
            |_| Ok(()),
        )
        .unwrap();
        assert!(
            context::tail_start_scanned() <= bytes.len() * 2,
            "tail_start scanned {} bytes for {} input bytes",
            context::tail_start_scanned(),
            bytes.len()
        );
    }

    #[test]
    fn run_search_preserves_partial_lines_across_reads_and_run_offsets() {
        let mut bytes = b"red\ngap\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', crate::scan::BUFFER * 2 + 17));
        bytes.extend_from_slice(b"red\n\ngap\nred\n");
        // The first read contains a complete run followed by a partial line;
        // that line survives multiple reads before the next callback.
        for prefix in [b"".as_slice(), b"\xef\xbb\xbf"] {
            let mut input = prefix.to_vec();
            input.extend_from_slice(&bytes);
            for suffix in [b"".as_slice(), b"red", b"\0red\n"] {
                let mut input = input.clone();
                input.extend_from_slice(suffix);
                for flags in ["-n", "-o", "-c", "-l", "-q", "-m1", "--json"] {
                    for text in [false, true] {
                        let mut args = vec![flags.into()];
                        if text {
                            args.push("-a".into());
                        }
                        args.push("red".into());
                        let Command::Search(plan) = parse(&args).unwrap() else {
                            panic!("search")
                        };
                        compare_scanners(&plan, &input);
                    }
                }
            }
        }
    }

    // Globs and types share one ordered list, so `-g` and `-t` interleave
    // as written, in every spelling.
    #[test]
    fn globs_and_types_keep_argv_order() {
        let args: Vec<OsString> = [
            "-tpy",
            "-g",
            "*.rs",
            "--type",
            "rust",
            "-T",
            "md",
            "--type-not=c",
            "--type=sh",
            "-g!x",
            "red",
        ]
        .iter()
        .map(Into::into)
        .collect();
        let Command::Search(plan) = parse(&args).unwrap() else {
            panic!("search")
        };
        let ty = |name: &str, include| Filter::Type {
            name: name.into(),
            include,
        };
        assert_eq!(
            plan.filters,
            [
                ty("py", true),
                Filter::Glob("*.rs".into()),
                ty("rust", true),
                ty("md", false),
                ty("c", false),
                ty("sh", true),
                Filter::Glob("!x".into()),
            ]
        );
    }

    #[test]
    fn offset_presentation_flags_remain_refused() {
        // These are outside the current grep contract, including this slice.
        for flag in ["--column", "-b"] {
            assert!(matches!(parse(&[flag.into(), "red".into()]),
                Err(ParseError::Unsupported(value)) if value == flag));
        }
    }

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
    fn replacement_cancellation_after_one_record_stops_expansion() {
        use std::cell::Cell;
        for flags in ["-or", "-Uor"] {
            let Command::Search(plan) =
                parse(&[flags.into(), "<$0>".into(), "red".into()]).unwrap()
            else {
                panic!("search")
            };
            let stopped = Cell::new(false);
            let mut emitted = 0;
            let bytes = b"red red\n";
            let result = scan_lines(
                &plan,
                &mut &bytes[..],
                bytes.len() as u64,
                &mut Vec::new(),
                &|| stopped.get(),
                |_| {
                    emitted += 1;
                    stopped.set(true);
                    Ok(())
                },
            );
            assert!(result.is_err(), "{flags}: cancelled expansion succeeded");
            assert_eq!(emitted, 1);
        }
    }

    #[test]
    fn multiline_replacement_splitting_stops_after_the_first_emitted_line() {
        use std::cell::Cell;
        let Command::Search(plan) =
            parse(&["-Ur".into(), "$2\n$1\nlast".into(), r"(red)\n(blue)".into()]).unwrap()
        else {
            panic!("search")
        };
        assert!(
            !plan.grep.excludes_lf(),
            "must exercise multiline splitting"
        );
        let bytes = b"red\nblue\nred\nblue\n";
        let expected = vec![
            (1, b"blue".to_vec()),
            (2, b"red".to_vec()),
            (3, b"last".to_vec()),
            (4, b"blue".to_vec()),
            (5, b"red".to_vec()),
            (6, b"last".to_vec()),
        ];
        for cancel in [false, true] {
            let stopped = Cell::new(false);
            let mut records = Vec::new();
            let result = scan_lines(
                &plan,
                &mut &bytes[..],
                bytes.len() as u64,
                &mut Vec::new(),
                &|| stopped.get(),
                |line| {
                    records.push((line.number, line.bytes.to_vec()));
                    if cancel {
                        stopped.set(true);
                    }
                    Ok(())
                },
            );
            if cancel {
                assert!(matches!(result, Err(ScanError::Output(ref error))
                    if error.kind() == io::ErrorKind::ConnectionAborted));
                assert_eq!(records, expected[..1]);
            } else {
                assert!(result.is_ok());
                assert_eq!(records, expected);
            }
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
        let profiles: &[(&[&str], &[u8])] = &[
            (&["--json", "-i", "KILO"], "Kilo\n".as_bytes()),
            (&["--json", "-U", "red\\nviolet"], b"red\nviolet\n"),
            (&["-r", "<$1>", "(red)"], b"red\nviolet\n"),
            (
                &["-Uor", "<$1>", "(?-u:(\\xFF))red\\nviolet"],
                b"\xffred\nviolet\n",
            ),
            (&["-r", "<$name>", "(?P<name>red)"], b"red.*\n"),
            (&["-U", "red\\nviolet"], b"red\nviolet\n"),
            (&["-U", "(?s:red.*violet)"], b"red\nviolet\n"),
            (&["-UF", "red\nviolet"], b"red\nviolet\n"),
            (&["-U", "red|\\A\\z\\n"], b"red.*\n"),
            (
                &["-U", "-e", "red", "-e", "violet\\nblue"],
                b"red\nviolet\n",
            ),
            (&["-Uiw", "KILO"], "Kilo\n".as_bytes()),
            (&["-Ux", "red\\nviolet"], b"red\nviolet\n"),
            (
                &["-U", "--multiline-dotall", "red.*violet"],
                b"red\nviolet\n",
            ),
            (&["-U", "(?-u:\\xFF)red\\nviolet"], b"\xffred\nviolet\n"),
            (&["-i", "KILO"], "Kilo\n".as_bytes()),
            (&["-Fi", "SEA"], "ſea\n".as_bytes()),
            (&["-F", "red.*"], b"red.*\n"),
            (&["-w", "red"], b"red\nviolet\n"),
            (&["-x", "red"], b"\xef\xbb\xbfred\n"),
            (&["-Sw", "red"], b"red\nviolet\n"),
            (&["-SFx", "red.*"], b"red.*\n"),
            (&["-S", "-e", "red", "-e", "VIOLET"], b"VIOLET\n"),
            (&["-wo", "@red@"], b"@red@\n"),
            (&["-wo", ""], b"\n"),
            (&["-e", "red", "-e", "(?i:violet)"], b"VIOLET\n"),
            (&["-o", "-e", "", "-e", "red"], b"\n"),
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
        for &(args, expected_positive) in profiles {
            let mut checked_positive = false;
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
                if bytes == expected_positive {
                    checked_positive = true;
                    assert!(
                        result.matched,
                        "missing required positive for {args:?}: {bytes:?}"
                    );
                }
                if result.matched {
                    positives += 1;
                    assert!(admits(plan.grep.trigrams(), bytes));
                }
            }
            assert!(checked_positive, "missing positive fixture for {args:?}");
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
