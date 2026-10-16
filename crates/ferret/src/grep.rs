//! Local grep host: resolve operands before opening a generation, adapt policy
//! and checked reads, and commit each file's bounded plain output as a group.

use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

use ferret_catalog::NameId;
use ferret_policy::GrepSelection;
use ferret_query::find::{CatalogSource, EntrySource, FileKind, OutputBuffer};
use ferret_query::line_grep::{self, InputResult, Plan};
use ferret_query::{CheckedReader, ReadFault, ReadOutcome, ReadRequest};

use crate::cli::{self, Exit};
use crate::engine::{self, Engine};
use crate::xdg::Dirs;

const HELP: &str = "ferret grep [OPTIONS] PATTERN [PATH...]\n\n\
  -n, --line-number   print 1-based line numbers\n\
  -g, --glob GLOB     select paths; repeat for ordered overrides\n\
  -q, --quiet         stop after a match\n\
  --color=never      plain output (M1)\n\
  --scan             accepted; broad patterns always scan\n\n\
Directories use snapshot names/postings and current candidate bytes.\n\
An index covering each directory is required; trigrams are optional.\n\
Explicit files and stdin search directly. UTF-8 BOMs are stripped; UTF-16\n\
decoding is deferred. Excluded paths are absent. Exit: 0 match, 1 none, 2 error.\n";

#[derive(Debug)]
enum Input {
    File(PathBuf),
    Directory(PathBuf),
    Stdin,
    Failed(PathBuf, io::Error),
}

/// Runs M1 locally. Help/version bypass configuration and index resolution;
/// all search errors use grep's status 2, including output failures.
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
        line_grep::Command::Search(plan) => plan,
    };
    if global_json {
        return failure(
            "global --json framing is not supported for grep; grep --json is deferred to M9",
        );
    }
    if std::env::var_os("RIPGREP_CONFIG_PATH").is_some_and(|s| !s.is_empty()) {
        return failure("RIPGREP_CONFIG_PATH is set; pass flags explicitly");
    }
    let mut selection = GrepSelection::default();
    for glob in &plan.globs {
        if let Err(error) = selection.push(glob) {
            return failure(&format!("glob {glob:?}: {error}"));
        }
    }
    let inputs = resolve_inputs(&plan.paths);
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
    let candidates = match pin
        .as_ref()
        .map(|pin| pin.grep_candidates(&plan.grep))
        .transpose()
    {
        Ok(candidates) => candidates,
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
    let mut checked = pin
        .as_ref()
        .zip(documents.as_ref())
        .map(|(pin, documents)| engine::reader::checked(pin.catalog(), documents));
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
    };
    for input in inputs {
        let result = match input {
            Input::Failed(path, error) => {
                host.error(&path, &error);
                Ok(())
            }
            Input::Stdin => host.direct(Path::new("-"), &mut io::stdin().lock(), u64::MAX),
            Input::File(path) => host.file(&path, false),
            Input::Directory(path) => {
                let (Some(pin), Some(candidates), Some(reader)) =
                    (pin.as_ref(), candidates.as_ref(), checked.as_mut())
                else {
                    return failure("directory input has no pinned reader");
                };
                let catalog = pin.catalog();
                let mut source =
                    CatalogSource::paths(catalog.clone(), vec![path.clone()], cwd.clone());
                let mut descend = true;
                while let Some(entry) = source.next(descend) {
                    descend = true;
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error) => {
                            host.error(&error.path, &error.error);
                            continue;
                        }
                    };
                    let kind = match entry.kind() {
                        Ok(kind) => kind,
                        Err(error) => {
                            host.error(entry.path(), &error);
                            continue;
                        }
                    };
                    let relative = entry.path().strip_prefix(&cwd).unwrap_or(entry.path());
                    if kind == FileKind::Directory {
                        descend = selection.descend(relative.as_os_str().as_bytes(), entry.depth());
                        continue;
                    }
                    if kind != FileKind::File {
                        continue;
                    }
                    if !selection.allows(relative.as_os_str().as_bytes(), entry.depth()) {
                        continue;
                    }
                    let result = if let Some(name) = entry.catalog_name() {
                        let inode = catalog.name(name).child;
                        if catalog
                            .doc(inode)
                            .is_none_or(|doc| !candidates.contains(doc.0))
                        {
                            continue;
                        }
                        host.checked(entry.path(), name, &mut **reader)
                    } else {
                        host.file(entry.path(), true)
                    };
                    if let Err(error) = result {
                        return failure(&format!("writing stdout: {error}"));
                    }
                    if host.plan.quiet && host.matched {
                        break;
                    }
                }
                Ok(())
            }
        };
        if let Err(error) = result {
            return failure(&format!("writing stdout: {error}"));
        }
        if plan.quiet && host.matched {
            return Exit::Ok;
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
    let index = index
        .map(Path::to_owned)
        .or_else(|| {
            std::env::var_os("FERRET_INDEX")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| Dirs::from_env().ok().map(|dirs| dirs.data))
        .ok_or("cannot locate index; set --index or FERRET_INDEX")?;
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

fn resolve_inputs(paths: &[PathBuf]) -> Vec<Input> {
    let defaults;
    let paths = if paths.is_empty() {
        defaults = vec![if readable_stdin() {
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
                Ok(metadata) if metadata.is_file() => Input::File(path.clone()),
                Ok(metadata) if metadata.is_dir() => Input::Directory(path.clone()),
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
}

impl<W: Write> Host<'_, W> {
    fn error(&mut self, path: &Path, error: &dyn std::fmt::Display) {
        cli::error(&format!("grep: {}: {error}", path.display()));
        self.errors = true;
    }

    fn file(&mut self, path: &Path, recursive: bool) -> io::Result<()> {
        let cap = recursive.then(|| ferret_policy::Config::default().size_cap);
        self.transaction(path, &mut |each| {
            engine::reader::direct(path, cap, &mut |input, size| {
                if recursive {
                    let mut buffered =
                        io::BufReader::with_capacity(ferret_policy::SNIFF_LEN, input);
                    if ferret_policy::sniff(buffered.fill_buf()?) == ferret_policy::Content::Binary
                    {
                        return Ok(());
                    }
                    each(&mut buffered, size)
                } else {
                    each(input, size)
                }
            })
        })
    }

    fn direct(&mut self, path: &Path, input: &mut dyn Read, size: u64) -> io::Result<()> {
        let mut output = OutputBuffer::default();
        let result = scan(
            self.plan,
            self.filename
                .then_some(label(path, self.plan.paths.is_empty())),
            input,
            size,
            &mut self.buffer,
            &mut output,
        );
        match result {
            Ok(result) => self.commit(path, result, output),
            Err(error) => {
                self.error(path, &error);
                Ok(())
            }
        }
    }

    fn checked(
        &mut self,
        path: &Path,
        name: NameId,
        reader: &mut CheckedReader<'_>,
    ) -> io::Result<()> {
        self.transaction(path, &mut |each| {
            reader(name, ReadRequest::Stream(each, &|| false))
        })
    }

    fn transaction(
        &mut self,
        path: &Path,
        read: &mut dyn FnMut(ferret_query::Consumer<'_>) -> ReadOutcome,
    ) -> io::Result<()> {
        let mut output = OutputBuffer::default();
        let mut result = InputResult::default();
        let outcome = ferret_query::retry_read(
            || {
                output = OutputBuffer::default();
                result = InputResult::default();
                let mut each = |input: &mut dyn Read, size| {
                    result = scan(
                        self.plan,
                        self.filename
                            .then_some(label(path, self.plan.paths.is_empty())),
                        input,
                        size,
                        &mut self.buffer,
                        &mut output,
                    )?;
                    Ok(())
                };
                read(&mut each)
            },
            &|| false,
        );
        match outcome {
            ReadOutcome::Stable(_) => return self.commit(path, result, output),
            ReadOutcome::Failed(error) => self.error(path, &error),
            ReadOutcome::Vanished | ReadOutcome::Cancelled => {}
            ReadOutcome::RetryChanged => self.error(path, &ReadFault::Unstable),
        }
        Ok(())
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
        if result.binary {
            if result.matched {
                self.writer.write_all(if path == Path::new("-") {
                    b"(standard input)"
                } else {
                    label(path, self.plan.paths.is_empty())
                })?;
                self.writer.write_all(b": binary file matches\n")?;
            }
        } else {
            output.write_to(&mut self.writer)?;
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

fn scan(
    plan: &Plan,
    prefix: Option<&[u8]>,
    input: &mut dyn Read,
    size: u64,
    buffer: &mut Vec<u8>,
    output: &mut OutputBuffer,
) -> io::Result<InputResult> {
    #[cfg(test)]
    let mut observed = seam::Observed(input);
    #[cfg(test)]
    let input: &mut dyn Read = &mut observed;
    line_grep::scan(plan, input, size, buffer, &|| false, |number, line| {
        if let Some(prefix) = prefix {
            output.write_all(prefix)?;
            output.write_all(b":")?;
        }
        if plan.line_number {
            write!(output, "{number}:")?;
        }
        output.write_all(line)?;
        output.write_all(b"\n")
    })
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
