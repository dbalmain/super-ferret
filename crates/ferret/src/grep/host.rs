//! The grep host: where results, diagnostics and exit status meet stdout.

use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

pub(super) struct Host<'a, W> {
    pub(super) plan: &'a Plan,
    pub(super) runtime: Option<&'a Runtime<'a>>,
    pub(super) filename: bool,
    pub(super) writer: W,
    pub(super) buffer: Vec<u8>,
    pub(super) matched: bool,
    pub(super) emitted_group: bool,
    /// Some group's output has been flushed. A deferring daemon host still
    /// flushes its first one, so a sparse or slow grep shows its first match
    /// before the block buffer fills.
    pub(super) sent_output: bool,
    pub(super) json_stats: json::Stats,
    pub(super) errors: bool,
    pub(super) cancelled: Option<&'a AtomicBool>,
}

impl<W: Write> Host<'_, W> {
    /// Reports each failed operand, returning the others.
    pub(super) fn report_failed(
        &mut self,
        inputs: Vec<Input>,
        runtime: &Runtime,
    ) -> Result<Vec<Input>, Exit> {
        let mut resolved = Vec::new();
        for input in inputs {
            if let Input::Failed(path, error) = input {
                self.error(&path, &error)
                    .map_err(|error| runtime.output_failure(&error))?;
            } else {
                resolved.push(input);
            }
        }
        Ok(resolved)
    }

    /// The search's exit status, once its output is flushed.
    pub(super) fn exit(&self, searched: bool, runtime: &Runtime) -> Exit {
        if self.plan.quiet && self.matched {
            Exit::Ok
        } else if self.plan.paths.is_empty() && !searched {
            let _ = (runtime.stderr)(
                "grep: no files were searched; a glob, type, depth, hidden or ignore filter probably \
         excluded every file under the current directory",
            );
            Exit::Usage
        } else if self.errors {
            Exit::Usage
        } else if self.matched {
            Exit::Ok
        } else {
            Exit::NoMatch
        }
    }
}

/// Live policy loading warns on stderr from search workers, which no group
/// flush orders against output, so live mode keeps std's line buffer, as
/// do NUL name modes.
pub(super) fn block_stdout(plan: &Plan) -> bool {
    !nul_names(plan) && !plan.traversal.live
}

/// Redirected path/count records may share the block buffer until an operand
/// or diagnostic boundary. Ttys, NUL names and live policy keep line output.
pub(super) fn buffered_path_output(plan: &Plan, tty: bool) -> bool {
    !tty && !plan.traversal.live
        && !nul_names(plan)
        && !plan.json
        && matches!(
            plan.output,
            Output::Names | Output::WithoutMatch | Output::Count | Output::CountMatches
        )
}

fn nul_names(plan: &Plan) -> bool {
    plan.presentation.null
        && matches!(plan.output, Output::Names | Output::WithoutMatch)
        && !plan.json
}

pub(super) fn report_shortfall(
    runtime: Option<&Runtime<'_>>,
    shortfall: &ferret_query::Shortfall,
) -> io::Result<()> {
    if runtime.is_none_or(|runtime| !runtime.warned.swap(true, Ordering::AcqRel)) {
        let message = format!(
            "grep: searching with {} of {} threads: {}",
            shortfall.running, shortfall.asked, shortfall.cause
        );
        if let Some(runtime) = runtime {
            (runtime.stderr)(&message)?;
        } else {
            cli::error(&message);
        }
    }
    Ok(())
}

impl<W: Write> Host<'_, W> {
    pub(super) fn tty(&self) -> bool {
        self.runtime
            .map_or_else(stdout_is_terminal, |runtime| runtime.tty)
    }
    fn diagnostic(&self, message: &str) -> io::Result<()> {
        if let Some(runtime) = self.runtime {
            (runtime.stderr)(message)
        } else {
            cli::error(message);
            Ok(())
        }
    }
    pub(super) fn report_shortfall(&self, shortfall: &ferret_query::Shortfall) -> io::Result<()> {
        report_shortfall(self.runtime, shortfall)
    }

    pub(super) fn error(&mut self, path: &Path, error: &dyn std::fmt::Display) -> io::Result<()> {
        if !nul_names(self.plan) {
            self.writer.flush()?;
        }
        self.diagnostic(&format!("grep: {}: {error}", path.display()))?;
        self.errors = true;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn file(&mut self, path: &Path, recursive: bool) -> io::Result<()> {
        let flag = self.cancelled;
        let stopped = || flag.is_some_and(|flag| flag.load(Ordering::Acquire));
        let plan = self.plan;
        // A walked file is opened beneath the directory it was listed in.
        let directory = recursive.then(|| {
            crate::engine::reader::Parent::Open(ferret_query::find::DirectoryHandle::from_file(
                std::sync::Arc::new(fs::File::open(path.parent().unwrap()).unwrap()),
            ))
        });
        self.transaction(path, &mut |each| {
            groups::read_file(
                Path::new(""),
                path,
                directory.as_ref(),
                plan,
                each,
                &stopped,
            )
        })
    }

    pub(super) fn direct(
        &mut self,
        path: &Path,
        input: &mut dyn Read,
        size: u64,
    ) -> io::Result<()> {
        let flag = self.cancelled;
        let stopped = || flag.is_some_and(|flag| flag.load(Ordering::Acquire));
        self.transaction(path, &mut |each| {
            groups::stream(input, size, each, &stopped)
        })
    }

    #[cfg(test)]
    pub(super) fn checked(
        &mut self,
        path: &Path,
        name: NameId,
        reader: &mut CheckedReader<'_>,
    ) -> io::Result<()> {
        let flag = self.cancelled;
        let stopped = || flag.is_some_and(|flag| flag.load(Ordering::Acquire));
        self.transaction(path, &mut |each| {
            reader(name, ReadRequest::Stream(each, &stopped))
        })
    }

    fn transaction(
        &mut self,
        path: &Path,
        read: &mut dyn FnMut(ferret_query::Consumer<'_>) -> ReadOutcome,
    ) -> io::Result<()> {
        let flag = self.cancelled;
        let stopped = || flag.is_some_and(|flag| flag.load(Ordering::Acquire));
        let render = groups::Render {
            capture: None,
            plan: self.plan,
            prefix: (self.filename || self.plan.json)
                .then_some(label(path, self.plan.paths.is_empty())),
            stopped: &stopped,
            // Stdin may be slow and stdout a terminal.
            batched: false,
        };
        let mut buffer = std::mem::take(&mut self.buffer);
        let mut stream = DirectStream {
            host: self,
            path,
            printed: None,
        };
        let observed = groups::observe_streaming(&render, &mut buffer, read, false, &mut stream);
        let printed = stream.printed;
        self.buffer = buffer;
        match observed? {
            groups::Observed::Ready(group) => self.commit(path, group.result, group.output),
            groups::Observed::Failed(error) => {
                self.error(path, &error)?;
                Ok(())
            }
            groups::Observed::Spool(error) => {
                self.error(path, &error)?;
                Ok(())
            }
            groups::Observed::Skipped => Ok(()),
            groups::Observed::Streamed(result, error, _capture) => {
                self.finish_stream(path, result, printed.unwrap_or(0), error)
            }
        }
    }

    pub(super) fn start_stream(&mut self, path: &Path) -> io::Result<u64> {
        if self.plan.json {
            let mut begin = Vec::new();
            json::begin(&mut begin, label(path, self.plan.paths.is_empty()))?;
            self.writer.write_all(&begin)?;
            return Ok(begin.len() as u64);
        }
        if self.emitted_group {
            if self.plan.presentation.heading == Some(true) {
                self.writer.write_all(b"\n")?;
            } else if self.plan.before > 0 || self.plan.after > 0 {
                self.writer.write_all(b"--\n")?;
            }
        }
        self.emitted_group = true;
        Ok(0)
    }

    pub(super) fn finish_stream(
        &mut self,
        path: &Path,
        result: InputResult,
        printed: u64,
        error: Option<String>,
    ) -> io::Result<()> {
        self.matched |= result.matched;
        if self.plan.json {
            json::end(
                &mut self.writer,
                label(path, self.plan.paths.is_empty()),
                result,
                printed,
            )?;
            self.json_stats.add(result, printed);
        } else if result.binary && result.matched {
            self.binary_warning(path)?;
        }
        self.writer.flush()?;
        if let Some(error) = error {
            self.error(path, &error)?;
        }
        Ok(())
    }

    fn binary_warning(&mut self, path: &Path) -> io::Result<()> {
        self.writer.write_all(if path == Path::new("-") {
            b"(standard input)"
        } else {
            label(path, self.plan.paths.is_empty())
        })?;
        self.writer.write_all(b": binary file matches\n")?;
        self.emitted_group = true;
        Ok(())
    }

    pub(super) fn commit(
        &mut self,
        path: &Path,
        result: InputResult,
        output: OutputBuffer,
    ) -> io::Result<()> {
        #[cfg(test)]
        seam::COMMIT.with_borrow_mut(|hook| {
            if let Some(hook) = hook {
                hook(&output);
            }
        });
        let has_output = !output.is_empty();
        let published = self.publish(path, result, output);
        // NUL names keep their line buffer. Redirected path and count
        // listings share the block buffer until an operand boundary.
        if nul_names(self.plan) || buffered_path_output(self.plan, self.tty()) {
            return published;
        }
        // Redirected daemon content shares that buffer, so a group is not
        // its own socket frame; only the first group with output goes out
        // alone, like rg's block buffer once it has something to show. A
        // failed publish still flushes: the accepted prefix has to leave the
        // buffer before the error returns.
        // A binary notice is written by `publish` itself, with an empty
        // buffer, and marks the group emitted.
        let wrote = has_output || self.emitted_group;
        let defer = self.runtime.is_some_and(|runtime| runtime.defer_stdout);
        if published.is_err() || !defer || (wrote && !self.sent_output) {
            self.writer.flush()?;
            self.sent_output |= wrote;
        }
        published
    }

    pub(super) fn publish(
        &mut self,
        path: &Path,
        result: InputResult,
        mut output: OutputBuffer,
    ) -> io::Result<()> {
        self.matched |= result.matched;
        if self.plan.json {
            return self.publish_json(path, result, output);
        }
        if self.plan.quiet {
            return Ok(());
        }
        if matches!(self.plan.output, Output::Names | Output::WithoutMatch) {
            if result.matched {
                self.path_label(path, b"\n")?;
            }
            return Ok(());
        }
        // M4 R1: counts stay numeric after a binary sentinel, as in rg.
        if matches!(self.plan.output, Output::Count | Output::CountMatches) {
            if result.count > 0 || (self.plan.include_zero && result.searched) {
                if self.filename {
                    self.path_label(path, b":")?;
                }
                writeln!(self.writer, "{}", result.count)?;
            }
        } else if result.binary {
            if result.matched {
                self.binary_warning(path)?;
            }
        } else {
            self.publish_group(&mut output)?;
        }
        Ok(())
    }

    /// Writes one file's JSON events and counts its stats.
    fn publish_json(
        &mut self,
        path: &Path,
        result: InputResult,
        mut output: OutputBuffer,
    ) -> io::Result<()> {
        let mut printed = 0;
        if !self.plan.quiet && !output.is_empty() {
            let path = label(path, self.plan.paths.is_empty());
            let mut begin = Vec::new();
            json::begin(&mut begin, path)?;
            printed = begin.len() as u64 + output.len();
            self.writer.write_all(&begin)?;
            let flag = self.cancelled;
            groups::emit(&mut output, &mut self.writer, &|| {
                flag.is_some_and(|flag| flag.load(Ordering::Acquire))
            })?;
            json::end(&mut self.writer, path, result, printed)?;
        }
        self.json_stats.add(result, printed);
        Ok(())
    }

    /// Writes `path`'s label and then `separator`, or NUL under `--null`.
    fn path_label(&mut self, path: &Path, separator: &[u8]) -> io::Result<()> {
        presentation::path(
            &self.plan.presentation,
            &mut self.writer,
            label(path, self.plan.paths.is_empty()),
        )?;
        self.writer.write_all(if self.plan.presentation.null {
            b"\0"
        } else {
            separator
        })
    }

    /// Writes one file's match groups, separated from the previous file's.
    fn publish_group(&mut self, output: &mut OutputBuffer) -> io::Result<()> {
        if self.emitted_group && !output.is_empty() {
            if self.plan.presentation.heading == Some(true) {
                self.writer.write_all(b"\n")?;
            } else if self.plan.before > 0 || self.plan.after > 0 {
                self.writer.write_all(b"--\n")?;
            }
        }
        self.emitted_group |= !output.is_empty();
        let flag = self.cancelled;
        groups::emit(output, &mut self.writer, &|| {
            flag.is_some_and(|flag| flag.load(Ordering::Acquire))
        })
    }
}

fn stdout_is_terminal() -> bool {
    static IS_TERMINAL: OnceLock<bool> = OnceLock::new();
    *IS_TERMINAL.get_or_init(|| io::stdout().is_terminal())
}

/// Stdin and checked single-thread test hosts need no channel handoff.
struct DirectStream<'h, 'p, W> {
    host: &'h mut Host<'p, W>,
    path: &'h Path,
    printed: Option<u64>,
}
impl<W: Write> Write for DirectStream<'_, '_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.printed.is_none() {
            self.printed = Some(self.host.start_stream(self.path)?);
        }
        if self
            .host
            .cancelled
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        let count = self
            .host
            .writer
            .write(&bytes[..bytes.len().min(64 << 10)])?;
        if let Some(printed) = &mut self.printed {
            *printed += count as u64;
        }
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.host.writer.flush()
    }
}

/// Called for every listed name, so it compares bytes: `Path` equality
/// parses components. `resolve_each` spells every stdin operand `-`; a
/// walked file of that name is `./-`.
pub(super) fn label(path: &Path, implicit: bool) -> &[u8] {
    if path.as_os_str() == "-" {
        b"<stdin>"
    } else {
        let bytes = path.as_os_str().as_bytes();
        if implicit {
            bytes.strip_prefix(b"./").unwrap_or(bytes)
        } else {
            bytes
        }
    }
}
