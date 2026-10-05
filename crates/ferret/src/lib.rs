//! The `ferret` command-line tool: argument parsing, the commands, human and
//! JSON-lines output, and the local query log. Wiring only; behaviour lives
//! in the library crates. [`engine`] coordinates resident query generations
//! and writer adoption for this host and the future batch/daemon hosts.
//!
//! - [`cli`]: the entry point, exit statuses, where the index is, usage.
//! - [`batch`]: one resident engine and the sequential S1B JSON-lines host.
//! - [`args`]: the command line, parsed; pure.
//! - [`find`], [`search`], [`index`] (with `roots`), [`stats`]: one module per
//!   command.
//! - [`json`]: the JSON writer, and how non-UTF-8 bytes are encoded.
//! - [`log`]: the query and timing log. No field holds a result path, a root
//!   path or an id, but query text is logged as typed and may contain a path.
//! - [`xdg`] resolves directories from the environment and touches no files;
//!   [`setup`] writes the files a new install starts with.
//! - [`protocol`]: the shared request reader for batch and the future socket.
//!
//! **Paths that are not UTF-8.** Human output writes a path's raw bytes. In
//! JSON lines, `path` is always a string: the path's text, with each invalid
//! sequence replaced by U+FFFD. When the path is not valid UTF-8,
//! `path_base64` follows, holding the exact bytes in standard padded base64
//! (RFC 4648 § 4). A consumer that needs the real path reads `path_base64`
//! when present and `path` otherwise. The query log writes query atoms the
//! same way inside its `query` array: a string, or `{"base64":"…"}` for an
//! atom that is not UTF-8. See [`json`].
//!
//! **Exit statuses** are stable ([`cli::Exit`]): 0 success, 1 `search` matched
//! nothing, 2 usage error, 3 runtime error. Find uses GNU's convention: 0
//! success (including no matches), 1 invalid syntax or execution error.

pub mod args;
pub(crate) mod batch;
pub mod cli;
pub mod engine;
pub mod find;
pub mod index;
pub mod json;
pub mod log;
pub(crate) mod protocol;
pub mod search;
pub mod setup;
pub mod stats;
pub mod xdg;
