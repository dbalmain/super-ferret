#![allow(clippy::unwrap_used)]

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Isolated, deliberately unindexed fixture for uncovered grep operands.
pub struct Fixture {
    pub base: PathBuf,
    pub tree: PathBuf,
    /// Extra environment for both binaries.
    pub env: std::cell::RefCell<Vec<(&'static str, PathBuf)>>,
}

impl Fixture {
    pub fn new(name: &str) -> Self {
        let base = scratch().join(format!(
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
        Self {
            base,
            tree,
            env: Default::default(),
        }
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
            .env("FERRET_INDEX", self.base.join("index"))
            .env("LC_ALL", "C.UTF-8")
            .envs(self.env.borrow().iter().cloned())
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

    pub fn index(&self, path: &Path) {
        let mut command = self.command(OsStr::new(env!("CARGO_BIN_EXE_ferret")), &self.tree);
        let output = command.arg("index").arg(path).output().unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    pub fn compare(&self, cwd: &Path, args: &[&str], names: bool) {
        let rg = self.oracle(cwd, args);
        let ferret = self.ferret(cwd, args);
        assert_equal(&rg, &ferret, names);
        // Every recursive name-producing content case also exercises --files.
        if names && args.contains(&"-l") {
            let files: Vec<_> = args
                .iter()
                .copied()
                .filter(|arg| *arg != "-l" && *arg != "hit")
                .collect();
            let mut files = files;
            files.insert(0, "--files");
            assert_equal(&self.oracle(cwd, &files), &self.ferret(cwd, &files), true);
        }
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

/// A scratch directory with no `.git` in any ancestor. Rg treats any `.git`
/// above a path as a work tree, so a stray `/tmp/.git` would silently turn
/// every outside-git case into an inside-git one on that machine alone.
fn scratch() -> PathBuf {
    let candidates = [
        std::env::temp_dir(),
        PathBuf::from("/dev/shm"),
        PathBuf::from("/var/tmp"),
    ];
    candidates
        .iter()
        .filter_map(|dir| fs::canonicalize(dir).ok())
        .find(|dir| {
            dir.ancestors()
                .all(|ancestor| fs::symlink_metadata(ancestor.join(".git")).is_err())
        })
        .unwrap_or_else(|| panic!("every scratch candidate has a .git ancestor: {candidates:?}"))
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

/// Output lines as text, sorted, for pinning a file set.
pub fn sorted(bytes: &[u8]) -> Vec<&str> {
    let mut lines: Vec<_> = std::str::from_utf8(bytes).unwrap().lines().collect();
    lines.sort_unstable();
    lines
}
