//! Shared grep host: resolve operands before opening a generation, adapt policy
//! and checked reads, and commit each file's bounded plain output as a group.

use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

mod groups;
mod holders;
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

// Both hosts supply the same resolved environment and pinned generation.
#[derive(Clone, Copy)]
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

impl<'a> Execution<'a> {
    fn run<W: Write>(self, inputs: Vec<Input>, writer: W) -> Exit {
        self.search(inputs, writer).unwrap_or_else(|exit| exit)
    }

    /// The search. A failure ends it early as `Err`, carrying the exit it
    /// reported.
    fn search<W: Write>(self, inputs: Vec<Input>, writer: W) -> Result<Exit, Exit> {
        let Self {
            plan,
            runtime,
            start,
            ..
        } = self;
        let cancelled = runtime.cancelled;
        let stopped = || cancelled.load(Ordering::Acquire);
        let recursive = inputs
            .iter()
            .any(|input| matches!(input, Input::Directory(_)));
        let prepared = self.prepare(&inputs)?;
        // D99 asks whether the walk selected any file; pruning can skip
        // the only ones. Kept to ask again without it.
        let operands: Option<Vec<PathBuf>> = (prepared.holders.is_some() && plan.paths.is_empty())
            .then(|| {
                inputs
                    .iter()
                    .filter_map(|input| match input {
                        Input::Directory(path) => Some(path.clone()),
                        _ => None,
                    })
                    .collect()
            });
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
        let inputs = host.report_failed(inputs, runtime)?;
        // D99: with no path operand, a walk whose filters selected no file is
        // diagnosed at exit.
        let mut searched = false;
        if self.search_inputs(&mut host, inputs, &prepared, &mut searched)? && !plan.json {
            return Ok(Exit::Ok);
        }
        if let Some(operands) = operands.filter(|_| !searched && !stopped()) {
            searched = self.selects_a_file(operands);
        }
        if plan.json
            && let Err(error) = json::summary(&mut host.writer, &host.json_stats, start)
        {
            return Err(runtime.output_failure(&error));
        }
        host.writer
            .flush()
            .map_err(|error| runtime.output_failure(&error))?;
        Ok(host.exit(searched, runtime))
    }

    /// Searches `inputs` in order, each run of walkable operands as one
    /// segment and each stream alone. Returns whether a quiet search
    /// stopped at its first match.
    fn search_inputs<W: Write>(
        &self,
        host: &mut Host<'_, W>,
        inputs: Vec<Input>,
        prepared: &Prepared<'_>,
        searched: &mut bool,
    ) -> Result<bool, Exit> {
        let Self {
            plan,
            live,
            runtime,
            ..
        } = *self;
        let mut inputs = inputs.into_iter().peekable();
        while inputs.peek().is_some() {
            let mut segment = Vec::new();
            while inputs.peek().is_some_and(|input| {
                !matches!(input, Input::Stream(_))
                    && (segment.is_empty()
                        || !buffered_path_output(plan, runtime.tty) && covered_inputs(live))
            }) {
                if let Some(input) = inputs.next() {
                    segment.push(input);
                }
            }
            if !segment.is_empty() {
                self.segment(host, segment, prepared, searched)?;
                if plan.quiet && host.matched {
                    return Ok(true);
                }
                if buffered_path_output(plan, runtime.tty)
                    && let Err(error) = host.writer.flush()
                {
                    return Err(runtime.output_failure(&error));
                }
            }
            if let Some(Input::Stream(path)) =
                inputs.next_if(|input| matches!(input, Input::Stream(_)))
            {
                *searched = true;
                self.stream(host, &path)?;
                if plan.quiet && host.matched {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// The catalog's part of a search, before any walk: the index's
    /// candidates, probed against the documents the walks can reach, the
    /// documents' names and reader, and the directories a pruned walk lists.
    fn prepare(&self, inputs: &[Input]) -> Result<Prepared<'a>, Exit> {
        let Self {
            plan, pin, runtime, ..
        } = *self;
        let stopped = || runtime.cancelled.load(Ordering::Acquire);
        let mut candidates = if !plan.invert {
            pin.map(|pin| pin.grep_candidates(&plan.grep, &stopped))
                .transpose()
                .map_err(|error| runtime.failure(&error.to_string()))?
        } else {
            None
        };
        let names = pin
            .map(|pin| pin.doc_names_until(stopped))
            .transpose()
            .map_err(|error| runtime.failure(&error.to_string()))?
            .flatten();
        let documents = pin
            .map(|pin| {
                ferret_crawl::Documents::by_name(pin.catalog()).map(|documents| {
                    documents.with_config(ferret_policy::Config { size_cap: u64::MAX })
                })
            })
            .transpose()
            .map_err(|error| runtime.failure(&error.to_string()))?;
        // The catalog documents the walks below can reach: candidates are
        // decoded for these alone (S5). Listing a name costs about what
        // decoding a candidate does, so past half the candidates' bound,
        // decoding them all is the cheaper answer.
        let mut reachable = candidates.as_ref().zip(pin).map(|(candidates, pin)| {
            (
                ferret_index::DocSet::empty(pin.catalog().doc_count()),
                candidates.bound() / 2,
            )
        });
        self.check_directories(inputs, &mut reachable)?;
        // A walk too large to probe lists only the directories holding a
        // candidate. Absent files, followed links and live walks need every
        // directory.
        let prune = reachable.is_none()
            && plan.universe() == line_grep::Universe::Candidates
            && !plan.traversal.follow
            && !plan.traversal.unrestricted;
        if let (Some(candidates), Some((reachable, _))) = (&mut candidates, reachable)
            && let Err(error) = candidates.probe(reachable, &stopped)
        {
            return Err(runtime.failure(&error.to_string()));
        }
        let holders = self.holders(candidates.as_ref(), prune, &stopped)?;
        Ok(Prepared {
            candidates,
            names,
            documents,
            holders,
        })
    }

    /// The directories a pruned walk lists. Past one candidate a document,
    /// every directory may hold one, unless the globs and types admit files
    /// by name.
    fn holders(
        &self,
        candidates: Option<&engine::GrepCandidates<'a>>,
        prune: bool,
        stopped: &dyn Fn() -> bool,
    ) -> Result<Option<holders::Holders>, Exit> {
        let Self {
            plan,
            selection,
            pin,
            runtime,
            ..
        } = *self;
        match candidates.zip(pin).filter(|(candidates, pin)| {
            prune
                && (candidates.bound() < u64::from(pin.catalog().doc_count())
                    || selection.whitelists_names())
        }) {
            Some((candidates, pin)) => {
                holders::Holders::new(pin.catalog(), candidates, selection, plan.text, stopped)
                    .map_err(|error| runtime.failure(&error.to_string()))
            }
            None => Ok(None),
        }
    }

    /// Refuses before any output when a live operand's anchors fail to
    /// validate or a catalogued operand's walk cannot start, and gathers
    /// the documents `reachable` probes for.
    fn check_directories(
        &self,
        inputs: &[Input],
        reachable: &mut Option<(ferret_index::DocSet, u64)>,
    ) -> Result<(), Exit> {
        let Self {
            pin,
            live,
            cwd,
            runtime,
            ..
        } = *self;
        let stopped = || runtime.cancelled.load(Ordering::Acquire);
        for input in inputs {
            if let Input::Directory(path) = input {
                if let Some(anchors) = live.filter(|anchors| anchors.walks(path)) {
                    if let Err(error) = anchors.validate(path) {
                        return Err(runtime.failure(&format!("{}: {error}", path.display())));
                    }
                } else if let Some(pin) = pin {
                    let mut source =
                        sources::Directory::new(path, Some(pin.catalog()), cwd, false, false)
                            .map_err(|error| runtime.failure(&error.to_string()))?;
                    if let Some(Err(error)) = source.next(true) {
                        return Err(runtime.failure(&format!(
                        "{}: {}; refresh the index with ferret index DIR, or use -u to search current files",
                        error.path.display(),
                        error.error
                    )));
                    }
                    if let Some((docs, budget)) = reachable {
                        match documents_below(
                            pin.catalog(),
                            &cwd.join(path),
                            docs,
                            budget,
                            &stopped,
                        ) {
                            Ok(true) => {}
                            Ok(false) => *reachable = None,
                            Err(error) => return Err(runtime.failure(&error.to_string())),
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The source that lists `segment`'s inputs and their candidates.
    fn source<'s>(&self, segment: Vec<Input>, prepared: &'s Prepared<'a>) -> groups::Source<'s> {
        let plan = self.plan;
        groups::Source::new(
            segment,
            self.pin.map(|p| p.catalog()),
            prepared
                .candidates
                .as_ref()
                .map(|candidates| candidates as &dyn groups::Candidates),
            self.selection,
            self.cwd.to_owned(),
        )
        .with_traversal(plan.traversal, plan.text)
        .with_live(self.live)
        .with_holders(prepared.holders.as_ref())
        .with_absence(plan.universe() == line_grep::Universe::Searchable)
    }

    /// Searches one segment of operands, on several workers when it walks
    /// live.
    fn segment<W: Write>(
        &self,
        host: &mut Host<'_, W>,
        segment: Vec<Input>,
        prepared: &Prepared<'_>,
        searched: &mut bool,
    ) -> Result<(), Exit> {
        let Self {
            pin,
            cwd,
            threads,
            runtime,
            ..
        } = *self;
        let cancelled = runtime.cancelled;
        let fork = || {
            pin.zip(prepared.documents.as_ref())
                .map(|(pin, documents)| engine::reader::checked(pin.catalog(), documents))
        };
        let threads = if segment
            .iter()
            .any(|input| matches!(input, Input::Directory(_)))
        {
            threads
        } else {
            threads.min(segment.len())
        };
        let mut source = self.source(segment, prepared);
        let walked = (threads > 1 && source.walks_live())
            .then(|| groups::walk(host, &mut source, threads, cancelled, searched));
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
                    return Err(runtime.output_failure(&error));
                }
                Some(1)
            }
            Some(Err(error)) => return Err(runtime.output_failure(&error)),
        };
        if let Some(threads) = serial {
            let result = groups::run(
                host,
                |stopped| source.next(stopped),
                cwd,
                &fork,
                prepared.names,
                threads,
                cancelled,
            );
            *searched |= source.searched();
            result.map_err(|error| runtime.output_failure(&error))?;
        }
        Ok(())
    }

    /// Searches a stream operand, `-` being stdin, front to back.
    fn stream<W: Write>(&self, host: &mut Host<'_, W>, path: &Path) -> Result<(), Exit> {
        let result = if path == Path::new("-") {
            host.direct(path, &mut io::stdin().lock(), u64::MAX)
        } else {
            match fs::File::open(self.cwd.join(path)) {
                Ok(mut file) => host.direct(path, &mut file, u64::MAX),
                Err(error) => host.error(path, &error),
            }
        };
        result.map_err(|error| self.runtime.output_failure(&error))
    }

    /// Whether an unpruned walk of `operands` selects any file (D99).
    fn selects_a_file(&self, operands: Vec<PathBuf>) -> bool {
        let Self {
            plan,
            selection,
            pin,
            live,
            cwd,
            runtime,
            ..
        } = *self;
        let stopped = || runtime.cancelled.load(Ordering::Acquire);
        let mut source = groups::Source::new(
            operands.into_iter().map(Input::Directory).collect(),
            pin.map(|p| p.catalog()),
            None,
            selection,
            cwd.to_owned(),
        )
        .with_traversal(plan.traversal, plan.text)
        .with_live(live)
        .until_searched();
        while source.next(&stopped).is_some() {}
        source.searched()
    }
}

/// What [`Execution::prepare`] takes from the pinned catalog and index.
struct Prepared<'a> {
    candidates: Option<engine::GrepCandidates<'a>>,
    names: Option<&'a ferret_query::DocNames>,
    documents: Option<ferret_crawl::Documents>,
    holders: Option<holders::Holders>,
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
    let engine = Engine::open_one_shot(&index)
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

impl<W: Write> Host<'_, W> {
    /// Reports each failed operand, returning the others.
    fn report_failed(&mut self, inputs: Vec<Input>, runtime: &Runtime) -> Result<Vec<Input>, Exit> {
        let mut resolved = Vec::new();
        for input in inputs {
            if let Input::Failed(path, error) = input {
                self.error(&path, &error)
                    .map_err(|error| runtime.output_failure(&error))?;
            } else {
                resolved.push(input);
            }
        }
        Ok(resolved)
    }

    /// The search's exit status, once its output is flushed.
    fn exit(&self, searched: bool, runtime: &Runtime) -> Exit {
        if self.plan.quiet && self.matched {
            Exit::Ok
        } else if self.plan.paths.is_empty() && !searched {
            let _ = (runtime.stderr)(
                "grep: no files were searched; a glob, type, depth, hidden or ignore filter probably \
         excluded every file under the current directory",
            );
            Exit::Usage
        } else if self.errors {
            Exit::Usage
        } else if self.matched {
            Exit::Ok
        } else {
            Exit::NoMatch
        }
    }
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
            groups::read_file(
                Path::new(""),
                path,
                directory.as_ref(),
                plan,
                each,
                &stopped,
            )
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
            // Stdin may be slow and stdout a terminal.
            batched: false,
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
            return self.publish_json(path, result, output);
        }
        if self.plan.quiet {
            return Ok(());
        }
        if matches!(self.plan.output, Output::Names | Output::WithoutMatch) {
            if result.matched {
                self.path_label(path, b"\n")?;
            }
            return Ok(());
        }
        // M4 R1: counts stay numeric after a binary sentinel, as in rg.
        if matches!(self.plan.output, Output::Count | Output::CountMatches) {
            if result.count > 0 || (self.plan.include_zero && result.searched) {
                if self.filename {
                    self.path_label(path, b":")?;
                }
                writeln!(self.writer, "{}", result.count)?;
            }
        } else if result.binary {
            if result.matched {
                self.binary_warning(path)?;
            }
        } else {
            self.publish_group(&mut output)?;
        }
        Ok(())
    }

    /// Writes one file's JSON events and counts its stats.
    fn publish_json(
        &mut self,
        path: &Path,
        result: InputResult,
        mut output: OutputBuffer,
    ) -> io::Result<()> {
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
        Ok(())
    }

    /// Writes `path`'s label and then `separator`, or NUL under `--null`.
    fn path_label(&mut self, path: &Path, separator: &[u8]) -> io::Result<()> {
        presentation::path(
            &self.plan.presentation,
            &mut self.writer,
            label(path, self.plan.paths.is_empty()),
        )?;
        self.writer.write_all(if self.plan.presentation.null {
            b"\0"
        } else {
            separator
        })
    }

    /// Writes one file's match groups, separated from the previous file's.
    fn publish_group(&mut self, output: &mut OutputBuffer) -> io::Result<()> {
        if self.emitted_group && !output.is_empty() {
            if self.plan.presentation.heading == Some(true) {
                self.writer.write_all(b"\n")?;
            } else if self.plan.before > 0 || self.plan.after > 0 {
                self.writer.write_all(b"--\n")?;
            }
        }
        self.emitted_group |= !output.is_empty();
        let flag = self.cancelled;
        groups::emit(output, &mut self.writer, &|| {
            flag.is_some_and(|flag| flag.load(Ordering::Acquire))
        })
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

/// Called for every listed name, so it compares bytes: `Path` equality
/// parses components. `resolve_each` spells every stdin operand `-`; a
/// walked file of that name is `./-`.
fn label(path: &Path, implicit: bool) -> &[u8] {
    if path.as_os_str() == "-" {
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
mod tests;

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
