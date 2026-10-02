//! `ferret find`: GNU syntax over the live source, without opening an index.

use std::ffi::OsString;
use std::io::{self, BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use ferret_query::find::{Effects, Plan, WalkError};

use crate::cli::{self, Exit};

/// Stdout buffer. The engine flushes it before every child process, so a
/// larger buffer changes only how often a plain walk writes.
const OUTPUT_BUFFER: usize = 64 * 1024;

/// Runs a find command. Find errors and usage errors both exit 1; no matches
/// is success. This path never reads config, opens an index or writes a log.
pub fn run(args: &[OsString]) -> Exit {
    let plan = match Plan::parse(args) {
        Ok(plan) => plan,
        Err(error) => {
            cli::error(&format!("find: {error}"));
            return Exit::NoMatch;
        }
    };
    if !plan.no_ignore() && !plan.is_information() {
        cli::error(
            "find: ignore-respecting mode is not implemented yet; use -I for GNU find behaviour",
        );
        return Exit::NoMatch;
    }
    if plan.permission_warning() {
        cli::error(
            "find: warning: -perm /000 now matches all files; use -perm -000 for the equivalent form",
        );
    }
    let stdout = io::stdout();
    let mut effects = Output {
        writer: BufWriter::with_capacity(OUTPUT_BUFFER, stdout.lock()),
    };
    let outcome = match plan.run(&mut plan.live_source(), &mut effects) {
        Ok(outcome) => outcome,
        Err(error) => {
            cli::error(&format!(
                "find: {}: not implemented yet",
                error.feature.to_string_lossy()
            ));
            return Exit::NoMatch;
        }
    };
    let flushed = effects.writer.flush();
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

struct Output<W> {
    writer: W,
}

impl<W: Write> Effects for Output<W> {
    fn print(&mut self, path: &Path, nul: bool) -> io::Result<()> {
        self.writer.write_all(path.as_os_str().as_bytes())?;
        self.writer.write_all(if nul { b"\0" } else { b"\n" })
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }

    fn warning(&mut self, message: &str) {
        cli::error(&format!("find: {message}"));
    }

    fn error(&mut self, error: &WalkError) {
        cli::error(&format!("find: {}: {}", error.path.display(), error.error));
    }
}
