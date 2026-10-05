//! `ferret find`: GNU syntax over catalog visibility or the unrestricted live
//! source.

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{Arc, Mutex};

use ferret_query::find::{Effects, OutputBuffer, Plan, WalkError};

use crate::cli::{self, Exit};
use crate::engine::Engine;
use crate::find_json::{self, FrameOutput};
use crate::protocol::ChildStdin;
use crate::xdg::Dirs;

pub(crate) const PERMISSION_WARNING: &str =
    "find: warning: -perm /000 now matches all files; use -perm -000 for the equivalent form";

/// Resolves the catalog a find plan should walk through: `None` for a live,
/// unindexed walk (`-I`, a configured `find_no_ignore`, or an information
/// plan), `Some` for a catalog-backed walk, or an error message (without a
/// `find: ` prefix) describing why neither is available.
fn resolve_catalog(plan: &Plan, index: Option<&Path>) -> Result<Option<Engine>, String> {
    if plan.no_ignore() || plan.is_information() {
        return Ok(None);
    }
    let dirs = Dirs::from_env();
    let config = Dirs::config_from_env().ok().map(|dir| dir.join("config"));
    let no_ignore = match config.as_deref().map(read_config).transpose() {
        Ok(value) => value.unwrap_or(false),
        Err(error) => {
            // read_config ran, so config was Some.
            let path = config.unwrap_or_default();
            return Err(format!("{}: {error}", path.display()));
        }
    };
    if no_ignore {
        return Ok(None);
    }
    let index = index
        .map(Path::to_owned)
        .or_else(|| {
            std::env::var_os("FERRET_INDEX")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| dirs.as_ref().ok().map(|dirs| dirs.data.clone()));
    let Some(index) = index else {
        return Err("cannot locate index; set --index or FERRET_INDEX, or use -I".to_owned());
    };
    match Engine::open(&index) {
        Ok(Some(engine)) => Ok(Some(engine)),
        Ok(None) => Err(format!(
            "no index in {}; run ferret index DIR or use -I",
            index.display()
        )),
        Err(error) => Err(format!(
            "cannot open index in {}: {error}; re-index or use -I",
            index.display()
        )),
    }
}

// Routing must precede engine open, but follow the same plan/config checks as
// local execution. Effects, information and configured live walks stay local.
fn remote(
    args: &[OsString],
    index: Option<&Path>,
    plan: &Plan,
    json: bool,
    now: std::time::SystemTime,
) -> Option<Exit> {
    if plan.has_side_effects() || plan.no_ignore() || plan.is_information() {
        return None;
    }
    let config = Dirs::config_from_env().ok()?.join("config");
    if read_config(&config).ok()? {
        return None;
    }
    let index = index
        .map(Path::to_owned)
        .or_else(|| {
            std::env::var_os("FERRET_INDEX")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| Dirs::from_env().ok().map(|dirs| dirs.data))?;
    crate::daemon::find(&index, args, json, now)
}

/// Runs a find command. Find errors and usage errors both exit 1; no matches
/// is success. Explicit -I never reads config or opens an index; neither mode
/// writes a query log.
pub fn run(args: &[OsString], index: Option<&Path>) -> Exit {
    let now = std::time::SystemTime::now();
    let plan = match Plan::parse_started(args, now) {
        Ok(plan) => plan,
        Err(error) => {
            cli::error(&format!("find: {error}"));
            return Exit::NoMatch;
        }
    };
    if let Some(feature) = plan.unsupported() {
        cli::error(&format!(
            "find: {}: not implemented yet",
            feature.to_string_lossy()
        ));
        return Exit::NoMatch;
    }
    if let Some(exit) = remote(args, index, &plan, false, now) {
        return exit;
    }
    let catalog = match resolve_catalog(&plan, index) {
        Ok(catalog) => catalog,
        Err(message) => {
            cli::error(&format!("find: {message}"));
            return Exit::NoMatch;
        }
    };
    if plan.permission_warning() {
        cli::error(PERMISSION_WARNING);
    }
    let mut effects = Output::Stdout;
    let workers = ferret_crawl::default_workers();
    let result = match catalog {
        Some(engine) => engine.pin().find(&plan, effects.clone(), workers),
        None => plan.run_parallel(plan.live_source(), effects.clone(), workers),
    };
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            cli::error(&format!(
                "find: {}: not implemented yet",
                error.feature.to_string_lossy()
            ));
            return Exit::NoMatch;
        }
    };
    let flushed = effects.flush();
    if let Err(error) = flushed {
        cli::error(&format!("find: writing stdout: {error}"));
        return Exit::NoMatch;
    }
    if outcome.errors == 0 {
        Exit::Ok
    } else {
        Exit::NoMatch
    }
}

/// Runs `ferret --json find ARGS...`: the CLI's own structured-output host.
/// Emits the same tagged begin/stdout/stderr/diagnostic/end events as a batch
/// find block (`crate::batch`), under the fixed id `"find"` — there is only
/// ever one find request in this process, so no id negotiation is needed.
/// The child's stdin inherits the caller's, as the raw CLI does;
/// `-ok`/`-okdir` still refuse outside a terminal (`FrameOutput::confirm`
/// checks that itself). There is no protocol caller here to withhold
/// `local-effects`/`interactive` from, so `find_json::refusal`'s capability
/// gate does not apply; this host runs every action the plan asks for. The
/// process exit status is find's native status (`Exit::Ok`/`Exit::NoMatch`);
/// parse and resolution failures are reported as a `diagnostic` plus an `end`
/// with the message, matching batch's shape, rather than a bare stderr line.
pub fn run_json(args: &[OsString], index: Option<&Path>) -> Exit {
    const ID: &str = "find";
    let started = std::time::Instant::now();
    let now = std::time::SystemTime::now();
    let end = |status: i128, error: Option<&str>| {
        find_json::emit(ID, "end", |o| {
            o.int("exit", status)
                .bool("cancelled", false)
                .int("elapsed_us", started.elapsed().as_micros() as i128);
            if let Some(error) = error {
                o.str("error", error);
            }
        })
    };
    let output_error = |error: io::Error| {
        cli::error(&format!("find: writing stdout: {error}"));
        Exit::NoMatch
    };
    let plan = match Plan::parse_started(args, now) {
        Ok(plan) => plan,
        Err(error) => {
            if let Err(error) = find_json::emit(ID, "begin", |o| find_json::generation(o, None)) {
                return output_error(error);
            }
            if let Err(error) = find_json::diagnostic(ID, "parse", "error", None) {
                return output_error(error);
            }
            if let Err(error) = end(1, Some(&error.to_string())) {
                return output_error(error);
            }
            return Exit::NoMatch;
        }
    };
    if let Some(feature) = plan.unsupported() {
        if let Err(error) = find_json::emit(ID, "begin", |o| find_json::generation(o, None)) {
            return output_error(error);
        }
        let message = feature.to_string_lossy();
        if let Err(error) = find_json::diagnostic(ID, &message, "error", None) {
            return output_error(error);
        }
        if let Err(error) = end(1, Some(&message)) {
            return output_error(error);
        }
        return Exit::NoMatch;
    }
    if let Some(exit) = remote(args, index, &plan, true, now) {
        return exit;
    }
    if let Err(error) = find_json::emit(ID, "begin", |o| find_json::generation(o, None)) {
        return output_error(error);
    }
    let catalog = match resolve_catalog(&plan, index) {
        Ok(catalog) => catalog,
        Err(message) => {
            if let Err(error) = find_json::diagnostic(ID, "resolve", "error", None) {
                return output_error(error);
            }
            if let Err(error) = end(1, Some(&message)) {
                return output_error(error);
            }
            return Exit::NoMatch;
        }
    };
    if plan.permission_warning()
        && let Err(error) = find_json::diagnostic(ID, "permission", "warning", None)
    {
        return output_error(error);
    }
    let host = FrameOutput::new(ID, ChildStdin::Inherit).with_warning_stderr();
    let workers = ferret_crawl::default_workers();
    let result = match catalog {
        Some(engine) => engine.pin().find(&plan, host.clone(), workers),
        None => plan.run_parallel(plan.live_source(), host.clone(), workers),
    };
    let status = match result {
        Ok(outcome) => i128::from(outcome.errors != 0),
        Err(error) => {
            if let Err(error) = find_json::diagnostic(ID, "runtime", "error", None) {
                return output_error(error);
            }
            if let Err(error) = end(1, Some(&error.to_string())) {
                return output_error(error);
            }
            return Exit::NoMatch;
        }
    };
    if let Err(error) = host.check_transport() {
        return output_error(error);
    }
    if let Err(error) = end(status, None) {
        return output_error(error);
    }
    if status == 0 { Exit::Ok } else { Exit::NoMatch }
}

#[derive(Clone)]
enum Output {
    Stdout,
    #[cfg(test)]
    Test(Arc<Mutex<Box<dyn Write + Send>>>),
}

impl Output {
    // The engine holds its transaction gate. Stdout's own lock covers the
    // destination, including its internal line buffer, without another mutex.
    fn with_writer(
        &mut self,
        write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
    ) -> io::Result<()> {
        match self {
            Self::Stdout => write(&mut io::stdout().lock()),
            #[cfg(test)]
            Self::Test(writer) => write(
                &mut **writer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            ),
        }
    }
    #[cfg(test)]
    fn test(writer: impl Write + Send + 'static) -> Self {
        Self::Test(Arc::new(Mutex::new(Box::new(writer))))
    }
}

impl Effects for Output {
    fn print(&mut self, path: &Path, nul: bool) -> io::Result<()> {
        self.with_writer(|writer| {
            writer.write_all(path.as_os_str().as_bytes())?;
            writer.write_all(if nul { b"\0" } else { b"\n" })
        })
    }
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.with_writer(|writer| writer.write_all(bytes))
    }
    fn flush(&mut self) -> io::Result<()> {
        self.with_writer(|writer| writer.flush())
    }
    fn output(&mut self, buffer: &mut OutputBuffer) -> io::Result<()> {
        self.with_writer(|writer| {
            buffer.write_to(writer)?;
            writer.flush()
        })
    }

    fn warning(&mut self, message: &str) {
        cli::error(&format!("find: {message}"));
    }

    fn error(&mut self, error: &WalkError) {
        cli::error(&format!("find: {}: {}", error.path.display(), error.error));
    }
}

// A single boolean needs no general configuration parser or new dependency.
fn read_config(path: &Path) -> io::Result<bool> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_config(&text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn parse_config(text: &str) -> io::Result<bool> {
    let mut no_ignore = None;
    for (line, text) in text.lines().enumerate() {
        let text = text.split('#').next().unwrap_or("").trim();
        if text.is_empty() {
            continue;
        }
        let value = text
            .strip_prefix("find_no_ignore")
            .and_then(|rest| rest.trim_start().strip_prefix('='))
            .map(str::trim);
        let value = match value {
            Some("true") => true,
            Some("false") => false,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("line {}: expected find_no_ignore = true or false", line + 1),
                ));
            }
        };
        if no_ignore.replace(value).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("line {}: duplicate find_no_ignore", line + 1),
            ));
        }
    }
    Ok(no_ignore.unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use std::io::BufWriter;

    use super::*;

    #[test]
    fn config_accepts_one_boolean_and_rejects_ambiguous_settings() {
        assert!(!parse_config("# empty\n").unwrap());
        assert!(parse_config("find_no_ignore = true # GNU mode\n").unwrap());
        assert!(!parse_config("find_no_ignore=false\n").unwrap());
        for text in [
            "find_no_ignore = yes",
            "other = true",
            "find_no_ignore = true\nfind_no_ignore = false",
        ] {
            assert!(parse_config(text).is_err(), "{text}");
        }
    }

    #[test]
    fn spilled_record_reaches_the_cli_destination_before_another_worker_flushes() {
        // R2 #4: force the next worker's complete short record between the
        // first worker's evaluation and completion flush, without scheduling
        // or sleeps. This drives the real evaluator and CLI Output host.
        use ferret_query::find::{Entry, EntrySource, FileKind, LiveWalk};
        struct Interleave {
            entry: Entry,
            yielded: bool,
            other: Option<(Plan, LiveWalk, Output)>,
        }
        impl EntrySource for Interleave {
            fn next(&mut self, _: bool) -> Option<Result<&Entry, WalkError>> {
                if !self.yielded {
                    self.yielded = true;
                    return Some(Ok(&self.entry));
                }
                if let Some((plan, mut walk, mut host)) = self.other.take() {
                    assert_eq!(plan.run(&mut walk, &mut host).unwrap().errors, 0);
                }
                None
            }
        }
        let directory = std::env::temp_dir().join(format!("ferret-r2-cli-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let long = directory.join("long");
        let short = directory.join("short");
        std::os::unix::fs::symlink("L".repeat(2000), &long).unwrap();
        std::os::unix::fs::symlink("S", &short).unwrap();
        let format = OsString::from(format!("{}\\n", "%l".repeat(100)));
        let make_plan = |path: &Path| {
            Plan::parse(&[
                "-I".into(),
                path.as_os_str().to_owned(),
                "-printf".into(),
                format.clone(),
            ])
            .unwrap()
        };
        let plan = make_plan(&long);
        let other = make_plan(&short);
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let mut host = Output::test(Sink(bytes.clone()));
        let mut source = Interleave {
            entry: Entry::new(long, 0, FileKind::Symlink),
            yielded: false,
            other: Some((other, make_plan(&short).live_source(), host.clone())),
        };
        let result = plan.run(&mut source, &mut host).unwrap();
        host.flush().unwrap();
        let shared = bytes.lock().unwrap();
        let records: Vec<_> = shared.split(|&byte| byte == b'\n').collect();
        std::fs::remove_dir_all(directory).unwrap();
        assert_eq!(result.errors, 0);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].len(), 200_000);
        assert!(records[0].iter().all(|&byte| byte == b'L'));
        assert_eq!(records[1].len(), 100);
        assert!(records[1].iter().all(|&byte| byte == b'S'));
        assert!(records[2].is_empty());
    }

    #[test]
    fn spilled_file_record_excludes_a_short_record_between_destination_writes() {
        // R2 #4: interpose after the first actual file-destination write.
        // The old single-action path allowed a second invocation of the same
        // prepared plan to insert a short record there. Whole transactions
        // bypass that byte-write seam; the source then runs the short record
        // after the long evaluation. No thread scheduling or sleeps are used.
        use ferret_query::find::{Entry, EntrySource, FileKind};
        use std::sync::atomic::{AtomicBool, Ordering};
        struct One {
            entry: Entry,
            yielded: bool,
        }
        impl EntrySource for One {
            fn next(&mut self, _: bool) -> Option<Result<&Entry, WalkError>> {
                if self.yielded {
                    None
                } else {
                    self.yielded = true;
                    Some(Ok(&self.entry))
                }
            }
        }
        fn short_record(plan: &Plan, path: &Path, mut host: Output) {
            let mut source = One {
                entry: Entry::new(path.to_owned(), 0, FileKind::Symlink),
                yielded: false,
            };
            assert_eq!(plan.run(&mut source, &mut host).unwrap().errors, 0);
        }
        struct Host {
            cli: Output,
            plan: Arc<Plan>,
            short: PathBuf,
            emitted: Arc<AtomicBool>,
        }
        impl Effects for Host {
            fn print(&mut self, path: &Path, nul: bool) -> io::Result<()> {
                self.cli.print(path, nul)
            }
            fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
                self.cli.write(bytes)
            }
            fn flush(&mut self) -> io::Result<()> {
                self.cli.flush()
            }
            fn error(&mut self, error: &WalkError) {
                panic!("{error:?}");
            }
            fn file(
                &mut self,
                file: &Arc<Mutex<Option<BufWriter<std::fs::File>>>>,
                bytes: &[u8],
            ) -> io::Result<()> {
                self.cli.file(file, bytes)?;
                if !self.emitted.swap(true, Ordering::SeqCst) {
                    short_record(&self.plan, &self.short, self.cli.clone());
                }
                Ok(())
            }
        }
        struct Source {
            first: One,
            plan: Arc<Plan>,
            short: PathBuf,
            cli: Output,
            emitted: Arc<AtomicBool>,
        }
        impl EntrySource for Source {
            fn next(&mut self, _: bool) -> Option<Result<&Entry, WalkError>> {
                if !self.first.yielded {
                    return self.first.next(true);
                }
                if !self.emitted.swap(true, Ordering::SeqCst) {
                    short_record(&self.plan, &self.short, self.cli.clone());
                }
                None
            }
        }
        let directory =
            std::env::temp_dir().join(format!("ferret-r2-cli-file-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let long = directory.join("long");
        let short = directory.join("short");
        let destination = directory.join("out");
        std::os::unix::fs::symlink("L".repeat(2000), &long).unwrap();
        std::os::unix::fs::symlink("S", &short).unwrap();
        let plan = Arc::new(
            Plan::parse(&[
                "-I".into(),
                long.as_os_str().to_owned(),
                "-fprintf".into(),
                destination.as_os_str().to_owned(),
                format!("{}\\n", "%l".repeat(100)).into(),
            ])
            .unwrap(),
        );
        let cli = Output::test(Vec::new());
        let emitted = Arc::new(AtomicBool::new(false));
        let mut host = Host {
            cli: cli.clone(),
            plan: plan.clone(),
            short: short.clone(),
            emitted: emitted.clone(),
        };
        let mut source = Source {
            first: One {
                entry: Entry::new(long, 0, FileKind::Symlink),
                yielded: false,
            },
            plan: plan.clone(),
            short,
            cli,
            emitted,
        };
        assert_eq!(plan.run(&mut source, &mut host).unwrap().errors, 0);
        let bytes = std::fs::read(&destination).unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        let records: Vec<_> = bytes.split(|&byte| byte == b'\n').collect();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].len(), 200_000);
        assert!(records[0].iter().all(|&byte| byte == b'L'));
        assert_eq!(records[1].len(), 100);
        assert!(records[1].iter().all(|&byte| byte == b'S'));
        assert!(records[2].is_empty());
    }

    #[test]
    fn a_batch_child_does_not_hold_back_another_workers_output() {
        // Two executor workers share the real CLI writer. A final execdir
        // batch waits for the other worker's committed output before exiting;
        // capturing under the writer lock used to make that wait time out.
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        struct Sink(mpsc::Sender<Vec<u8>>);
        impl Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.send(bytes.to_vec()).map_err(io::Error::other)?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let directory =
            std::env::temp_dir().join(format!("ferret-r1-output-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let entry = directory.join("entry");
        std::fs::write(&entry, b"").unwrap();
        let started = directory.join("started");
        let release = directory.join("release");
        let args = vec![
            OsString::from("-I"),
            entry.clone().into_os_string(),
            "-maxdepth".into(),
            "0".into(),
            "-execdir".into(),
            "timeout".into(),
            "8".into(),
            "sh".into(),
            "-c".into(),
            ": > \"$1\"; while ! test -e \"$2\"; do sleep 0.01; done; head -c 70000 /dev/zero"
                .into(),
            "sh".into(),
            started.clone().into_os_string(),
            release.clone().into_os_string(),
            "{}".into(),
            "+".into(),
        ];
        let plan = Plan::parse(&args).unwrap();
        let (sent, received) = mpsc::channel();
        let effects = Output::test(Sink(sent));
        let slow = effects.clone();
        let batch =
            std::thread::spawn(move || plan.run_parallel(plan.live_source(), slow, 1).unwrap());
        let deadline = Instant::now() + Duration::from_secs(3);
        while !started.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let child_started = started.exists();
        let fast = effects.clone();
        let fast_plan = Plan::parse(&[
            OsString::from("-I"),
            entry.into_os_string(),
            "-maxdepth".into(),
            "0".into(),
            "-printf".into(),
            "fast\n".into(),
        ])
        .unwrap();
        let other = std::thread::spawn(move || {
            fast_plan
                .run_parallel(fast_plan.live_source(), fast, 1)
                .unwrap()
        });
        let first = received.recv_timeout(Duration::from_secs(2));
        // Release and await both children before asserting, including failures.
        std::fs::write(&release, b"").unwrap();
        let batch = batch.join().unwrap();
        let other = other.join().unwrap();
        let mut bytes = Vec::new();
        if let Ok(first) = &first {
            bytes.extend_from_slice(first);
        }
        bytes.extend(received.try_iter().flatten());
        std::fs::remove_dir_all(directory).unwrap();
        assert!(child_started, "batch child did not start within 3 seconds");
        assert_eq!(
            first.unwrap(),
            b"fast\n",
            "another worker's output was held back"
        );
        assert_eq!(batch.errors + other.errors, 0);
        assert_eq!(bytes.len(), 5 + 70000);
        assert!(bytes[5..].iter().all(|byte| *byte == 0));
    }
}
