//! Shared grep host: resolve operands before opening a generation, adapt policy
//! and checked reads, and commit each file's bounded plain output as a group.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use ferret_policy::GrepSelection;
use ferret_query::line_grep::{self, Plan};

use crate::cli::{self, Exit};
use crate::engine::{self, Engine};
use crate::xdg::Dirs;

mod execution;
mod groups;
mod holders;
mod host;
mod json;
mod live;
mod names;
mod output;
mod preflight;
mod presentation;
pub(crate) mod remote;
mod routing;
mod sources;
mod visibility;

use execution::Execution;
use host::{Host, block_stdout, buffered_path_output};

const HELP: &str = "ferret grep [OPTIONS] PATTERN [PATH...]\n\n\
  -n, --line-number   print 1-based line numbers\n\
  -g, --glob GLOB     select paths; repeat for ordered overrides\n\
  -t, --type TYPE     search only files of rg's type TYPE (`all` for any)\n\
  -T, --type-not TYPE skip files of rg's type TYPE\n\
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
  -M, --max-columns N print lines over N bytes as an omission notice\n\
  --files            list selected file names without reading contents\n\
  --hidden, -.       include hidden files and directories (the default)\n\
  --no-hidden        exclude hidden files and directories\n\
  -A/-B/-C N         after/before/around context\n\
  -H/-I              force/suppress filenames\n\
  --heading/--no-heading group matches by file\n\
  -N, --no-line-number suppress line numbers\n\
  -0, --null         NUL-terminate filenames\n\
  -p, --pretty       colour, headings and line numbers\n\
  --color MODE       auto/always/ansi/never\n\
  --live             current tree with ferret visibility policy\n\
  --rg               rg's visibility: hidden names hidden, uncovered\n\
                     directories under rg's ignore rules\n\
  -u/--no-ignore     unrestricted current-tree walk\n\
  -uu                unrestricted including hidden paths\n\
  -a, --text         search binary bytes as text\n\
  --max-depth N      limit directory traversal depth\n\
  --scan             accepted; broad patterns always scan\n\n\
Directories use snapshot names and postings. Candidates and catalogued\n\
files over the 8 MiB index cap are read live.\n\
Uncovered directories search live, each under ferret's policy as if it\n\
were an index root; with --rg, under rg's ignore rules, including ancestor\n\
.ignore and .rgignore up to /. Trigrams are optional.\n\
Explicit files and stdin search directly. UTF-8 BOMs are stripped; UTF-16\n\
decoding is deferred. Excluded paths are absent. Exit: 0 match, 1 none, 2 error.\n";

#[derive(Debug)]
enum Input {
    File(PathBuf, u64),
    Directory(PathBuf),
    /// Read once, front to back: `-` is stdin; any other path is a special
    /// file (a FIFO or a device such as `/dev/null`), which rg reads as a
    /// stream too.
    Stream(PathBuf),
    Failed(PathBuf, io::Error),
}

/// Runs grep through the resident daemon when eligible. Help/version bypass
/// configuration and index resolution; all search errors use grep's status 2,
/// including output failures.
pub fn run(args: &[OsString], index: Option<&Path>, global_json: bool) -> Exit {
    let start = std::time::Instant::now();
    let Parsed {
        command,
        pattern_files,
        consumed,
    } = match parse(args) {
        Ok(parsed) => parsed,
        Err(exit) => return exit,
    };
    let plan = match command {
        line_grep::Command::Help => return print(HELP.as_bytes()),
        line_grep::Command::Version => {
            return print(format!("ferret grep {}\n", env!("CARGO_PKG_VERSION")).as_bytes());
        }
        line_grep::Command::Files(mut plan) => {
            if let Some(exit) = context_error(args, global_json, consumed) {
                return exit;
            }
            let tty = presentation::resolve(&mut plan.presentation);
            return names::run(&plan, index, args, tty, consumed);
        }
        line_grep::Command::Search(plan) => *plan,
    };
    if let Some(exit) = context_error(args, global_json, consumed) {
        return exit;
    }
    search(args, index, plan, &pattern_files, consumed, start)
}

/// A parsed command line, with the bytes of every pattern file it read.
struct Parsed {
    command: line_grep::Command,
    pattern_files: Vec<Vec<u8>>,
    /// Whether it read a pattern source rg could not read again: stdin, or
    /// any pipe or device spelling of it (`/dev/stdin`, `/proc/self/fd/0`).
    /// Falling back to rg after reading one would hand rg an exhausted
    /// source.
    consumed: bool,
}

/// Parses `args`, reading pattern files as it meets them.
fn parse(args: &[OsString]) -> Result<Parsed, Exit> {
    let mut pattern_files = Vec::new();
    let mut consumed = false;
    let command = match line_grep::parse_with(args, &mut |path| {
        let bytes = if path == Path::new("-") {
            consumed = true;
            let mut bytes = Vec::new();
            io::stdin().lock().read_to_end(&mut bytes)?;
            bytes
        } else {
            consumed |= !fs::metadata(path).is_ok_and(|metadata| metadata.is_file());
            fs::read(path)?
        };
        pattern_files.push(bytes.clone());
        Ok(bytes)
    }) {
        Ok(command) => command,
        Err(error @ line_grep::ParseError::Unsupported(_)) => {
            return Err(unsupported(args, &error.to_string(), false));
        }
        Err(error) => return Err(failure(&error.to_string())),
    };
    Ok(Parsed {
        command,
        pattern_files,
        consumed,
    })
}

/// Runs a content search: through the daemon when routing allows, else in
/// this process.
fn search(
    args: &[OsString],
    index: Option<&Path>,
    mut plan: line_grep::Plan,
    pattern_files: &[Vec<u8>],
    consumed: bool,
    start: std::time::Instant,
) -> Exit {
    let tty = presentation::resolve(&mut plan.presentation);
    plan.line_number = plan.presentation.numbers.unwrap_or(tty || plan.json);
    let selection = match preflight::prepare(&plan) {
        Ok(preflight::Prepared::Search(selection)) => selection,
        Ok(preflight::Prepared::NoMatch) => return Exit::NoMatch,
        Err(refusal) => return refusal.exit(args, consumed),
    };
    let inputs = if plan.stdin_patterns && plan.paths.is_empty() {
        resolve_inputs(&[PathBuf::from(".")], false)
    } else {
        resolve_inputs(&plan.paths, false)
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => return failure(&error.to_string()),
    };
    let routed = match routing::route(inputs, plan.traversal, index, selection.positive()) {
        Ok(routed) => routed,
        Err(refusal) => return refusal.exit(args, consumed),
    };
    if routed.daemon
        && let Ok(index) = index_path(index)
        && let Some(exit) = remote::run(args, pattern_files, &plan, tty, &index)
    {
        return exit;
    }
    let routing::Routed {
        inputs,
        live,
        catalog,
        ..
    } = routed;
    let engine = if catalog {
        match open_index(index) {
            Ok(engine) => Some(engine),
            Err(error) => return failure(&error),
        }
    } else {
        None
    };
    let pin = engine.as_ref().map(Engine::pin);
    let threads = pin.as_ref().map_or_else(
        || local_threads(live.as_ref()),
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

/// Adds the documents below catalog directory `path`, read from the
/// catalog's tree rather than walked: a superset of those a walk of it can
/// ask about, or, where the catalog cannot resolve `path`, none of them.
/// False once more than `budget` names in all would be listed.
fn documents_below(
    catalog: &ferret_catalog::Catalog,
    path: &Path,
    docs: &mut ferret_index::DocSet,
    budget: &mut u64,
    stopped: &dyn Fn() -> bool,
) -> Result<bool, ferret_index::ReadError> {
    let Some(ferret_catalog::Resolved {
        target: ferret_catalog::Target::Inode(start),
        remainder: b"",
        ..
    }) = catalog.resolve(path.as_os_str().as_bytes())
    else {
        return Ok(true);
    };
    let mut directories = vec![start];
    while let Some(directory) = directories.pop() {
        if stopped() {
            return Err(ferret_index::ReadError::Cancelled);
        }
        for name in catalog.children(directory) {
            let Some(left) = budget.checked_sub(1) else {
                return Ok(false);
            };
            *budget = left;
            let ferret_catalog::Target::Inode(child) =
                ferret_catalog::Target::from_child(catalog.child(name))
            else {
                continue;
            };
            if catalog.is_directory(child) {
                directories.push(child);
            } else if let Some(doc) = catalog.doc(child) {
                docs.insert(doc.0);
            }
        }
    }
    Ok(true)
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
        stdout_fault(error, |message| self.failure(message))
    }
}

fn covered_inputs(live: Option<&live::Anchors>) -> bool {
    live.is_none_or(live::Anchors::all_covered)
}

fn test_injections() -> bool {
    std::env::vars_os().any(|(key, _)| key.as_bytes().starts_with(b"FERRET_TEST_"))
}

fn context_error(args: &[OsString], global_json: bool, consumed: bool) -> Option<Exit> {
    if global_json {
        return Some(failure(
            "global --json framing is not supported for grep; use ferret grep --json",
        ));
    }
    if std::env::var_os("RIPGREP_CONFIG_PATH").is_some_and(|s| !s.is_empty()) {
        return Some(unsupported(
            args,
            "RIPGREP_CONFIG_PATH is set; pass flags explicitly",
            consumed,
        ));
    }
    None
}

/// Why a request is not run, decided before any output or input read.
enum Refusal {
    /// ferret lacks the feature; rg may answer instead.
    Unsupported(String),
    /// The command is wrong, or its resources are; rg would not help.
    Failed(String),
}

impl Refusal {
    fn exit(self, args: &[OsString], consumed: bool) -> Exit {
        match self {
            Self::Unsupported(message) => unsupported(args, &message, consumed),
            Self::Failed(message) => failure(&message),
        }
    }

    fn into_message(self) -> String {
        match self {
            Self::Unsupported(message) | Self::Failed(message) => message,
        }
    }
}

/// Refuses a command ferret grep does not support. When
/// `FERRET_GREP_FALLBACK` names a real rg (D118 step 2: the agents' stand-in
/// `rg` sets it), that rg runs the same argv in this process instead. Every
/// such refusal comes before any output, so the caller sees only rg's
/// answer; the exception is a command whose `-f` already consumed a source
/// rg could not read again. rg has no `--rg`, so each argument the parser
/// takes as that flag is dropped.
fn unsupported(args: &[OsString], message: &str, consumed: bool) -> Exit {
    if let Some(rg) = std::env::var_os("FERRET_GREP_FALLBACK").filter(|rg| !rg.is_empty())
        && !consumed
    {
        let own = line_grep::own_flags(args);
        let args = args
            .iter()
            .enumerate()
            .filter(|(at, _)| !own.contains(at))
            .map(|(_, arg)| arg);
        let error = std::process::Command::new(&rg).args(args).exec();
        return failure(&format!(
            "{message}; running {}: {error}",
            Path::new(&rg).display()
        ));
    }
    failure(message)
}

fn print(bytes: &[u8]) -> Exit {
    let stdout = io::stdout();
    let mut writer = output::Writer::new(&stdout, true);
    match writer.write_all(bytes).and_then(|()| writer.flush()) {
        Ok(()) => Exit::Ok,
        Err(error) => output_failure(&error),
    }
}

fn output_failure(error: &io::Error) -> Exit {
    stdout_fault(error, failure)
}

/// File staging faults have already been diagnosed by the per-file
/// transaction; any other fault is reported through `failure`.
fn stdout_fault(error: &io::Error, failure: impl FnOnce(&str) -> Exit) -> Exit {
    stdout_error(error).map_or(Exit::Ok, |message| failure(&message))
}

/// D94: a reader that closes the pipe ends grep quietly with success, so a
/// closed pipe has no message; any other write fault does.
fn stdout_error(error: &io::Error) -> Option<String> {
    (error.kind() != io::ErrorKind::BrokenPipe).then(|| format!("writing stdout: {error}"))
}

fn failure(message: &str) -> Exit {
    cli::error(&format!("grep: {message}"));
    Exit::Usage
}

fn open_index(index: Option<&Path>) -> Result<Engine, String> {
    let index = index_path(index)?;
    // Grep reads content and names, never the name planner: no interning.
    let engine = Engine::open_with_content(&index, true)
        .map_err(|error| match error {
            crate::engine::Error::Catalog(error) => format!(
                "{}: {error}; re-index or use a direct file/stdin",
                index.display()
            ),
            error => error.to_string(),
        })?
        .ok_or_else(|| {
            format!(
                "no index in {}; run ferret index DIR, use -u, or omit --live to search uncovered paths",
                index.display()
            )
        })?;
    Ok(engine)
}

fn index_path(index: Option<&Path>) -> Result<PathBuf, String> {
    crate::xdg::index_dir(index, || Dirs::from_env().ok().map(|dirs| dirs.data))
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

/// Content search reads named special files after every other operand: a
/// pipe's producer may hold it open, and rg answers `-q` or `-l` from the
/// files meanwhile. Stdin keeps its place.
fn resolve_inputs_at(paths: &[PathBuf], names_only: bool, cwd: &Path) -> Vec<Input> {
    let mut inputs = resolve_each(paths, names_only, cwd);
    inputs.sort_by_key(|input| matches!(input, Input::Stream(path) if path != Path::new("-")));
    inputs
}

fn resolve_each(paths: &[PathBuf], names_only: bool, cwd: &Path) -> Vec<Input> {
    paths
        .iter()
        .map(|path| {
            // `-/` and `-/.` are stdin too; one spelling keeps `label` a
            // byte comparison.
            if path == Path::new("-") {
                return Input::Stream(PathBuf::from("-"));
            }
            match fs::metadata(cwd.join(path)) {
                Ok(metadata) if metadata.is_dir() => Input::Directory(path.clone()),
                Ok(metadata) if metadata.is_file() || names_only => {
                    Input::File(path.clone(), metadata.len())
                }
                Ok(_) => Input::Stream(path.clone()),
                Err(error) => Input::Failed(path.clone(), error),
            }
        })
        .collect()
}

/// Search threads for a walk with no index pinned: the policy index's
/// setting under live anchors, else the environment's or the default.
fn local_threads(live: Option<&live::Anchors>) -> usize {
    live.map_or_else(
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

#[cfg(test)]
mod tests;

#[cfg(any(test, debug_assertions))]
mod seam {
    #[cfg(test)]
    use ferret_query::find::OutputBuffer;
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
        #[cfg(debug_assertions)]
        appended: bool,
    }

    impl<'a> Observed<'a> {
        pub(super) fn new(input: &'a mut dyn Read) -> Self {
            Self {
                input,
                #[cfg(debug_assertions)]
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
