//! Private, identity-named endpoints anchored to retained directory handles.
//! Linux /proc/self/fd intentionally resolves our own handle; supplied runtime
//! and ferret directory components are opened with O_NOFOLLOW.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

// Linux ABI constants. OpenOptionsExt accepts these flags but std exposes no
// names for them. This application and its context checks are Linux-specific.
const NOFOLLOW: i32 = 0o400000;
const DIRECTORY: i32 = 0o200000;

pub(super) struct Endpoint {
    _runtime: File,
    directory: File,
    pub(super) identity: String,
    name: String,
}

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "runtime directory must be owned by this uid with mode 0700",
    )
}

fn directory(path: &Path, uid: u32) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW | DIRECTORY)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o7777 != 0o700 {
        return Err(denied());
    }
    Ok(file)
}
fn anchored(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

impl Endpoint {
    pub(super) fn open(index: &Path) -> io::Result<Self> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or_else(denied)?;
        let uid = uid()?;
        let runtime = directory(&runtime, uid)?;
        let path = anchored(&runtime).join("ferret");
        match DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let directory = directory(&path, uid)?;
        let metadata = fs::metadata(index)?;
        if !metadata.is_dir() {
            return Err(io::Error::other("index is not a directory"));
        }
        let identity = format!("{:x}-{:x}", metadata.dev(), metadata.ino());
        let name = format!("{identity}.sock");
        Ok(Self {
            _runtime: runtime,
            directory,
            identity,
            name,
        })
    }
    pub(super) fn socket(&self) -> PathBuf {
        anchored(&self.directory).join(&self.name)
    }
    pub(super) fn private_file(&self, suffix: &str) -> io::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .append(suffix == "log")
            .truncate(false)
            .mode(0o600)
            .custom_flags(NOFOLLOW)
            .open(anchored(&self.directory).join(format!("{}.{}", self.identity, suffix)))?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != uid()?
            || metadata.mode() & 0o7777 != 0o600
            || metadata.nlink() != 1
        {
            return Err(denied());
        }
        Ok(file)
    }
    pub(super) fn remove_socket(&self) -> io::Result<()> {
        fs::remove_file(self.socket())
    }
}

pub(super) fn uid() -> io::Result<u32> {
    let status = fs::read_to_string("/proc/self/status")?;
    status
        .lines()
        .find_map(|line| {
            line.strip_prefix("Uid:")
                .and_then(|values| values.split_whitespace().nth(1))
                .and_then(|value| value.parse().ok())
        })
        .ok_or_else(|| io::Error::other("cannot read effective uid"))
}

/// Permissions authenticate same-user clients. Compare effective credentials
/// and supplementary groups, mount namespace and user namespace for live
/// lookup. We do not transfer cwd descriptors or authenticate a supplied
/// process id.
pub(super) fn context() -> io::Result<String> {
    let status = fs::read_to_string("/proc/self/status")?;
    let credentials = status
        .lines()
        .filter(|line| {
            line.starts_with("Uid:") || line.starts_with("Gid:") || line.starts_with("Groups:")
        })
        .collect::<Vec<_>>()
        .join(";");
    let mount = fs::metadata("/proc/self/ns/mnt")?;
    let user = fs::metadata("/proc/self/ns/user")?;
    Ok(format!(
        "{credentials};mnt:{}:{};user:{}:{}",
        mount.dev(),
        mount.ino(),
        user.dev(),
        user.ino()
    ))
}
