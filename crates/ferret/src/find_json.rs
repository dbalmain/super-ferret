//! JSON find effects for the batch host. Stdout uses the
//! evaluator's transaction gate; stderr streams independently during capture.

use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ferret_query::find::{Effects, OutputBuffer, Plan, WalkError, mark_output_failure};

use crate::json::Object;
use crate::protocol::{ChildStdin, Request};

const PART: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    ChildStdinOnProtocol,
    LocalEffectsRequired,
    InteractiveRequired,
    Noninteractive,
    ChildStdinRequired,
}

impl Refusal {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::ChildStdinOnProtocol => "ChildStdinOnProtocol",
            Self::LocalEffectsRequired => "LocalEffectsRequired",
            Self::InteractiveRequired => "InteractiveRequired",
            Self::Noninteractive => "Noninteractive",
            Self::ChildStdinRequired => "ChildStdinRequired",
        }
    }
}

pub(crate) fn refusal(plan: &Plan, request: &Request, protocol_stdin: bool) -> Option<Refusal> {
    if request.child_stdin == Some(ChildStdin::Inherit) && protocol_stdin {
        return Some(Refusal::ChildStdinOnProtocol);
    }
    if plan.has_side_effects()
        && !request
            .capabilities
            .iter()
            .any(|name| name == "local-effects")
    {
        return Some(Refusal::LocalEffectsRequired);
    }
    if plan.requires_interactive() {
        if !request
            .capabilities
            .iter()
            .any(|name| name == "interactive")
        {
            return Some(Refusal::InteractiveRequired);
        }
        if protocol_stdin || !io::stdin().is_terminal() {
            return Some(Refusal::Noninteractive);
        }
    }
    if plan.has_side_effects() && request.child_stdin.is_none() {
        return Some(Refusal::ChildStdinRequired);
    }
    None
}

#[derive(Clone)]
pub(crate) struct FrameOutput<'a> {
    id: &'a str,
    record: Arc<AtomicU64>,
    stdin: ChildStdin,
    transport_error: Arc<Mutex<Option<io::Error>>>,
}
impl Effects for FrameOutput<'_> {
    fn print(&mut self, path: &Path, nul: bool) -> io::Result<()> {
        let mut bytes = path.as_os_str().as_bytes().to_vec();
        bytes.push(if nul { 0 } else { b'\n' });
        self.emit("stdout", &bytes)
    }
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.emit("stdout", bytes)
    }
    fn error(&mut self, error: &WalkError) {
        let result = diagnostic(
            self.id,
            "walk",
            "error",
            Some(error.path.as_os_str().as_bytes()),
        );
        let _ = self.track(result);
    }
    fn output(&mut self, buffer: &mut OutputBuffer) -> io::Result<()> {
        let record = self.record.fetch_add(1, Ordering::Relaxed) + 1;
        let mut writer = FrameWriter {
            id: self.id,
            record,
            part: 0,
            pending: Vec::with_capacity(PART),
        };
        let result = buffer.write_to(&mut writer).and_then(|()| writer.finish());
        self.track(result)
    }
    fn capture(&mut self, command: &mut Command, output: &mut dyn Write) -> io::Result<bool> {
        let mut child = command
            .stdin(match self.stdin {
                ChildStdin::Null => Stdio::null(),
                ChildStdin::Inherit => Stdio::inherit(),
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stderr_host = self.clone();
        // Both pipes must drain concurrently: a child can fill stderr before
        // producing a single stdout byte. Join before wait/end, even on errors.
        let (stdout, stderr) = std::thread::scope(|scope| {
            let stderr = scope.spawn(|| -> io::Result<()> {
                if let Some(mut pipe) = child.stderr.take() {
                    let mut bytes = vec![0; PART];
                    loop {
                        let count = match pipe.read(&mut bytes) {
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                            result => result?,
                        };
                        if count == 0 {
                            break;
                        }
                        stderr_host.emit("stderr", &bytes[..count])?;
                    }
                }
                Ok(())
            });
            let stdout = match child.stdout.take() {
                Some(mut pipe) => io::copy(&mut pipe, output).map(|_| ()),
                None => Ok(()),
            };
            let stderr = stderr
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("stderr reader panicked")));
            (stdout, stderr)
        });
        let status = child.wait().map_err(mark_output_failure)?;
        stdout.map_err(mark_output_failure)?;
        stderr.map_err(mark_output_failure)?;
        Ok(status.success())
    }
    fn confirm(&mut self, program: &std::ffi::OsStr, path: &Path) -> io::Result<bool> {
        if !io::stdin().is_terminal() {
            return Err(io::Error::other("Noninteractive"));
        }
        static PROMPT: Mutex<()> = Mutex::new(());
        let _prompt = PROMPT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut bytes = b"< ".to_vec();
        bytes.extend_from_slice(program.as_bytes());
        bytes.extend_from_slice(b" ... ");
        bytes.extend_from_slice(path.as_os_str().as_bytes());
        bytes.extend_from_slice(b" > ? ");
        self.emit("stderr", &bytes)?;
        let mut line = Vec::new();
        io::stdin().lock().read_until(b'\n', &mut line)?;
        Ok(matches!(line.first(), Some(b'y' | b'Y')))
    }
}
impl FrameOutput<'_> {
    pub(crate) fn new(id: &'_ str, stdin: ChildStdin) -> FrameOutput<'_> {
        FrameOutput {
            id,
            stdin,
            record: Arc::new(AtomicU64::new(0)),
            transport_error: Arc::new(Mutex::new(None)),
        }
    }
    pub(crate) fn check_transport(&self) -> io::Result<()> {
        match self
            .transport_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    fn track(&self, result: io::Result<()>) -> io::Result<()> {
        if let Err(error) = &result {
            let mut saved = self
                .transport_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if saved.is_none() {
                *saved = Some(io::Error::new(error.kind(), error.to_string()));
            }
        }
        result
    }
    fn emit(&mut self, event: &str, bytes: &[u8]) -> io::Result<()> {
        let record = self.record.fetch_add(1, Ordering::Relaxed) + 1;
        self.track(emit_parts(self.id, event, record, bytes))
    }
}

struct FrameWriter<'a> {
    id: &'a str,
    record: u64,
    part: u64,
    pending: Vec<u8>,
}
impl Write for FrameWriter<'_> {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let total = bytes.len();
        while !bytes.is_empty() {
            if self.pending.len() == PART {
                frame(
                    self.id,
                    "stdout",
                    self.record,
                    self.part,
                    &self.pending,
                    false,
                )?;
                self.part += 1;
                self.pending.clear();
            }
            let take = (PART - self.pending.len()).min(bytes.len());
            self.pending.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
        }
        Ok(total)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.finish()
    }
}
impl FrameWriter<'_> {
    fn finish(&mut self) -> io::Result<()> {
        if !self.pending.is_empty() {
            frame(
                self.id,
                "stdout",
                self.record,
                self.part,
                &self.pending,
                true,
            )?;
            self.pending.clear();
        }
        Ok(())
    }
}
fn emit_parts(id: &str, event: &str, record: u64, bytes: &[u8]) -> io::Result<()> {
    let chunks: Vec<_> = bytes.chunks(PART).collect();
    for (part, chunk) in chunks.iter().enumerate() {
        frame(
            id,
            event,
            record,
            part as u64,
            chunk,
            part + 1 == chunks.len(),
        )?;
    }
    Ok(())
}

fn frame(
    id: &str,
    event: &str,
    record: u64,
    part: u64,
    bytes: &[u8],
    last: bool,
) -> io::Result<()> {
    let mut line = Vec::new();
    let mut object = Object::new(&mut line);
    object
        .str("id", id)
        .str("event", event)
        .int("record", record)
        .int("part", part)
        .bool("last", last)
        .str("bytes_base64", &base64(bytes));
    object.end();
    send(&line)
}

fn base64(bytes: &[u8]) -> String {
    let mut out = Vec::new();
    crate::json::encode_base64(&mut out, bytes);
    String::from_utf8(out).unwrap_or_default()
}
pub(crate) fn diagnostic(
    id: &str,
    code: &str,
    severity: &str,
    path: Option<&[u8]>,
) -> io::Result<()> {
    let mut line = Vec::new();
    let mut object = Object::new(&mut line);
    object
        .str("id", id)
        .str("event", "diagnostic")
        .str("code", code)
        .str("severity", severity);
    if let Some(path) = path {
        object.bytes("path", path);
    }
    object.end();
    send(&line)
}
fn send(line: &[u8]) -> io::Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(line)?;
    out.write_all(b"\n")?;
    out.flush()
}
