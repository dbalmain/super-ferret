//! The index's settings: [`FILE`] in the index directory, a small versioned
//! text file that `ferret index` writes when it creates the index, and that
//! a person, or later a calibration, may edit (D77).
//!
//! The format is one `key = value` per line; blank lines and lines starting
//! with `#` are ignored. `version = 1` is required. Keys:
//!
//! - `search_threads`: at most this many threads verify a search's content.
//!   Seeded with the physical cores counted at creation; absent when they could
//!   not be counted. A search still caps it at `available_parallelism` (see
//!   `engine::search_threads`).
//!
//! An unknown key, a repeated key or a value that does not parse makes the
//! whole file invalid, so a typo in a hand edit is reported rather than
//! ignored. A reader treats an invalid file as absent.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::atomic::AtomicU64;

/// The settings file's name inside the index directory.
pub const FILE: &str = "settings";

/// Where Linux describes the CPUs: one `cpuN` directory per logical CPU.
pub const SYSFS_CPU: &str = "/sys/devices/system/cpu";

const VERSION: u32 = 1;

/// What the settings file holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// The most threads a search verifies content on, before the
    /// `available_parallelism` cap.
    pub search_threads: Option<usize>,
}

/// Why a settings file could not be used.
#[derive(Debug)]
pub enum ReadError {
    Io(io::Error),
    /// Line `line` (from 1) is not valid; `0` for the file as a whole.
    Parse {
        line: usize,
        reason: String,
    },
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Parse { line: 0, reason } => f.write_str(reason),
            Self::Parse { line, reason } => write!(f, "line {line}: {reason}"),
        }
    }
}

impl Settings {
    /// The settings a new index starts with, from the CPU topology under
    /// `cpu_dir` (normally [`SYSFS_CPU`]).
    pub fn detect(cpu_dir: &Path) -> Self {
        Self {
            search_threads: physical_cores(cpu_dir),
        }
    }

    /// Reads `index`'s settings file. `Ok(None)` when there is none, as in
    /// an index created before the file existed.
    pub fn read(index: &Path) -> Result<Option<Self>, ReadError> {
        match fs::read_to_string(index.join(FILE)) {
            Ok(text) => Self::parse(&text).map(Some),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => Err(ReadError::Parse {
                line: 0,
                reason: "not UTF-8".into(),
            }),
            Err(e) => Err(ReadError::Io(e)),
        }
    }

    /// Parses the file's text.
    pub fn parse(text: &str) -> Result<Self, ReadError> {
        let mut settings = Self::default();
        let mut version = None;
        let mut seen = BTreeSet::new();
        for (index, line) in text.lines().enumerate() {
            let line_number = index + 1;
            let fail = |reason: String| ReadError::Parse {
                line: line_number,
                reason,
            };
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(fail("expected `key = value`".into()));
            };
            let (key, value) = (key.trim(), value.trim());
            if !seen.insert(key) {
                return Err(fail(format!("`{key}` is set twice")));
            }
            match key {
                "version" => match value.parse::<u32>() {
                    Ok(VERSION) => version = Some(VERSION),
                    _ => return Err(fail(format!("unsupported version `{value}`"))),
                },
                "search_threads" => match value.parse::<usize>() {
                    Ok(threads) if threads > 0 => settings.search_threads = Some(threads),
                    _ => {
                        return Err(fail(format!(
                            "search_threads must be a positive number, not `{value}`"
                        )));
                    }
                },
                _ => return Err(fail(format!("unknown setting `{key}`"))),
            }
        }
        match version {
            Some(_) => Ok(settings),
            None => Err(ReadError::Parse {
                line: 0,
                reason: "no `version` line".into(),
            }),
        }
    }

    /// The file's text: what [`Settings::parse`] reads back, with comments
    /// for whoever edits it.
    pub fn render(&self) -> String {
        let mut text = format!(
            "# Super Ferret index settings, written when the index was created.\n\
             # Edit by hand if you like: one `key = value` per line.\n\
             version = {VERSION}\n\
             \n\
             # At most this many threads verify a search's content, capped by the\n\
             # CPUs this process may use. Set to the physical cores counted when\n\
             # the index was created. FERRET_SEARCH_THREADS overrides it.\n"
        );
        match self.search_threads {
            Some(threads) => text.push_str(&format!("search_threads = {threads}\n")),
            None => text.push_str(
                "# The cores could not be counted, so searches use at most 16.\n\
                 # search_threads = 16\n",
            ),
        }
        text
    }

    /// Writes the settings into `index` unless a file is already there,
    /// which is kept: it may hold someone's edits. The text goes to a
    /// temporary file that is renamed into place, so a reader sees the old
    /// state or the whole new file. Returns whether it wrote.
    pub fn write_new(&self, index: &Path) -> io::Result<bool> {
        let path = index.join(FILE);
        match fs::symlink_metadata(&path) {
            Ok(_) => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);
        let (temp, mut file) = crate::setup::create_temp(index, FILE, &NEXT_TEMP_ID)?;
        let written = file
            .write_all(self.render().as_bytes())
            .and_then(|()| file.sync_all())
            .and_then(|()| fs::rename(&temp, &path));
        if let Err(e) = written {
            let _ = fs::remove_file(&temp);
            return Err(e);
        }
        crate::setup::sync_dir(index)?;
        Ok(true)
    }
}

/// Physical cores: the distinct `topology/core_cpus_list` values over the
/// online `cpuN` directories under `cpu_dir`. Hardware threads of one core
/// share a list (`0,16`), so each core counts once. A CPU without an
/// `online` file is online (the kernel omits it for the boot CPU). `None`
/// when no CPU is found or an online CPU's list cannot be read: a partial
/// count would be a guess.
pub fn physical_cores(cpu_dir: &Path) -> Option<usize> {
    let mut cores = BTreeSet::new();
    for entry in fs::read_dir(cpu_dir).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let is_cpu = name
            .to_str()
            .and_then(|name| name.strip_prefix("cpu"))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if !is_cpu {
            continue;
        }
        let dir = entry.path();
        match fs::read_to_string(dir.join("online")) {
            Ok(online) if online.trim() == "0" => continue,
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        let list = fs::read_to_string(dir.join("topology/core_cpus_list")).ok()?;
        cores.insert(list.trim().to_owned());
    }
    (!cores.is_empty()).then_some(cores.len())
}

#[cfg(test)]
mod tests;
