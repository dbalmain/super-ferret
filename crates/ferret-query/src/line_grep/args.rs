//! rg's argument grammar: spellings, clusters, attached values, `--rg`
//! flags and pattern files, parsed into a [`Command`].

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;

use ferret_verify::{GrepFlags, LineFlags};

use super::*;

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
    /// A flag's value names no choice rg offers: an error, not a missing
    /// feature, so the host never hands it to rg.
    Choice(String, String),
    /// Either a flag rg has and ferret lacks, or one neither has; the host
    /// may hand both to rg, which diagnoses the second.
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
            Self::Choice(flag, value) => {
                write!(
                    f,
                    "error parsing flag {flag}: choice '{value}' is unrecognized"
                )
            }
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
    parse_marked(args, read, &mut Vec::new())
}

/// The argv positions a parse took as ferret's own `--rg` flag, which rg
/// rejects: a host handing a refused command to rg drops exactly these. A
/// flag ferret lacks stops the parse, and only rg knows that flag's grammar
/// (`-Pe` makes the next argument a pattern), so a `--rg` after it is not
/// one of these: rg reads it as it would, rather than a guess changing its
/// answer. A refusal after the last argument leaves every flag found.
pub fn own_flags(args: &[OsString]) -> Vec<usize> {
    let mut flags = Vec::new();
    let _ = parse_marked(args, &mut |_| Ok(Vec::new()), &mut flags);
    flags
}

/// [`parse_with`], recording in `rg` the argv position of each `--rg` flag.
fn parse_marked(
    args: &[OsString],
    read: &mut dyn FnMut(&Path) -> io::Result<Vec<u8>>,
    rg: &mut Vec<usize>,
) -> Result<Command, ParseError> {
    let total = args.len();
    let mut options = Options::default();
    let mut done = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let bytes = arg.as_bytes();
        if done || !bytes.starts_with(b"-") || bytes.len() == 1 {
            options.positionals.push(arg.clone());
        } else if bytes == b"--" {
            done = true;
        } else if bytes.starts_with(b"--") {
            let (flag, value) = match bytes.iter().position(|&byte| byte == b'=') {
                Some(at) => (&bytes[..at], Some(OsStr::from_bytes(&bytes[at + 1..]))),
                None => (bytes, None),
            };
            // Only a path or a replacement may carry non-UTF-8 bytes.
            let text = arg.to_str();
            let flag = match std::str::from_utf8(flag) {
                Ok(flag @ ("--file" | "--replace")) => flag,
                Ok(flag) if text.is_some() => flag,
                _ => return Err(ParseError::Utf8("option")),
            };
            if VALUED.contains(&flag) {
                let value = match value {
                    Some(value) => value,
                    None => args
                        .next()
                        .ok_or_else(|| ParseError::Value(flag.to_owned()))?,
                };
                options.set(flag, flag, value)?;
            } else if value.is_none() && options.switch(flag)? {
                if flag == "--rg" {
                    rg.push(total - args.len() - 1);
                }
            } else {
                let Some(text) = text else { unreachable!() };
                return Err(unsupported(text));
            }
        } else {
            for (at, &letter) in bytes.iter().enumerate().skip(1) {
                let spelling = || format!("-{}", char::from(letter));
                let Some(flag) = long_name(letter) else {
                    return Err(unsupported(&spelling()));
                };
                if !VALUED.contains(&flag) {
                    let true = options.switch(flag)? else {
                        unreachable!("{flag} is a switch")
                    };
                    continue;
                }
                // A value flag takes the rest of its cluster, or the next
                // argument.
                let value = if at + 1 < bytes.len() {
                    OsStr::from_bytes(&bytes[at + 1..])
                } else {
                    args.next().ok_or_else(|| ParseError::Value(spelling()))?
                };
                options.set(flag, &spelling(), value)?;
                break;
            }
        }
    }
    let Options {
        mut positionals,
        mut sources,
        filters,
        line_number,
        quiet,
        fold,
        fixed,
        invert,
        mut only_matching,
        smart_case,
        mut replacement,
        json,
        word,
        line,
        hidden,
        text_mode,
        multiline,
        dotall,
        unrestricted_count: _,
        traversal,
        files,
        include_zero,
        max_count,
        max_columns,
        sort,
        before,
        after,
        around,
        presentation,
        output,
        help,
        version,
    } = options;
    if help {
        return Ok(Command::Help);
    }
    if version {
        return Ok(Command::Version);
    }
    let sort = match sort {
        Some(Ok(sort)) if files => Some(sort),
        Some(Ok(sort)) => {
            let flag = if sort == Sort::Oldest {
                "--sort"
            } else {
                "--sortr"
            };
            return Err(unsupported(&format!("{flag}=modified without --files")));
        }
        Some(Err(spelling)) => return Err(unsupported(&spelling)),
        None => None,
    };
    let hidden = hidden.unwrap_or(!traversal.rg);
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
            sort,
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

/// Long options that take a value, attached with `=` or as the next argument.
const VALUED: [&str; 15] = [
    "--file",
    "--replace",
    "--glob",
    "--type",
    "--type-not",
    "--color",
    "--regexp",
    "--max-count",
    "--before-context",
    "--after-context",
    "--context",
    "--max-depth",
    "--max-columns",
    "--sort",
    "--sortr",
];

/// The long option a short letter spells; a cluster applies each in turn.
fn long_name(letter: u8) -> Option<&'static str> {
    Some(match letter {
        b'n' => "--line-number",
        b'N' => "--no-line-number",
        b'H' => "--with-filename",
        b'I' => "--no-filename",
        b'0' => "--null",
        b'p' => "--pretty",
        b'L' => "--follow",
        b'q' => "--quiet",
        b'u' => "--unrestricted",
        b'a' => "--text",
        b'U' => "--multiline",
        b'.' => "--hidden",
        b'i' => "--ignore-case",
        b's' => "--case-sensitive",
        b'S' => "--smart-case",
        b'w' => "--word-regexp",
        b'x' => "--line-regexp",
        b'F' => "--fixed-strings",
        b'v' => "--invert-match",
        b'o' => "--only-matching",
        b'l' => "--files-with-matches",
        b'c' => "--count",
        b'h' => "--help",
        b'V' => "--version",
        b'r' => "--replace",
        b'f' => "--file",
        b'e' => "--regexp",
        b'g' => "--glob",
        b't' => "--type",
        b'T' => "--type-not",
        b'm' => "--max-count",
        b'M' => "--max-columns",
        b'A' => "--after-context",
        b'B' => "--before-context",
        b'C' => "--context",
        _ => return None,
    })
}

/// Everything argv sets, before the finished command is checked. Later
/// flags override earlier ones, in argv order.
#[derive(Default)]
struct Options {
    positionals: Vec<OsString>,
    sources: Vec<PatternSource>,
    filters: Vec<Filter>,
    line_number: bool,
    quiet: bool,
    fold: bool,
    fixed: bool,
    invert: bool,
    only_matching: bool,
    smart_case: bool,
    replacement: Option<String>,
    json: bool,
    word: bool,
    line: bool,
    // `--hidden` and its relatives; otherwise the mode decides (D128).
    hidden: Option<bool>,
    text_mode: bool,
    multiline: bool,
    dotall: bool,
    unrestricted_count: u8,
    traversal: Traversal,
    files: bool,
    include_zero: bool,
    max_count: Option<u64>,
    max_columns: Option<u64>,
    // The spelling of a sort ferret lacks is kept for its refusal.
    sort: Option<Result<Sort, String>>,
    before: Option<u64>,
    after: Option<u64>,
    around: u64,
    presentation: Presentation,
    output: Output,
    help: bool,
    version: bool,
}

impl Options {
    /// Apply a flag that takes no value; false when `flag` is not one.
    fn switch(&mut self, flag: &str) -> Result<bool, ParseError> {
        match flag {
            "--help" => self.help = true,
            "--version" => self.version = true,
            "--line-number" => {
                self.line_number = true;
                self.presentation.numbers = Some(true);
            }
            "--no-line-number" => {
                self.line_number = false;
                self.presentation.numbers = Some(false);
            }
            "--with-filename" => self.presentation.filename = Some(true),
            "--no-filename" => self.presentation.filename = Some(false),
            "--heading" => self.presentation.heading = Some(true),
            "--no-heading" => self.presentation.heading = Some(false),
            "--null" => self.presentation.null = true,
            "--pretty" => {
                self.presentation.pretty();
                self.line_number = true;
            }
            "--quiet" => self.quiet = true,
            "--json" => self.json = true,
            "--no-json" => self.json = false,
            "--live" => self.traversal.live = true,
            "--rg" => self.traversal.rg = true,
            "--follow" => self.traversal.follow = true,
            "--no-follow" => self.traversal.follow = false,
            "--no-ignore" => self.traversal.unrestricted = true,
            "--ignore" => self.traversal.unrestricted = false,
            "--unrestricted" => unrestricted(
                &mut self.unrestricted_count,
                &mut self.traversal,
                &mut self.hidden,
            )?,
            "--multiline" => self.multiline = true,
            "--no-multiline" => self.multiline = false,
            "--multiline-dotall" => self.dotall = true,
            "--no-multiline-dotall" => self.dotall = false,
            "--text" => self.text_mode = true,
            "--no-text" => self.text_mode = false,
            "--hidden" => self.hidden = Some(true),
            "--no-hidden" => self.hidden = Some(false),
            "--ignore-case" => (self.fold, self.smart_case) = (true, false),
            "--case-sensitive" => (self.fold, self.smart_case) = (false, false),
            "--smart-case" => (self.fold, self.smart_case) = (false, true),
            "--word-regexp" => (self.word, self.line) = (true, false),
            "--line-regexp" => (self.word, self.line) = (false, true),
            "--fixed-strings" => self.fixed = true,
            "--no-fixed-strings" => self.fixed = false,
            "--invert-match" => self.invert = true,
            "--no-invert-match" => self.invert = false,
            "--only-matching" => self.only_matching = true,
            "--files" => self.files = true,
            "--include-zero" => self.include_zero = true,
            "--no-include-zero" => self.include_zero = false,
            "--count-matches" => self.produce(Output::CountMatches),
            "--files-without-match" => self.produce(Output::WithoutMatch),
            "--files-with-matches" => self.produce(Output::Names),
            "--count" => self.produce(Output::Count),
            "--scan" | "--scan-uncovered" => {}
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// A names or count mode replaces `--files` and any earlier mode.
    fn produce(&mut self, output: Output) {
        self.output = output;
        self.files = false;
    }

    /// Apply a `VALUED` flag; `spelling` is how argv wrote it, for errors.
    fn set(&mut self, flag: &str, spelling: &str, value: &OsStr) -> Result<(), ParseError> {
        match flag {
            "--file" => {
                self.sources.push(PatternSource::File(PathBuf::from(value)));
                return Ok(());
            }
            "--replace" => {
                let value = value.to_str().ok_or(ParseError::Utf8("replacement"))?;
                self.replacement = Some(value.to_owned());
                return Ok(());
            }
            _ => {}
        }
        let value = value
            .to_str()
            .ok_or_else(|| ParseError::Value(spelling.to_owned()))?
            .to_owned();
        match flag {
            "--before-context" | "--after-context" | "--context" => {
                let number = value.parse().map_err(|_| ParseError::Context(value))?;
                match flag {
                    "--before-context" => self.before = Some(number),
                    "--after-context" => self.after = Some(number),
                    _ => self.around = number,
                }
            }
            "--max-depth" => {
                self.traversal.max_depth =
                    Some(value.parse().map_err(|_| ParseError::Depth(value))?)
            }
            "--max-count" => {
                self.max_count = Some(value.parse().map_err(|_| ParseError::Number(value))?)
            }
            "--max-columns" => self.max_columns = columns(&value)?,
            "--glob" => self.filters.push(Filter::Glob(value)),
            "--type" | "--type-not" => self.filters.push(Filter::Type {
                name: value,
                include: flag == "--type",
            }),
            "--regexp" => self.sources.push(PatternSource::Argument(value)),
            "--sort" | "--sortr" => {
                self.sort = match value.as_str() {
                    "none" => None,
                    "modified" if flag == "--sort" => Some(Ok(Sort::Oldest)),
                    "modified" => Some(Ok(Sort::Newest)),
                    "path" | "accessed" | "created" => Some(Err(format!("{flag}={value}"))),
                    _ => return Err(ParseError::Choice(flag.to_owned(), value)),
                }
            }
            "--color" => {
                self.presentation.color = match value.as_str() {
                    "auto" => Color::Auto,
                    "never" => Color::Never,
                    "always" => Color::Always,
                    "ansi" => Color::Ansi,
                    _ => return Err(ParseError::Choice(flag.to_owned(), value)),
                }
            }
            _ => unreachable!("{flag} is not in VALUED"),
        }
        Ok(())
    }
}

// Each -u occurrence applies its next widening step at that argv position.
// --ignore can reverse the first without resetting the occurrence count.
fn unrestricted(
    count: &mut u8,
    traversal: &mut Traversal,
    hidden: &mut Option<bool>,
) -> Result<(), ParseError> {
    *count += 1;
    match *count {
        1 => traversal.unrestricted = true,
        2 => *hidden = Some(true),
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
