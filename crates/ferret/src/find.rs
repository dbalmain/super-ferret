//! `ferret find`: GNU syntax over catalog visibility or the unrestricted live
//! source.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use ferret_catalog::{Catalog, Generation};
use ferret_query::find::{Effects, Outcome, OutputBuffer, Plan, Unsupported, WalkError};

use crate::cli::{self, Exit};
use crate::config_file::Contents;
use crate::engine::QuerySession;
use crate::find_json::{self, FrameOutput};
use crate::protocol::ChildStdin;
use crate::xdg::Dirs;
use std::os::unix::ffi::OsStrExt;

pub(crate) const PERMISSION_WARNING: &str =
    "find: warning: -perm /000 now matches all files; use -perm -000 for the equivalent form";

/// The source a prepared find walks, and the catalog view a JSON `begin`
/// names. A local find walks the bare catalog: a plain listing after
/// checking every section, an action plan with only the sections it reads
/// (D56 A still runs actions here). `Pinned` is the resident engine of a
/// batch or socket find, whose name index serves `-name`. `Live` walks the
/// unrestricted tree and selects no view.
pub(crate) enum FindSource {
    Pinned(Box<QuerySession>),
    Sections(Box<Catalog>),
    Live,
}

impl FindSource {
    /// The catalog view this source selected, if any.
    pub(crate) fn generation(&self) -> Option<Generation> {
        match self {
            Self::Pinned(session) => Some(session.generation()),
            Self::Sections(catalog) => Some(catalog.generation()),
            Self::Live => None,
        }
    }

    pub(crate) fn run<E: Effects + Clone + Send>(
        self,
        plan: &Plan,
        effects: E,
        workers: usize,
    ) -> Result<Outcome, Unsupported> {
        match self {
            Self::Pinned(session) => session.find(plan, effects, workers),
            Self::Sections(catalog) => {
                plan.run_parallel(plan.parallel_catalog_source(*catalog), effects, workers)
            }
            Self::Live => plan.run_parallel(plan.live_source(), effects, workers),
        }
    }
}

/// Where a supported find plan runs, resolved once per invocation: daemon
/// routing and the local fallback read this one value, so they see the same
/// config and index precedence. Never cached across invocations; a later
/// command must see config edits.
enum Route {
    /// An unindexed walk: `-I`, a configured `find_no_ignore`, or an
    /// information plan.
    Live,
    Indexed(PathBuf),
}

/// Resolves a plan's route, or an error message (without a `find: ` prefix)
/// saying why neither is available. Explicit `-I` and information plans never
/// read config.
fn route(plan: &Plan, index: Option<&Path>) -> Result<Route, String> {
    if plan.no_ignore() || plan.is_information() {
        return Ok(Route::Live);
    }
    let config = Dirs::config_from_env().ok().map(|dir| dir.join("config"));
    let no_ignore = match config.as_deref().map(read_config).transpose() {
        Ok(value) => value.unwrap_or(false),
        Err(error) => {
            // read_config ran, so config was Some.
            let path = config.unwrap_or_default();
            return Err(format!("{}: {error}", path.display()));
        }
    };
    if no_ignore {
        return Ok(Route::Live);
    }
    crate::xdg::index_dir(index, || Dirs::from_env().ok().map(|dirs| dirs.data))
        .map(Route::Indexed)
        .ok_or_else(|| "cannot locate index; set --index or FERRET_INDEX, or use -I".to_owned())
}

/// Opens the source a resolved route walks.
fn open_source(plan: &Plan, route: Route) -> Result<FindSource, String> {
    match route {
        Route::Live => Ok(FindSource::Live),
        Route::Indexed(index) => open_catalog(plan, &index),
    }
}

fn open_catalog(plan: &Plan, index: &Path) -> Result<FindSource, String> {
    let missing = || {
        format!(
            "no index in {}; run ferret index DIR or use -I",
            index.display()
        )
    };
    // Postings need a resident name index, which only the daemon holds;
    // building one for a single find costs more than the walk, so a local
    // find walks the catalog, loading and checking only the sections its
    // plan reads (D140 A): damage elsewhere is left to the daemon's open and
    // `ferret search`.
    let failed = |error| format!("{}: {error}; re-index or use -I", index.display());
    let catalog = Catalog::open(index).map_err(failed)?.ok_or_else(missing)?;
    catalog.load(&plan.catalog_sections()).map_err(failed)?;
    Ok(FindSource::Sections(Box::new(catalog)))
}

// Routing must precede catalog open. Effects stay local; so do live routes.
fn remote(
    args: &[OsString],
    route: &Route,
    plan: &Plan,
    json: bool,
    now: std::time::SystemTime,
) -> Option<Exit> {
    match route {
        Route::Indexed(index) if !plan.has_side_effects() => {
            crate::daemon::find(index, args, json, now)
        }
        _ => None,
    }
}

/// A supported plan after routing: answered by the daemon, or to run here.
enum Prepared {
    Answered(Exit),
    Local(FindSource),
}

/// Resolves the plan's route once, offers an indexed read-only plan to the
/// daemon, and otherwise opens the local source from that same route.
fn prepare(
    args: &[OsString],
    plan: &Plan,
    index: Option<&Path>,
    json: bool,
    now: std::time::SystemTime,
) -> Result<Prepared, String> {
    let route = route(plan, index)?;
    if let Some(exit) = remote(args, &route, plan, json, now) {
        return Ok(Prepared::Answered(exit));
    }
    open_source(plan, route).map(Prepared::Local)
}

/// Runs a find command. Find errors and usage errors both exit 1; no matches
/// is success. Explicit -I never reads config or opens an index; neither mode
/// writes a query log.
pub fn run(args: &[OsString], index: Option<&Path>) -> Exit {
    let now = std::time::SystemTime::now();
    let plan = match Plan::parse_started(args, now) {
        Ok(plan) => plan,
        Err(error) => {
            cli::error(&format!("find: {error}"));
            return Exit::NoMatch;
        }
    };
    if let Some(feature) = plan.unsupported() {
        cli::error(&format!(
            "find: {}: not implemented yet",
            feature.to_string_lossy()
        ));
        return Exit::NoMatch;
    }
    let source = match prepare(args, &plan, index, false, now) {
        Ok(Prepared::Answered(exit)) => return exit,
        Ok(Prepared::Local(source)) => source,
        Err(message) => {
            cli::error(&format!("find: {message}"));
            return Exit::NoMatch;
        }
    };
    if plan.permission_warning() {
        cli::error(PERMISSION_WARNING);
    }
    let mut effects = Output::stdout();
    let workers = ferret_crawl::default_workers();
    let outcome = match source.run(&plan, effects.clone(), workers) {
        Ok(outcome) => outcome,
        Err(error) => {
            cli::error(&format!(
                "find: {}: not implemented yet",
                error.feature.to_string_lossy()
            ));
            return Exit::NoMatch;
        }
    };
    let flushed = effects.flush();
    if effects.closed() {
        return Exit::Ok;
    }
    if let Err(error) = flushed {
        cli::error(&format!("find: writing stdout: {error}"));
        return Exit::NoMatch;
    }
    if outcome.errors == 0 {
        Exit::Ok
    } else {
        Exit::NoMatch
    }
}

/// Runs `ferret --json find ARGS...`: the CLI's own structured-output host.
/// Emits the same framed block as a batch or socket find
/// (`find_json::run_block`) under the fixed id `"find"` — there is only ever
/// one find request in this process, so no id negotiation is needed. The
/// child's stdin inherits the caller's, as the raw CLI does; `-ok`/`-okdir`
/// still refuse outside a terminal (`FrameOutput::confirm` checks that
/// itself). There is no protocol caller here to withhold
/// `local-effects`/`interactive` from, so `find_json::refusal`'s capability
/// gate does not apply; this host runs every action the plan asks for. The
/// process exit status is find's native status (`Exit::Ok`/`Exit::NoMatch`);
/// parse and resolution failures are reported as a `diagnostic` plus an `end`
/// with the message, after a `begin` whose generation is null because no view
/// was selected.
pub fn run_json(args: &[OsString], index: Option<&Path>) -> Exit {
    const ID: &str = "find";
    let started = std::time::Instant::now();
    let now = std::time::SystemTime::now();
    let (plan, source) = match Plan::parse_started(args, now) {
        Err(error) => (
            Err(find_json::Failure::new("parse", error.to_string())),
            FindSource::Live,
        ),
        // The block reports the unsupported feature; nothing is routed or
        // opened for a plan that cannot run.
        Ok(plan) if plan.unsupported().is_some() => (Ok(plan), FindSource::Live),
        Ok(plan) => match prepare(args, &plan, index, true, now) {
            Ok(Prepared::Answered(exit)) => return exit,
            Ok(Prepared::Local(source)) => (Ok(plan), source),
            Err(message) => (
                Err(find_json::Failure::new("resolve", message)),
                FindSource::Live,
            ),
        },
    };
    let host = FrameOutput::new(ID, ChildStdin::Inherit).with_warning_stderr();
    let workers = ferret_crawl::default_workers();
    match find_json::run_block(host, source, plan, workers, started) {
        Ok(0) => Exit::Ok,
        Ok(_) => Exit::NoMatch,
        Err(error) => {
            cli::error(&format!("find: writing stdout: {error}"));
            Exit::NoMatch
        }
    }
}

#[derive(Clone)]
enum Output {
    /// Set once a write finds the reader gone, and shared by this invocation's
    /// worker clones. As for grep (D94), find then ends silently with exit 0:
    /// what it would still report has nowhere to go, and GNU find would
    /// already have died of SIGPIPE.
    Stdout(Arc<AtomicBool>),
    #[cfg(test)]
    Test(Arc<Mutex<Box<dyn Write + Send>>>),
}

impl Output {
    fn stdout() -> Self {
        Self::Stdout(Arc::new(AtomicBool::new(false)))
    }
    fn closed(&self) -> bool {
        match self {
            Self::Stdout(closed) => closed.load(Ordering::Relaxed),
            #[cfg(test)]
            Self::Test(_) => false,
        }
    }
    // The engine holds its transaction gate. Stdout's own lock covers the
    // destination, including its internal line buffer, without another mutex.
    fn with_writer(
        &mut self,
        write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
    ) -> io::Result<()> {
        match self {
            Self::Stdout(closed) => {
                let written = write(&mut io::stdout().lock());
                if let Err(error) = &written
                    && error.kind() == io::ErrorKind::BrokenPipe
                {
                    closed.store(true, Ordering::Relaxed);
                }
                written
            }
            #[cfg(test)]
            Self::Test(writer) => write(&mut **crate::lock(writer)),
        }
    }
    #[cfg(test)]
    fn test(writer: impl Write + Send + 'static) -> Self {
        Self::Test(Arc::new(Mutex::new(Box::new(writer))))
    }
}

impl Effects for Output {
    fn print(&mut self, path: &Path, nul: bool) -> io::Result<()> {
        self.with_writer(|writer| {
            writer.write_all(path.as_os_str().as_bytes())?;
            writer.write_all(if nul { b"\0" } else { b"\n" })
        })
    }
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.with_writer(|writer| writer.write_all(bytes))
    }
    fn flush(&mut self) -> io::Result<()> {
        self.with_writer(|writer| writer.flush())
    }
    fn output(&mut self, buffer: &mut OutputBuffer) -> io::Result<()> {
        self.with_writer(|writer| {
            buffer.write_to(writer)?;
            writer.flush()
        })
    }

    fn warning(&mut self, message: &str) {
        if !self.closed() {
            cli::error(&format!("find: {message}"));
        }
    }

    fn error(&mut self, error: &WalkError) {
        if !self.closed() {
            cli::error(&format!("find: {}: {}", error.path.display(), error.error));
        }
    }
}

// A single boolean needs no general configuration parser or new dependency.
// A dangling symlink is no file, as it always was; a FIFO must not block.
fn read_config(path: &Path) -> io::Result<bool> {
    match crate::config_file::read(path) {
        Ok(Contents::Bytes(bytes)) => match String::from_utf8(bytes) {
            Ok(text) => parse_config(&text),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stream did not contain valid UTF-8",
            )),
        },
        Ok(Contents::Missing) => Ok(false),
        Ok(Contents::NotRegular) => Err(crate::config_file::not_regular()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn parse_config(text: &str) -> io::Result<bool> {
    let mut no_ignore = None;
    for (line, text) in text.lines().enumerate() {
        let text = text.split('#').next().unwrap_or("").trim();
        if text.is_empty() {
            continue;
        }
        let value = text
            .strip_prefix("find_no_ignore")
            .and_then(|rest| rest.trim_start().strip_prefix('='))
            .map(str::trim);
        let value = match value {
            Some("true") => true,
            Some("false") => false,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("line {}: expected find_no_ignore = true or false", line + 1),
                ));
            }
        };
        if no_ignore.replace(value).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("line {}: duplicate find_no_ignore", line + 1),
            ));
        }
    }
    Ok(no_ignore.unwrap_or(false))
}

#[cfg(test)]
mod tests;
