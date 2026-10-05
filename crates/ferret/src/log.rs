//! The local query and timing log: one JSON line per `search` and per index
//! run, appended to `$XDG_STATE_HOME/ferret/log.jsonl`.
//!
//! It is the future source of the opt-in upload and of structure choices
//! (DESIGN § Experiments and metrics), so it records query text, plans,
//! counts and timings. No field holds an id (D27 renumbers them), a result
//! path or a root path; a line says how many rows or roots there were. The
//! query atoms are logged as typed, though, so **query text may itself
//! contain a path** (`search path:/home/me/private`) or any other name the
//! user searched for (D45).
//!
//! The file is private: [`append`] sets it to 0600 on every write, not only
//! when it creates it, so a log left readable by an older version or by hand
//! is narrowed before another line goes in.
//!
//! **Whole lines.** Each line and its newline go in one `write_all` while
//! the process holds an exclusive `flock` on the file, and every writer
//! takes that lock. So concurrent `ferret` processes never interleave within
//! a line, even when a write is short and `write_all` writes the rest in a
//! second call. The lock does not guard against a process killed mid-line,
//! which leaves a partial last line; a reader should skip a line that does
//! not parse.
//!
//! The lock is tried, not waited on: [`append`] retries for at most
//! [`LOCK_WAIT`] and then drops the line with an error. A holder that has
//! stopped (SIGSTOP, a debugger) must not hang a command whose work is done;
//! the log is best-effort, and finishing the command wins.
//!
//! Best-effort: [`append`] returns the error and the caller only warns. A
//! command never fails because its log line could not be written.

use std::fs::{DirBuilder, File, OpenOptions, Permissions, TryLockError};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::json::Object;

/// The log's file name inside ferret's state directory.
pub const FILE: &str = "log.jsonl";

/// How long [`append`] retries the log's lock before dropping the line.
/// Lines are held for one write, so a live holder releases it in well
/// under a millisecond.
pub const LOCK_WAIT: Duration = Duration::from_millis(150);

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
/// directory (0700, as XDG asks) and the file. The file is set to 0600 and
/// the line is written under an exclusive lock; see the module doc.
pub fn append(state: &Path, line: &[u8]) -> io::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state)?;
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path(state))?;
    // fchmod on the open file, so it is the file written to that is
    // narrowed. Fails, and the line is not written, if another user owns it.
    file.set_permissions(Permissions::from_mode(0o600))?;
    lock(&file)?;
    let mut bytes = Vec::with_capacity(line.len() + 1);
    bytes.extend_from_slice(line);
    bytes.push(b'\n');
    file.write_all(&bytes)
}

/// Takes the exclusive lock on `file`, retrying every few milliseconds for
/// at most [`LOCK_WAIT`]. The lock is released when `file` is closed.
fn lock(file: &File) -> io::Result<()> {
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(TryLockError::Error(e)) => return Err(e),
            Err(TryLockError::WouldBlock) if started.elapsed() >= LOCK_WAIT => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!(
                        "another process has held the log's lock for over {} ms",
                        LOCK_WAIT.as_millis()
                    ),
                ));
            }
            Err(TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(5)),
        }
    }
}
