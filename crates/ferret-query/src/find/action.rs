//! Process, deletion and output primaries. Compiled actions are immutable;
//! pending exec batches belong to one run. Workers stage bounded chunks; only
//! the shared batch partitions ordinary exec argv. Output files open during
//! parsing.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, BufRead, BufWriter, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use ferret_verify::FindRegex;

use super::printf::Format;
use super::{Effects, Entry, EvaluationError, FileKind, WalkError, glob, walk};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Exec(Exec),
    Delete,
    Output(Target, Format),
    List(Target),
    Regex(FindRegex),
    Link(glob::Pattern),
    Xtype(Vec<FileKind>),
}

#[derive(Clone, Debug)]
pub(super) enum Target {
    Stdout,
    File(PathBuf, Arc<Mutex<BufWriter<File>>>),
}

impl PartialEq for Target {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Stdout, Self::Stdout) => true,
            (Self::File(a, _), Self::File(b, _)) => a == b,
            _ => false,
        }
    }
}
impl Eq for Target {}

impl Target {
    fn write(&self, bytes: &[u8], effects: &mut impl Effects) -> io::Result<()> {
        match self {
            Self::Stdout => effects.write(bytes),
            Self::File(_, file) => effects.file(file, bytes),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Exec {
    pub id: usize,
    pub args: Vec<OsString>,
    pub batch: bool,
    pub directory: bool,
    pub prompt: bool,
}

pub(super) struct Batch {
    exec: Exec,
    directory: Option<PathBuf>,
    handle: Option<Arc<File>>,
    paths: Vec<OsString>,
    bytes: usize,
}

#[derive(Default)]
pub(super) struct State {
    batches: BTreeMap<usize, Batch>,
    staged: BTreeMap<usize, Batch>,
    pub shared: Arc<Mutex<BTreeMap<usize, Batch>>>,
    buffer: Vec<u8>,
    limit: Option<Budget>,
    files: Vec<Arc<Mutex<BufWriter<File>>>>,
    pub errors: u64,
}

impl State {
    pub(super) fn flush_files(&self) -> io::Result<()> {
        for file in &self.files {
            file.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .flush()?;
        }
        Ok(())
    }

    pub fn flush(&mut self, effects: &mut impl Effects, directories_only: bool) -> io::Result<()> {
        if !directories_only
            || self
                .batches
                .values()
                .any(|batch| batch.exec.directory && !batch.paths.is_empty())
        {
            self.flush_files()?;
        }
        if !directories_only && !self.staged.is_empty() {
            let limit = *self.limit.get_or_insert_with(batch_limit);
            for batch in self.staged.values_mut() {
                self.errors += batch.collect(&self.shared, limit, effects)?;
            }
        }
        for batch in self.batches.values_mut() {
            if !directories_only || batch.exec.directory {
                self.errors += u64::from(!batch.run(effects)?);
            }
        }
        Ok(())
    }

    pub fn change_directory(&mut self, path: &Path, effects: &mut impl Effects) -> io::Result<()> {
        // Called for every entry; without an -execdir batch there is nothing
        // to flush, so skip splitting the path.
        if !self.batches.values().any(|batch| batch.exec.directory) {
            return Ok(());
        }
        let (directory, _) = exec_path(path);
        if self.batches.values().any(|batch| {
            batch.exec.directory
                && !batch.paths.is_empty()
                && batch.directory.as_deref() != Some(&directory)
        }) {
            self.flush_files()?;
        }
        for batch in self.batches.values_mut() {
            if batch.exec.directory && batch.directory.as_deref() != Some(&directory) {
                self.errors += u64::from(!batch.run(effects)?);
            }
        }
        Ok(())
    }
}

pub(super) fn flush_shared(
    shared: &Mutex<BTreeMap<usize, Batch>>,
    effects: &mut impl Effects,
) -> io::Result<u64> {
    let mut errors = 0;
    let mut batches = std::mem::take(
        &mut *shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    for batch in batches.values_mut() {
        errors += u64::from(!batch.run(effects)?);
    }
    Ok(errors)
}

impl Batch {
    fn new(exec: &Exec, directory: Option<PathBuf>, handle: Option<Arc<File>>) -> Self {
        Self {
            exec: exec.clone(),
            directory,
            handle,
            paths: Vec::new(),
            bytes: command_bytes(&exec.args),
        }
    }

    fn full(&self, bytes: usize, limit: Budget) -> bool {
        // Linux counts argv pointers as well as strings against ARG_MAX.
        let pointers = (self.exec.args.len() + self.paths.len() + 2) * std::mem::size_of::<usize>();
        self.bytes + bytes > limit.strings || self.bytes + bytes + pointers > limit.kernel
    }

    fn collect(
        &mut self,
        shared: &Mutex<BTreeMap<usize, Self>>,
        limit: Budget,
        effects: &mut impl Effects,
    ) -> io::Result<u64> {
        if self.paths.is_empty() {
            return Ok(0);
        }
        let mut ready = Vec::new();
        {
            let mut batches = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let batch = batches
                .entry(self.exec.id)
                .or_insert_with(|| Self::new(&self.exec, None, None));
            for path in self.paths.drain(..) {
                let bytes = path.as_bytes().len() + 1;
                if batch.full(bytes, limit) {
                    ready.push(std::mem::replace(batch, Self::new(&self.exec, None, None)));
                }
                batch.paths.push(path);
                batch.bytes += bytes;
            }
        }
        self.bytes = command_bytes(&self.exec.args);
        // Workers append to a fresh batch while detached full batches run.
        let mut errors = 0;
        for batch in &mut ready {
            errors += u64::from(!batch.run(effects)?);
        }
        Ok(errors)
    }

    fn run(&mut self, effects: &mut impl Effects) -> io::Result<bool> {
        if self.paths.is_empty() {
            return Ok(true);
        }
        let mut args = self.exec.args.clone();
        args.append(&mut self.paths);
        self.bytes = command_bytes(&self.exec.args);
        spawn(
            &args,
            self.directory.as_deref(),
            self.handle.as_deref(),
            false,
            effects,
        )
    }
}

fn command_bytes(args: &[OsString]) -> usize {
    args.iter().map(|arg| arg.as_bytes().len() + 1).sum()
}

#[derive(Clone, Copy)]
struct Budget {
    strings: usize,
    kernel: usize,
}

fn batch_limit() -> Budget {
    // GNU's default observed buffer is 128 KiB. Linux ARG_MAX is a quarter of
    // the stack limit, with a 128 KiB floor; environment strings and safety
    // slack only shrink that default on a constrained process.
    let stack = fs::read_to_string("/proc/self/limits")
        .ok()
        .and_then(|limits| {
            limits.lines().find_map(|line| {
                line.strip_prefix("Max stack size")?
                    .split_whitespace()
                    .next()?
                    .parse::<usize>()
                    .ok()
            })
        })
        .unwrap_or(8 * 1024 * 1024);
    let environment: usize = std::env::vars_os()
        .map(|(key, value)| {
            key.as_bytes().len() + value.as_bytes().len() + 2 + std::mem::size_of::<usize>()
        })
        .sum();
    let kernel = (stack / 4)
        .max(128 * 1024)
        .saturating_sub(environment + 2048);
    Budget {
        strings: kernel.min(128 * 1024),
        kernel,
    }
}

pub(super) fn evaluate(
    action: &Action,
    entry: &Entry,
    effects: &mut impl Effects,
    state: &mut State,
) -> Result<bool, EvaluationError> {
    match action {
        Action::Exec(exec) => execute(exec, entry, effects, state).map_err(EvaluationError::Output),
        Action::Delete => {
            let result = if entry.name() == b"." {
                Ok(())
            } else if entry.kind().map_err(metadata_error)? == FileKind::Directory {
                fs::remove_dir(entry.path())
            } else {
                fs::remove_file(entry.path())
            };
            match result {
                Ok(()) => {
                    entry.note_deleted();
                    Ok(true)
                }
                Err(error) => {
                    effects.error(&WalkError {
                        path: entry.path().to_owned(),
                        error,
                    });
                    state.errors += 1;
                    Ok(false)
                }
            }
        }
        Action::Output(target, format) => {
            register_file(state, target);
            state.buffer.clear();
            format
                .render(entry, &mut state.buffer)
                .map_err(EvaluationError::Metadata)?;
            target
                .write(&state.buffer, effects)
                .map_err(EvaluationError::Output)?;
            Ok(true)
        }
        Action::List(target) => {
            register_file(state, target);
            state.buffer.clear();
            super::printf::list(entry, &mut state.buffer).map_err(EvaluationError::Metadata)?;
            target
                .write(&state.buffer, effects)
                .map_err(EvaluationError::Output)?;
            Ok(true)
        }
        Action::Regex(regex) => regex
            .try_is_match(entry.path().as_os_str().as_bytes())
            .map_err(|error| EvaluationError::Metadata(io::Error::other(error))),
        Action::Link(pattern) => {
            if entry.kind().map_err(metadata_error)? != FileKind::Symlink {
                return Ok(false);
            }
            let target = entry.link_target().map_err(EvaluationError::Metadata)?;
            Ok(pattern.matches(&target))
        }
        Action::Xtype(kinds) => {
            Ok(kinds.contains(&entry.opposite_kind().map_err(EvaluationError::Metadata)?))
        }
    }
}

fn register_file(state: &mut State, target: &Target) {
    if let Target::File(_, file) = target
        && !state
            .files
            .iter()
            .any(|existing| Arc::ptr_eq(existing, file))
    {
        state.files.push(file.clone());
    }
}

fn metadata_error(error: &io::Error) -> EvaluationError {
    EvaluationError::Metadata(walk::copy_error(error))
}

fn execute(
    exec: &Exec,
    entry: &Entry,
    effects: &mut impl Effects,
    state: &mut State,
) -> io::Result<bool> {
    let (directory, path) = if exec.directory {
        let (directory, path) = exec_path(entry.path());
        (Some(directory), path)
    } else {
        (None, entry.path().as_os_str().to_owned())
    };
    let handle = if exec.directory {
        Some(match entry.directory_handle() {
            Some(handle) => handle,
            None => Arc::new(File::open(directory.as_deref().unwrap_or(Path::new(".")))?),
        })
    } else {
        None
    };
    if exec.batch {
        let limit = *state.limit.get_or_insert_with(batch_limit);
        if !exec.directory {
            let batch = state
                .staged
                .entry(exec.id)
                .or_insert_with(|| Batch::new(exec, None, None));
            batch.bytes += path.as_bytes().len() + 1;
            batch.paths.push(path);
            // Bound worker staging by count and bytes. A single oversized path
            // is immediately collected; only the shared batch partitions argv.
            if batch.paths.len() >= 32 || batch.bytes - command_bytes(&exec.args) >= 4096 {
                for file in &state.files {
                    file.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .flush()?;
                }
                state.errors += batch.collect(&state.shared, limit, effects)?;
            }
            return Ok(true);
        }
        let batch = state
            .batches
            .entry(exec.id)
            .or_insert_with(|| Batch::new(exec, directory.clone(), handle.clone()));
        let bytes = path.as_bytes().len() + 1;
        if batch.directory != directory || batch.full(bytes, limit) {
            for file in &state.files {
                file.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .flush()?;
            }
            state.errors += u64::from(!batch.run(effects)?);
        }
        batch.directory = directory;
        batch.handle = handle;
        batch.paths.push(path);
        batch.bytes += bytes;
        return Ok(true);
    }
    state.flush_files()?;
    if exec.prompt && !effects.confirm(&exec.args[0], entry.path())? {
        return Ok(false);
    }
    let args = exec
        .args
        .iter()
        .map(|arg| substitute(arg, &path))
        .collect::<Vec<_>>();
    spawn(
        &args,
        directory.as_deref(),
        handle.as_deref(),
        exec.prompt,
        effects,
    )
}

fn substitute(arg: &OsStr, path: &OsStr) -> OsString {
    let mut result = Vec::new();
    let mut bytes = arg.as_bytes();
    while let Some(at) = bytes.windows(2).position(|pair| pair == b"{}") {
        result.extend_from_slice(&bytes[..at]);
        result.extend_from_slice(path.as_bytes());
        bytes = &bytes[at + 2..];
    }
    result.extend_from_slice(bytes);
    OsString::from_vec(result)
}

fn exec_path(path: &Path) -> (PathBuf, OsString) {
    let bytes = path.as_os_str().as_bytes();
    let end = bytes
        .iter()
        .rposition(|&b| b != b'/')
        .map_or(0, |at| at + 1);
    if end == 0 {
        return (PathBuf::from("/"), OsString::from("/"));
    }
    let at = bytes[..end].iter().rposition(|&b| b == b'/');
    let (directory, name) = match at {
        Some(at) => (&bytes[..=at], &bytes[at + 1..]),
        None => (b".".as_slice(), bytes),
    };
    (
        PathBuf::from(OsStr::from_bytes(directory)),
        OsString::from_vec(
            [
                b"./".as_slice(),
                &name[..name.len() - (bytes.len() - end)],
                if end < bytes.len() {
                    b"/".as_slice()
                } else {
                    b"".as_slice()
                },
            ]
            .concat(),
        ),
    )
}

fn spawn(
    args: &[OsString],
    directory: Option<&Path>,
    handle: Option<&File>,
    close_stdin: bool,
    effects: &mut impl Effects,
) -> io::Result<bool> {
    effects.flush()?;
    // GNU closes fd 0 for interactive actions. A POSIX shell exec trampoline
    // provides that child-only operation without unsafe pre_exec hooks.
    let mut command = if close_stdin {
        let mut command = Command::new("sh");
        command
            .args(["-c", "exec \"$@\" <&-", "find-ok"])
            .args(args);
        command
    } else {
        let mut command = Command::new(&args[0]);
        command.args(&args[1..]);
        command
    };
    if let Some(handle) = handle {
        // CLOEXEC still allows the child to chdir through its inherited fd
        // before exec. This also works after the directory has been unlinked.
        command.current_dir(format!("/proc/self/fd/{}", handle.as_raw_fd()));
    } else if let Some(directory) = directory {
        command.current_dir(directory);
    }
    match effects.command(&mut command) {
        Ok(success) => Ok(success),
        Err(error) => {
            effects.error(&WalkError {
                path: PathBuf::from(&args[0]),
                error,
            });
            Ok(false)
        }
    }
}

pub(super) fn confirm(program: &OsStr, path: &Path) -> io::Result<bool> {
    static PROMPT: Mutex<()> = Mutex::new(());
    let _prompt = PROMPT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut stderr = io::stderr().lock();
    stderr.write_all(b"< ")?;
    stderr.write_all(program.as_bytes())?;
    stderr.write_all(b" ... ")?;
    stderr.write_all(path.as_os_str().as_bytes())?;
    stderr.write_all(b" > ? ")?;
    stderr.flush()?;
    let mut line = Vec::new();
    io::stdin().lock().read_until(b'\n', &mut line)?;
    Ok(matches!(line.first(), Some(b'y' | b'Y')))
}

#[cfg(test)]
pub(super) mod tests;
