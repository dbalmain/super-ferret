//! Local grep host: resolve operands before opening a generation, adapt policy
//! and checked reads, and commit each file's bounded plain output as a group.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

mod groups;
mod names;

use ferret_catalog::NameId;
use ferret_policy::GrepSelection;
use ferret_query::find::{CatalogSource, EntrySource, FileKind, OutputBuffer};
use ferret_query::line_grep::{self, InputResult, Output, Plan, ScanError};
use ferret_query::{CheckedReader, ReadFault, ReadOutcome, ReadRequest};

use crate::cli::{self, Exit};
use crate::engine::{self, Engine};
use crate::xdg::Dirs;

const HELP: &str = "ferret grep [OPTIONS] PATTERN [PATH...]\n\n\
  -n, --line-number   print 1-based line numbers\n\
  -g, --glob GLOB     select paths; repeat for ordered overrides\n\
  -q, --quiet         stop after a match\n\
  -i, --ignore-case   Unicode case-insensitive matching\n\
  -S, --smart-case    fold unless a literal is uppercase\n\
  -s, --case-sensitive force case-sensitive matching\n\
  -w, --word-regexp   match between half word boundaries\n\
  -x, --line-regexp   match whole lines\n\
  -o, --only-matching print nonoverlapping match spans\n\
  -l, --files-with-matches print each matching path once\n\
  -v, --invert-match  select nonmatching lines\n\
  -F, --fixed-strings treat patterns literally\n\
  -e, --regexp PATTERN add an alternative pattern\n\
  -f, --file FILE     read patterns from FILE (- for stdin)\n\
  -c, --count         count selected lines (-co counts spans)\n\
  --count-matches    count match spans, including empty spans\n\
  --include-zero     print zero counts (exit 1 without matches)\n\
  --files-without-match print paths with no selected line\n\
  -m, --max-count N   stop after N selected lines per file\n\
  --files            list selected file names without reading contents\n\
  --hidden, -.       include held hidden files and directories\n\
  --color=never      plain output\n\
  --scan             accepted; broad patterns always scan\n\n\
Directories use snapshot names/postings and current candidate bytes.\n\
An index covering each directory is required; trigrams are optional.\n\
Explicit files and stdin search directly. UTF-8 BOMs are stripped; UTF-16\n\
decoding is deferred. Excluded paths are absent. Exit: 0 match, 1 none, 2 error.\n";

#[derive(Debug)]
enum Input {
    File(PathBuf, u64),
    Directory(PathBuf),
    Stdin,
    Failed(PathBuf, io::Error),
}

/// Runs the resolved grep profile locally. Help/version bypass configuration
/// and index resolution; all search errors use grep's status 2, including
/// output failures.
pub fn run(args: &[OsString], index: Option<&Path>, global_json: bool) -> Exit {
    let command = match line_grep::parse(args) {
        Ok(command) => command,
        Err(error) => return failure(&error.to_string()),
    };
    let plan = match command {
        line_grep::Command::Help => return print(HELP.as_bytes()),
        line_grep::Command::Version => {
            return print(format!("ferret grep {}\n", env!("CARGO_PKG_VERSION")).as_bytes());
        }
        line_grep::Command::Files(plan) => {
            if let Some(exit) = context_error(global_json) {
                return exit;
            }
            return names::run(&plan, index);
        }
        line_grep::Command::Search(plan) => plan,
    };
    if let Some(exit) = context_error(global_json) {
        return exit;
    }
    let mut selection = GrepSelection::default().with_hidden(plan.hidden);
    for glob in &plan.globs {
        if let Err(error) = selection.push(glob) {
            return failure(&format!("glob {glob:?}: {error}"));
        }
    }
    if plan.max_count == Some(0) {
        return Exit::NoMatch;
    }
    let cancelled = AtomicBool::new(false);
    let stopped = || cancelled.load(Ordering::Acquire);
    let inputs = if plan.stdin_patterns && plan.paths.is_empty() {
        resolve_inputs(&[PathBuf::from(".")], false)
    } else {
        resolve_inputs(&plan.paths, false)
    };
    let recursive = inputs
        .iter()
        .any(|input| matches!(input, Input::Directory(_)));
    let engine = if recursive {
        match open_index(index) {
            Ok(engine) => Some(engine),
            Err(error) => return failure(&error),
        }
    } else {
        None
    };
    let pin = engine.as_ref().map(Engine::pin);
    let candidates = if !plan.invert {
        match pin
            .as_ref()
            .map(|pin| pin.grep_candidates(&plan.grep, &stopped))
            .transpose()
        {
            Ok(candidates) => candidates,
            Err(error) => return failure(&error.to_string()),
        }
    } else {
        None
    };
    let names = match pin
        .as_ref()
        .map(|pin| pin.doc_names_until(stopped))
        .transpose()
    {
        Ok(names) => names.flatten(),
        Err(error) => return failure(&error.to_string()),
    };
    let documents = match pin
        .as_ref()
        .map(|pin| ferret_crawl::Documents::by_name(pin.catalog()))
        .transpose()
    {
        Ok(documents) => documents,
        Err(error) => return failure(&error.to_string()),
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => return failure(&error.to_string()),
    };
    if let Some(pin) = &pin {
        for input in &inputs {
            if let Input::Directory(path) = input {
                let mut source =
                    CatalogSource::paths(pin.catalog().clone(), vec![path.clone()], cwd.clone());
                if let Some(Err(error)) = source.next(true) {
                    return failure(&format!(
                        "{}: {}; index this directory; live modes arrive in M6",
                        error.path.display(),
                        error.error
                    ));
                }
            }
        }
    }
    let stdout = io::stdout();
    let mut host = Host {
        plan: &plan,
        filename: recursive || inputs.len() > 1,
        writer: stdout.lock(),
        buffer: Vec::new(),
        matched: false,
        errors: false,
        cancelled: Some(&cancelled),
    };
    // rg diagnoses failed explicit operands before searching, even when an
    // earlier operand supplies a quiet match.
    let inputs: Vec<_> = inputs
        .into_iter()
        .filter_map(|input| {
            if let Input::Failed(path, error) = input {
                host.error(&path, &error);
                None
            } else {
                Some(input)
            }
        })
        .collect();
    let threads = pin.as_ref().map_or_else(
        || {
            let available = std::thread::available_parallelism().map_or(1, |n| n.get());
            engine::search_threads(
                engine::env_threads(std::env::var("FERRET_SEARCH_THREADS").ok().as_deref()),
                None,
                available,
            )
        },
        |pin| pin.search_threads(),
    );
    let fork = || {
        pin.as_ref()
            .zip(documents.as_ref())
            .map(|(pin, documents)| engine::reader::checked(pin.catalog(), documents))
    };
    let mut inputs = inputs.into_iter().peekable();
    while inputs.peek().is_some() {
        let mut segment = Vec::new();
        while inputs
            .peek()
            .is_some_and(|input| !matches!(input, Input::Stdin))
        {
            if let Some(input) = inputs.next() {
                segment.push(input);
            }
        }
        if !segment.is_empty() {
            let threads = if segment
                .iter()
                .any(|input| matches!(input, Input::Directory(_)))
            {
                threads
            } else {
                threads.min(segment.len())
            };
            let mut source = groups::Source::new(
                segment,
                pin.as_ref().map(|p| p.catalog()),
                candidates.as_ref(),
                &selection,
                cwd.clone(),
            )
            .with_absence(plan.universe() == line_grep::Universe::Searchable);
            if let Err(error) = groups::run(
                &mut host,
                move |stopped| source.next(stopped),
                &fork,
                names,
                threads,
                &cancelled,
            ) {
                return failure(&format!("writing stdout: {error}"));
            }
            if plan.quiet && host.matched {
                return Exit::Ok;
            }
        }
        if matches!(inputs.peek(), Some(Input::Stdin)) {
            inputs.next();
            if let Err(error) = host.direct(Path::new("-"), &mut io::stdin().lock(), u64::MAX) {
                return failure(&format!("writing stdout: {error}"));
            }
            if plan.quiet && host.matched {
                return Exit::Ok;
            }
        }
    }
    if let Err(error) = host.writer.flush() {
        return failure(&format!("writing stdout: {error}"));
    }
    if host.errors {
        Exit::Usage
    } else if host.matched {
        Exit::Ok
    } else {
        Exit::NoMatch
    }
}

fn context_error(global_json: bool) -> Option<Exit> {
    if global_json {
        return Some(failure(
            "global --json framing is not supported for grep; grep --json is deferred to M9",
        ));
    }
    if std::env::var_os("RIPGREP_CONFIG_PATH").is_some_and(|s| !s.is_empty()) {
        return Some(failure("RIPGREP_CONFIG_PATH is set; pass flags explicitly"));
    }
    None
}

fn print(bytes: &[u8]) -> Exit {
    match io::stdout().lock().write_all(bytes) {
        Ok(()) => Exit::Ok,
        Err(error) => failure(&format!("writing stdout: {error}")),
    }
}

fn failure(message: &str) -> Exit {
    cli::error(&format!("grep: {message}"));
    Exit::Usage
}

fn open_index(index: Option<&Path>) -> Result<Engine, String> {
    let index = index_path(index)?;
    let engine = Engine::open(&index)
        .map_err(|error| {
            format!(
                "{}: {error}; re-index or use a direct file/stdin",
                index.display()
            )
        })?
        .ok_or_else(|| {
            format!(
                "no index in {}; run ferret index DIR; live modes arrive in M6",
                index.display()
            )
        })?;
    engine
        .open_content(&index)
        .map_err(|error| error.to_string())?;
    Ok(engine)
}

fn index_path(index: Option<&Path>) -> Result<PathBuf, String> {
    index
        .map(Path::to_owned)
        .or_else(|| {
            std::env::var_os("FERRET_INDEX")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| Dirs::from_env().ok().map(|dirs| dirs.data))
        .ok_or("cannot locate index; set --index or FERRET_INDEX")
        .map_err(str::to_owned)
}

fn resolve_inputs(paths: &[PathBuf], names_only: bool) -> Vec<Input> {
    let defaults;
    let paths = if paths.is_empty() {
        defaults = vec![if !names_only && readable_stdin() {
            PathBuf::from("-")
        } else {
            PathBuf::from(".")
        }];
        &defaults
    } else {
        paths
    };
    paths
        .iter()
        .map(|path| {
            if path == Path::new("-") {
                return Input::Stdin;
            }
            match fs::metadata(path) {
                Ok(metadata) if metadata.is_dir() => Input::Directory(path.clone()),
                Ok(metadata) if metadata.is_file() || names_only => {
                    Input::File(path.clone(), metadata.len())
                }
                Ok(_) => Input::Failed(
                    path.clone(),
                    io::Error::other("special-file operands are not supported"),
                ),
                Err(error) => Input::Failed(path.clone(), error),
            }
        })
        .collect()
}

fn readable_stdin() -> bool {
    fs::metadata("/proc/self/fd/0").is_ok_and(|metadata| {
        let kind = metadata.file_type();
        kind.is_file() || kind.is_fifo() || kind.is_socket()
    })
}

struct Host<'a, W> {
    plan: &'a Plan,
    filename: bool,
    writer: W,
    buffer: Vec<u8>,
    matched: bool,
    errors: bool,
    cancelled: Option<&'a AtomicBool>,
}

impl<W: Write> Host<'_, W> {
    fn error(&mut self, path: &Path, error: &dyn std::fmt::Display) {
        cli::error(&format!("grep: {}: {error}", path.display()));
        self.errors = true;
    }

    #[cfg(test)]
    fn file(&mut self, path: &Path, recursive: bool) -> io::Result<()> {
        let flag = self.cancelled;
        let stopped = || flag.is_some_and(|flag| flag.load(Ordering::Acquire));
        self.transaction(path, &mut |each| {
            groups::read_file(path, recursive, each, &stopped)
        })
    }

    fn direct(&mut self, path: &Path, input: &mut dyn Read, size: u64) -> io::Result<()> {
        let flag = self.cancelled;
        let stopped = || flag.is_some_and(|flag| flag.load(Ordering::Acquire));
        self.transaction(path, &mut |each| {
            groups::stream(input, size, each, &stopped)
        })
    }

    #[cfg(test)]
    fn checked(
        &mut self,
        path: &Path,
        name: NameId,
        reader: &mut CheckedReader<'_>,
    ) -> io::Result<()> {
        let flag = self.cancelled;
        let stopped = || flag.is_some_and(|flag| flag.load(Ordering::Acquire));
        self.transaction(path, &mut |each| {
            reader(name, ReadRequest::Stream(each, &stopped))
        })
    }

    fn transaction(
        &mut self,
        path: &Path,
        read: &mut dyn FnMut(ferret_query::Consumer<'_>) -> ReadOutcome,
    ) -> io::Result<()> {
        let flag = self.cancelled;
        let stopped = || flag.is_some_and(|flag| flag.load(Ordering::Acquire));
        let render = groups::Render {
            plan: self.plan,
            prefix: self
                .filename
                .then_some(label(path, self.plan.paths.is_empty())),
            stopped: &stopped,
        };
        match groups::observe(&render, &mut self.buffer, read, false)? {
            groups::Observed::Ready(group) => self.commit(path, group.result, group.output),
            groups::Observed::Failed(error) => {
                self.error(path, &error);
                Ok(())
            }
            groups::Observed::Skipped => Ok(()),
        }
    }

    fn commit(
        &mut self,
        path: &Path,
        result: InputResult,
        mut output: OutputBuffer,
    ) -> io::Result<()> {
        self.matched |= result.matched;
        if self.plan.quiet {
            return Ok(());
        }
        if matches!(self.plan.output, Output::Names | Output::WithoutMatch) {
            if result.matched {
                self.writer
                    .write_all(label(path, self.plan.paths.is_empty()))?;
                self.writer.write_all(b"\n")?;
            }
            return Ok(());
        }
        // M4 R1: counts stay numeric after a binary sentinel, as in rg.
        if matches!(self.plan.output, Output::Count | Output::CountMatches) {
            if result.matched || (self.plan.include_zero && result.searched) {
                if self.filename {
                    self.writer
                        .write_all(label(path, self.plan.paths.is_empty()))?;
                    self.writer.write_all(b":")?;
                }
                writeln!(self.writer, "{}", result.count)?;
            }
        } else if result.binary && result.matched {
            self.writer.write_all(if path == Path::new("-") {
                b"(standard input)"
            } else {
                label(path, self.plan.paths.is_empty())
            })?;
            self.writer.write_all(b": binary file matches\n")?;
        } else {
            let flag = self.cancelled;
            groups::emit(&mut output, &mut self.writer, &|| {
                flag.is_some_and(|flag| flag.load(Ordering::Acquire))
            })?;
        }
        Ok(())
    }
}

fn label(path: &Path, implicit: bool) -> &[u8] {
    if path == Path::new("-") {
        b"<stdin>"
    } else {
        let bytes = path.as_os_str().as_bytes();
        if implicit {
            bytes.strip_prefix(b"./").unwrap_or(bytes)
        } else {
            bytes
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferret_query::{ReadOperation, ReadVersion};
    use std::fs::OpenOptions;

    // R1/P1: splice two nonmatching versions after the first real read.
    // Neither explicit input nor the opaque fallback may publish the splice.
    #[test]
    fn regular_files_discard_torn_matches_and_retry_current_bytes() {
        use std::os::unix::fs::FileExt;
        let mut outcomes = Vec::new();
        for recursive in [false, true] {
            let path = std::env::temp_dir()
                .join(format!("ferret-torn-{}-{recursive}", std::process::id()));
            let mut bytes = vec![b'x'; 100_000];
            bytes[..3].copy_from_slice(b"red");
            bytes[99_997..].copy_from_slice(b"OLD");
            fs::write(&path, bytes).unwrap();
            let writer = OpenOptions::new().write(true).open(&path).unwrap();
            let mut changed = false;
            seam::AFTER_READ.with_borrow_mut(|hook| {
                *hook = Some(Box::new(move || {
                    if !changed {
                        writer.write_all_at(b"blu", 0).unwrap();
                        writer.write_all_at(b"NEW", 99_997).unwrap();
                        changed = true;
                    }
                }))
            });
            let line_grep::Command::Search(plan) =
                line_grep::parse(&["^red.*NEW$".into()]).unwrap()
            else {
                panic!("search")
            };
            let mut host = Host {
                plan: &plan,
                filename: false,
                writer: Vec::new(),
                buffer: Vec::new(),
                matched: false,
                errors: false,
                cancelled: None,
            };
            host.file(&path, recursive).unwrap();
            seam::AFTER_READ.with_borrow_mut(|hook| *hook = None);
            fs::remove_file(path).unwrap();
            outcomes.push((recursive, host.writer.is_empty(), host.matched, host.errors));
        }
        assert_eq!(
            outcomes,
            [(false, true, false, false), (true, true, false, false)]
        );
    }

    // R3: size zero changes the readable extent, never the version check.
    // Fill after the opening stat, then splice a nonmatching line mid-read.
    #[test]
    fn initially_empty_files_discard_torn_matches_and_bound_retries() {
        use std::cell::Cell;
        use std::os::unix::fs::FileExt;
        use std::rc::Rc;

        let mut outcomes = Vec::new();
        for recursive in [false, true] {
            for unstable in [false, true] {
                let path = std::env::temp_dir().join(format!(
                    "ferret-empty-torn-{}-{recursive}-{unstable}",
                    std::process::id()
                ));
                fs::write(&path, b"").unwrap();
                let writer = OpenOptions::new().write(true).open(&path).unwrap();
                let attempts = Rc::new(Cell::new(0));
                let observed = attempts.clone();
                seam::BEFORE_SCAN.with_borrow_mut(|hook| {
                    *hook = Some(Box::new(move || {
                        let attempt = observed.get() + 1;
                        observed.set(attempt);
                        if attempt == 1 {
                            let mut bytes = vec![b'x'; 100_000];
                            bytes[..3].copy_from_slice(b"red");
                            bytes[99_997..].copy_from_slice(b"OLD");
                            writer.write_all_at(&bytes, 0).unwrap();
                        }
                        let writer = writer.try_clone().unwrap();
                        let mut changed = false;
                        seam::AFTER_READ.with_borrow_mut(|hook| {
                            *hook = Some(Box::new(move || {
                                if changed {
                                    return;
                                }
                                changed = true;
                                if attempt == 1 {
                                    writer.write_all_at(b"blu", 0).unwrap();
                                    writer.write_all_at(b"NEW", 99_997).unwrap();
                                } else if unstable {
                                    // A size change makes retry failure
                                    // independent
                                    // of timestamp resolution or clock timing.
                                    writer.write_all_at(b"x", 100_000).unwrap();
                                }
                            }));
                        });
                    }));
                });
                let line_grep::Command::Search(plan) =
                    line_grep::parse(&["^red.*NEW$".into()]).unwrap()
                else {
                    panic!("search")
                };
                let mut host = Host {
                    plan: &plan,
                    filename: false,
                    writer: Vec::new(),
                    buffer: Vec::new(),
                    matched: false,
                    errors: false,
                    cancelled: None,
                };
                host.file(&path, recursive).unwrap();
                seam::BEFORE_SCAN.with_borrow_mut(|hook| *hook = None);
                seam::AFTER_READ.with_borrow_mut(|hook| *hook = None);
                fs::remove_file(path).unwrap();
                outcomes.push((
                    recursive,
                    unstable,
                    attempts.get(),
                    host.writer.is_empty(),
                    host.matched,
                    host.errors,
                ));
            }
        }
        assert_eq!(
            outcomes,
            [
                (false, false, 2, true, false, false),
                (false, true, 2, true, false, true),
                (true, false, 2, true, false, false),
                (true, true, 2, true, false, true),
            ]
        );
    }

    #[test]
    fn checked_streams_retry_once_and_commit_only_stable_output() {
        let line_grep::Command::Search(plan) = line_grep::parse(&["red".into()]).unwrap() else {
            panic!("search")
        };
        let mut host = Host {
            plan: &plan,
            filename: false,
            writer: Vec::new(),
            buffer: Vec::new(),
            matched: false,
            errors: false,
            cancelled: None,
        };
        let mut attempts = 0;
        let mut reader = |_, request: ReadRequest<'_>| {
            attempts += 1;
            let ReadRequest::Stream(each, _) = request else {
                panic!("stream")
            };
            let bytes: &[u8] = if attempts == 1 {
                b"red partial\n"
            } else {
                b"red stable\n"
            };
            each(&mut &bytes[..], bytes.len() as u64).unwrap();
            if attempts == 1 {
                ReadOutcome::RetryChanged
            } else {
                ReadOutcome::Stable(ReadVersion::Current)
            }
        };
        host.checked(Path::new("file"), NameId(0), &mut reader)
            .unwrap();
        assert_eq!(attempts, 2);
        assert_eq!(host.writer, b"red stable\n");
        assert!(host.matched && !host.errors);
    }

    #[test]
    fn failed_or_unstable_streams_never_commit_partial_matches() {
        let line_grep::Command::Search(plan) = line_grep::parse(&["red".into()]).unwrap() else {
            panic!("search")
        };
        for mode in 0..4 {
            let mut host = Host {
                plan: &plan,
                filename: false,
                writer: Vec::new(),
                buffer: Vec::new(),
                matched: false,
                errors: false,
                cancelled: None,
            };
            let mut attempts = 0;
            let mut reader = |_, request: ReadRequest<'_>| {
                attempts += 1;
                let ReadRequest::Stream(each, _) = request else {
                    panic!("stream")
                };
                each(&mut &b"red partial\n"[..], 12).unwrap();
                match mode {
                    0 => ReadOutcome::RetryChanged,
                    1 => ReadOutcome::Failed(ReadFault::Io(
                        ReadOperation::Read,
                        io::ErrorKind::PermissionDenied.into(),
                    )),
                    2 => ReadOutcome::Vanished,
                    _ => ReadOutcome::Cancelled,
                }
            };
            host.checked(Path::new("file"), NameId(0), &mut reader)
                .unwrap();
            assert_eq!(attempts, if mode == 0 { 2 } else { 1 });
            assert!(host.writer.is_empty() && !host.matched);
            assert_eq!(host.errors, mode < 2);
        }
    }

    #[test]
    fn output_failure_is_returned_to_stop_the_host() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let line_grep::Command::Search(plan) = line_grep::parse(&["red".into()]).unwrap() else {
            panic!("search")
        };
        let mut host = Host {
            plan: &plan,
            filename: false,
            writer: Broken,
            buffer: Vec::new(),
            matched: false,
            errors: false,
            cancelled: None,
        };
        assert_eq!(
            host.direct(Path::new("file"), &mut &b"red\n"[..], 4)
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
    }
}

#[cfg(test)]
mod seam {
    use std::cell::RefCell;
    use std::io::{self, Read};

    thread_local! {
        pub(super) static BEFORE_SCAN: RefCell<Option<Box<dyn FnMut()>>> = RefCell::new(None);
        pub(super) static AFTER_READ: RefCell<Option<Box<dyn FnMut()>>> = RefCell::new(None);
    }

    pub(super) struct Observed<'a>(pub(super) &'a mut dyn Read);

    impl Read for Observed<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            let count = self.0.read(bytes)?;
            if count != 0 {
                AFTER_READ.with_borrow_mut(|hook| {
                    if let Some(hook) = hook {
                        hook();
                    }
                });
            }
            Ok(count)
        }
    }
}
