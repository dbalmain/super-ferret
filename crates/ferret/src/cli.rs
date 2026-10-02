//! The program: parse the command line, locate the index and ferret's
//! directories, run one command, and map its outcome to an exit status.
//!
//! Each command lives in its own module ([`crate::find`], [`crate::search`],
//! [`crate::index`], [`crate::stats`]) and returns an [`Exit`]; this module is
//! the only one that reads the process environment.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use crate::args::{self, Command};
use crate::xdg::Dirs;

/// ferret's exit statuses. They are stable: scripts and the agent skill
/// depend on them. Search follows grep's convention; find uses GNU's 0/1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// The command succeeded; for `search`, at least one row was printed.
    Ok = 0,
    /// `search` matched nothing, or `find` encountered an error.
    NoMatch = 1,
    /// The command line, or a query atom in it, is not valid. Nothing ran.
    Usage = 2,
    /// The command could not do its job: no index, an I/O error, another
    /// index run holding the lock, or a walk fault that published nothing.
    Error = 3,
}

impl From<Exit> for ExitCode {
    fn from(exit: Exit) -> Self {
        ExitCode::from(exit as u8)
    }
}

/// What every command needs to know about where things are.
pub struct Context {
    /// The index directory: `--index`, `FERRET_INDEX`, or
    /// `$XDG_DATA_HOME/ferret`.
    pub index: PathBuf,
    /// ferret's per-user directories, when `HOME` or the XDG variables
    /// place them. Without them there is no global ignore file and no log.
    pub dirs: Option<Dirs>,
}

impl Context {
    /// Appends a query-log line, warning instead of failing.
    pub fn log(&self, line: &[u8]) {
        let Some(dirs) = &self.dirs else {
            return;
        };
        if let Err(e) = crate::log::append(&dirs.state, line) {
            warn(&format!(
                "query log {} not written: {e}",
                crate::log::path(&dirs.state).display()
            ));
        }
    }
}

/// The usage text, printed by `ferret help`.
pub const USAGE: &str = "\
ferret: search files by name and metadata.

usage:
  ferret index [DIR...]         add each DIR as a root and index it;
                                with none, re-index every root
  ferret roots list             print the roots, one per line
  ferret roots remove DIR...    stop indexing DIR (roots inside it stay)
  ferret find [-I|--no-ignore] [-P] [PATH...] [EXPRESSION]
                                GNU find syntax; -I walks without an index
                                default uses catalog visibility and respects ignore rules
                                pasted find ... -delete skips ignored files and still exits 0
  ferret search [--json] [--limit N] [--] ATOM...
                                print each path that matches every ATOM
  ferret stats                  counts, sizes and a census of the index
  ferret help | --version

  --index DIR   the index to use; else $FERRET_INDEX, else
                $XDG_DATA_HOME/ferret (~/.local/share/ferret)

find expressions:
  -name/-iname GLOB, -path/-ipath GLOB, -wholename/-iwholename GLOB
  -type f,d,l,p,s,b,c          one type or a comma list
  -maxdepth N, -mindepth N, -depth, -xdev/-mount
  -print, -print0, -prune, -quit, -true, -false
  ( EXPR ), !/-not, -a/-and, -o/-or, comma; adjacent tests imply AND
  No action adds -print to the entire expression. Stars match dots and slashes.

query atoms (all must match; one atom per argument):
  WORD            the name contains WORD; with a '/', the path does
  GLOB            *.rs matches the name; src/**/*.rs the path's end,
                  /home/*/x the whole path
  re:REGEX        a regex over the name
  path:TEXT       the path contains TEXT
  ext:EXT         the name ends in .EXT
  size:[<>]N[kMGT]              size in bytes, powers of 1024
  mtime:(<|>)N(s|m|h|d|w|y)     age: mtime:<1d changed within a day
  type:(f|d|l)                  file, directory or symlink
  case:ATOM       match ATOM case-sensitively. Otherwise words, globs and
                  ext: fold ASCII case only, and re: folds Unicode case.

output: one path per line, raw bytes, in index order (unsorted);
  --json prints one object per line:
  {\"path\":…,\"type\":\"file\"|\"dir\"|\"symlink\",\"size\":N,\"mtime\":SECONDS,\"doc\":N|null}
  and \"path_base64\" with the exact bytes when the path is not UTF-8.

find exit status: 0 success, 1 error (including invalid syntax).
  Default mode needs an index covering each start. Missing/incompatible indexes
  and unresolved starts fail: re-index or use -I.
  $XDG_CONFIG_HOME/ferret/config: find_no_ignore = true makes -I the default.
  Pasted find ... -delete skips ignored files and still exits 0; a failed
  deletion (for example a directory still holding ignored files) exits 1.

search exit status: 0 success (search printed a row), 1 search matched nothing,
  2 usage error, 3 runtime error (no index, I/O, lock held, walk faults).

each search and index run appends one JSON line to
$XDG_STATE_HOME/ferret/log.jsonl (mode 0600): the query as typed, its plan,
counts and timings. No field holds a result path, a root path or an id, but
the query text may itself contain a path (search path:/some/dir).
";

/// Runs `ferret` with the process's arguments and environment.
pub fn main() -> ExitCode {
    run(std::env::args_os().skip(1)).into()
}

fn run(args: impl IntoIterator<Item = OsString>) -> Exit {
    let args = match args::parse(args) {
        Ok(args) => args,
        Err(e) => {
            error(&format!("{e}\nrun `ferret help` for usage"));
            return Exit::Usage;
        }
    };
    match args.command {
        Command::Find(ref find_args) => return crate::find::run(find_args, args.index.as_deref()),
        Command::Help => return print("usage", USAGE.as_bytes()),
        Command::Version => {
            let version = format!("ferret {}\n", env!("CARGO_PKG_VERSION"));
            return print("the version", version.as_bytes());
        }
        _ => {}
    }
    let dirs = Dirs::from_env();
    let index = args
        .index
        .or_else(|| {
            std::env::var_os("FERRET_INDEX")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| dirs.as_ref().ok().map(|d| d.data.clone()));
    let Some(index) = index else {
        // Only reachable when HOME is unusable, so `dirs` holds the reason.
        if let Err(e) = &dirs {
            error(&format!("{e}; name the index with --index or FERRET_INDEX"));
        }
        return Exit::Error;
    };
    let context = Context {
        index,
        dirs: dirs.ok(),
    };
    match args.command {
        Command::Search { atoms, json, limit } => crate::search::run(&context, &atoms, json, limit),
        Command::Index(roots) => crate::index::index(&context, &roots),
        Command::RootsList => crate::index::list(&context),
        Command::RootsRemove(roots) => crate::index::remove(&context, &roots),
        Command::Stats => crate::stats::run(&context),
        Command::Help | Command::Version | Command::Find(_) => Exit::Ok,
    }
}

/// Writes a command's whole report to stdout. A reader that went away
/// (`ferret … | head`) is not an error: what it read was delivered, and the
/// command's own outcome stands. Any other write failure is reported and is
/// [`Exit::Error`], since the output was the point. `what` names the output
/// in that message.
///
/// Rust ignores SIGPIPE, so without this a closed pipe is an `EPIPE` that
/// `println!` turns into a panic, exit 101, before the command logs.
pub fn print(what: &str, bytes: &[u8]) -> Exit {
    let mut out = io::stdout().lock();
    match out.write_all(bytes).and_then(|()| out.flush()) {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => {
            error(&format!("writing {what}: {e}"));
            Exit::Error
        }
        _ => Exit::Ok,
    }
}

/// Writes to stderr, ignoring failure: a closed or full stderr has nowhere
/// to be reported, and must not panic as `eprintln!` would.
pub fn note(text: &str) {
    let _ = io::stderr().lock().write_all(text.as_bytes());
}

/// Prints `ferret: MESSAGE` to stderr.
pub fn error(message: &str) {
    note(&format!("ferret: {message}\n"));
}

/// Prints `ferret: warning: MESSAGE` to stderr.
pub fn warn(message: &str) {
    note(&format!("ferret: warning: {message}\n"));
}
