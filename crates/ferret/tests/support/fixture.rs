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
    command
        .current_dir(base)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_RUNTIME_DIR", home.join("runtime"))
        .env("FERRET_INDEX", base.join("index"))
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .stdin(Stdio::null());
}
