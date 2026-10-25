//! One command environment for CLI fixtures, including child commands.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Stdio};

pub fn command(binary: impl AsRef<OsStr>, base: &Path) -> Command {
    let mut command = Command::new(binary);
    isolate(&mut command, base);
    command
}

pub fn bounded_command(binary: impl AsRef<OsStr>, base: &Path) -> Command {
    let mut command = command("timeout", base);
    command.arg("15s").arg(binary);
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
        .env("FERRET_NO_DAEMON", "1")
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
