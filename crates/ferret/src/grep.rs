//! Shared grep host: resolve operands before opening a generation, adapt policy
//! and checked reads, and commit each file's bounded plain output as a group.

use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

mod groups;
mod json;
mod live;
mod names;
mod output;
mod presentation;
pub(crate) mod remote;
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
Directories use snapshot names and postings. Candidates and catalogued\n\
files over the 8 MiB index cap are read live.\n\
Uncovered directories search live with rg ignore rules, including ancestor\n\
.ignore and .rgignore up to /. Trigrams are optional.\n\
Explicit files and stdin search directly. UTF-8 BOMs are stripped; UTF-16\n\
decoding is deferred. Excluded paths are absent. Exit: 0 match, 1 none, 2 error.\n";

#[derive(Debug)]
enum Input {
    File(PathBuf, u64),
    Directory(PathBuf),
    Stdin,
    Failed(PathBuf, io::Error),
}

/// Runs grep through the resident daemon when eligible. Help/version bypass
/// configuration and index resolution; all search errors use grep's status 2,
/// including output failures.
pub fn run(args: &[OsString], index: Option<&Path>, global_json: bool) -> Exit {
    let start = std::time::Instant::now();
    let mut pattern_files = Vec::new();
    let command = match line_grep::parse_with(args, &mut |path| {
        let bytes = if path == Path::new("-") {
            let mut bytes = Vec::new();
            io::stdin().lock().read_to_end(&mut bytes)?;
            bytes
        } else {
            fs::read(path)?
        };
        pattern_files.push(bytes.clone());
        Ok(bytes)
    }) {
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
            let tty = presentation::resolve(&mut plan.presentation);
            return names::run(&plan, index, args, tty);
        }
        line_grep::Command::Search(plan) => *plan,
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
    let inputs = if plan.stdin_patterns && plan.paths.is_empty() {
        resolve_inputs(&[PathBuf::from(".")], false)
    } else {
        resolve_inputs(&plan.paths, false)
    };
    let recursive = inputs
        .iter()
        .any(|input| matches!(input, Input::Directory(_)));
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => return failure(&error.to_string()),
    };
    let routing = if recursive && !plan.traversal.unrestricted && !plan.traversal.live {
        match live::Anchors::routing(index, &inputs) {
            Ok(anchors) => Some(anchors),
            Err(error) => return failure(&error),
        }
    } else {
        None
    };
    let covered = routing.as_ref().is_none_or(live::Anchors::all_covered);
    if recursive
        && plan.traversal.follow
        && !plan.traversal.unrestricted
        && routing
            .as_ref()
            .is_none_or(|anchors| anchors.any_covered(&inputs))
    {
        return failure(
            "-L is supported for uncovered directories and -u; use -u to follow links in an indexed tree",
        );
    }
    if covered
        && daemon_operands(&inputs)
        && !plan.traversal.unrestricted
        && !plan.traversal.live
        && !test_injections()
        && let Ok(index) = index_path(index)
        && let Some(exit) = remote::run(args, &pattern_files, &plan, tty, &index)
    {
        return exit;
    }
    let live = if recursive && plan.traversal.live && !plan.traversal.unrestricted {
        match live::Anchors::open(index) {
            Ok(anchors) => Some(anchors),
            Err(error) => return failure(&error),
        }
    } else {
        routing
    };
    let engine = if recursive
        && !plan.traversal.unrestricted
        && !plan.traversal.live
        && live
            .as_ref()
            .is_none_or(|anchors| anchors.any_covered(&inputs))
    {
        match open_index(index) {
            Ok(engine) => Some(engine),
            Err(error) => return failure(&error),
        }
    } else {
        None
    };
    let pin = engine.as_ref().map(Engine::pin);
    let threads = pin.as_ref().map_or_else(
        || {
            live.as_ref().map_or_else(
                || {
                    let available = std::thread::available_parallelism().map_or(1, |n| n.get());
                    engine::search_threads(
                        engine::env_threads(std::env::var("FERRET_SEARCH_THREADS").ok().as_deref()),
                        None,
                        available,
                    )
                },
                live::Anchors::search_threads,
            )
        },
        engine::QuerySession::search_threads,
    );
    let cancelled = AtomicBool::new(false);
    let stderr = |message: &str| {
        cli::error(message);
        Ok(())
    };
    let runtime = Runtime {
        cancelled: &cancelled,
        stderr: &stderr,
        tty,
        warned: AtomicBool::new(false),
        descriptors: None,
        defer_stdout: false,
    };
    let stdout = io::stdout();
    Execution {
        plan: &plan,
        selection: &selection,
        pin: pin.as_ref(),
        live: live.as_ref(),
        cwd: &cwd,
        threads,
        runtime: &runtime,
        start,
    }
    .run(inputs, output::Writer::new(&stdout, block_stdout(&plan)))
}

struct Runtime<'a> {
    cancelled: &'a AtomicBool,
    stderr: &'a (dyn Fn(&str) -> io::Result<()> + Sync),
    tty: bool,
    warned: AtomicBool,
    descriptors: Option<std::sync::Arc<line_grep::IoBudget>>,
    /// Redirected daemon content stays in the block buffer across groups.
    /// In-process hosts, terminals and live walks leave this off and publish
    /// each group.
    defer_stdout: bool,
}

impl Runtime<'_> {
    fn failure(&self, message: &str) -> Exit {
        let _ = (self.stderr)(&format!("grep: {message}"));
        Exit::Usage
    }
    fn output_failure(&self, error: &io::Error) -> Exit {
        if error.kind() == io::ErrorKind::BrokenPipe {
            Exit::Ok
        } else {
            self.failure(&format!("writing stdout: {error}"))
        }
    }
}

// Both hosts supply the same resolved environment and pinned generation.
struct Execution<'a> {
    plan: &'a Plan,
    selection: &'a GrepSelection,
    pin: Option<&'a engine::QuerySession>,
    live: Option<&'a live::Anchors>,
    cwd: &'a Path,
    threads: usize,
    runtime: &'a Runtime<'a>,
    start: std::time::Instant,
}

impl Execution<'_> {
    fn run<W: Write>(self, inputs: Vec<Input>, writer: W) -> Exit {
        let Self {
            plan,
            selection,
            pin,
            live,
            cwd,
            threads,
            runtime,
            start,
        } = self;
        let cancelled = runtime.cancelled;
        let stopped = || cancelled.load(Ordering::Acquire);
        let recursive = inputs
            .iter()
            .any(|input| matches!(input, Input::Directory(_)));
        let candidates = if !plan.invert {
            match pin
                .map(|pin| pin.grep_candidates(&plan.grep, &stopped))
                .transpose()
            {
                Ok(candidates) => candidates,
                Err(error) => return runtime.failure(&error.to_string()),
            }
        } else {
            None
        };
        let names = match pin.map(|pin| pin.doc_names_until(stopped)).transpose() {
            Ok(names) => names.flatten(),
            Err(error) => return runtime.failure(&error.to_string()),
        };
        let documents = match pin
            .map(|pin| {
                ferret_crawl::Documents::by_name(pin.catalog()).map(|documents| {
                    documents.with_config(ferret_policy::Config { size_cap: u64::MAX })
                })
            })
            .transpose()
        {
            Ok(documents) => documents,
            Err(error) => return runtime.failure(&error.to_string()),
        };
        for input in &inputs {
            if let Input::Directory(path) = input {
                if let Some(anchors) = live.filter(|anchors| anchors.walks(path)) {
                    if let Err(error) = anchors.validate(path) {
                        return runtime.failure(&format!("{}: {error}", path.display()));
                    }
                } else if let Some(pin) = pin {
                    let mut source =
                        match sources::Directory::new(path, Some(pin.catalog()), cwd, false) {
                            Ok(source) => source,
                            Err(error) => return runtime.failure(&error.to_string()),
                        };
                    if let Some(Err(error)) = source.next(true) {
                        return runtime.failure(&format!(
                        "{}: {}; refresh the index with ferret index DIR, or use -u to search current files",
                        error.path.display(),
                        error.error
                    ));
                    }
                }
            }
        }
        let mut host = Host {
            plan,
            filename: plan
                .presentation
                .filename
                .unwrap_or(recursive || inputs.len() > 1),
            writer,
            runtime: Some(runtime),
            buffer: Vec::new(),
            matched: false,
            emitted_group: false,
            sent_output: false,
            json_stats: json::Stats::default(),
            errors: false,
            cancelled: Some(cancelled),
        };
        // rg diagnoses failed explicit operands before searching, even when an
        // earlier operand supplies a quiet match.
        let mut resolved = Vec::new();
        for input in inputs {
            if let Input::Failed(path, error) = input {
                if let Err(error) = host.error(&path, &error) {
                    return runtime.output_failure(&error);
                }
            } else {
                resolved.push(input);
            }
        }
        let inputs = resolved;
        let fork = || {
            pin.zip(documents.as_ref())
                .map(|(pin, documents)| engine::reader::checked(pin.catalog(), documents))
        };
        // D99: with no path operand, a walk whose filters selected no file is
        // diagnosed at exit.
        let mut searched = false;
        let mut inputs = inputs.into_iter().peekable();
        'inputs: while inputs.peek().is_some() {
            let mut segment = Vec::new();
            while inputs.peek().is_some_and(|input| {
                !matches!(input, Input::Stdin)
                    && (segment.is_empty()
                        || !buffered_path_output(plan, runtime.tty) && covered_inputs(live))
            }) {
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
                    pin.map(|p| p.catalog()),
                    candidates.as_ref(),
                    selection,
                    cwd.to_owned(),
                )
                .with_traversal(plan.traversal, plan.text)
                .with_live(live)
                .with_absence(plan.universe() == line_grep::Universe::Searchable);
                let walked = (threads > 1 && source.walks_live()).then(|| {
                    groups::walk(&mut host, &mut source, threads, cancelled, &mut searched)
                });
                let serial = match walked {
                    None => Some(threads),
                    Some(Ok(groups::Walk::Done)) => None,
                    // D108 B: the calling thread searches the operands alone.
                    Some(Ok(groups::Walk::Refused(cause))) => {
                        if let Err(error) = host.report_shortfall(&ferret_query::Shortfall {
                            running: 1,
                            asked: threads,
                            cause,
                        }) {
                            return runtime.output_failure(&error);
                        }
                        Some(1)
                    }
                    Some(Err(error)) => return runtime.output_failure(&error),
                };
                if let Some(threads) = serial {
                    let result = groups::run(
                        &mut host,
                        |stopped| source.next(stopped),
                        &fork,
                        names,
                        threads,
                        cancelled,
                    );
                    searched |= source.searched();
                    if let Err(error) = result {
                        return runtime.output_failure(&error);
                    }
                }
                if plan.quiet && host.matched {
                    if !plan.json {
                        return Exit::Ok;
                    }
                    break 'inputs;
                }
                if buffered_path_output(plan, runtime.tty)
                    && let Err(error) = host.writer.flush()
                {
                    return runtime.output_failure(&error);
                }
            }
            if matches!(inputs.peek(), Some(Input::Stdin)) {
                inputs.next();
                searched = true;
                if let Err(error) = host.direct(Path::new("-"), &mut io::stdin().lock(), u64::MAX) {
                    return runtime.output_failure(&error);
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
            return runtime.output_failure(&error);
        }
        if let Err(error) = host.writer.flush() {
            return runtime.output_failure(&error);
        }
        if plan.quiet && host.matched {
            Exit::Ok
        } else if plan.paths.is_empty() && !searched {
            let _ = (runtime.stderr)(
                "grep: no files were searched; a glob, depth, hidden or ignore filter probably \
             excluded every file under the current directory",
            );
            Exit::Usage
        } else if host.errors {
            Exit::Usage
        } else if host.matched {
            Exit::Ok
        } else {
            Exit::NoMatch
        }
    }
}

fn covered_inputs(live: Option<&live::Anchors>) -> bool {
    live.is_none_or(live::Anchors::all_covered)
}

fn test_injections() -> bool {
    std::env::vars_os().any(|(key, _)| key.as_bytes().starts_with(b"FERRET_TEST_"))
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

/// D94: a reader that closes the pipe ends grep quietly with success. File
/// staging faults have already been diagnosed by the per-file transaction.
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
                "no index in {}; run ferret index DIR, use -u, or omit --live to search uncovered paths",
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
    resolve_inputs_at(paths, names_only, Path::new("."))
}

fn resolve_inputs_at(paths: &[PathBuf], names_only: bool, cwd: &Path) -> Vec<Input> {
    paths
        .iter()
        .map(|path| {
            if path == Path::new("-") {
                return Input::Stdin;
            }
            match fs::metadata(cwd.join(path)) {
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

/// D111 routes a request through ferretd only when every operand is a
/// directory that names the same directory there. A file operand's identity
/// belongs to this process (an inherited `/proc/self/fd/N`, say), and ferretd
/// would resolve its spelling again in its own, so mixed requests stay here.
fn daemon_operands(inputs: &[Input]) -> bool {
    inputs.iter().all(
        |input| matches!(input, Input::Directory(path) if line_grep::same_in_every_process(path)),
    )
}

fn readable_stdin() -> bool {
    fs::metadata("/proc/self/fd/0").is_ok_and(|metadata| {
        let kind = metadata.file_type();
        kind.is_file() || kind.is_fifo() || kind.is_socket()
    })
}

struct Host<'a, W> {
    plan: &'a Plan,
    runtime: Option<&'a Runtime<'a>>,
    filename: bool,
    writer: W,
    buffer: Vec<u8>,
    matched: bool,
    emitted_group: bool,
    /// Some group's output has been flushed. A deferring daemon host still
    /// flushes its first one, so a sparse or slow grep shows its first match
    /// before the block buffer fills.
    sent_output: bool,
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

/// Redirected path/count records may share the block buffer until an operand
/// or diagnostic boundary. Ttys, NUL names and live policy keep line output.
fn buffered_path_output(plan: &Plan, tty: bool) -> bool {
    !tty && !plan.traversal.live
        && !nul_names(plan)
        && !plan.json
        && matches!(
            plan.output,
            Output::Names | Output::WithoutMatch | Output::Count | Output::CountMatches
        )
}

fn nul_names(plan: &Plan) -> bool {
    plan.presentation.null
        && matches!(plan.output, Output::Names | Output::WithoutMatch)
        && !plan.json
}

fn report_shortfall(
    runtime: Option<&Runtime<'_>>,
    shortfall: &ferret_query::Shortfall,
) -> io::Result<()> {
    if runtime.is_none_or(|runtime| !runtime.warned.swap(true, Ordering::AcqRel)) {
        let message = format!(
            "grep: searching with {} of {} threads: {}",
            shortfall.running, shortfall.asked, shortfall.cause
        );
        if let Some(runtime) = runtime {
            (runtime.stderr)(&message)?;
        } else {
            cli::error(&message);
        }
    }
    Ok(())
}

impl<W: Write> Host<'_, W> {
    fn tty(&self) -> bool {
        self.runtime
            .map_or_else(stdout_is_terminal, |runtime| runtime.tty)
    }
    fn diagnostic(&self, message: &str) -> io::Result<()> {
        if let Some(runtime) = self.runtime {
            (runtime.stderr)(message)
        } else {
            cli::error(message);
            Ok(())
        }
    }
    fn report_shortfall(&self, shortfall: &ferret_query::Shortfall) -> io::Result<()> {
        report_shortfall(self.runtime, shortfall)
    }

    fn error(&mut self, path: &Path, error: &dyn std::fmt::Display) -> io::Result<()> {
        if !nul_names(self.plan) {
            self.writer.flush()?;
        }
        self.diagnostic(&format!("grep: {}: {error}", path.display()))?;
        self.errors = true;
        Ok(())
    }

    #[cfg(test)]
    fn file(&mut self, path: &Path, recursive: bool) -> io::Result<()> {
        let flag = self.cancelled;
        let stopped = || flag.is_some_and(|flag| flag.load(Ordering::Acquire));
        let plan = self.plan;
        // A walked file is opened beneath the directory it was listed in.
        let directory = recursive.then(|| {
            crate::engine::reader::Parent::Open(ferret_query::find::DirectoryHandle::from_file(
                std::sync::Arc::new(fs::File::open(path.parent().unwrap()).unwrap()),
            ))
        });
        self.transaction(path, &mut |each| {
            groups::read_file(path, directory.as_ref(), plan, each, &stopped)
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
            capture: None,
            plan: self.plan,
            prefix: (self.filename || self.plan.json)
                .then_some(label(path, self.plan.paths.is_empty())),
            stopped: &stopped,
        };
        let mut buffer = std::mem::take(&mut self.buffer);
        let mut stream = DirectStream {
            host: self,
            path,
            printed: None,
        };
        let observed = groups::observe_streaming(&render, &mut buffer, read, false, &mut stream);
        let printed = stream.printed;
        self.buffer = buffer;
        match observed? {
            groups::Observed::Ready(group) => self.commit(path, group.result, group.output),
            groups::Observed::Failed(error) => {
                self.error(path, &error)?;
                Ok(())
            }
            groups::Observed::Spool(error) => {
                self.error(path, &error)?;
                Ok(())
            }
            groups::Observed::Skipped => Ok(()),
            groups::Observed::Streamed(result, error, _capture) => {
                self.finish_stream(path, result, printed.unwrap_or(0), error)
            }
        }
    }

    fn start_stream(&mut self, path: &Path) -> io::Result<u64> {
        if self.plan.json {
            let mut begin = Vec::new();
            json::begin(&mut begin, label(path, self.plan.paths.is_empty()))?;
            self.writer.write_all(&begin)?;
            return Ok(begin.len() as u64);
        }
        if self.emitted_group {
            if self.plan.presentation.heading == Some(true) {
                self.writer.write_all(b"\n")?;
            } else if self.plan.before > 0 || self.plan.after > 0 {
                self.writer.write_all(b"--\n")?;
            }
        }
        self.emitted_group = true;
        Ok(0)
    }

    fn finish_stream(
        &mut self,
        path: &Path,
        result: InputResult,
        printed: u64,
        error: Option<String>,
    ) -> io::Result<()> {
        self.matched |= result.matched;
        if self.plan.json {
            json::end(
                &mut self.writer,
                label(path, self.plan.paths.is_empty()),
                result,
                printed,
            )?;
            self.json_stats.add(result, printed);
        } else if result.binary && result.matched {
            self.binary_warning(path)?;
        }
        self.writer.flush()?;
        if let Some(error) = error {
            self.error(path, &error)?;
        }
        Ok(())
    }

    fn binary_warning(&mut self, path: &Path) -> io::Result<()> {
        self.writer.write_all(if path == Path::new("-") {
            b"(standard input)"
        } else {
            label(path, self.plan.paths.is_empty())
        })?;
        self.writer.write_all(b": binary file matches\n")?;
        self.emitted_group = true;
        Ok(())
    }

    fn commit(&mut self, path: &Path, result: InputResult, output: OutputBuffer) -> io::Result<()> {
        #[cfg(test)]
        seam::COMMIT.with_borrow_mut(|hook| {
            if let Some(hook) = hook {
                hook(&output);
            }
        });
        let has_output = !output.is_empty();
        let published = self.publish(path, result, output);
        // NUL names keep their line buffer. Redirected path and count
        // listings share the block buffer until an operand boundary.
        if nul_names(self.plan) || buffered_path_output(self.plan, self.tty()) {
            return published;
        }
        // Redirected daemon content shares that buffer, so a group is not
        // its own socket frame; only the first group with output goes out
        // alone, like rg's block buffer once it has something to show. A
        // failed publish still flushes: the accepted prefix has to leave the
        // buffer before the error returns.
        // A binary notice is written by `publish` itself, with an empty
        // buffer, and marks the group emitted.
        let wrote = has_output || self.emitted_group;
        let defer = self.runtime.is_some_and(|runtime| runtime.defer_stdout);
        if published.is_err() || !defer || (wrote && !self.sent_output) {
            self.writer.flush()?;
            self.sent_output |= wrote;
        }
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
                self.binary_warning(path)?;
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

fn stdout_is_terminal() -> bool {
    static IS_TERMINAL: OnceLock<bool> = OnceLock::new();
    *IS_TERMINAL.get_or_init(|| io::stdout().is_terminal())
}

/// Stdin and checked single-thread test hosts need no channel handoff.
struct DirectStream<'h, 'p, W> {
    host: &'h mut Host<'p, W>,
    path: &'h Path,
    printed: Option<u64>,
}
impl<W: Write> Write for DirectStream<'_, '_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.printed.is_none() {
            self.printed = Some(self.host.start_stream(self.path)?);
        }
        if self
            .host
            .cancelled
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        let count = self
            .host
            .writer
            .write(&bytes[..bytes.len().min(64 << 10)])?;
        if let Some(printed) = &mut self.printed {
            *printed += count as u64;
        }
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.host.writer.flush()
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

    // A shrinking input retries once; neither path may publish an unstable
    // attempt as a search result.
    #[test]
    fn regular_files_discard_torn_matches_and_retry_current_bytes() {
        let mut outcomes = Vec::new();
        for recursive in [false, true] {
            let path = std::env::temp_dir()
                .join(format!("ferret-torn-{}-{recursive}", std::process::id()));
            let mut bytes = vec![b'x'; 100_000];
            bytes[..3].copy_from_slice(b"red");
            bytes[99_997..].copy_from_slice(b"OLD");
            fs::write(&path, bytes).unwrap();
            let writer = OpenOptions::new().write(true).open(&path).unwrap();
            let mut attempt = 0;
            seam::AFTER_READ.with_borrow_mut(|hook| {
                *hook = Some(Box::new(move || {
                    writer.set_len(50_000 >> attempt).unwrap();
                    attempt += 1;
                }))
            });
            let line_grep::Command::Search(plan) = line_grep::parse(&["^red".into()]).unwrap()
            else {
                panic!("search")
            };
            let mut host = Host {
                plan: &plan,
                runtime: None,
                filename: false,
                writer: Vec::new(),
                buffer: Vec::new(),
                matched: false,
                emitted_group: false,
                sent_output: false,
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
            [(false, true, false, true), (true, true, false, true)]
        );
    }

    // A changed attempt that crosses the output threshold cannot be retried,
    // even when most output consists of before-context.
    #[test]
    fn changed_streamed_context_keeps_one_attempt_and_reports_the_file() {
        use std::cell::RefCell;
        use std::os::unix::fs::FileExt;
        use std::rc::Rc;
        let mut context = b"old context ".to_vec();
        context.resize(1023, b'x');
        context.push(b'\n');
        let lines = groups::GROUP_BYTES / context.len() + 1;
        for recursive in [false, true] {
            let path = std::env::temp_dir().join(format!(
                "ferret-context-retry-{}-{recursive}",
                std::process::id()
            ));
            let mut original = context.repeat(lines);
            original.extend_from_slice(b"red\n");
            fs::write(&path, &original).unwrap();
            let writer = OpenOptions::new().write(true).open(&path).unwrap();
            let mut changed = false;
            let original_len = original.len();
            seam::AFTER_READ.with_borrow_mut(|hook| {
                *hook = Some(Box::new(move || {
                    if !changed {
                        writer.write_all_at(b"NEW", 0).unwrap();
                        // Shrinkage rejects the closing check even though
                        // search readers accept appends (D104).
                        writer.set_len(original_len as u64 - 1).unwrap();
                        changed = true;
                    }
                }));
            });
            let spilled = Rc::new(RefCell::new(Vec::new()));
            let seen = spilled.clone();
            seam::COMMIT.with_borrow_mut(|hook| {
                *hook = Some(Box::new(move |output| {
                    seen.borrow_mut().push(output.is_spilled());
                }));
            });
            let line_grep::Command::Search(plan) =
                line_grep::parse(&[format!("-B{lines}").into(), "red".into()]).unwrap()
            else {
                panic!("search")
            };
            let mut host = Host {
                plan: &plan,
                runtime: None,
                filename: false,
                writer: Vec::new(),
                buffer: Vec::new(),
                matched: false,
                emitted_group: false,
                sent_output: false,
                json_stats: json::Stats::default(),
                errors: false,
                cancelled: None,
            };
            host.file(&path, recursive).unwrap();
            seam::AFTER_READ.with_borrow_mut(|hook| *hook = None);
            seam::COMMIT.with_borrow_mut(|hook| *hook = None);
            let expected = original;
            fs::remove_file(path).unwrap();
            assert!(host.writer == expected, "replay differs");
            assert!(host.matched && host.errors);
            assert!(
                spilled.borrow().is_empty(),
                "streamed output bypasses capture commit"
            );
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
                                    writer.set_len(50_000).unwrap();
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
                    runtime: None,
                    filename: false,
                    writer: Vec::new(),
                    buffer: Vec::new(),
                    matched: false,
                    emitted_group: false,
                    sent_output: false,
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
            runtime: None,
            filename: false,
            writer: Vec::new(),
            buffer: Vec::new(),
            matched: false,
            emitted_group: false,
            sent_output: false,
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
                runtime: None,
                filename: false,
                writer: Vec::new(),
                buffer: Vec::new(),
                matched: false,
                emitted_group: false,
                sent_output: false,
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
            runtime: None,
            filename: false,
            writer: Broken,
            buffer: Vec::new(),
            matched: false,
            emitted_group: false,
            sent_output: false,
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

    // A deferring daemon host batches groups after its first, so the flush
    // before a diagnostic is what keeps an earlier group ahead of it. When
    // that flush fails, the host stops before reporting the input.
    #[test]
    fn deferred_groups_flush_before_diagnostics_and_stop_on_flush_failure() {
        struct Recorder<'a> {
            pending: Vec<u8>,
            log: &'a std::sync::Mutex<Vec<String>>,
            flushes: usize,
            fail_flush: Option<usize>,
        }
        impl Write for Recorder<'_> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.pending.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                if self.pending.is_empty() {
                    return Ok(());
                }
                self.flushes += 1;
                if self.fail_flush == Some(self.flushes) {
                    return Err(io::ErrorKind::StorageFull.into());
                }
                let text = String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned();
                self.log.lock().unwrap().push(format!("out {text}"));
                Ok(())
            }
        }
        let line_grep::Command::Search(plan) = line_grep::parse(&["red".into()]).unwrap() else {
            panic!("search")
        };
        for fail_flush in [None, Some(2)] {
            let log = std::sync::Mutex::new(Vec::new());
            let stderr = |message: &str| {
                log.lock().unwrap().push(format!("err {message}"));
                Ok(())
            };
            let cancelled = AtomicBool::new(false);
            let runtime = Runtime {
                cancelled: &cancelled,
                stderr: &stderr,
                tty: false,
                warned: AtomicBool::new(false),
                descriptors: None,
                defer_stdout: true,
            };
            let mut host = Host {
                plan: &plan,
                runtime: Some(&runtime),
                filename: true,
                writer: Recorder {
                    pending: Vec::new(),
                    log: &log,
                    flushes: 0,
                    fail_flush,
                },
                buffer: Vec::new(),
                matched: false,
                emitted_group: false,
                sent_output: false,
                json_stats: json::Stats::default(),
                errors: false,
                cancelled: None,
            };
            for name in ["a", "b"] {
                let line = format!("red {name}\n");
                let mut reader = |_, request: ReadRequest<'_>| {
                    let ReadRequest::Stream(each, _) = request else {
                        panic!("stream")
                    };
                    each(&mut line.as_bytes(), line.len() as u64).unwrap();
                    ReadOutcome::Stable(ReadVersion::Current)
                };
                host.checked(Path::new(name), NameId(0), &mut reader)
                    .unwrap();
            }
            let mut failing = |_, _: ReadRequest<'_>| {
                ReadOutcome::Failed(ReadFault::Io(
                    ReadOperation::Read,
                    io::ErrorKind::PermissionDenied.into(),
                ))
            };
            let result = host.checked(Path::new("c"), NameId(0), &mut failing);
            let log = log.into_inner().unwrap();
            match fail_flush {
                None => {
                    result.unwrap();
                    assert_eq!(log.len(), 3, "{log:?}");
                    assert_eq!(log[0], "out a:red a\n");
                    assert_eq!(log[1], "out b:red b\n");
                    assert!(log[2].starts_with("err grep: c: "), "{log:?}");
                }
                Some(_) => {
                    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::StorageFull);
                    assert_eq!(log, ["out a:red a\n"]);
                }
            }
        }
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
            runtime: None,
            filename: false,
            writer: io::BufWriter::new(Broken),
            buffer: Vec::new(),
            matched: false,
            emitted_group: false,
            sent_output: false,
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

    // A later destination fault must not
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
            runtime: None,
            filename: false,
            writer: FailSecond::default(),
            buffer: Vec::new(),
            matched: false,
            emitted_group: false,
            sent_output: false,
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
                    runtime: None,
                    filename: false,
                    writer: Vec::new(),
                    buffer: Vec::new(),
                    matched: false,
                    emitted_group: false,
                    sent_output: false,
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

#[cfg(any(test, debug_assertions))]
mod seam {
    #[cfg(test)]
    use super::OutputBuffer;
    #[cfg(test)]
    use std::cell::RefCell;
    use std::io::{self, Read};

    #[cfg(test)]
    thread_local! {
        pub(super) static BEFORE_SCAN: RefCell<Option<Box<dyn FnMut()>>> = RefCell::new(None);
        pub(super) static AFTER_READ: RefCell<Option<Box<dyn FnMut()>>> = RefCell::new(None);
        /// Called on the publishing thread with each group about to be
        /// committed, so tests can distinguish capture replay from streaming.
        pub(super) static COMMIT: RefCell<Option<Box<OnCommit>>> = RefCell::new(None);
    }

    #[cfg(test)]
    pub(super) type OnCommit = dyn FnMut(&OutputBuffer);

    pub(super) struct Observed<'a> {
        input: &'a mut dyn Read,
        appended: bool,
    }

    impl<'a> Observed<'a> {
        pub(super) fn new(input: &'a mut dyn Read) -> Self {
            Self {
                input,
                appended: false,
            }
        }
    }

    impl Read for Observed<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            let count = self.input.read(bytes)?;
            if count != 0 {
                #[cfg(debug_assertions)]
                if !self.appended
                    && let Some(path) = std::env::var_os("FERRET_TEST_GREP_APPEND_PATH")
                {
                    use std::io::Write;
                    let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
                    file.write_all(
                        std::env::var_os("FERRET_TEST_GREP_APPEND_BYTES")
                            .as_deref()
                            .unwrap_or_default()
                            .as_encoded_bytes(),
                    )?;
                    self.appended = true;
                }
                #[cfg(test)]
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
