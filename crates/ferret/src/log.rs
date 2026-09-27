//! The local query and timing log: one JSON line per `find` and per index
//! run, appended to `$XDG_STATE_HOME/ferret/log.jsonl`.
//!
//! It is the future source of the opt-in upload and of structure choices
//! (DESIGN § Experiments and metrics), so it records query text, plans,
//! counts and timings, and never ids (D27 renumbers them) or result paths.
//! Root paths are left out too; a line says how many roots a run touched.
//!
//! Best-effort: [`append`] returns the error and the caller only warns. A
//! command never fails because its log line could not be written.

use std::fs::{DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::json::Object;

/// The log's file name inside ferret's state directory.
pub const FILE: &str = "log.jsonl";

/// The version of the line shape. Bump it when a field changes meaning.
pub const VERSION: u8 = 1;

/// The log file in `state`.
pub fn path(state: &Path) -> PathBuf {
    state.join(FILE)
}

/// Opens a log line: the version, the command and the wall-clock time in
/// Unix milliseconds. The caller adds its fields and calls
/// [`Object::end`].
pub fn line<'a>(out: &'a mut Vec<u8>, command: &str, at: SystemTime) -> Object<'a> {
    let millis = at
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i128);
    let mut object = Object::new(out);
    object
        .int("v", VERSION)
        .str("cmd", command)
        .int("at_ms", millis);
    object
}

/// Appends `line` and a newline to the log in `state`, creating the
/// directory (0700, as XDG asks) and the file (0600). One `write` per line,
/// on a file opened for append, so concurrent `ferret` processes do not
/// interleave within a line.
pub fn append(state: &Path, line: &[u8]) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(state)?;
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path(state))?;
    let mut bytes = Vec::with_capacity(line.len() + 1);
    bytes.extend_from_slice(line);
    bytes.push(b'\n');
    file.write_all(&bytes)
}
