//! rg's argument grammar: spellings, clusters, attached values, `--rg`
//! flags and pattern files, parsed into a [`Command`].

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
    let mut positionals = Vec::new();
    let mut sources = Vec::new();
    let mut filters = Vec::new();
    let (mut line_number, mut quiet, mut done) = (false, false, false);
    let (mut fold, mut fixed, mut invert, mut only_matching) = (false, false, false, false);
    let mut smart_case = false;
    let mut replacement = None;
    let mut json = false;
    let (mut word, mut line) = (false, false);
    // `--hidden` and its relatives; otherwise the mode decides (D128).
    let mut hidden = None;
    let mut text_mode = false;
    let (mut multiline, mut dotall) = (false, false);
    let mut unrestricted_count = 0;
    let mut traversal = Traversal::default();
    let mut files = false;
    let mut include_zero = false;
    let mut max_count = None;
    let mut max_columns = None;
    // The spelling of a sort ferret lacks is kept for its refusal.
    let mut sort: Option<Result<Sort, String>> = None;
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
                "--rg" if value.is_none() => {
                    traversal.rg = true;
                    rg.push(total - args.len() - 1);
                }
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
                "--hidden" if value.is_none() => hidden = Some(true),
                "--no-hidden" if value.is_none() => hidden = Some(false),
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
                | "--max-columns" | "--sort" | "--sortr" => {
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
                        "--sort" | "--sortr" => {
                            sort = match value.as_str() {
                                "none" => None,
                                "modified" if flag == "--sort" => Some(Ok(Sort::Oldest)),
                                "modified" => Some(Ok(Sort::Newest)),
                                "path" | "accessed" | "created" => {
                                    Some(Err(format!("{flag}={value}")))
                                }
                                _ => return Err(ParseError::Choice(flag.to_owned(), value)),
                            }
                        }
                        _ => {
                            presentation.color = match value.as_str() {
                                "auto" => Color::Auto,
                                "never" => Color::Never,
                                "always" => Color::Always,
                                "ansi" => Color::Ansi,
                                _ => return Err(ParseError::Choice(flag.to_owned(), value)),
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
                    b'.' => hidden = Some(true),
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
