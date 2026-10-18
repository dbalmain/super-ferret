//! Grep's stdout destination. Publication flushes each complete group so
//! diagnostics and subsequent operands cannot overtake buffered output.
//! NUL name modes retain std's original line buffer and diagnostic ordering.

use std::fs::File;
use std::io::{self, BufWriter, IsTerminal, Stdout, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd};

const BLOCK: usize = 64 << 10;

/// Linux's `EBADF`, which std's stdout reports as a successful write.
const EBADF: i32 = 9;

pub(super) struct Writer<'a> {
    line: &'a Stdout,
    block: Option<BufWriter<Raw>>,
}

impl<'a> Writer<'a> {
    pub(super) fn new(stdout: &'a Stdout, block: bool) -> Self {
        let mut line = stdout.lock();
        // Anything std still holds must go first. If it cannot, keep std's
        // destination, which will report the fault on the next write.
        let block = (block && !stdout.is_terminal() && line.flush().is_ok())
            .then(|| BufWriter::with_capacity(BLOCK, Raw::stdout(stdout)));
        drop(line);
        Self {
            line: stdout,
            block,
        }
    }
}

impl Write for Writer<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match &mut self.block {
            Some(writer) => writer.write(bytes),
            None => self.line.lock().write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.block {
            Some(writer) => writer.flush(),
            None => self.line.lock().flush(),
        }
    }
}

impl Drop for Writer<'_> {
    fn drop(&mut self) {
        // All success paths flush explicitly. After an output fault, retrying
        // from BufWriter::drop could publish bytes after the error diagnostic.
        if let Some(writer) = self.block.take() {
            let _ = writer.into_parts();
        }
    }
}

/// Descriptor 1 itself, written while grep holds its publication mutex. Unlike
/// std's stdout it has no line buffer to search each block for its last LF, and
/// unlike a duplicate it takes no descriptor an input may need.
struct Raw(ManuallyDrop<File>);

impl Raw {
    fn stdout(stdout: &Stdout) -> Self {
        let fd = stdout.as_raw_fd();
        // D11: std offers no safe way to write to descriptor 1 without a
        // second descriptor or its line writer, which cost 9% (0.961 s against
        // 0.880 s) on indexed `grep -F -a the` at 32 threads.
        // SAFETY: std never closes descriptor 1, and `ManuallyDrop` keeps
        // this `File` from closing it either, so it is borrowed, not owned.
        // The grep host is its sole stdout writer; workers serialize all
        // publication through that host and never replace descriptor 1.
        #[allow(unsafe_code)]
        let file = unsafe { File::from_raw_fd(fd) };
        Self(ManuallyDrop::new(file))
    }
}

impl Write for Raw {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self.0.write(bytes) {
            Err(error) if error.raw_os_error() == Some(EBADF) => Ok(bytes.len()),
            result => result,
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
