//! One command environment for CLI fixtures, including child commands.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// How long a fixture waits for a step that should finish before calling it a
/// hang. Every wait that guards against a hang, rather than asserting a
/// latency, uses this one value, so a correct run on a loaded machine (other
/// builds sharing the cores) is slow, never failed. A real hang still fails,
/// just later. Latency claims keep their own tight bounds next to the
/// assertion they make.
#[allow(dead_code)]
pub const HANG: Duration = Duration::from_secs(60);

/// [`HANG`] in milliseconds, for product deadlines a fixture sets through the
/// environment (`FERRET_DAEMON_STARTUP_MS`).
#[allow(dead_code)]
pub fn hang_ms() -> String {
    HANG.as_millis().to_string()
}

/// Where a fixture's commands record catalog-lock refusals; see
/// [`lock_refusals`].
const LOCK_DIAG: &str = "lock-refusals.log";

/// What every refused catalog lock under `base` saw at the moment of refusal:
/// the holders in `/proc/locks` and every process with the lock file open.
/// Empty when nothing was refused.
#[allow(dead_code)]
pub fn lock_refusals(base: &Path) -> String {
    std::fs::read_to_string(base.join(LOCK_DIAG)).unwrap_or_default()
}

/// A new, empty directory under the temporary directory, named `prefix`, the
/// process id and a per-process sequence number. Two fixtures never share
/// one: a clock-based name repeats when threads ask at the same moment (a
/// 32-thread burst of `SystemTime::now()` produced a duplicate in about 3% of
/// tries), and two tests sharing a base race each other's first `ferret index`
/// for its catalog lock. `create_dir` also skips a leftover from an earlier
/// process that had this id. The number is fixed-width so that no base is a
/// prefix of another: fixtures find their daemons by the base in argv.
#[allow(dead_code)]
pub fn reserve(prefix: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        let base = std::env::temp_dir().join(format!(
            "{prefix}-{}-{:08}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::create_dir(&base) {
            Ok(()) => return base,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => panic!("reserve fixture {base:?}: {error}"),
        }
    }
}

pub fn command(binary: impl AsRef<OsStr>, base: &Path) -> Command {
    let mut command = Command::new(binary);
    isolate(&mut command, base);
    command
}

/// Runs `binary` under `timeout(1)`. The process bound is twice [`HANG`], so a
/// deadline inside the command (a client waiting [`HANG`] for its daemon)
/// reports first, with its own message, rather than as exit 124.
pub fn bounded_command(binary: impl AsRef<OsStr>, base: &Path) -> Command {
    let mut command = command("timeout", base);
    command.arg(format!("{}s", 2 * HANG.as_secs())).arg(binary);
    command
}

fn isolate(command: &mut Command, base: &Path) {
    let home = base.join("home");
    // Lifecycle/semantic fixtures must not depend on the machine's current
    // pressure or battery. These are real procfs-shaped inputs to the
    // production Signals reader, not a policy bypass. Transition tests
    // inject the trait.
    // One directory serves every test process, so none is left behind per
    // run. Each file is renamed into place, so a concurrent process reads
    // either the previous copy or this one, never a partial write.
    static SIGNALS: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    let signals = SIGNALS.get_or_init(|| {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fixture-signals");
        std::fs::create_dir_all(path.join("proc/pressure"))
            .unwrap_or_else(|e| panic!("signal fixture: {e}"));
        std::fs::create_dir_all(path.join("power"))
            .unwrap_or_else(|e| panic!("signal fixture: {e}"));
        for (file, value) in [
            ("pressure/cpu", "some avg10=0.00 avg60=0.00\n"),
            ("pressure/io", "some avg10=0.00 avg60=0.00\n"),
            ("meminfo", "MemAvailable: 1073741824 kB\n"),
            ("loadavg", "0.00 0.00 0.00 1/100 1\n"),
        ] {
            let target = path.join("proc").join(file);
            let partial = target.with_extension(format!("{}.partial", std::process::id()));
            std::fs::write(&partial, value)
                .and_then(|()| std::fs::rename(&partial, &target))
                .unwrap_or_else(|e| panic!("signal fixture: {e}"));
        }
        path
    });
    command
        .current_dir(base)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_RUNTIME_DIR", home.join("runtime"))
        .env("FERRET_INDEX", base.join("index"))
        // Debug builds record who held the catalog lock when one refuses it.
        .env("FERRET_LOCK_DIAG", base.join(LOCK_DIAG))
        .env("FERRET_NO_DAEMON", "1")
        // The notice depends on the index's wall-clock age: a slow run
        // passes its minute and gains a line no expectation includes.
        .env("FERRET_STALE_NOTICE", "0")
        // A daemon a test spawns must not outlive the run by default.
        .env("FERRET_DAEMON_IDLE_MS", "60000")
        .env("FERRET_SIGNAL_PROC", signals.join("proc"))
        .env("FERRET_SIGNAL_POWER", signals.join("power"))
        // Background index threads keep normal priority, so deadlines hold
        // while other work loads the machine. Tests of the nice drop set 19.
        .env("FERRET_INDEX_NICE", "0")
        .env("GIT_CONFIG_GLOBAL", home.join("gitconfig"))
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .stdin(Stdio::null());
}
