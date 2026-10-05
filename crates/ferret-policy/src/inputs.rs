//! Git config discovery delegates parsing, includes and path expansion to the
//! git binary. The callback is the dependency source for the caller's watches.
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
        let mut present = Vec::new();
        for source in sources {
            let source = absolute(worktree, &source);
            consulted(&source);
            match std::fs::metadata(&source) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
                Ok(m) if !m.is_file() => {
                    return Err(io::Error::other("git config is not a regular file"));
                }
                Ok(_) => present.push(source),
            }
        }
        if !present.is_empty() {
            // One ordered include document preserves cross-scope conditional
            // includes, while --file also permits a minimal .git directory.
            let merged = ConfigFile::new(&present)?;
            let command = || {
                let mut command = Command::new("git");
                command
                    .current_dir(worktree)
                    .arg("--git-dir")
                    .arg(gitdir)
                    .arg("config")
                    .arg("--file")
                    .arg(&merged.0)
                    .arg("--includes");
                command
            };
            let mut paths = present;
            // Discover includes, arm them, then read again. Never use rules
            // from a newly found dependency before its parent is watched.
            for attempt in 0..16 {
                let output = bounded(command().args(["--null", "--show-origin", "--list"]))?;
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
                    if origin != merged.0 && !paths.contains(&origin) {
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
            let output =
                bounded(command().args(["--null", "--path", "--get", "core.excludesFile"]))?;
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

/// Private, short-lived input to git, never a watch dependency.
struct ConfigFile(PathBuf);
impl ConfigFile {
    fn new(sources: &[PathBuf]) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ferret-git-{}-{}.config",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let config = Self(path);
        for source in sources {
            file.write_all(b"[include]\npath = \"")?;
            for &byte in source.as_os_str().as_bytes() {
                file.write_all(match byte {
                    b'\\' => b"\\\\",
                    b'"' => b"\\\"",
                    b'\n' => b"\\n",
                    b'\t' => b"\\t",
                    b'\x08' => b"\\b",
                    _ => std::slice::from_ref(&byte),
                })?;
            }
            file.write_all(b"\"\n")?;
        }
        Ok(config)
    }
}
impl Drop for ConfigFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A FIFO include or unusually large config is a typed observation failure,
/// not an unbounded writer stall. Only active subprocess waits use this timer.
fn bounded(command: &mut Command) -> io::Result<Output> {
    const CAP: u64 = 4 << 20;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing git stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing git stderr"))?;
    std::thread::scope(|scope| {
        let read = |pipe: Box<dyn Read + Send>| {
            let mut bytes = Vec::new();
            pipe.take(CAP + 1).read_to_end(&mut bytes)?;
            Ok::<_, io::Error>(bytes)
        };
        let out = scope.spawn(move || read(Box::new(stdout)));
        let err = scope.spawn(move || read(Box::new(stderr)));
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break Ok(status);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "git policy discovery timed out",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let stdout = out
            .join()
            .map_err(|_| io::Error::other("git stdout reader panicked"))??;
        let stderr = err
            .join()
            .map_err(|_| io::Error::other("git stderr reader panicked"))??;
        if stdout.len() as u64 > CAP || stderr.len() as u64 > CAP {
            return Err(io::Error::other("git config output exceeds 4 MiB"));
        }
        Ok(Output {
            status: status?,
            stdout,
            stderr,
        })
    })
}
