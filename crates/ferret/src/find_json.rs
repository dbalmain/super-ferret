//! JSON find effects for the batch host. Stdout uses the
//! evaluator's transaction gate; stderr streams independently during capture.

use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::{Command, Stdio};
#[cfg(debug_assertions)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ferret_query::find::{Effects, OutputBuffer, Plan, WalkError, mark_output_failure};

use crate::json::Object;
use crate::protocol::{ChildStdin, Request};
use crate::transport::Destination;

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
    if plan.runs_commands() && request.child_stdin.is_none() {
        return Some(Refusal::ChildStdinRequired);
    }
    None
}

#[derive(Clone)]
pub(crate) struct FrameOutput<'a> {
    id: &'a str,
    destination: Destination,
    record: Arc<AtomicU64>,
    stdin: ChildStdin,
    transport_error: Arc<Mutex<Option<io::Error>>>,
    warning_stderr: bool,
    #[cfg(debug_assertions)]
    panic: Arc<AtomicBool>,
}
impl Effects for FrameOutput<'_> {
    fn cancelled(&self) -> bool {
        self.cancelled()
    }
    fn print(&mut self, path: &Path, nul: bool) -> io::Result<()> {
        let mut bytes = path.as_os_str().as_bytes().to_vec();
        bytes.push(if nul { 0 } else { b'\n' });
        self.emit("stdout", &bytes)
    }
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.emit("stdout", bytes)
    }
    fn error(&mut self, error: &WalkError) {
        let result = emit_to(&self.destination, self.id, "diagnostic", |o| {
            o.str("code", "walk")
                .str("severity", "error")
                .bytes("path", error.path.as_os_str().as_bytes())
                .str("message", &error.error.to_string());
        });
        let _ = self.track(result);
    }
    fn warning(&mut self, message: &str) {
        let result = emit_to(&self.destination, self.id, "diagnostic", |o| {
            o.str("code", "warning")
                .str("severity", "warning")
                .str("message", message);
        });
        let _ = self.track(result);
        if self.warning_stderr {
            crate::cli::error(&format!("find: {message}"));
        }
    }
    fn output(&mut self, buffer: &mut OutputBuffer) -> io::Result<()> {
        let record = self.record.fetch_add(1, Ordering::Relaxed) + 1;
        let mut writer = FrameWriter {
            host: self,
            raw: RawStdout::new(record),
        };
        buffer.write_to(&mut writer)?;
        writer.finish()
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
            destination: Destination::Stdout,
            stdin,
            record: Arc::new(AtomicU64::new(0)),
            transport_error: Arc::new(Mutex::new(None)),
            warning_stderr: false,
            #[cfg(debug_assertions)]
            panic: Arc::new(AtomicBool::new(false)),
        }
    }
    pub(crate) fn with_destination(mut self, destination: Destination) -> Self {
        self.destination = destination;
        self
    }
    pub(crate) fn with_warning_stderr(mut self) -> Self {
        self.warning_stderr = true;
        self
    }
    #[cfg(debug_assertions)]
    pub(crate) fn with_panic(self, enabled: bool) -> Self {
        self.panic.store(enabled, Ordering::Release);
        self
    }
    fn cancelled(&self) -> bool {
        #[cfg(debug_assertions)]
        if self.record.load(Ordering::Acquire) > 0 && self.panic.swap(false, Ordering::AcqRel) {
            panic!("injected find worker panic");
        }
        self.destination.cancelled()
            || self
                .transport_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some()
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
        self.track(emit_parts(&self.destination, self.id, event, record, bytes))
    }
}

/// Find's stdout chunking. Bytes fill `PART` and each full part is one raw
/// frame; the tail is its own frame. Search uses the same buffer, so a plain
/// daemon search writes many `path\n` lines into those frames.
pub(crate) struct RawStdout {
    record: u64,
    part: u64,
    pending: Vec<u8>,
}

impl RawStdout {
    pub(crate) fn new(record: u64) -> Self {
        Self {
            record,
            part: 0,
            pending: Vec::with_capacity(PART),
        }
    }

    pub(crate) fn write(
        &mut self,
        destination: &Destination,
        id: &str,
        mut bytes: &[u8],
    ) -> io::Result<()> {
        while !bytes.is_empty() {
            if self.pending.len() == PART {
                frame(
                    destination,
                    id,
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
        Ok(())
    }

    /// Sends what is pending as a non-final part, so a caller can put its
    /// first bytes on the wire before a part fills.
    pub(crate) fn send_pending(&mut self, destination: &Destination, id: &str) -> io::Result<()> {
        if !self.pending.is_empty() {
            frame(
                destination,
                id,
                "stdout",
                self.record,
                self.part,
                &self.pending,
                false,
            )?;
            self.part += 1;
            self.pending.clear();
        }
        Ok(())
    }

    /// Ends the record. A record with parts already sent always gets a
    /// `last` part, empty if nothing is pending.
    pub(crate) fn finish(&mut self, destination: &Destination, id: &str) -> io::Result<()> {
        if !self.pending.is_empty() || self.part > 0 {
            frame(
                destination,
                id,
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

struct FrameWriter<'host, 'id> {
    host: &'host FrameOutput<'id>,
    raw: RawStdout,
}
impl Write for FrameWriter<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let total = bytes.len();
        self.host
            .track(self.raw.write(&self.host.destination, self.host.id, bytes))?;
        Ok(total)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.finish()
    }
}
impl FrameWriter<'_, '_> {
    fn finish(&mut self) -> io::Result<()> {
        self.host
            .track(self.raw.finish(&self.host.destination, self.host.id))
    }
}
pub(crate) fn emit_parts(
    destination: &Destination,
    id: &str,
    event: &str,
    record: u64,
    bytes: &[u8],
) -> io::Result<()> {
    let chunks: Vec<_> = bytes.chunks(PART).collect();
    for (part, chunk) in chunks.iter().enumerate() {
        frame(
            destination,
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
    destination: &Destination,
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
        .bool("last", last);
    if destination.raw_output() {
        object.int("length", bytes.len() as u64);
        object.end();
        destination.send_payload(&line, bytes)
    } else {
        object.str("bytes_base64", &base64(bytes));
        object.end();
        destination.send(&line)
    }
}

fn base64(bytes: &[u8]) -> String {
    let mut out = Vec::new();
    crate::json::encode_base64(&mut out, bytes);
    String::from_utf8(out).unwrap_or_default()
}
/// Writes one tagged event line: `{"id":ID,"event":NAME,...fill...}`. Shared
/// by batch's per-request begin/end framing and the CLI's `--json find`
/// host, so both encode the same event shape through one writer.
pub(crate) fn emit(id: &str, name: &str, fill: impl FnOnce(&mut Object<'_>)) -> io::Result<()> {
    emit_to(&Destination::Stdout, id, name, fill)
}

pub(crate) fn emit_to(
    destination: &Destination,
    id: &str,
    name: &str,
    fill: impl FnOnce(&mut Object<'_>),
) -> io::Result<()> {
    let mut line = Vec::new();
    let mut object = Object::new(&mut line);
    object.str("id", id).str("event", name);
    fill(&mut object);
    object.end();
    destination.send(&line)
}

/// Writes a `"generation"` field: an object for a pinned engine, else null.
/// Shared between batch's `begin`/`status`/`reload` events and the CLI's
/// `--json find` host.
pub(crate) fn generation(object: &mut Object<'_>, value: Option<ferret_catalog::Generation>) {
    if let Some(value) = value {
        object.object("generation", |o| {
            o.str("incarnation", &hex(&value.incarnation))
                .int("checkpoint", value.checkpoint)
                .int("sequence", value.sequence);
        });
    } else {
        object.null("generation");
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn diagnostic(
    id: &str,
    code: &str,
    severity: &str,
    path: Option<&[u8]>,
) -> io::Result<()> {
    diagnostic_to(&Destination::Stdout, id, code, severity, path)
}

pub(crate) fn diagnostic_to(
    destination: &Destination,
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
    destination.send(&line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    // Raw payloads may contain NUL, invalid UTF-8 and complete fake envelopes.
    // Neither frame batching nor the next header may interpret those bytes.
    #[test]
    fn socket_frames_preserve_parts_and_arbitrary_bytes_with_both_codecs() -> io::Result<()> {
        let payload = b"\xff\0\n{\"event\":\"end\"}\n".repeat(4000);
        for raw in [false, true] {
            let (writer, reader) = UnixStream::pair()?;
            reader.set_read_timeout(Some(Duration::from_secs(5)))?;
            let destination =
                Destination::socket(writer, Arc::new(AtomicBool::new(false)))?.with_raw_output(raw);
            std::thread::scope(|scope| -> io::Result<()> {
                let send = scope.spawn(|| -> io::Result<()> {
                    emit_parts(&destination, "g", "stdout", 7, &payload)?;
                    emit_parts(&destination, "g", "stderr", 8, b"diagnostic\n")?;
                    emit_to(&destination, "g", "end", |o| {
                        o.int("exit", 2);
                    })
                });
                let mut reader = BufReader::new(reader);
                let mut received = Vec::new();
                for (part, chunk) in payload.chunks(PART).enumerate() {
                    let mut line = Vec::new();
                    reader.read_until(b'\n', &mut line)?;
                    let value = crate::protocol::parse_object(&line)
                        .ok_or_else(|| io::Error::other("header"))?;
                    assert_eq!(
                        value
                            .field("record")
                            .and_then(crate::protocol::Value::number),
                        Some(7)
                    );
                    assert_eq!(
                        value.field("part").and_then(crate::protocol::Value::number),
                        Some(part as u64)
                    );
                    let bytes = if raw {
                        assert_eq!(
                            value
                                .field("length")
                                .and_then(crate::protocol::Value::number),
                            Some(chunk.len() as u64)
                        );
                        let mut bytes = vec![0; chunk.len()];
                        reader.read_exact(&mut bytes)?;
                        bytes
                    } else {
                        let encoded = value
                            .field("bytes_base64")
                            .and_then(crate::protocol::Value::text)
                            .ok_or_else(|| io::Error::other("base64"))?;
                        crate::protocol::base64_decode(encoded)
                            .ok_or_else(|| io::Error::other("base64"))?
                    };
                    received.extend(bytes);
                }
                assert_eq!(received, payload);
                let mut line = String::new();
                reader.read_line(&mut line)?;
                assert!(line.contains("\"event\":\"stderr\""));
                if raw {
                    let mut bytes = [0; 11];
                    reader.read_exact(&mut bytes)?;
                    assert_eq!(&bytes, b"diagnostic\n");
                }
                line.clear();
                reader.read_line(&mut line)?;
                assert!(line.contains("\"exit\":2"));
                send.join()
                    .map_err(|_| io::Error::other("sender panicked"))??;
                Ok(())
            })?;
        }
        Ok(())
    }
}
