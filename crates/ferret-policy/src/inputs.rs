//! Git config discovery delegates parsing, includes and path expansion to the
//! git binary. The callback is the dependency source for the caller's watches.
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The effective git global exclude path. Consultations, including missing
/// inputs, are delivered before their rules are used.
pub struct GitInputs {
    pub excludes: Option<PathBuf>,
}
impl GitInputs {
    /// Reads system, global and repository config, including include files.
    /// Held parent descriptor paths are valid for `gitdir` and `worktree`.
    /// Config is explicit so discovery also works for a minimal `.git`
    /// directory.
    pub fn discover(
        gitdir: &Path,
        worktree: &Path,
        mut consulted: impl FnMut(&Path),
    ) -> io::Result<Self> {
        let mut sources = Vec::new();
        if std::env::var_os("GIT_CONFIG_NOSYSTEM").is_none() {
            sources.push(
                std::env::var_os("GIT_CONFIG_SYSTEM")
                    .map_or_else(|| PathBuf::from("/etc/gitconfig"), PathBuf::from),
            );
        }
        if let Some(global) = std::env::var_os("GIT_CONFIG_GLOBAL") {
            sources.push(global.into());
        } else if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            let config = std::env::var_os("XDG_CONFIG_HOME")
                .map_or_else(|| home.join(".config"), PathBuf::from);
            sources.push(config.join("git/config"));
            sources.push(home.join(".gitconfig"));
        }
        sources.push(gitdir.join("config"));
        sources.retain(|p| p != Path::new("/dev/null"));
        let mut excludes = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|p| p.join("git/ignore"));
        for source in sources {
            consulted(&source);
            match std::fs::symlink_metadata(&source) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
                Ok(_) => {}
            }
            let command = || {
                let mut command = Command::new("git");
                command
                    .current_dir(worktree)
                    .arg("--git-dir")
                    .arg(gitdir)
                    .arg("config")
                    .arg("--file")
                    .arg(&source)
                    .arg("--includes");
                command
            };
            let mut paths = vec![source.clone()];
            // Discover includes, arm them, then read again. An include can
            // appear during discovery; never use pre-watch rules from it.
            for attempt in 0..16 {
                let output = command()
                    .args(["--null", "--show-origin", "--list"])
                    .output()?;
                if !output.status.success() {
                    return Err(io::Error::other(
                        String::from_utf8_lossy(&output.stderr).into_owned(),
                    ));
                }
                let before = paths.len();
                for pair in output
                    .stdout
                    .split(|b| *b == 0)
                    .collect::<Vec<_>>()
                    .chunks(2)
                {
                    let Some(origin) = pair.first().and_then(|s| s.strip_prefix(b"file:")) else {
                        continue;
                    };
                    let origin = absolute(worktree, Path::new(OsStr::from_bytes(origin)));
                    if !paths.contains(&origin) {
                        consulted(&origin);
                        paths.push(origin.clone());
                    }
                    if let Some(record) = pair.get(1)
                        && let Some(at) = record.iter().position(|b| *b == b'\n')
                    {
                        let key = &record[..at];
                        if key == b"include.path"
                            || (key.starts_with(b"includeif.") && key.ends_with(b".path"))
                        {
                            let include = expand(
                                origin.parent().unwrap_or(worktree),
                                Path::new(OsStr::from_bytes(&record[at + 1..])),
                            );
                            if !paths.contains(&include) {
                                consulted(&include);
                                paths.push(include);
                            }
                        }
                    }
                }
                if paths.len() == before {
                    break;
                }
                if attempt == 15 {
                    return Err(io::Error::other(
                        "git policy dependencies did not stabilize",
                    ));
                }
            }
            let output = command()
                .args(["--null", "--path", "--get", "core.excludesFile"])
                .output()?;
            if output.status.success() {
                let raw = output.stdout.strip_suffix(&[0]).unwrap_or(&output.stdout);
                excludes = (!raw.is_empty())
                    .then(|| absolute(worktree, Path::new(OsStr::from_bytes(raw))));
            } else if output.status.code() != Some(1) {
                return Err(io::Error::other(
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                ));
            }
        }
        if let Some(path) = &excludes {
            consulted(path);
        }
        Ok(Self { excludes })
    }
}
fn absolute(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    }
}
fn expand(base: &Path, path: &Path) -> PathBuf {
    if let Ok(suffix) = path.strip_prefix("~")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(suffix);
    }
    absolute(base, path)
}
