//! The `ferret` command-line tool: argument parsing, the commands, human and
//! JSON-lines output, and the local query log. Wiring only; behaviour lives
//! in the library crates.
//!
//! - [`cli`]: the entry point, exit statuses, where the index is, usage.
//! - [`args`]: the command line, parsed; pure.
//! - [`find`], [`index`] (with `roots`), [`stats`]: one module per command.
//! - [`json`]: the JSON writer, and how non-UTF-8 bytes are encoded.
//! - [`log`]: the query and timing log.
//! - [`xdg`] resolves directories from the environment and touches no files;
//!   [`setup`] writes the files a new install starts with.

pub mod args;
pub mod cli;
pub mod find;
pub mod index;
pub mod json;
pub mod log;
pub mod setup;
pub mod stats;
pub mod xdg;
