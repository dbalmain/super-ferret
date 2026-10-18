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
mod json;
mod live;
mod names;
mod output;
mod presentation;
mod sources;

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
  -U, --multiline     permit matches across LF boundaries\n\
  --multiline-dotall  let dot match LF with -U\n\
  -r, --replace TEXT replace matches in output; inputs stay unchanged\n\
  --json             rg JSON Lines events and summary\n\
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
  -A/-B/-C N         after/before/around context\n\
  -H/-I              force/suppress filenames\n\
  --heading/--no-heading group matches by file\n\
  -N, --no-line-number suppress line numbers\n\
  -0, --null         NUL-terminate filenames\n\
  -p, --pretty       colour, headings and line numbers\n\
  --color MODE       auto/always/ansi/never\n\
  --live             current tree with ferret visibility policy\n\
  -u/--no-ignore     unrestricted current-tree walk\n\
  -uu                unrestricted including hidden paths\n\
  -a, --text         search binary bytes as text\n\
  --max-depth N      limit directory traversal depth\n\
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
    let start = std::time::Instant::now();
    let command = match line_grep::parse(args) {
        Ok(command) => command,
        Err(error) => return failure(&error.to_string()),
    };
    let mut plan = match command {
        line_grep::Command::Help => return print(HELP.as_bytes()),
        line_grep::Command::Version => {
            return print(format!("ferret grep {}\n", env!("CARGO_PKG_VERSION")).as_bytes());
        }
        line_grep::Command::Files(mut plan) => {
            if let Some(exit) = context_error(global_json) {
                return exit;
            }
            presentation::resolve(&mut plan.presentation);
            return names::run(&plan, index);
        }
        line_grep::Command::Search(plan) => plan,
    };
    if let Some(exit) = context_error(global_json) {
        return exit;
    }
    let tty = presentation::resolve(&mut plan.presentation);
    plan.line_number = plan.presentation.numbers.unwrap_or(tty || plan.json);
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
    let live = if recursive && plan.traversal.live && !plan.traversal.unrestricted {
        match live::Anchors::open(index) {
            Ok(anchors) => Some(anchors),
            Err(error) => return failure(&error),
        }
    } else {
        None
    };
    let engine = if recursive && !plan.traversal.unrestricted && !plan.traversal.live {
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
    for input in &inputs {
        if let Input::Directory(path) = input {
            if let Some(anchors) = &live {
                if let Err(error) = anchors.validate(path) {
                    return failure(&format!("{}: {error}", path.display()));
                }
            } else if let Some(pin) = &pin {
                let mut source =
                    match sources::Directory::new(path, Some(pin.catalog()), &cwd, false) {
                        Ok(source) => source,
                        Err(error) => return failure(&error.to_string()),
                    };
                if let Some(Err(error)) = source.next(true) {
                    return failure(&format!(
                        "{}: {}; index this directory or use --live / --no-ignore",
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
        filename: plan
            .presentation
            .filename
            .unwrap_or(recursive || inputs.len() > 1),
        writer: output::Writer::new(&stdout, block_stdout(&plan)),
        buffer: Vec::new(),
        matched: false,
        emitted_group: false,
        json_stats: json::Stats::default(),
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
            if let Some(anchors) = &live {
                return anchors.search_threads();
            }
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
    'inputs: while inputs.peek().is_some() {
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
            .with_traversal(plan.traversal, plan.text)
            .with_live(live.as_ref())
            .with_absence(plan.universe() == line_grep::Universe::Searchable);
            if let Err(error) = groups::run(
                &mut host,
                move |stopped| source.next(stopped),
                &fork,
                names,
                threads,
                &cancelled,
            ) {
                return output_failure(&error);
            }
            if plan.quiet && host.matched {
                if !plan.json {
                    return Exit::Ok;
                }
                break 'inputs;
            }
        }
        if matches!(inputs.peek(), Some(Input::Stdin)) {
            inputs.next();
            if let Err(error) = host.direct(Path::new("-"), &mut io::stdin().lock(), u64::MAX) {
                return output_failure(&error);
            }
            if plan.quiet && host.matched {
                if !plan.json {
                    return Exit::Ok;
                }
                break 'inputs;
            }
        }
    }
    if plan.json
        && let Err(error) = json::summary(&mut host.writer, &host.json_stats, start)
    {
        return output_failure(&error);
    }
    if let Err(error) = host.writer.flush() {
        return output_failure(&error);
    }
    if plan.quiet && host.matched {
        Exit::Ok
    } else if host.errors {
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
            "global --json framing is not supported for grep; use ferret grep --json",
        ));
    }
    if std::env::var_os("RIPGREP_CONFIG_PATH").is_some_and(|s| !s.is_empty()) {
        return Some(failure("RIPGREP_CONFIG_PATH is set; pass flags explicitly"));
    }
    None
}

fn print(bytes: &[u8]) -> Exit {
    let stdout = io::stdout();
    let mut writer = output::Writer::new(&stdout, true);
    match writer.write_all(bytes).and_then(|()| writer.flush()) {
        Ok(()) => Exit::Ok,
        Err(error) => output_failure(&error),
    }
}

/// D94: a reader that closes the pipe ends grep quietly with success, as rg
/// does. Every other output fault is reported.
fn output_failure(error: &io::Error) -> Exit {
    if error.kind() == io::ErrorKind::BrokenPipe {
        Exit::Ok
    } else {
        failure(&format!("writing stdout: {error}"))
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
    emitted_group: bool,
    json_stats: json::Stats,
    errors: bool,
    cancelled: Option<&'a AtomicBool>,
}

/// Live policy loading warns on stderr from search workers, which no group
/// flush orders against output, so live mode keeps std's line buffer, as
/// do NUL name modes.
fn block_stdout(plan: &Plan) -> bool {
    !nul_names(plan) && !plan.traversal.live
}

fn nul_names(plan: &Plan) -> bool {
    plan.presentation.null
        && matches!(plan.output, Output::Names | Output::WithoutMatch)
        && !plan.json
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
        let plan = self.plan;
        self.transaction(path, &mut |each| {
            groups::read_file(path, recursive, plan, each, &stopped)
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
            prefix: (self.filename || self.plan.json)
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

    fn commit(&mut self, path: &Path, result: InputResult, output: OutputBuffer) -> io::Result<()> {
        let published = self.publish(path, result, output);
        // NUL names retain std's existing line buffering and diagnostic
        // ordering, including a pending name when the next operand fails.
        if nul_names(self.plan) {
            return published;
        }
        // Surface even a short final group's write error before another
        // operand, diagnostic, quiet return or stdin barrier can run.
        self.writer.flush()?;
        published
    }

    fn publish(
        &mut self,
        path: &Path,
        result: InputResult,
        mut output: OutputBuffer,
    ) -> io::Result<()> {
        self.matched |= result.matched;
        if self.plan.json {
            let mut printed = 0;
            if !self.plan.quiet && !output.is_empty() {
                let path = label(path, self.plan.paths.is_empty());
                let mut begin = Vec::new();
                json::begin(&mut begin, path)?;
                printed = begin.len() as u64 + output.len();
                self.writer.write_all(&begin)?;
                let flag = self.cancelled;
                groups::emit(&mut output, &mut self.writer, &|| {
                    flag.is_some_and(|flag| flag.load(Ordering::Acquire))
                })?;
                json::end(&mut self.writer, path, result, printed)?;
            }
            self.json_stats.add(result, printed);
            return Ok(());
        }
        if self.plan.quiet {
            return Ok(());
        }
        if matches!(self.plan.output, Output::Names | Output::WithoutMatch) {
            if result.matched {
                presentation::path(
                    &self.plan.presentation,
                    &mut self.writer,
                    label(path, self.plan.paths.is_empty()),
                )?;
                self.writer.write_all(if self.plan.presentation.null {
                    b"\0"
                } else {
                    b"\n"
                })?;
            }
            return Ok(());
        }
        // M4 R1: counts stay numeric after a binary sentinel, as in rg.
        if matches!(self.plan.output, Output::Count | Output::CountMatches) {
            if result.count > 0 || (self.plan.include_zero && result.searched) {
                if self.filename {
                    presentation::path(
                        &self.plan.presentation,
                        &mut self.writer,
                        label(path, self.plan.paths.is_empty()),
                    )?;
                    self.writer.write_all(if self.plan.presentation.null {
                        b"\0"
                    } else {
                        b":"
                    })?;
                }
                writeln!(self.writer, "{}", result.count)?;
            }
        } else if result.binary {
            if result.matched {
                self.writer.write_all(if path == Path::new("-") {
                    b"(standard input)"
                } else {
                    label(path, self.plan.paths.is_empty())
                })?;
                self.writer.write_all(b": binary file matches\n")?;
                self.emitted_group = true;
            }
        } else {
            if self.emitted_group && !output.is_empty() {
                if self.plan.presentation.heading == Some(true) {
                    self.writer.write_all(b"\n")?;
                } else if self.plan.before > 0 || self.plan.after > 0 {
                    self.writer.write_all(b"--\n")?;
                }
            }
            self.emitted_group |= !output.is_empty();
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
                emitted_group: false,
                json_stats: json::Stats::default(),
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

    // Context and the selected line belong to one checked read. Force a
    // first-attempt spill, then change its prefix after the first real read.
    #[test]
    fn retried_context_discards_the_whole_spilled_group() {
        use std::os::unix::fs::FileExt;
        for recursive in [false, true] {
            let path = std::env::temp_dir().join(format!(
                "ferret-context-retry-{}-{recursive}",
                std::process::id()
            ));
            let mut original = b"old context\n".repeat(24_000);
            original.extend_from_slice(b"red\n");
            fs::write(&path, &original).unwrap();
            let writer = OpenOptions::new().write(true).open(&path).unwrap();
            let mut changed = false;
            seam::AFTER_READ.with_borrow_mut(|hook| {
                *hook = Some(Box::new(move || {
                    if !changed {
                        writer.write_all_at(b"NEW", 0).unwrap();
                        // A changed extent makes the retry independent of
                        // timestamp resolution.
                        writer
                            .write_all_at(b"tail\n", original.len() as u64)
                            .unwrap();
                        changed = true;
                    }
                }));
            });
            let line_grep::Command::Search(plan) =
                line_grep::parse(&["-B25000".into(), "red".into()]).unwrap()
            else {
                panic!("search")
            };
            let mut host = Host {
                plan: &plan,
                filename: false,
                writer: Vec::new(),
                buffer: Vec::new(),
                matched: false,
                emitted_group: false,
                json_stats: json::Stats::default(),
                errors: false,
                cancelled: None,
            };
            host.file(&path, recursive).unwrap();
            seam::AFTER_READ.with_borrow_mut(|hook| *hook = None);
            let mut expected = fs::read(&path).unwrap();
            expected.truncate(expected.len() - b"tail\n".len());
            fs::remove_file(path).unwrap();
            assert_eq!(host.writer, expected);
            assert!(host.matched && !host.errors);
        }
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
                    emitted_group: false,
                    json_stats: json::Stats::default(),
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
            emitted_group: false,
            json_stats: json::Stats::default(),
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
                emitted_group: false,
                json_stats: json::Stats::default(),
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
            emitted_group: false,
            json_stats: json::Stats::default(),
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

    // A short group can fit entirely in the destination buffer. Its late
    // failure must stop publication before a later input or diagnostic runs.
    #[test]
    fn final_group_flush_errors_stop_publication() {
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
            writer: io::BufWriter::new(Broken),
            buffer: Vec::new(),
            matched: false,
            emitted_group: false,
            json_stats: json::Stats::default(),
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

    // A later publication fault (including a spill read fault) must not
    // discard an already accepted JSON begin event before its diagnostic.
    #[test]
    fn publication_faults_flush_the_previously_accepted_prefix() {
        #[derive(Default)]
        struct FailSecond {
            pending: Vec<u8>,
            published: Vec<u8>,
            calls: usize,
        }
        impl Write for FailSecond {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.calls += 1;
                if self.calls > 1 {
                    return Err(io::Error::other("late output fault"));
                }
                self.pending.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                self.published.append(&mut self.pending);
                Ok(())
            }
        }
        let line_grep::Command::Search(plan) =
            line_grep::parse(&["--json".into(), "red".into()]).unwrap()
        else {
            panic!("search")
        };
        let mut host = Host {
            plan: &plan,
            filename: false,
            writer: FailSecond::default(),
            buffer: Vec::new(),
            matched: false,
            emitted_group: false,
            json_stats: json::Stats::default(),
            errors: false,
            cancelled: None,
        };
        let error = host
            .direct(Path::new("file"), &mut &b"red\n"[..], 4)
            .unwrap_err();
        assert_eq!(error.to_string(), "late output fault");
        assert_eq!(
            host.writer.published,
            b"{\"type\":\"begin\",\"data\":{\"path\":{\"text\":\"file\"}}}\n"
        );
        assert!(host.writer.pending.is_empty());
    }

    #[test]
    fn json_retry_discards_events_offsets_binary_state_and_counts_together() {
        for multiline in [false, true] {
            let mut args = vec!["--json".into(), "-B1".into(), "red".into()];
            if multiline {
                args.insert(0, "-U".into());
            }
            let line_grep::Command::Search(mut plan) = line_grep::parse(&args).unwrap() else {
                panic!("search")
            };
            plan.line_number = true;
            for failure in [false, true] {
                let mut host = Host {
                    plan: &plan,
                    filename: false,
                    writer: Vec::new(),
                    buffer: Vec::new(),
                    matched: false,
                    emitted_group: false,
                    json_stats: json::Stats::default(),
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
                        b"obsolete context\nred red\0"
                    } else {
                        b"blue\nred\n"
                    };
                    each(&mut &bytes[..], bytes.len() as u64).unwrap();
                    if attempts == 1 {
                        ReadOutcome::RetryChanged
                    } else if failure {
                        ReadOutcome::Failed(ReadFault::Unstable)
                    } else {
                        ReadOutcome::Stable(ReadVersion::Current)
                    }
                };
                host.checked(Path::new("actual-path"), NameId(0), &mut reader)
                    .unwrap();
                assert_eq!(attempts, 2);
                assert_eq!(host.matched, !failure);
                assert_eq!(host.errors, failure);
                if failure {
                    assert!(host.writer.is_empty());
                    let mut summary = Vec::new();
                    json::summary(&mut summary, &host.json_stats, std::time::Instant::now())
                        .unwrap();
                    let parsed = crate::protocol::parse_object(&summary).unwrap();
                    let stats = parsed.field("data").unwrap().field("stats").unwrap();
                    for key in [
                        "matches",
                        "matched_lines",
                        "searches_with_match",
                        "searches",
                        "bytes_searched",
                        "bytes_printed",
                    ] {
                        assert_eq!(
                            stats.field(key).unwrap().number(),
                            Some(0),
                            "{key}: failed attempts reached summary"
                        );
                    }
                } else {
                    let text = String::from_utf8(host.writer).unwrap();
                    assert!(!text.contains("obsolete"));
                    assert!(text.contains(r#""absolute_offset":5"#));
                    assert!(text.contains(r#""binary_offset":null"#));
                    assert!(text.contains(r#""matched_lines":1,"matches":1"#));
                    assert_eq!(text.matches(r#""type":"begin""#).count(), 1);
                    assert_eq!(text.matches(r#""type":"end""#).count(), 1);
                    let mut summary = Vec::new();
                    json::summary(&mut summary, &host.json_stats, std::time::Instant::now())
                        .unwrap();
                    let parsed = crate::protocol::parse_object(&summary).unwrap();
                    let stats = parsed.field("data").unwrap().field("stats").unwrap();
                    assert_eq!(stats.field("matches").unwrap().number(), Some(1));
                    assert_eq!(
                        stats.field("searches_with_match").unwrap().number(),
                        Some(1)
                    );
                }
            }
        }
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
