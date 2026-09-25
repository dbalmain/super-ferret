//! Files a new install starts with. Setup writes each once; after that the
//! file is the user's, and setup never overwrites it.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
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
pub fn write_ignore_file(path: &Path) -> io::Result<Written> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(Written::Kept),
        Err(error) => return Err(error),
    };
    file.write_all(DEFAULT_IGNORE.as_bytes())?;
    Ok(Written::Created)
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
}
