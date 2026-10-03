//! `ferret find`: GNU syntax over catalog visibility or the unrestricted live
//! source.

use std::ffi::OsString;
use std::io::{self, BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use ferret_catalog::Catalog;

use ferret_query::find::{Effects, OutputBuffer, Plan, WalkError};

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
            if let Err(error) = catalog.load(&plan.catalog_sections()) {
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
    let mut effects = Output {
        writer: Arc::new(Mutex::new(BufWriter::with_capacity(
            OUTPUT_BUFFER,
            io::stdout(),
        ))),
        buffer: Vec::with_capacity(OUTPUT_BUFFER),
    };
    let workers = ferret_crawl::default_workers();
    let result = match catalog {
        Some(catalog) => plan.run_parallel(
            plan.parallel_catalog_source(catalog),
            effects.clone(),
            workers,
        ),
        None => plan.run_parallel(plan.live_source(), effects.clone(), workers),
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
    let flushed = effects.flush();
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

struct Output<W: Write = io::Stdout> {
    writer: Arc<Mutex<BufWriter<W>>>,
    buffer: Vec<u8>,
}

impl<W: Write> Clone for Output<W> {
    fn clone(&self) -> Self {
        Self {
            writer: self.writer.clone(),
            buffer: Vec::with_capacity(OUTPUT_BUFFER),
        }
    }
}

impl<W: Write> Output<W> {
    fn record(&mut self, bytes: &[u8], terminator: &[u8]) -> io::Result<()> {
        if self.buffer.len() + bytes.len() + terminator.len() > OUTPUT_BUFFER {
            self.flush()?;
        }
        self.buffer.extend_from_slice(bytes);
        self.buffer.extend_from_slice(terminator);
        Ok(())
    }
}

impl<W: Write> Effects for Output<W> {
    fn print(&mut self, path: &Path, nul: bool) -> io::Result<()> {
        self.record(path.as_os_str().as_bytes(), if nul { b"\0" } else { b"\n" })
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.record(bytes, b"")
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        writer.write_all(&self.buffer)?;
        self.buffer.clear();
        writer.flush()
    }

    fn command(&mut self, command: &mut Command) -> io::Result<bool> {
        self.flush()?;
        let mut output = OutputBuffer::default();
        let success = self.capture(command, &mut output)?;
        let shared = self.writer.clone();
        let mut writer = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        output.write_to(&mut *writer)?;
        writer.flush()?;
        Ok(success)
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
    use super::*;

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

    #[test]
    fn a_batch_child_does_not_hold_back_another_workers_output() {
        // Two executor workers share the real CLI writer. A final execdir
        // batch waits for the other worker's committed output before exiting;
        // capturing under the writer lock used to make that wait time out.
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        struct Sink(mpsc::Sender<Vec<u8>>);
        impl Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.send(bytes.to_vec()).map_err(io::Error::other)?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let directory =
            std::env::temp_dir().join(format!("ferret-r1-output-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let entry = directory.join("entry");
        std::fs::write(&entry, b"").unwrap();
        let started = directory.join("started");
        let release = directory.join("release");
        let args = vec![
            OsString::from("-I"),
            entry.clone().into_os_string(),
            "-maxdepth".into(),
            "0".into(),
            "-execdir".into(),
            "timeout".into(),
            "8".into(),
            "sh".into(),
            "-c".into(),
            ": > \"$1\"; while ! test -e \"$2\"; do sleep 0.01; done; head -c 70000 /dev/zero"
                .into(),
            "sh".into(),
            started.clone().into_os_string(),
            release.clone().into_os_string(),
            "{}".into(),
            "+".into(),
        ];
        let plan = Plan::parse(&args).unwrap();
        let (sent, received) = mpsc::channel();
        let effects = Output {
            writer: Arc::new(Mutex::new(BufWriter::new(Sink(sent)))),
            buffer: Vec::new(),
        };
        let slow = effects.clone();
        let batch =
            std::thread::spawn(move || plan.run_parallel(plan.live_source(), slow, 1).unwrap());
        let deadline = Instant::now() + Duration::from_secs(3);
        while !started.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let child_started = started.exists();
        let fast = effects.clone();
        let fast_plan = Plan::parse(&[
            OsString::from("-I"),
            entry.into_os_string(),
            "-maxdepth".into(),
            "0".into(),
            "-printf".into(),
            "fast\n".into(),
        ])
        .unwrap();
        let other = std::thread::spawn(move || {
            fast_plan
                .run_parallel(fast_plan.live_source(), fast, 1)
                .unwrap()
        });
        let first = received.recv_timeout(Duration::from_secs(2));
        // Release and await both children before asserting, including failures.
        std::fs::write(&release, b"").unwrap();
        let batch = batch.join().unwrap();
        let other = other.join().unwrap();
        let mut bytes = Vec::new();
        if let Ok(first) = &first {
            bytes.extend_from_slice(first);
        }
        bytes.extend(received.try_iter().flatten());
        std::fs::remove_dir_all(directory).unwrap();
        assert!(child_started, "batch child did not start within 3 seconds");
        assert_eq!(
            first.unwrap(),
            b"fast\n",
            "another worker's output was held back"
        );
        assert_eq!(batch.errors + other.errors, 0);
        assert_eq!(bytes.len(), 5 + 70000);
        assert!(bytes[5..].iter().all(|byte| *byte == 0));
    }
}
