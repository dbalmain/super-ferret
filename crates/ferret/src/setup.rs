//! Files a new install starts with. Setup writes each once; after that the
//! file is the user's, and setup never overwrites it.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
    write_ignore_file_with_sequence(path, &NEXT_TEMP_ID)
}

fn write_ignore_file_with_sequence(path: &Path, sequence: &AtomicU64) -> io::Result<Written> {
    let dir = path.parent().unwrap_or(Path::new("."));
    if destination_exists(path)? {
        return kept(path);
    }

    // XDG: a base directory setup creates is 0700; one that exists keeps its
    // mode.
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;

    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let (temp, mut file) = create_temp(dir, &name, sequence)?;
    let written = file
        .write_all(DEFAULT_IGNORE.as_bytes())
        .and_then(|()| file.sync_all());
    if let Err(error) = written {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }

    let linked = fs::hard_link(&temp, path);
    let removed = fs::remove_file(&temp);
    match linked {
        Ok(()) => removed.map(|()| Written::Created),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => kept(path),
        Err(error) => {
            let _ = removed;
            Err(error)
        }
    }
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// Creates an owned temporary file, retrying names occupied by another setup
/// invocation. `sequence` is injectable so collision handling can be tested
/// without depending on process-global call order.
fn create_temp(dir: &Path, name: &str, sequence: &AtomicU64) -> io::Result<(PathBuf, fs::File)> {
    const MAX_ATTEMPTS: usize = 128;

    for _ in 0..MAX_ATTEMPTS {
        let id = sequence.fetch_add(1, Ordering::Relaxed);
        let temp = dir.join(format!(".{name}.{}-{id}.tmp", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => return Ok((temp, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a unique temporary file name",
    ))
}

/// Distinguishes an absent path from a dangling symlink, which is an existing
/// destination but cannot be kept as an ignore file.
fn destination_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Something already sits at `path`. Only a file (or a link to one) is the
/// user's ignore file; a directory or a dangling link is an error to report.
fn kept(path: &Path) -> io::Result<Written> {
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() => Ok(Written::Kept),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a file", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a file", path.display()),
        )),
        Err(error) => Err(error),
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
    // A create_new collision must never trigger cleanup of a file we did not
    // create.
    fn retries_a_temporary_name_collision_without_removing_the_existing_file() {
        let dir = scratch("temp-collision");
        fs::create_dir_all(&dir).unwrap();
        let sequence = AtomicU64::new(17);
        let collision = dir.join(format!(".ignore.{}-17.tmp", std::process::id()));
        fs::write(&collision, "belongs to another setup").unwrap();
        let destination = dir.join("ignore");

        assert_eq!(
            write_ignore_file_with_sequence(&destination, &sequence).unwrap(),
            Written::Created
        );

        assert_eq!(
            fs::read_to_string(collision).unwrap(),
            "belongs to another setup"
        );
        assert_eq!(fs::read_to_string(destination).unwrap(), DEFAULT_IGNORE);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    // Existing ignore files must be kept even when creating a temporary would
    // fail.
    fn existing_destination_is_kept_before_temporary_names_are_touched() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch("existing-before-temp");
        fs::create_dir_all(&dir).unwrap();
        let sequence = AtomicU64::new(0);
        let destination = dir.join("ignore");
        fs::write(&destination, "user rules").unwrap();
        let collision = dir.join(format!(".ignore.{}-0.tmp", std::process::id()));
        fs::write(&collision, "unrelated file").unwrap();

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let result = write_ignore_file_with_sequence(&destination, &sequence);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(result.unwrap(), Written::Kept);

        assert_eq!(sequence.load(Ordering::Relaxed), 0);
        assert_eq!(fs::read_to_string(collision).unwrap(), "unrelated file");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    // Exhausting collision retries must not delete any occupied candidate path.
    fn exhausted_temporary_collisions_are_left_untouched() {
        let dir = scratch("temp-collisions-exhausted");
        fs::create_dir_all(&dir).unwrap();
        let sequence = AtomicU64::new(40);
        for id in 40..40 + 128 {
            let collision = dir.join(format!(".ignore.{}-{id}.tmp", std::process::id()));
            fs::write(collision, "belongs to another setup").unwrap();
        }

        let error = create_temp(&dir, "ignore", &sequence).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        for id in 40..40 + 128 {
            let collision = dir.join(format!(".ignore.{}-{id}.tmp", std::process::id()));
            assert_eq!(
                fs::read_to_string(collision).unwrap(),
                "belongs to another setup"
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn keeps_a_symlink_to_an_existing_file() {
        let dir = scratch("valid-symlink");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        fs::write(&target, "user rules").unwrap();
        let path = dir.join("ignore");
        std::os::unix::fs::symlink(&target, &path).unwrap();

        assert_eq!(write_ignore_file(&path).unwrap(), Written::Kept);
        assert_eq!(fs::read_to_string(target).unwrap(), "user rules");
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
