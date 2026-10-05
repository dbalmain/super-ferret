//! Event destinations shared by batch, JSON find and the socket host. Encoding
//! stays in batch/find_json; the socket supplies only blocking backpressure.

use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub(crate) enum Destination {
    Stdout,
    Socket {
        writer: Arc<Mutex<UnixStream>>,
        cancelled: Arc<AtomicBool>,
    },
}

impl Destination {
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
    pub(crate) fn send(&self, line: &[u8]) -> io::Result<()> {
        fn write(out: &mut impl Write, line: &[u8]) -> io::Result<()> {
            out.write_all(line)?;
            out.write_all(b"\n")?;
            out.flush()
        }
        match self {
            Self::Stdout => write(&mut io::stdout().lock(), line),
            Self::Socket { writer, .. } => write(
                &mut *writer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                line,
            ),
        }
    }
}
