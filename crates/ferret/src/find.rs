//! `ferret find`: GNU syntax over catalog visibility or the unrestricted live
//! source.

use std::ffi::OsString;
use std::io::{self, BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use ferret_catalog::{Catalog, Section};

use ferret_query::find::{Effects, Plan, WalkError};

use crate::cli::{self, Exit};
use crate::xdg::Dirs;

/// Stdout buffer. The engine flushes it before every child process, so a
/// larger buffer changes only how often a plain walk writes.
const OUTPUT_BUFFER: usize = 64 * 1024;

/// Runs a find command. Find errors and usage errors both exit 1; no matches
/// is success. Explicit -I never reads config or opens an index; neither mode
/// writes a query log.
pub fn run(args: &[OsString], index: Option<&Path>) -> Exit {
    let plan = match Plan::parse(args) {
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
    let catalog = if plan.no_ignore() || plan.is_information() {
        None
    } else {
        let dirs = Dirs::from_env();
        let config = Dirs::config_from_env().ok().map(|dir| dir.join("config"));
        let no_ignore = match config.as_ref().map(|path| read_config(path)).transpose() {
            Ok(value) => value.unwrap_or(false),
            Err(error) => {
                if let Some(path) = config {
                    cli::error(&format!("find: {}: {error}", path.display()));
                }
                return Exit::NoMatch;
            }
        };
        if no_ignore {
            None
        } else {
            let index = index
                .map(Path::to_owned)
                .or_else(|| {
                    std::env::var_os("FERRET_INDEX")
                        .filter(|value| !value.is_empty())
                        .map(PathBuf::from)
                })
                .or_else(|| dirs.as_ref().ok().map(|dirs| dirs.data.clone()));
            let Some(index) = index else {
                cli::error("find: cannot locate index; set --index or FERRET_INDEX, or use -I");
                return Exit::NoMatch;
            };
            let catalog = match Catalog::open(&index) {
                Ok(Some(catalog)) => catalog,
                Ok(None) => {
                    cli::error(&format!(
                        "find: no index in {}; run ferret index DIR or use -I",
                        index.display()
                    ));
                    return Exit::NoMatch;
                }
                Err(error) => {
                    cli::error(&format!(
                        "find: cannot open index in {}: {error}; re-index or use -I",
                        index.display()
                    ));
                    return Exit::NoMatch;
                }
            };
            if let Err(error) = catalog.load(&[
                Section::Names,
                Section::Links,
                Section::Roots,
                Section::Entries,
            ]) {
                cli::error(&format!(
                    "find: cannot read index: {error}; re-index or use -I"
                ));
                return Exit::NoMatch;
            }
            Some(catalog)
        }
    };
    if plan.permission_warning() {
        cli::error(
            "find: warning: -perm /000 now matches all files; use -perm -000 for the equivalent form",
        );
    }
    let stdout = io::stdout();
    let mut effects = Output {
        writer: BufWriter::with_capacity(OUTPUT_BUFFER, stdout.lock()),
    };
    let result = match catalog {
        Some(catalog) => plan.run(&mut plan.catalog_source(catalog), &mut effects),
        None => plan.run(&mut plan.live_source(), &mut effects),
    };
    let outcome = match result {
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

// A single boolean needs no general configuration parser or new dependency.
fn read_config(path: &Path) -> io::Result<bool> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_config(&text),
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
mod tests {
    use super::parse_config;

    #[test]
    fn config_accepts_one_boolean_and_rejects_ambiguous_settings() {
        assert!(!parse_config("# empty\n").unwrap());
        assert!(parse_config("find_no_ignore = true # GNU mode\n").unwrap());
        assert!(!parse_config("find_no_ignore=false\n").unwrap());
        for text in [
            "find_no_ignore = yes",
            "other = true",
            "find_no_ignore = true\nfind_no_ignore = false",
        ] {
            assert!(parse_config(text).is_err(), "{text}");
        }
    }
}
