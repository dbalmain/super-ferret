//! Grep's line-oriented profile and argument grammar. The host supplies input
//! resolution, policy selection and output; matching and streaming are shared
//! with content search through ferret-verify and `scan`.

use std::ffi::OsString;
use std::io::{self, Read};
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use ferret_verify::{Grep, GrepFlags};

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
}

/// Name and count switches override one another in argv order; quiet is
/// independent and overrides every rendering mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    Lines,
    Names,
    Count,
}

/// Names required after resolving output and selection switches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Universe {
    Candidates,
    Searchable,
}

impl Plan {
    /// Inversion requires every selected searchable name, including documents
    /// excluded by the positive postings. Absence never proves a selected line.
    pub fn universe(&self) -> Universe {
        if self.invert {
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
    Utf8(&'static str),
    Unsupported(String),
    Pattern(ferret_verify::GrepError),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingPattern => f.write_str("a pattern is required"),
            Self::Value(flag) => write!(f, "{flag} needs a UTF-8 value"),
            Self::Utf8(argument) => write!(f, "{argument} must be UTF-8"),
            Self::Unsupported(flag) => {
                write!(f, "{flag} is not supported; use rg for this command")
            }
            Self::Pattern(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse rg spellings, clusters, attached values and `--`. Glob validation
/// follows in the policy adapter, still before input resolution.
pub fn parse(args: &[OsString]) -> Result<Command, ParseError> {
    let mut positionals = Vec::new();
    let mut patterns = Vec::new();
    let mut globs = Vec::new();
    let (mut line_number, mut quiet, mut done) = (false, false, false);
    let (mut fold, mut fixed, mut invert, mut only_matching) = (false, false, false, false);
    let mut hidden = false;
    let mut files = false;
    let mut output = Output::Lines;
    let (mut help, mut version) = (false, false);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let bytes = arg.as_bytes();
        if !done && bytes == b"--" {
            done = true;
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
                "--line-number" if value.is_none() => line_number = true,
                "--quiet" if value.is_none() => quiet = true,
                "--hidden" if value.is_none() => hidden = true,
                "--no-hidden" if value.is_none() => hidden = false,
                "--ignore-case" if value.is_none() => fold = true,
                "--fixed-strings" if value.is_none() => fixed = true,
                "--no-fixed-strings" if value.is_none() => fixed = false,
                "--invert-match" if value.is_none() => invert = true,
                "--no-invert-match" if value.is_none() => invert = false,
                "--only-matching" if value.is_none() => only_matching = true,
                "--files" if value.is_none() => files = true,
                "--files-with-matches" if value.is_none() => {
                    output = Output::Names;
                    files = false;
                }
                "--count" if value.is_none() => {
                    output = Output::Count;
                    files = false;
                }
                "--scan" | "--scan-uncovered" if value.is_none() => {}
                "--glob" | "--color" | "--regexp" => {
                    let value = value
                        .map(str::to_owned)
                        .or_else(|| args.next().and_then(|s| s.to_str()).map(str::to_owned))
                        .ok_or_else(|| ParseError::Value(flag.to_owned()))?;
                    match flag {
                        "--glob" => globs.push(value),
                        "--regexp" => patterns.push(value),
                        _ if value != "never" => return Err(unsupported(text)),
                        _ => {}
                    }
                }
                _ => return Err(unsupported(text)),
            }
        } else if !done && bytes.starts_with(b"-") && bytes.len() > 1 {
            for (at, &flag) in bytes.iter().enumerate().skip(1) {
                match flag {
                    b'n' => line_number = true,
                    b'q' => quiet = true,
                    b'.' => hidden = true,
                    b'i' => fold = true,
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
                    b'g' | b'e' => {
                        let value = if at + 1 < bytes.len() {
                            std::str::from_utf8(&bytes[at + 1..]).ok()
                        } else {
                            args.next().and_then(|s| s.to_str())
                        }
                        .ok_or_else(|| ParseError::Value(format!("-{}", char::from(flag))))?;
                        if flag == b'g' {
                            globs.push(value.to_owned());
                        } else {
                            patterns.push(value.to_owned());
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
        }));
    }
    if patterns.is_empty() {
        if positionals.is_empty() {
            return Err(ParseError::MissingPattern);
        }
        patterns.push(
            positionals
                .remove(0)
                .into_string()
                .map_err(|_| ParseError::Utf8("pattern"))?,
        );
    }
    let paths = positionals.into_iter().map(PathBuf::from).collect();
    let grep = GrepFlags {
        case_insensitive: fold,
    }
    .compile_line_patterns(&patterns, fixed)
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
    }))
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
    let search = plan.grep.search();
    let mut result = InputResult::default();
    let mut number = 0;
    crate::scan::runs(
        input,
        size,
        buffer,
        stopped,
        Some(0),
        |run, base, terminated| {
            let run = if base == 0 {
                run.strip_prefix(b"\xef\xbb\xbf").unwrap_or(run)
            } else {
                run
            };
            let binary = memchr::memchr(0, run);
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
                if search.find(line).is_some() != plan.invert {
                    result.matched = true;
                    if plan.quiet || plan.output == Output::Names {
                        return Ok(ControlFlow::Break(()));
                    }
                    if plan.only_matching && !plan.invert {
                        for span in search.spans(line) {
                            if stopped() {
                                return Err(ScanError::Input(io::ErrorKind::Interrupted.into()));
                            }
                            result.count += 1;
                            if plan.output == Output::Lines {
                                emit(number, &line[span]).map_err(ScanError::Output)?;
                            }
                        }
                    } else {
                        result.count += 1;
                        if plan.output == Output::Lines {
                            emit(number, line).map_err(ScanError::Output)?;
                        }
                    }
                }
            }
            if binary.is_some() {
                result.binary = true;
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

    // M2: compile, matching and necessary keys see the same resolved profile.
    #[test]
    fn fixed_folded_and_repeated_profiles_are_sound_over_original_bytes() {
        let profiles: &[&[&str]] = &[
            &["-i", "KILO"],
            &["-Fi", "SEA"],
            &["-F", "red.*"],
            &["-e", "red", "-e", "(?i:violet)"],
            &["-o", "-e", "", "-e", "red"],
        ];
        let documents: &[&[u8]] = &[
            b"red.*\n",
            b"\xef\xbb\xbfred\n",
            b"VIOLET\n",
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
    // R1/P2: the scanner must stop within the chunk containing NUL, even
    // when the source has no LF or EOF and refuses a second read.
    #[test]
    fn binary_sentinel_stops_before_another_read_or_buffer_growth() {
        struct Source {
            reads: usize,
        }
        impl Read for Source {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                self.reads += 1;
                if self.reads != 1 {
                    return Err(io::Error::other("read past NUL"));
                }
                bytes[..5].copy_from_slice(b"red\0x");
                Ok(5)
            }
        }
        let Command::Search(plan) = parse(&["red".into()]).unwrap() else {
            panic!("search")
        };
        let mut source = Source { reads: 0 };
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
        assert!(result.matched && result.binary);
        assert_eq!(source.reads, 1);
        assert_eq!(buffer.len(), crate::scan::BUFFER);
    }
}
