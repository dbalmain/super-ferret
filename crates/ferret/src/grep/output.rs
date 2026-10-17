//! Grep's stdout destination. Publication flushes each complete group so
//! diagnostics and subsequent operands cannot overtake buffered output.
//! NUL name modes retain std's original line buffer and diagnostic ordering.

use std::fs::File;
use std::io::{self, BufWriter, IsTerminal, Stdout, StdoutLock, Write};
use std::os::fd::AsFd;

const BLOCK: usize = 64 << 10;

pub(super) struct Writer<'a> {
    line: StdoutLock<'a>,
    block: Option<BufWriter<File>>,
}

impl<'a> Writer<'a> {
    pub(super) fn new(stdout: &'a Stdout, block: bool) -> Self {
        // Duplication preserves the shared file offset of `2>&1`. Reopening
        // /dev/stdout would instead give regular files an independent offset.
        // If no descriptor is available, retain std's existing destination.
        let block = (block && !stdout.is_terminal())
            .then(|| stdout.as_fd().try_clone_to_owned().ok())
            .flatten()
            .map(|fd| BufWriter::with_capacity(BLOCK, File::from(fd)));
        Self {
            line: stdout.lock(),
            block,
        }
    }
}

impl Write for Writer<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match &mut self.block {
            Some(writer) => writer.write(bytes),
            None => self.line.write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.block {
            Some(writer) => writer.flush(),
            None => self.line.flush(),
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
