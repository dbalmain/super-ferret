//! Bounded captures and entry output transactions. Larger output spills
//! to an unlinked file. Commit and quit share a lock across all worker tasks.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::{Effects, WalkError};

const MEMORY_LIMIT: usize = 64 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(0);

/// Bounded output capture. Up to 64 KiB stays in memory; larger streams spill
/// to a private, unlinked temporary file. Capture before locking the
/// destination, then write the complete stream while holding that lock.
#[derive(Default)]
pub struct OutputBuffer {
    bytes: Vec<u8>,
    file: Option<File>,
    path: Option<PathBuf>,
    len: u64,
}

impl Write for OutputBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.file.is_none() && self.bytes.len() + bytes.len() > MEMORY_LIMIT {
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
                        file.write_all(&self.bytes)
                            .map_err(|error| spill_error("write", &path, error))?;
                        self.bytes.clear();
                        self.path = Some(path);
                        self.file = Some(file);
                        break;
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(spill_error("create", &path, error)),
                }
            }
        }
        if let Some(file) = &mut self.file {
            file.write_all(bytes).map_err(|error| {
                spill_error(
                    "write",
                    self.path.as_deref().unwrap_or(Path::new("")),
                    error,
                )
            })?;
        } else {
            self.bytes.extend_from_slice(bytes);
        }
        self.len += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl OutputBuffer {
    /// Writes all captured bytes to the destination, preserving their order.
    pub fn write_to(&mut self, writer: &mut dyn Write) -> io::Result<()> {
        self.emit(|bytes| writer.write_all(bytes))
    }

    fn emit(&mut self, mut write: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
        if let Some(file) = &mut self.file {
            file.rewind().map_err(|error| {
                spill_error(
                    "rewind",
                    self.path.as_deref().unwrap_or(Path::new("")),
                    error,
                )
            })?;
            let mut bytes = [0; MEMORY_LIMIT];
            loop {
                let count = file.read(&mut bytes).map_err(|error| {
                    spill_error("read", self.path.as_deref().unwrap_or(Path::new("")), error)
                })?;
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
        if let Some(file) = &mut self.file {
            file.set_len(len)?;
            file.seek(SeekFrom::Start(len))?;
        } else {
            self.bytes.truncate(len as usize);
        }
        self.len = len;
        Ok(())
    }
    fn clear(&mut self) {
        self.bytes.clear();
        self.file = None;
        self.path = None;
        self.len = 0;
    }
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
    files: Vec<u64>,
}

impl Record {
    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            stdout: self.stdout.len,
            files: self.files.iter().map(|(_, spool)| spool.len).collect(),
        }
    }
    pub fn rollback(&mut self, checkpoint: Checkpoint) -> io::Result<()> {
        self.stdout.truncate(checkpoint.stdout)?;
        self.files.truncate(checkpoint.files.len());
        for ((_, spool), len) in self.files.iter_mut().zip(checkpoint.files) {
            spool.truncate(len)?;
        }
        Ok(())
    }

    pub fn clear(&mut self) {
        self.stdout.clear();
        for (_, spool) in &mut self.files {
            spool.clear();
        }
    }
    pub fn full(&self) -> bool {
        self.stdout.file.is_some()
            || self.stdout.bytes.len() >= MEMORY_LIMIT / 2
            || self
                .files
                .iter()
                .any(|(_, spool)| spool.file.is_some() || spool.bytes.len() >= MEMORY_LIMIT / 2)
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
                    self.host.flush()
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
        self.write(path.as_os_str().as_bytes())?;
        self.write(if nul { b"\0" } else { b"\n" })
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

// Commands and quit observe output immediately; pure output expressions may
// stage complete records into a bounded transaction to amortize destination
// I/O.
pub(super) fn immediate(expression: &super::Expression) -> bool {
    let mut immediate = false;
    expression.visit(&mut |leaf| {
        immediate |= matches!(
            leaf,
            super::Expression::Quit | super::Expression::Action(super::action::Action::Exec(_))
        );
    });
    immediate
}
