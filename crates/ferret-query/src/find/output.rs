//! Entry output transactions. Memory is capped per stream; larger output spills
//! to an unlinked file. Commit and quit share a lock across all worker tasks.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::{Effects, WalkError};

const MEMORY_LIMIT: usize = 64 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct Spool {
    bytes: Vec<u8>,
    file: Option<File>,
}

impl Write for Spool {
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
                        std::fs::remove_file(path)?;
                        file.write_all(&self.bytes)?;
                        self.bytes.clear();
                        self.file = Some(file);
                        break;
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            }
        }
        if let Some(file) = &mut self.file {
            file.write_all(bytes)?;
        } else {
            self.bytes.extend_from_slice(bytes);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Spool {
    fn emit(&mut self, mut write: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
        if let Some(file) = &mut self.file {
            file.rewind()?;
            let mut bytes = [0; MEMORY_LIMIT];
            loop {
                let count = file.read(&mut bytes)?;
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
    fn clear(&mut self) {
        self.bytes.clear();
        self.file = None;
    }
}

#[derive(Default)]
pub(super) struct Record {
    stdout: Spool,
    files: Vec<(Arc<Mutex<io::BufWriter<File>>>, Spool)>,
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
        let _gate = self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = if self.quit.load(Ordering::Acquire) {
            Ok(())
        } else {
            let result = self
                .record
                .stdout
                .emit(|bytes| self.host.write(bytes))
                .and_then(|()| {
                    for (file, spool) in &mut self.record.files {
                        let mut file = file
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        spool.emit(|bytes| file.write_all(bytes))?;
                        file.flush()?;
                    }
                    self.host.flush()
                });
            if quit {
                self.quit.store(true, Ordering::Release);
            }
            result
        };
        self.record.stdout.clear();
        for (_, spool) in &mut self.record.files {
            spool.clear();
        }
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
    fn file(
        &mut self,
        file: &std::sync::Arc<std::sync::Mutex<std::io::BufWriter<std::fs::File>>>,
        bytes: &[u8],
    ) -> io::Result<()> {
        let index = match self
            .record
            .files
            .iter()
            .position(|(existing, _)| Arc::ptr_eq(existing, file))
        {
            Some(index) => index,
            None => {
                self.record.files.push((file.clone(), Spool::default()));
                self.record.files.len() - 1
            }
        };
        self.record.files[index].1.write_all(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.host.flush()
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

// A single output primary is already a whole entry record. Preserve that
// common path's worker buffer without adding a lock for every selected file.
pub(super) fn needs_record(expression: &super::Expression) -> bool {
    fn count(expression: &super::Expression) -> usize {
        use super::Expression;
        match expression {
            Expression::And(a, b) | Expression::Or(a, b) | Expression::Comma(a, b) => {
                count(a).saturating_add(count(b))
            }
            Expression::Not(inner) => count(inner),
            Expression::Quit | Expression::Action(super::action::Action::Exec(_)) => 2,
            Expression::Print(_)
            | Expression::Action(
                super::action::Action::Output(..) | super::action::Action::List(_),
            ) => 1,
            _ => 0,
        }
    }
    count(expression) > 1
}
