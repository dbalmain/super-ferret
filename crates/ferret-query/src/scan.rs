//! Shared bounded streaming for document search and grep line output. A run
//! ends just before its last LF; its trailing partial line stays in the buffer.

use std::io::{self, Read};
use std::ops::ControlFlow;

pub(crate) const BUFFER: usize = 64 << 10;

/// Visits complete runs without LF at the end. `terminated` distinguishes an
/// empty line from empty input. `len` is the bracketed size, or `u64::MAX` for
/// an unbounded direct stream. Allocation is fallible, cancellation is checked
/// between reads, and a consumer can stop without reading the rest of a file.
pub(crate) fn runs(
    source: &mut dyn Read,
    len: u64,
    buffer: &mut Vec<u8>,
    stopped: &dyn Fn() -> bool,
    mut each: impl FnMut(&[u8], usize, bool) -> io::Result<ControlFlow<()>>,
) -> io::Result<()> {
    if buffer.len() < BUFFER {
        buffer
            .try_reserve_exact(BUFFER - buffer.len())
            .map_err(io::Error::other)?;
        buffer.resize(BUFFER, 0);
    }
    let (mut filled, mut base) = (0, 0);
    loop {
        if stopped() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let left = len.saturating_sub(base as u64 + filled as u64);
        let fresh = filled;
        let n = if left == 0 {
            0
        } else {
            if filled == buffer.len() {
                let grown = buffer
                    .len()
                    .saturating_mul(2)
                    .min(filled.saturating_add(usize::try_from(left).unwrap_or(usize::MAX)));
                buffer
                    .try_reserve_exact(grown - buffer.len())
                    .map_err(io::Error::other)?;
                buffer.resize(grown, 0);
            }
            match source.read(&mut buffer[filled..]) {
                Ok(n) => n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        };
        filled += n;
        let end = if n == 0 {
            filled
        } else if let Some(end) = memchr::memrchr(b'\n', &buffer[fresh..filled]) {
            fresh + end
        } else {
            continue;
        };
        if each(&buffer[..end], base, n != 0)?.is_break() || n == 0 {
            return Ok(());
        }
        buffer.copy_within(end + 1..filled, 0);
        filled -= end + 1;
        base += end + 1;
    }
}
