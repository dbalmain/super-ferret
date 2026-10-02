//! `ferret find`: GNU syntax over the live source, without opening an index.

use std::ffi::OsString;

use crate::cli::Exit;

/// Runs a find command. Find errors and usage errors both exit 1.
pub fn run(_args: &[OsString]) -> Exit {
    crate::cli::error("find: not implemented yet");
    Exit::NoMatch
}
