#![allow(clippy::unwrap_used)]

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Isolated, deliberately unindexed fixture for uncovered grep operands.
pub struct Fixture {
    pub base: PathBuf,
    pub tree: PathBuf,
}

impl Fixture {
    pub fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "ferret-grep-uncovered-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let tree = base.join("tree");
        fs::create_dir_all(&tree).unwrap();
        for path in [
            "home/config/ferret",
            "home/data",
            "home/state",
            "home/runtime",
        ] {
            fs::create_dir_all(base.join(path)).unwrap();
        }
        fs::write(base.join("home/config/ferret/ignore"), b"# fixture\n").unwrap();
        Self { base, tree }
    }

    pub fn write(&self, path: &str, bytes: &[u8]) {
        let path = self.tree.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    pub fn repo(&self) {
        fs::create_dir_all(self.tree.join(".git/info")).unwrap();
    }

    pub fn command(&self, binary: &OsStr, cwd: &Path) -> Command {
        let mut command = Command::new(binary);
        command
            .current_dir(cwd)
            .env_clear()
            .env("HOME", self.base.join("home"))
            .env("XDG_CONFIG_HOME", self.base.join("home/config"))
            .env("XDG_DATA_HOME", self.base.join("home/data"))
            .env("XDG_STATE_HOME", self.base.join("home/state"))
            .env("XDG_RUNTIME_DIR", self.base.join("home/runtime"))
            .env("FERRET_NO_DAEMON", "1")
            .env("LC_ALL", "C.UTF-8")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    pub fn oracle(&self, cwd: &Path, args: &[&str]) -> Output {
        let binary = std::env::var_os("FERRET_RG")
            .unwrap_or_else(|| panic!("nix develop supplies the pinned FERRET_RG oracle"));
        let version = Command::new(&binary).arg("--version").output().unwrap();
        assert!(
            version.stdout.starts_with(b"ripgrep 15.2.0\n"),
            "wrong oracle: {version:?}"
        );
        let mut command = self.command(&binary, cwd);
        command.args(args);
        command.output().unwrap()
    }

    pub fn ferret(&self, cwd: &Path, args: &[&str]) -> Output {
        let mut command = self.command(OsStr::new(env!("CARGO_BIN_EXE_ferret")), cwd);
        command
            .arg("--index")
            .arg(self.base.join("index"))
            .arg("grep")
            .args(args);
        command.output().unwrap()
    }

    pub fn compare(&self, cwd: &Path, args: &[&str], names: bool) {
        let rg = self.oracle(cwd, args);
        let ferret = self.ferret(cwd, args);
        assert_equal(&rg, &ferret, names);
    }

    pub fn compare_all(&self, cwd: &Path, cases: &[(&[&str], bool)]) {
        let outputs: Vec<_> = cases
            .iter()
            .map(|(args, names)| {
                let rg = self.oracle(cwd, args);
                let ferret = self.ferret(cwd, args);
                (*args, *names, rg, ferret)
            })
            .collect();
        let mut differences = Vec::new();
        for (args, names, rg, ferret) in outputs {
            if !equal(&rg, &ferret, names) {
                differences.push(format!("{args:?}: rg: {rg:?}; ferret: {ferret:?}"));
            }
        }
        assert!(differences.is_empty(), "{}", differences.join("\n"));
    }
}

fn equal(rg: &Output, ferret: &Output, names: bool) -> bool {
    ferret.status.code() == rg.status.code()
        && if names {
            sorted_lines(&ferret.stdout) == sorted_lines(&rg.stdout)
        } else {
            ferret.stdout == rg.stdout
        }
}

fn assert_equal(rg: &Output, ferret: &Output, names: bool) {
    assert!(equal(rg, ferret, names), "rg: {rg:?}\nferret: {ferret:?}");
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn sorted_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut lines: Vec<_> = bytes
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect();
    lines.sort_unstable();
    lines
}
