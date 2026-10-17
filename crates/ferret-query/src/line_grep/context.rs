//! A rolling before-context window over find's spill buffer. Record headers
//! live in the same spool as bytes, so large line counts need no in-memory
//! index. Compaction bounds retained disk to twice the live window plus 64 KiB.

use std::io::{self, Write};

use super::{Line, LineKind};
use crate::find::OutputBuffer;

pub(super) struct Before<'a> {
    store: OutputBuffer,
    head: u64,
    count: u64,
    limit: u64,
    stopped: &'a dyn Fn() -> bool,
}

impl<'a> Before<'a> {
    pub fn new(limit: u64, stopped: &'a dyn Fn() -> bool) -> Self {
        Self {
            store: OutputBuffer::default(),
            head: 0,
            count: 0,
            limit,
            stopped,
        }
    }
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        for bytes in bytes.chunks(64 << 10) {
            if (self.stopped)() {
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            self.store.write_all(bytes)?;
        }
        Ok(())
    }
    fn header(&self, offset: u64) -> io::Result<(u64, u64)> {
        let mut bytes = [0; 16];
        self.store
            .read_range(offset, 16, &mut &mut bytes[..], self.stopped)?;
        Ok((
            u64::from_le_bytes(bytes[..8].try_into().map_err(io::Error::other)?),
            u64::from_le_bytes(bytes[8..].try_into().map_err(io::Error::other)?),
        ))
    }
    pub fn push(&mut self, number: u64, bytes: &[u8]) -> io::Result<()> {
        if self.limit == 0 {
            return Ok(());
        }
        // Reclaim discarded in-memory records before they can force a spill.
        // Small -B windows must not create one temporary file per 64 KiB of
        // history that no longer belongs to the window.
        let incoming = 16_u64.saturating_add(bytes.len() as u64);
        if self.head > 0
            && self.store.len().saturating_add(incoming) > 64 << 10
            && (self.store.len() - self.head).saturating_add(incoming) <= 64 << 10
        {
            self.compact()?;
        }
        self.append(&number.to_le_bytes())?;
        self.append(&(bytes.len() as u64).to_le_bytes())?;
        self.append(bytes)?;
        self.count += 1;
        if self.count > self.limit {
            let (_, len) = self.header(self.head)?;
            self.head += 16 + len;
            self.count -= 1;
        }
        if self.head > 64 << 10 && self.head > self.store.len() / 2 {
            self.compact()?;
        }
        Ok(())
    }
    fn compact(&mut self) -> io::Result<()> {
        let mut compact = OutputBuffer::default();
        self.store.read_range(
            self.head,
            self.store.len() - self.head,
            &mut compact,
            self.stopped,
        )?;
        self.store = compact;
        self.head = 0;
        Ok(())
    }
    pub fn emit(&mut self, emit: &mut impl FnMut(Line<'_>) -> io::Result<()>) -> io::Result<()> {
        let mut offset = self.head;
        let mut bytes = Vec::new();
        for _ in 0..self.count {
            let (number, len) = self.header(offset)?;
            let len = usize::try_from(len).map_err(io::Error::other)?;
            bytes.clear();
            bytes.try_reserve(len).map_err(io::Error::other)?;
            bytes.resize(len, 0);
            self.store
                .read_range(offset + 16, len as u64, &mut &mut bytes[..], self.stopped)?;
            emit(Line {
                source_lines: None,
                terminated: true,
                number,
                bytes: &bytes,
                kind: LineKind::Context,
                fragment: false,
                highlights: None,
            })?;
            offset += 16 + len as u64;
        }
        self.store = OutputBuffer::default();
        self.head = 0;
        self.count = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spilled_window_repeatedly_evicts_compacts_and_replays_exact_bytes() {
        use std::collections::VecDeque;

        let mut window = Before::new(5, &|| false);
        let mut reference = VecDeque::new();
        let mut compactions = 0;
        let mut spilled = false;
        for number in 1_u64..=512 {
            let mut bytes = format!("record {number}: ").into_bytes();
            bytes.resize(
                17_000 + (number as usize % 7) * 97,
                b'a' + (number % 26) as u8,
            );
            let previous_head = window.head;
            let was_spilled = window.store.is_spilled();
            window.push(number, &bytes).unwrap();
            spilled |= window.store.is_spilled();
            if window.head < previous_head {
                assert!(was_spilled && window.store.is_spilled());
                compactions += 1;
            }
            reference.push_back((number, bytes));
            if reference.len() > 5 {
                reference.pop_front();
            }
        }
        assert!(spilled);
        assert!(compactions >= 50, "only {compactions} spilled compactions");
        let mut actual = Vec::new();
        window
            .emit(&mut |line| {
                assert_eq!(line.kind, LineKind::Context);
                assert!(!line.fragment);
                actual.extend_from_slice(&line.number.to_le_bytes());
                actual.extend_from_slice(line.bytes);
                actual.push(b'\n');
                Ok(())
            })
            .unwrap();
        let mut expected = Vec::new();
        for (number, bytes) in reference {
            expected.extend_from_slice(&number.to_le_bytes());
            expected.extend_from_slice(&bytes);
            expected.push(b'\n');
        }
        assert_eq!(actual, expected);
        assert_eq!(window.store.len(), 0);
    }

    #[test]
    fn large_context_counts_spill_headers_and_rolling_windows_compact() {
        for limit in [2, 20_000] {
            let mut window = Before::new(limit, &|| false);
            for number in 0..2000 {
                window.push(number, &[b'x'; 128]).unwrap();
                assert!(window.store.memory_bytes() <= 128 << 10);
            }
            if limit == 2 {
                assert!(window.store.len() <= (64 << 10) + 2 * 144);
            } else {
                assert!(window.store.len() > 64 << 10);
            }
            let mut numbers = Vec::new();
            window
                .emit(&mut |line| {
                    assert_eq!(line.bytes, &[b'x'; 128]);
                    numbers.push(line.number);
                    Ok(())
                })
                .unwrap();
            let expected: Vec<_> = if limit == 2 {
                (1998..2000).collect()
            } else {
                (0..2000).collect()
            };
            assert_eq!(numbers, expected);
            assert_eq!(window.store.len(), 0);
        }
    }
}
