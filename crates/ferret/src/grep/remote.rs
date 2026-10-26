//! Socket adapter for the shared grep host. Environment and pattern-file
//! reads belong to the client; the server only parses supplied bytes.

use super::*;
use crate::protocol::{GrepOptions, Request};
use crate::transport::Destination;
use std::io::{BufWriter, LineWriter};
use std::os::unix::ffi::OsStringExt;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

pub(super) fn run(
    args: &[OsString],
    pattern_files: &[Vec<u8>],
    plan: &Plan,
    tty: bool,
    index: &Path,
) -> Option<Exit> {
    crate::daemon::grep(
        index,
        args,
        GrepOptions {
            color: plan.presentation.color.enabled(),
            heading: plan.presentation.heading.unwrap_or(false),
            numbers: plan.line_number,
            tty,
            threads: engine::search_threads_for(index),
            pattern_files: pattern_files.to_vec(),
            temp_dir: temp_dir()?,
        },
    )
}

pub(super) fn names(
    args: &[OsString],
    plan: &line_grep::Names,
    tty: bool,
    index: &Path,
) -> Option<Exit> {
    crate::daemon::grep(
        index,
        args,
        GrepOptions {
            color: plan.presentation.color.enabled(),
            heading: plan.presentation.heading.unwrap_or(false),
            numbers: false,
            tty,
            threads: 1,
            pattern_files: Vec::new(),
            temp_dir: temp_dir()?,
        },
    )
}

/// The temporary directory a local run would spill into, made absolute
/// against this process's cwd. `None` keeps the search in process.
fn temp_dir() -> Option<Vec<u8>> {
    std::path::absolute(std::env::temp_dir())
        .ok()
        .map(PathBuf::into_os_string)
        .map(OsString::into_vec)
}

pub(crate) fn serve(
    request: &Request,
    pin: Option<&engine::QuerySession>,
    destination: &Destination,
    requests: usize,
    granted: Option<usize>,
) -> io::Result<Exit> {
    let start = std::time::Instant::now();
    let record = AtomicU64::new(0);
    let emit = |event: &str, bytes: &[u8]| {
        crate::find_json::emit_parts(
            destination,
            &request.id,
            event,
            record.fetch_add(1, Ordering::Relaxed) + 1,
            bytes,
        )?;
        if request.grep.as_ref().is_some_and(|options| options.tty) {
            destination.flush()?;
        }
        Ok(())
    };
    let stderr = |message: &str| emit("stderr", format!("ferret: {message}\n").as_bytes());
    let local_cancelled = AtomicBool::new(false);
    let mut runtime = Runtime {
        cancelled: destination.cancellation().unwrap_or(&local_cancelled),
        stderr: &stderr,
        tty: request.grep.as_ref().is_some_and(|options| options.tty),
        warned: AtomicBool::new(false),
        descriptors: None,
        defer_stdout: false,
    };
    let Some(options) = &request.grep else {
        return Ok(runtime.failure("missing resolved grep environment"));
    };
    let Some(cwd) = request
        .cwd
        .as_ref()
        .map(|bytes| PathBuf::from(OsString::from_vec(bytes.clone())))
    else {
        return Ok(runtime.failure("missing grep cwd"));
    };
    if !cwd.is_absolute() {
        return Ok(runtime.failure("grep cwd must be absolute"));
    }
    let args: Vec<_> = request
        .args
        .iter()
        .cloned()
        .map(OsString::from_vec)
        .collect();
    let mut files = options.pattern_files.iter();
    let parsed = line_grep::parse_with(&args, &mut |_| {
        files
            .next()
            .cloned()
            .ok_or_else(|| io::Error::other("missing grep pattern-file bytes"))
    });
    let flush = || {
        crate::find_json::emit_to(destination, &request.id, "stdout", |o| {
            o.str("bytes", "").bool("flush", true);
        })?;
        destination.flush()
    };
    let writer = FrameWriter {
        emit: &emit,
        flush: &flush,
    };
    let mut plan = match parsed {
        Ok(line_grep::Command::Search(plan)) => *plan,
        Ok(line_grep::Command::Files(mut plan)) => {
            plan.presentation.color = if options.color {
                line_grep::Color::Always
            } else {
                line_grep::Color::Never
            };
            plan.presentation.heading = Some(options.heading);
            if plan.traversal.live || plan.traversal.unrestricted {
                return Ok(runtime.failure("socket grep requires indexed directory operands"));
            }
            let Some(pin) = pin else {
                return Ok(runtime.failure("no index available"));
            };
            let block = !plan.presentation.null;
            return Ok(with_writer(writer, block && !options.tty, |mut writer| {
                match super::names::pinned(&plan, pin.catalog(), &cwd, &mut writer, &runtime) {
                    Ok(exit) => exit,
                    Err(error) => runtime.failure(&error),
                }
            }));
        }
        Ok(_) => return Ok(runtime.failure("socket grep requires a search or file listing")),
        Err(error) => return Ok(runtime.failure(&error.to_string())),
    };
    plan.presentation.color = if options.color {
        line_grep::Color::Always
    } else {
        line_grep::Color::Never
    };
    plan.presentation.heading = Some(options.heading);
    plan.line_number = options.numbers;
    plan.spill_dir = Some(Arc::from(Path::new(std::ffi::OsStr::from_bytes(
        &options.temp_dir,
    ))));
    // Before operands, as in the local host: a refused combination or `-m0`
    // never depends on the operands or the pin.
    let selection = match super::preflight::prepare(&plan) {
        Ok(super::preflight::Prepared::Search(selection)) => selection,
        Ok(super::preflight::Prepared::NoMatch) => return Ok(Exit::NoMatch),
        Err(refusal) => return Ok(runtime.failure(&refusal.into_message())),
    };
    let paths = if plan.paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        plan.paths.clone()
    };
    let inputs = resolve_inputs_at(&paths, false, &cwd);
    if plan.traversal.live
        || plan.traversal.unrestricted
        || inputs.iter().any(|input| matches!(input, Input::Stream(_)))
    {
        return Ok(runtime.failure("socket grep requires indexed directory operands without stdin"));
    }
    let Some(pin) = pin else {
        return Ok(runtime.failure("no index available"));
    };
    // The daemon's shared pool grants `granted` before any pool is sized.
    // Client `options.threads` is only an upper bound, and so is the resident
    // search-thread cap. A batch host has no pool and passes `None`.
    let cap = options.threads.min(pin.search_threads());
    let threads = granted.unwrap_or(cap).min(cap).max(1);
    runtime.descriptors = Some(Arc::new(line_grep::IoBudget::for_request(
        threads, requests,
    )));
    // Same block buffer as in-process content, but a per-group flush would
    // emit one socket frame per file. `commit` still flushes a failed publish.
    runtime.defer_stdout = block_stdout(&plan) && !options.tty;
    let execution = Execution {
        plan: &plan,
        selection: &selection,
        pin: Some(pin),
        live: None,
        cwd: &cwd,
        threads,
        runtime: &runtime,
        start,
    };
    Ok(with_writer(
        writer,
        block_stdout(&plan) && !options.tty,
        |writer| execution.run(inputs, writer),
    ))
}

// Discard failed publication buffers instead of retrying from Drop. Both grep
// hosts retain their original block/line choices and explicit flush points.
fn with_writer(
    writer: FrameWriter<'_>,
    block: bool,
    execute: impl FnOnce(&mut dyn Write) -> Exit,
) -> Exit {
    if block {
        let mut writer = BufWriter::with_capacity(64 << 10, writer);
        let exit = execute(&mut writer);
        let _ = writer.into_parts();
        exit
    } else {
        execute(&mut LineWriter::new(writer))
    }
}

struct FrameWriter<'a> {
    emit: &'a dyn Fn(&str, &[u8]) -> io::Result<()>,
    flush: &'a dyn Fn() -> io::Result<()>,
}
impl Write for FrameWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        (self.emit)("stdout", bytes)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        (self.flush)()
    }
}
