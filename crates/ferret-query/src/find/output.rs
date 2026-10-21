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
    memory_limit: usize,
    spill_dir: Option<Arc<Path>>,
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
        self.append(bytes)
    }
    fn write_vectored(&mut self, bytes: &[IoSlice<'_>]) -> io::Result<usize> {
        if let Some(spill) = &mut self.file {
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
        Ok(())
    }
}

impl OutputBuffer {
    /// Raise the capture threshold, bounding its Vec capacity as well as
    /// length. Larger captures spill immediately; replay uses 64 KiB chunks.
    pub fn with_memory_limit(mut self, limit: usize) -> Self {
        self.memory_limit = limit.max(MEMORY_LIMIT);
        self
    }

    /// Spill into `dir` instead of the process's temporary directory, which
    /// is read when the spill is created. A resident host serving another
    /// process's request passes that caller's resolved directory.
    pub fn with_spill_dir(mut self, dir: Arc<Path>) -> Self {
        self.spill_dir = Some(dir);
        self
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let limit = self.memory_limit.max(MEMORY_LIMIT);
        if self.file.is_none() && self.bytes.len() + bytes.len() > limit {
            let dir = self
                .spill_dir
                .as_deref()
                .map_or_else(std::env::temp_dir, Path::to_path_buf);
            loop {
                let path = dir.join(format!(
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
                        file.write_all(&self.bytes)
                            .map_err(|error| spill_error("write", &path, error))?;
                        self.bytes = Vec::new();
                        self.file = Some(Spill { file, path });
                        break;
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(spill_error("create", &path, error)),
                }
            }
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
}

impl OutputBuffer {
    /// Captured byte length, including spilled bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether no bytes have been captured.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes the capture holds in memory. Spilled captures release their
    /// in-memory allocation and write subsequent bytes directly to disk.
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
    /// polled between at most 64 KiB chunks. Spill faults retain their
    /// temporary-file provenance for the host's error policy.
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
        // Only disk reads need a scratch block; in-memory context headers
        // must not zero 64 KiB for each small range.
        let mut at = start;
        while at < end {
            if stopped() {
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            let count = (end - at).min(MEMORY_LIMIT as u64) as usize;
            if let Some(spill) = &self.file {
                let mut buffer = [0; MEMORY_LIMIT];
                spill
                    .file
                    .read_exact_at(&mut buffer[..count], at)
                    .map_err(|error| spill_error("read", &spill.path, error))?;
                writer.write_all(&buffer[..count])?;
            } else {
                let from = at as usize;
                writer.write_all(&self.bytes[from..from + count])?;
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

/// The spill file an output error names, when the fault was in a capture's
/// temporary file rather than its destination.
pub fn spill_path(error: &io::Error) -> Option<&Path> {
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

    // Configurable in-memory captures keep their allocation within the cap,
    // then release it when they spill. Grep intercepts this transition itself.
    #[test]
    fn larger_captures_bound_capacity_and_release_it_after_spilling() {
        let limit = 256 << 10;
        let mut output = OutputBuffer::default().with_memory_limit(limit);
        let mut expected = Vec::new();
        for _ in 0..300 {
            output.write_all(&[b'x'; 997]).unwrap();
            expected.extend_from_slice(&[b'x'; 997]);
            assert!(output.memory_bytes() <= limit);
        }
        assert!(output.is_spilled());
        assert_eq!(output.memory_bytes(), 0);
        let mut actual = Vec::new();
        output.write_to(&mut actual).unwrap();
        assert_eq!(actual, expected);
    }

    // No deferred staging survives D103: a spill write fault is returned by
    // that write, with its spill path, before the host can publish the capture.
    #[test]
    fn immediate_spill_faults_name_the_spill_before_publication() {
        for zero in [false, true] {
            let mut output = OutputBuffer::default();
            output.write_all(&vec![b'x'; MEMORY_LIMIT + 1]).unwrap();
            let spill = output.file.as_mut().unwrap();
            let path = spill.path.clone();
            if zero {
                WRITE.with_borrow_mut(|hook| *hook = Some(Box::new(|_, _| Ok(0))));
            } else {
                spill.file = OpenOptions::new().write(true).open("/dev/full").unwrap();
            }
            let before = output.len();
            let error = output.write_vectored(&[IoSlice::new(b"tail")]).unwrap_err();
            WRITE.with_borrow_mut(|hook| *hook = None);
            assert_eq!(output.len(), before);
            assert_eq!(spill_path(&error), Some(path.as_path()));
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
        // Read-back faults carry the same provenance; the host must classify
        // them by spill_path rather than confusing them with stdout errors.
        let mut output = OutputBuffer::default();
        output.write_all(&vec![b'x'; MEMORY_LIMIT + 1]).unwrap();
        let spill = output.file.as_mut().unwrap();
        let path = spill.path.clone();
        spill.file = OpenOptions::new().write(true).open("/dev/null").unwrap();
        let mut published = Vec::new();
        let error = output.write_to(&mut published).unwrap_err();
        assert!(published.is_empty());
        assert_eq!(spill_path(&error), Some(path.as_path()));
        assert!(error.to_string().starts_with("read output spill "));
    }

    // Partial vector writes and EINTR must preserve the append cursor and
    // lengths used by context replay, truncation and replacement appends.
    #[test]
    fn spill_partial_writes_and_ranges_keep_exact_bytes() {
        let mut output = OutputBuffer::default();
        let mut expected = vec![b'x'; MEMORY_LIMIT];
        output.write_all(&expected).unwrap();
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
            }));
        });
        let mut tail = b"second tail".as_slice();
        while !tail.is_empty() {
            match output.write_vectored(&[IoSlice::new(tail)]) {
                Ok(count) => tail = &tail[count..],
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                other => panic!("unexpected partial write: {other:?}"),
            }
        }
        WRITE.with_borrow_mut(|hook| *hook = None);
        assert_eq!(calls.get(), 5);
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
    fn spilled_writes_are_immediate_and_keep_no_memory_tail() {
        let mut output = OutputBuffer::default();
        output.write_all(&[b'x'; MEMORY_LIMIT]).unwrap();
        output.write_all(&[b'y'; MEMORY_LIMIT - 1]).unwrap();
        assert!(output.is_spilled());
        assert_eq!(output.memory_bytes(), 0);
        assert_eq!(output.write(&[b'z'; MEMORY_LIMIT]).unwrap(), MEMORY_LIMIT);
        assert_eq!(
            output.file.as_ref().unwrap().file.metadata().unwrap().len(),
            output.len()
        );
        assert_eq!(output.memory_bytes(), 0);
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
        assert_eq!(spill_path(&error), Some(path));
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
        assert_eq!(spill_path(&error), Some(path));
    }
}
