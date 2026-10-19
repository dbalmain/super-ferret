//! Bounded captures and entry output transactions. Larger output spills
//! to an unlinked file. Commit and quit share a lock across all worker tasks.

use std::fs::{File, OpenOptions};
use std::io::{self, IoSlice, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::{Effects, WalkError};

const MEMORY_LIMIT: usize = 64 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(0);

/// Bounded output capture. By default 64 KiB stays in memory; larger streams
/// spill to a private, unlinked temporary file. Capture before locking the
/// destination, then write the complete stream while holding that lock.
#[derive(Default)]
pub struct OutputBuffer {
    bytes: Vec<u8>,
    file: Option<Spill>,
    len: u64,
    checkpoint: u64,
    buffered_spill: bool,
    memory_limit: usize,
}

struct Spill {
    file: File,
    path: PathBuf,
}

impl Spill {
    fn write_vectored(&mut self, bytes: &[IoSlice<'_>]) -> io::Result<usize> {
        #[cfg(test)]
        if let Some(result) = spill_tests::WRITE
            .with_borrow_mut(|hook| hook.as_mut().map(|hook| hook(&mut self.file, bytes)))
        {
            return result;
        }
        self.file.write_vectored(bytes)
    }
}

impl Write for OutputBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.write_checked(bytes, &|| false)
    }
    fn write_vectored(&mut self, bytes: &[IoSlice<'_>]) -> io::Result<usize> {
        if !self.buffered_spill
            && let Some(spill) = &mut self.file
        {
            let written = spill_progress(spill.write_vectored(bytes), bytes, &spill.path)?;
            self.len += written as u64;
            Ok(written)
        } else {
            self.write(
                bytes
                    .iter()
                    .find(|part| !part.is_empty())
                    .map_or(&[], |part| &**part),
            )
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.finish_spill()
    }
}

impl OutputBuffer {
    /// Raise the capture threshold, bounding its Vec capacity as well as
    /// length. Spill staging and replay remain 64 KiB. Use `write_checked`
    /// to poll cancellation while the initial capture is copied to its
    /// spill file.
    pub fn with_memory_limit(mut self, limit: usize) -> Self {
        self.memory_limit = limit.max(MEMORY_LIMIT);
        self
    }

    /// Append bytes, polling between at most 64 KiB initial spill-copy chunks.
    pub fn write_checked(&mut self, bytes: &[u8], stopped: &dyn Fn() -> bool) -> io::Result<usize> {
        let limit = self.memory_limit.max(MEMORY_LIMIT);
        if self.file.is_none() && self.bytes.len() + bytes.len() > limit {
            loop {
                let path = std::env::temp_dir().join(format!(
                    "ferret-output-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)
                {
                    Ok(mut file) => {
                        std::fs::remove_file(&path)
                            .map_err(|error| spill_error("unlink", &path, error))?;
                        if self.memory_limit == 0 {
                            file.write_all(&self.bytes)
                                .map_err(|error| spill_error("write", &path, error))?;
                        } else {
                            for chunk in self.bytes.chunks(MEMORY_LIMIT) {
                                if stopped() {
                                    return Err(io::ErrorKind::ConnectionAborted.into());
                                }
                                file.write_all(chunk)
                                    .map_err(|error| spill_error("write", &path, error))?;
                            }
                        }
                        self.bytes.clear();
                        self.file = Some(Spill { file, path });
                        break;
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(spill_error("create", &path, error)),
                }
            }
        }
        if self.file.is_some() && self.buffered_spill {
            if bytes.len() > MEMORY_LIMIT - self.bytes.len() {
                self.flush()?;
            }
            // Accept only what fits, so one call spills at most the pending
            // block and the caller's cancellation check runs between blocks.
            let accepted = bytes.len().min(MEMORY_LIMIT - self.bytes.len());
            self.bytes.extend_from_slice(&bytes[..accepted]);
            self.len += accepted as u64;
            return Ok(accepted);
        }
        if let Some(spill) = &mut self.file {
            spill
                .file
                .write_all(bytes)
                .map_err(|error| spill_error("write", &spill.path, error))?;
        } else {
            let needed = self.bytes.len() + bytes.len();
            if self.memory_limit > 0 && needed > self.bytes.capacity() {
                let capacity = needed
                    .max(self.bytes.capacity().saturating_mul(2))
                    .min(limit);
                self.bytes.reserve_exact(capacity - self.bytes.len());
            }
            self.bytes.extend_from_slice(bytes);
        }
        self.len += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn finish_spill(&mut self) -> io::Result<()> {
        if !self.buffered_spill {
            return Ok(());
        }
        if let Some(spill) = &mut self.file {
            let mut written = 0;
            let result = (|| {
                while written < self.bytes.len() {
                    let parts = [IoSlice::new(&self.bytes[written..])];
                    match spill_progress(spill.write_vectored(&parts), &parts, &spill.path) {
                        Ok(count) => written += count,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error),
                    }
                }
                Ok(())
            })();
            self.bytes.drain(..written);
            result?;
        }
        Ok(())
    }
}

impl OutputBuffer {
    /// Stage spill writes in at most 64 KiB of the capture's allocation. Call
    /// `flush` before completing a capture to surface deferred spill faults
    /// before publication. Default captures retain immediate write faults.
    pub fn with_buffered_spill(mut self) -> Self {
        self.buffered_spill = true;
        self
    }
    /// Captured byte length, including spilled bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether no bytes have been captured.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes the capture holds in memory: its buffer's allocation, which a
    /// spilled capture keeps for staging.
    pub fn memory_bytes(&self) -> usize {
        self.bytes.capacity()
    }

    /// Whether the capture has moved to its spill file. Hosts' tests use it
    /// to prove a fixture still exceeds the memory limit.
    pub fn is_spilled(&self) -> bool {
        self.file.is_some()
    }

    /// Replay a bounded range without changing the append cursor. Context
    /// windows use this for rolling retention and replay; cancellation is
    /// polled between at most 64 KiB chunks. Faults remain output faults.
    pub fn read_range(
        &self,
        start: u64,
        len: u64,
        writer: &mut dyn Write,
        stopped: &dyn Fn() -> bool,
    ) -> io::Result<()> {
        let end = start
            .checked_add(len)
            .filter(|end| *end <= self.len)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "output range outside capture")
            })?;
        let mut at = start;
        let mut buffer = [0; MEMORY_LIMIT];
        while at < end {
            if stopped() {
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            let count = (end - at).min(MEMORY_LIMIT as u64) as usize;
            if let Some(spill) = &self.file {
                let disk_len = self.len - self.bytes.len() as u64;
                let disk_count = disk_len.saturating_sub(at).min(count as u64) as usize;
                if disk_count > 0 {
                    spill
                        .file
                        .read_exact_at(&mut buffer[..disk_count], at)
                        .map_err(|error| spill_error("read", &spill.path, error))?;
                }
                if disk_count < count {
                    let tail = (at + disk_count as u64 - disk_len) as usize;
                    buffer[disk_count..count]
                        .copy_from_slice(&self.bytes[tail..tail + count - disk_count]);
                }
                writer.write_all(&buffer[..count])?;
            } else {
                writer.write_all(&self.bytes[at as usize..at as usize + count])?;
            }
            at += count as u64;
        }
        Ok(())
    }

    fn record(&mut self, bytes: &[u8], terminator: &[u8]) -> io::Result<()> {
        let len = bytes.len() + terminator.len();
        if self.file.is_none() && self.bytes.len() + len <= MEMORY_LIMIT {
            self.bytes.extend_from_slice(bytes);
            self.bytes.extend_from_slice(terminator);
            self.len += len as u64;
            Ok(())
        } else {
            let len = self.len;
            match self
                .write_all(bytes)
                .and_then(|()| self.write_all(terminator))
            {
                Ok(()) => Ok(()),
                Err(error) => {
                    self.truncate(len)?;
                    Err(error)
                }
            }
        }
    }

    /// Writes all captured bytes to the destination, preserving their order.
    pub fn write_to(&mut self, writer: &mut dyn Write) -> io::Result<()> {
        self.emit(|bytes| writer.write_all(bytes))
    }

    fn emit(&mut self, mut write: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
        self.flush()?;
        if let Some(spill) = &mut self.file {
            spill
                .file
                .rewind()
                .map_err(|error| spill_error("rewind", &spill.path, error))?;
            let mut bytes = [0; MEMORY_LIMIT];
            loop {
                let count = spill
                    .file
                    .read(&mut bytes)
                    .map_err(|error| spill_error("read", &spill.path, error))?;
                if count == 0 {
                    break;
                }
                write(&bytes[..count])?;
            }
        } else if !self.bytes.is_empty() {
            write(&self.bytes)?;
        }
        Ok(())
    }
    fn truncate(&mut self, len: u64) -> io::Result<()> {
        if let Some(spill) = &mut self.file {
            let disk_len = self.len - self.bytes.len() as u64;
            if self.buffered_spill && len >= disk_len {
                self.bytes.truncate((len - disk_len) as usize);
                self.len = len;
                return Ok(());
            }
            self.bytes.clear();
            spill
                .file
                .set_len(len)
                .map_err(|error| spill_error("truncate", &spill.path, error))?;
            spill
                .file
                .seek(SeekFrom::Start(len))
                .map_err(|error| spill_error("seek", &spill.path, error))?;
        } else {
            self.bytes.truncate(len as usize);
        }
        self.len = len;
        Ok(())
    }
    fn clear(&mut self) {
        self.bytes.clear();
        self.file = None;
        self.len = 0;
    }
}

/// A vectored spill write's progress. No progress on nonempty input is the
/// `WriteZero` that `write_all` would report, still naming the spill.
fn spill_progress(
    written: io::Result<usize>,
    bytes: &[IoSlice<'_>],
    path: &Path,
) -> io::Result<usize> {
    let written = written.map_err(|error| spill_error("write", path, error))?;
    if written == 0 && bytes.iter().any(|part| !part.is_empty()) {
        return Err(spill_error(
            "write",
            path,
            io::Error::new(io::ErrorKind::WriteZero, "failed to write whole buffer"),
        ));
    }
    Ok(written)
}

fn spill_error(operation: &'static str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        SpillError {
            operation,
            path: path.to_owned(),
            error,
        },
    )
}

#[derive(Debug)]
struct SpillError {
    operation: &'static str,
    path: PathBuf,
    error: io::Error,
}
impl std::fmt::Display for SpillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} output spill {}: {}",
            self.operation,
            self.path.display(),
            self.error
        )
    }
}
impl std::error::Error for SpillError {}

#[derive(Default)]
pub(super) struct Record {
    stdout: OutputBuffer,
    files: Vec<(super::action::SharedFile, OutputBuffer)>,
}

pub(super) fn error_path(error: &io::Error) -> Option<&Path> {
    error
        .get_ref()?
        .downcast_ref::<SpillError>()
        .map(|spill| spill.path.as_path())
}

pub(super) struct Checkpoint {
    stdout: u64,
    files: usize,
}

impl Record {
    pub fn checkpoint(&mut self) -> Checkpoint {
        for (_, spool) in &mut self.files {
            spool.checkpoint = spool.len;
        }
        Checkpoint {
            stdout: self.stdout.len,
            files: self.files.len(),
        }
    }
    pub fn rollback(&mut self, checkpoint: Checkpoint) -> io::Result<()> {
        self.stdout.truncate(checkpoint.stdout)?;
        self.files.truncate(checkpoint.files);
        for (_, spool) in &mut self.files {
            spool.truncate(spool.checkpoint)?;
        }
        Ok(())
    }

    pub fn clear(&mut self) {
        self.stdout.clear();
        for (_, spool) in &mut self.files {
            spool.clear();
        }
    }
    // Leave room for another ordinary PATH_MAX-sized print without spilling
    // a batch of otherwise small records.
    pub fn full(&self) -> bool {
        self.stdout.file.is_some()
            || self.stdout.bytes.len() >= MEMORY_LIMIT - 4096
            || self
                .files
                .iter()
                .any(|(_, spool)| spool.file.is_some() || spool.bytes.len() >= MEMORY_LIMIT - 4096)
    }
}

// Batches and entries use the same destination operation, including host
// buffering. The caller holds the run's gate until the final flush completes.
pub(super) fn commit_stdout(
    effects: &mut impl Effects,
    buffer: &mut OutputBuffer,
    gate: &Mutex<()>,
) -> io::Result<()> {
    commit(gate, || effects.output(buffer))
}

fn commit(gate: &Mutex<()>, write: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    let _gate = gate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    write()
}

pub(super) struct EntryEffects<'a, E> {
    pub host: &'a mut E,
    pub record: &'a mut Record,
    pub gate: &'a Mutex<()>,
    pub quit: &'a AtomicBool,
}

impl<E: Effects> EntryEffects<'_, E> {
    pub fn commit(&mut self, quit: bool) -> io::Result<()> {
        if !quit
            && self.record.stdout.bytes.is_empty()
            && self.record.stdout.file.is_none()
            && self
                .record
                .files
                .iter()
                .all(|(_, spool)| spool.bytes.is_empty() && spool.file.is_none())
        {
            return Ok(());
        }
        let result = commit(self.gate, || {
            if self.quit.load(Ordering::Acquire) {
                Ok(())
            } else {
                let result = self.host.output(&mut self.record.stdout).and_then(|()| {
                    for (file, spool) in &mut self.record.files {
                        let mut guard = file
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let file = guard.as_mut().ok_or_else(|| {
                            io::Error::other("output file target was not opened during preparation")
                        })?;
                        spool.emit(|bytes| file.write_all(bytes))?;
                        file.flush()?;
                    }
                    Ok(())
                });
                if quit {
                    self.quit.store(true, Ordering::Release);
                }
                result
            }
        });
        self.record.clear();
        result
    }
}

impl<E: Effects> Effects for EntryEffects<'_, E> {
    fn print(&mut self, path: &Path, nul: bool) -> io::Result<()> {
        self.record
            .stdout
            .record(path.as_os_str().as_bytes(), if nul { b"\0" } else { b"\n" })
    }
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.record.stdout.write_all(bytes)
    }
    fn file(&mut self, file: &super::action::SharedFile, bytes: &[u8]) -> io::Result<()> {
        let index = match self
            .record
            .files
            .iter()
            .position(|(existing, _)| Arc::ptr_eq(existing, file))
        {
            Some(index) => index,
            None => {
                self.record
                    .files
                    .push((file.clone(), OutputBuffer::default()));
                self.record.files.len() - 1
            }
        };
        self.record.files[index].1.write_all(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.host.flush()
    }
    fn output(&mut self, buffer: &mut OutputBuffer) -> io::Result<()> {
        self.host.output(buffer)
    }
    fn command(&mut self, command: &mut std::process::Command) -> io::Result<bool> {
        self.host.capture(command, &mut self.record.stdout)
    }
    fn capture(
        &mut self,
        command: &mut std::process::Command,
        output: &mut dyn Write,
    ) -> io::Result<bool> {
        self.host.capture(command, output)
    }
    fn quit(&mut self) -> io::Result<()> {
        self.commit(true)
    }
    fn error(&mut self, error: &WalkError) {
        self.host.error(error);
    }
    fn confirm(&mut self, program: &std::ffi::OsStr, path: &Path) -> io::Result<bool> {
        self.host.confirm(program, path)
    }
}

#[derive(Clone, Copy)]
pub(super) struct Policy {
    pub immediate: bool,
    pub checkpoint: bool,
}

// A single print appends its whole record atomically, including on a spill
// failure. Other outputs can fail after capturing a prefix and need an entry
// checkpoint. All records still reach destinations through the same commit.
pub(super) fn policy(expression: &super::Expression) -> Policy {
    let mut policy = Policy {
        immediate: false,
        checkpoint: false,
    };
    let mut prints = 0;
    expression.visit(&mut |leaf| match leaf {
        super::Expression::Quit | super::Expression::Action(super::action::Action::Exec(_)) => {
            policy.immediate = true;
            policy.checkpoint = true;
        }
        super::Expression::Action(
            super::action::Action::Output(..) | super::action::Action::List(_),
        ) => policy.checkpoint = true,
        super::Expression::Print(_) => prints += 1,
        _ => {}
    });
    policy.checkpoint |= prints > 1;
    policy
}

#[cfg(test)]
mod spill_tests {
    use super::*;
    use std::cell::RefCell;

    type SpillWrite = dyn FnMut(&mut File, &[IoSlice<'_>]) -> io::Result<usize>;
    thread_local! {
        pub(super) static WRITE: RefCell<Option<Box<SpillWrite>>> = RefCell::new(None);
    }

    // A larger capture must bound allocated capacity, including geometric
    // growth on uneven records, and poll while first copying it to disk.
    #[test]
    fn larger_captures_bound_capacity_and_cancel_initial_spill() {
        let limit = 256 << 10;
        let mut output = OutputBuffer::default()
            .with_memory_limit(limit)
            .with_buffered_spill();
        while output.len() < (limit - 1000) as u64 {
            output.write_all(&[b'x'; 997]).unwrap();
            assert!(output.memory_bytes() <= limit);
        }
        let calls = std::cell::Cell::new(0);
        let error = output
            .write_checked(&[b'y'; 2000], &|| {
                calls.set(calls.get() + 1);
                calls.get() > 1
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        assert_eq!(calls.get(), 2);
        assert!(output.memory_bytes() <= limit);
    }

    // Deferred faults must still name the spill, and must precede publication.
    #[test]
    fn buffered_spill_faults_never_publish_a_prefix() {
        for zero in [false, true] {
            let mut output = OutputBuffer::default().with_buffered_spill();
            output.write_all(&vec![b'x'; MEMORY_LIMIT]).unwrap();
            output.write_all(b"tail").unwrap();
            let spill = output.file.as_mut().unwrap();
            let path = spill.path.clone();
            if zero {
                let mut returned_zero = false;
                WRITE.with_borrow_mut(|hook| {
                    *hook = Some(Box::new(move |_, _| {
                        if returned_zero {
                            return Err(io::Error::other("retried zero progress"));
                        }
                        returned_zero = true;
                        Ok(0)
                    }))
                });
            } else {
                spill.file = OpenOptions::new().write(true).open("/dev/full").unwrap();
            }
            let mut published = Vec::new();
            let error = output.write_to(&mut published).unwrap_err();
            WRITE.with_borrow_mut(|hook| *hook = None);
            assert!(published.is_empty());
            assert_eq!(error_path(&error), Some(path.as_path()));
            if zero {
                assert_eq!(error.kind(), io::ErrorKind::WriteZero);
                assert_eq!(
                    error.to_string(),
                    format!(
                        "write output spill {}: failed to write whole buffer",
                        path.display()
                    )
                );
            } else {
                assert_eq!(error.raw_os_error(), None);
                assert!(error.to_string().starts_with(&format!(
                    "write output spill {}: No space left on device",
                    path.display()
                )));
            }
        }
    }

    // A short write followed by EINTR must neither repeat nor lose bytes.
    #[test]
    fn buffered_spill_partial_writes_and_ranges_keep_exact_bytes() {
        let mut output = OutputBuffer::default().with_buffered_spill();
        let mut expected = vec![b'x'; MEMORY_LIMIT];
        output.write_all(&expected).unwrap();
        // A small write after the spill exists stays pending until flush.
        output.write_all(b"first tail").unwrap();
        expected.extend_from_slice(b"first tail");
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let counted = calls.clone();
        WRITE.with_borrow_mut(|hook| {
            *hook = Some(Box::new(move |file, parts| {
                counted.set(counted.get() + 1);
                if counted.get() == 2 {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                file.write(&parts[0][..parts[0].len().min(3)])
            }))
        });
        output.flush().unwrap();
        WRITE.with_borrow_mut(|hook| *hook = None);
        // Ten pending bytes, three at a time, plus the interrupted call.
        assert_eq!(calls.get(), 5);
        output.write_all(b"second tail").unwrap();
        expected.extend_from_slice(b"second tail");
        let mut range = Vec::new();
        output
            .read_range((MEMORY_LIMIT - 2) as u64, 23, &mut range, &|| false)
            .unwrap();
        assert_eq!(range, expected[MEMORY_LIMIT - 2..]);
        output.truncate((MEMORY_LIMIT + 2) as u64).unwrap();
        expected.truncate(MEMORY_LIMIT + 2);
        output.write_all(b"replacement").unwrap();
        expected.extend_from_slice(b"replacement");
        let mut actual = Vec::new();
        output.write_to(&mut actual).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn buffered_spill_writes_at_most_one_block_per_call() {
        // The caller checks cancellation between write calls, so one call
        // must not drain the pending block and then spill a large part too.
        let mut output = OutputBuffer::default().with_buffered_spill();
        output.write_all(&[b'x'; MEMORY_LIMIT]).unwrap();
        output.write_all(&[b'y'; MEMORY_LIMIT - 1]).unwrap();
        let spilled =
            |output: &OutputBuffer| output.file.as_ref().unwrap().file.metadata().unwrap().len();
        let before = spilled(&output);
        let accepted = output.write(&[b'z'; MEMORY_LIMIT]).unwrap();
        assert!(accepted > 0);
        assert!(spilled(&output) - before <= MEMORY_LIMIT as u64);
        output.write_all(&[b'z'; MEMORY_LIMIT][accepted..]).unwrap();
        let mut actual = Vec::new();
        output.write_to(&mut actual).unwrap();
        let mut expected = vec![b'x'; MEMORY_LIMIT];
        expected.extend_from_slice(&[b'y'; MEMORY_LIMIT - 1]);
        expected.extend_from_slice(&[b'z'; MEMORY_LIMIT]);
        assert_eq!(actual, expected);
    }

    #[test]
    fn zero_progress_on_a_nonempty_vector_names_the_spill() {
        let path = Path::new("/spill/ferret-output-1");
        let error = spill_progress(Ok(0), &[IoSlice::new(b""), IoSlice::new(b"x")], path)
            .expect_err("no progress on a nonempty vector");
        assert_eq!(error.kind(), io::ErrorKind::WriteZero);
        assert_eq!(error_path(&error), Some(path));
        assert_eq!(
            error.to_string(),
            "write output spill /spill/ferret-output-1: failed to write whole buffer"
        );
    }

    #[test]
    fn progress_and_empty_vectors_pass_through() {
        let path = Path::new("/spill/ferret-output-1");
        assert_eq!(
            spill_progress(Ok(3), &[IoSlice::new(b"xyz")], path).unwrap(),
            3
        );
        assert_eq!(
            spill_progress(Ok(0), &[IoSlice::new(b"")], path).unwrap(),
            0
        );
        let error = spill_progress(
            Err(io::ErrorKind::Interrupted.into()),
            &[IoSlice::new(b"x")],
            path,
        )
        .expect_err("errors keep their kind");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(error_path(&error), Some(path));
    }
}
