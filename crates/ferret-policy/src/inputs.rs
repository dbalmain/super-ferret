//! Git config discovery delegates parsing/includes/path expansion to git's
//! binary. Returned inputs are the same consultations used for the rules.
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Config dependencies and the effective git global exclude path. Missing
/// config files remain dependencies so later creation is observed.
pub struct GitInputs {
    pub paths: Vec<PathBuf>,
    pub excludes: Option<PathBuf>,
}
impl GitInputs {
    /// Reads system, global and repository config, including include files.
    /// `gitdir` can be a held descriptor path; no repository source is read.
    pub fn discover(gitdir: &Path) -> io::Result<Self> {
        let mut paths = Vec::new();
        paths.push(
            std::env::var_os("GIT_CONFIG_SYSTEM")
                .map_or_else(|| PathBuf::from("/etc/gitconfig"), PathBuf::from),
        );
        if let Some(global) = std::env::var_os("GIT_CONFIG_GLOBAL") {
            paths.push(global.into());
        } else if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            paths.push(home.join(".gitconfig"));
            let config = std::env::var_os("XDG_CONFIG_HOME")
                .map_or_else(|| home.join(".config"), PathBuf::from);
            paths.push(config.join("git/config"));
        }
        paths.push(gitdir.join("config"));
        paths.retain(|p| p != Path::new("/dev/null"));
        if !paths.iter().any(|p| p.exists()) {
            return Ok(Self {
                paths,
                excludes: None,
            });
        }
        let command = || {
            let mut command = Command::new("git");
            command.arg("--git-dir").arg(gitdir).arg("config");
            command
        };
        let output = command()
            .args(["--null", "--show-origin", "--includes", "--list"])
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        for pair in output
            .stdout
            .split(|b| *b == 0)
            .collect::<Vec<_>>()
            .chunks(2)
        {
            if let Some(origin) = pair.first().and_then(|s| s.strip_prefix(b"file:")) {
                paths.push(PathBuf::from(OsStr::from_bytes(origin)));
            }
        }
        let output = command()
            .args(["--null", "--path", "--get", "core.excludesFile"])
            .output()?;
        let excludes = if output.status.success() {
            Some(PathBuf::from(OsString::from(OsStr::from_bytes(
                output.stdout.strip_suffix(&[0]).unwrap_or(&output.stdout),
            ))))
        } else if output.status.code() == Some(1) {
            None
        } else {
            return Err(io::Error::other(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        };
        if let Some(path) = &excludes {
            paths.push(path.clone());
        }
        paths.sort();
        paths.dedup();
        Ok(Self { paths, excludes })
    }
}
