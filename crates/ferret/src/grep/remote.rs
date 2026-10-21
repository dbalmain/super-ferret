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
        },
    )
}

pub(crate) fn serve(
    request: &Request,
    pin: Option<&engine::QuerySession>,
    destination: &Destination,
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
    let runtime = Runtime {
        cancelled: destination.cancellation().unwrap_or(&local_cancelled),
        stderr: &stderr,
        tty: request.grep.as_ref().is_some_and(|options| options.tty),
        warned: AtomicBool::new(false),
        // Retained-directory caches and RLIMIT_NOFILE belong to the process.
        // Force admission even when one request's workers would fit alone.
        descriptors: Some({
            static BUDGET: OnceLock<Arc<line_grep::IoBudget>> = OnceLock::new();
            BUDGET
                .get_or_init(|| Arc::new(line_grep::IoBudget::new(usize::MAX)))
                .clone()
        }),
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
    let mut plan = match parsed {
        Ok(line_grep::Command::Search(plan)) => plan,
        Ok(_) => return Ok(runtime.failure("socket grep requires a content search")),
        Err(error) => return Ok(runtime.failure(&error.to_string())),
    };
    plan.presentation.color = if options.color {
        line_grep::Color::Always
    } else {
        line_grep::Color::Never
    };
    plan.presentation.heading = Some(options.heading);
    plan.line_number = options.numbers;
    let paths = if plan.paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        plan.paths.clone()
    };
    let inputs = resolve_inputs_at(&paths, false, &cwd);
    if plan.traversal.live
        || plan.traversal.unrestricted
        || inputs.iter().any(|input| matches!(input, Input::Stdin))
    {
        return Ok(runtime.failure("socket grep requires indexed directory operands without stdin"));
    }
    let Some(pin) = pin else {
        return Ok(runtime.failure("no index available"));
    };
    let mut selection = GrepSelection::default().with_hidden(plan.hidden);
    for glob in &plan.globs {
        if let Err(error) = selection.push(glob) {
            return Ok(runtime.failure(&format!("glob {glob:?}: {error}")));
        }
    }
    if plan.max_count == Some(0) {
        return Ok(Exit::NoMatch);
    }
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
    let execution = Execution {
        plan: &plan,
        selection: &selection,
        pin: Some(pin),
        live: None,
        cwd: &cwd,
        threads: options.threads,
        runtime: &runtime,
        start,
    };
    // Match the local block/line buffering, including pending NUL names before
    // diagnostics and the explicit operand/group flush points in the host.
    if block_stdout(&plan) && !options.tty {
        let mut writer = BufWriter::with_capacity(64 << 10, writer);
        let exit = execution.run(inputs, &mut writer);
        // Never retry a failed publication from BufWriter::drop.
        let _ = writer.into_parts();
        Ok(exit)
    } else {
        Ok(execution.run(inputs, LineWriter::new(writer)))
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
