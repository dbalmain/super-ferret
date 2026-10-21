//! Event destinations shared by batch, JSON find and the socket host. Encoding
//! stays in batch/find_json; destinations batch writes at event boundaries.

use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

pub(crate) const MAX_RAW_BYTES: usize = 64 * 1024;
const FLUSH_AT: usize = MAX_RAW_BYTES;

#[derive(Default)]
struct Buffer {
    pending: Vec<u8>,
    saw_data: bool,
}
impl Buffer {
    fn send(&mut self, line: &[u8], write: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
        self.send_payload(line, &[], write)
    }
    fn send_payload(
        &mut self,
        line: &[u8],
        payload: &[u8],
        mut write: impl FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        self.pending.extend_from_slice(line);
        self.pending.push(b'\n');
        self.pending.extend_from_slice(payload);
        let data = contains(line, b"\"event\":\"row\"") || contains(line, b"\"event\":\"stdout\"");
        let first_data = data && !self.saw_data;
        if data {
            self.saw_data = true;
        }
        let end = contains(line, b"\"event\":\"end\"");
        let special =
            contains(line, b"\"event\":\"diagnostic\"") || contains(line, b"\"event\":\"stderr\"");
        let boundary = !data || first_data || special || end;
        if self.pending.len() >= FLUSH_AT || boundary {
            write(&self.pending)?;
            self.pending.clear();
        }
        if end {
            self.saw_data = false;
        }
        Ok(())
    }
}
fn contains(line: &[u8], needle: &[u8]) -> bool {
    line.windows(needle.len()).any(|part| part == needle)
}

#[derive(Clone)]
pub(crate) enum Destination {
    Stdout,
    Socket {
        writer: Arc<Mutex<SocketWriter>>,
        cancelled: Arc<AtomicBool>,
        raw_output: bool,
    },
}
pub(crate) struct SocketWriter {
    stream: UnixStream,
    buffer: Buffer,
}
impl SocketWriter {
    fn send(&mut self, line: &[u8]) -> io::Result<()> {
        self.buffer.send(line, |bytes| self.stream.write_all(bytes))
    }
}

fn stdout_buffer() -> &'static Mutex<Buffer> {
    static BUFFER: OnceLock<Mutex<Buffer>> = OnceLock::new();
    BUFFER.get_or_init(|| Mutex::new(Buffer::default()))
}

impl Destination {
    pub(crate) fn socket(stream: UnixStream, cancelled: Arc<AtomicBool>) -> io::Result<Self> {
        Ok(Self::Socket {
            writer: Arc::new(Mutex::new(SocketWriter {
                stream: stream.try_clone()?,
                buffer: Buffer::default(),
            })),
            cancelled,
            raw_output: false,
        })
    }
    pub(crate) fn with_raw_output(mut self, enabled: bool) -> Self {
        if let Self::Socket { raw_output, .. } = &mut self {
            *raw_output = enabled;
        }
        self
    }
    pub(crate) fn raw_output(&self) -> bool {
        matches!(
            self,
            Self::Socket {
                raw_output: true,
                ..
            }
        )
    }
    /// Header and exact-length payload share the socket lock and batch. Only
    /// the JSON header determines flush boundaries; arbitrary payload bytes
    /// must never be inspected as envelopes or interleaved with another frame.
    pub(crate) fn send_payload(&self, header: &[u8], payload: &[u8]) -> io::Result<()> {
        let Self::Socket { writer, .. } = self else {
            return Err(io::Error::other("raw output requires a socket"));
        };
        let mut writer = writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let SocketWriter { stream, buffer } = &mut *writer;
        buffer.send_payload(header, payload, |bytes| stream.write_all(bytes))
    }
    pub(crate) fn cancellation(&self) -> Option<&AtomicBool> {
        match self {
            Self::Stdout => None,
            Self::Socket { cancelled, .. } => Some(cancelled),
        }
    }
    pub(crate) fn cancelled(&self) -> bool {
        match self {
            Self::Stdout => false,
            Self::Socket { cancelled, .. } => cancelled.load(Ordering::Acquire),
        }
    }
    /// Publishes an explicit host flush boundary even below the frame batch
    /// size.
    pub(crate) fn flush(&self) -> io::Result<()> {
        match self {
            Self::Stdout => Ok(()),
            Self::Socket { writer, .. } => {
                let mut writer = writer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let SocketWriter { stream, buffer } = &mut *writer;
                stream.write_all(&buffer.pending)?;
                buffer.pending.clear();
                Ok(())
            }
        }
    }
    pub(crate) fn send(&self, line: &[u8]) -> io::Result<()> {
        match self {
            Self::Stdout => stdout_buffer()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .send(line, |bytes| {
                    let mut stdout = io::stdout().lock();
                    stdout.write_all(bytes)?;
                    stdout.flush()
                }),
            Self::Socket { writer, .. } => writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .send(line),
        }
    }
}
