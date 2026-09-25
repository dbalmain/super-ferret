//! Files a new install starts with. Setup writes each once; after that the
//! file is the user's, and setup never overwrites it.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

use ferret_policy::DEFAULT_IGNORE;

/// What [`write_ignore_file`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Written {
    /// The file did not exist and now holds the defaults.
    Created,
    /// The file already existed and was left as it was.
    Kept,
}

/// Seeds the global ignore file at `path` with [`DEFAULT_IGNORE`], creating
/// its directory. An existing file, even an empty one, is kept: emptying it
/// is how a user opts out of every default.
///
/// The bytes go to a temporary file first and are linked into place only once
/// they are on disk, so a failed or interrupted setup never leaves an empty
/// file that a retry would mistake for that opt-out.
pub fn write_ignore_file(path: &Path) -> io::Result<Written> {
    let dir = path.parent().unwrap_or(Path::new("."));
    // XDG: a base directory setup creates is 0700; one that exists keeps its
    // mode.
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let linked = write_synced(&temp).and_then(|()| fs::hard_link(&temp, path));
    let removed = fs::remove_file(&temp);
    match linked {
        Ok(()) => removed.map(|()| Written::Created),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => kept(path),
        Err(error) => Err(error),
    }
}

fn write_synced(temp: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(temp)?;
    file.write_all(DEFAULT_IGNORE.as_bytes())?;
    file.sync_all()
}

/// Something already sits at `path`. Only a file (or a link to one) is the
/// user's ignore file; a directory or a dangling link is an error to report.
fn kept(path: &Path) -> io::Result<Written> {
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() => Ok(Written::Kept),
        _ => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a file", path.display()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A fresh directory for one test, removed first if a past run left it.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ferret-setup-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn creates_the_file_and_its_directory() {
        let dir = scratch("create");
        let path = dir.join("ferret/ignore");
        assert_eq!(write_ignore_file(&path).unwrap(), Written::Created);
        assert_eq!(fs::read_to_string(&path).unwrap(), DEFAULT_IGNORE);
        let left: Vec<_> = fs::read_dir(dir.join("ferret"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["ignore"], "the temporary file is removed");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn never_overwrites_an_edited_file() {
        let dir = scratch("keep");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ignore");
        fs::write(&path, "").unwrap();
        assert_eq!(write_ignore_file(&path).unwrap(), Written::Kept);
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_created_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("mode");
        write_ignore_file(&dir.join("ferret/ignore")).unwrap();
        let mode = fs::metadata(dir.join("ferret"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn something_other_than_a_file_is_an_error() {
        let dir = scratch("dir");
        let path = dir.join("ignore");
        fs::create_dir_all(&path).unwrap();
        assert!(write_ignore_file(&path).is_err());
        let dangling = dir.join("dangling");
        std::os::unix::fs::symlink(dir.join("nowhere"), &dangling).unwrap();
        assert!(write_ignore_file(&dangling).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
