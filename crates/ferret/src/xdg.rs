//! Where ferret keeps its files, per the XDG Base Directory specification.
//!
//! Each base comes from its `XDG_*_HOME` variable when that is set to an
//! absolute path, and otherwise from its default under `$HOME`; the spec says
//! an empty or relative value is ignored. Ferret's own directory is `ferret`
//! beneath each base. The system-wide `XDG_CONFIG_DIRS` / `XDG_DATA_DIRS`
//! are not read: like git's global ignore file, ferret's files are per user.
//!
//! Pure over an environment lookup, so tests never touch the process
//! environment; [`Dirs::from_env`] is the one caller of `std::env`.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

/// Ferret's per-user directories. None is created here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dirs {
    /// `$XDG_CONFIG_HOME/ferret`: files the user edits.
    pub config: PathBuf,
    /// `$XDG_DATA_HOME/ferret`: the catalog and index.
    pub data: PathBuf,
    /// `$XDG_STATE_HOME/ferret`: the query log.
    pub state: PathBuf,
    /// `$XDG_CACHE_HOME/ferret`: anything safe to delete.
    pub cache: PathBuf,
}

/// A base directory that could not be resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The `XDG_*_HOME` variable named here gave no usable path, and `HOME`
    /// is unset, empty or relative, so there is no default to fall back on.
    NoHome {
        /// The variable that needed the fallback, e.g. `XDG_CONFIG_HOME`.
        var: &'static str,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHome { var } => write!(
                f,
                "cannot place ferret's files: {var} is not an absolute path \
                 and HOME is not set to one"
            ),
        }
    }
}

impl std::error::Error for Error {}

impl Dirs {
    /// Resolves from the process environment.
    pub fn from_env() -> Result<Self, Error> {
        Self::resolve(|name| std::env::var_os(name))
    }

    /// Resolves from `var`, which returns an environment variable's value.
    pub fn resolve(var: impl Fn(&str) -> Option<OsString>) -> Result<Self, Error> {
        Ok(Self {
            config: base(&var, "XDG_CONFIG_HOME", ".config")?,
            data: base(&var, "XDG_DATA_HOME", ".local/share")?,
            state: base(&var, "XDG_STATE_HOME", ".local/state")?,
            cache: base(&var, "XDG_CACHE_HOME", ".cache")?,
        })
    }

    /// Resolves only the configuration directory. Find's config setting must
    /// work even when an unused data/state/cache base cannot be resolved.
    pub fn config_from_env() -> Result<PathBuf, Error> {
        base(&|name| std::env::var_os(name), "XDG_CONFIG_HOME", ".config")
    }

    /// The user's global ignore file, which setup seeds with the defaults.
    pub fn ignore_file(&self) -> PathBuf {
        self.config.join("ignore")
    }
}

fn base(
    var: &impl Fn(&str) -> Option<OsString>,
    name: &'static str,
    default: &str,
) -> Result<PathBuf, Error> {
    if let Some(path) = absolute(var(name)) {
        return Ok(path.join("ferret"));
    }
    let home = absolute(var("HOME")).ok_or(Error::NoHome { var: name })?;
    Ok(home.join(default).join("ferret"))
}

/// `value` as a path if it is absolute; the spec treats anything else,
/// including empty, as unset.
fn absolute(value: Option<OsString>) -> Option<PathBuf> {
    value
        .map(PathBuf::from)
        .filter(|path| Path::is_absolute(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(vars: &[(&str, &str)]) -> Result<Dirs, Error> {
        Dirs::resolve(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        })
    }

    #[test]
    fn defaults_sit_under_home() {
        let dirs = resolve(&[("HOME", "/home/u")]).unwrap();
        assert_eq!(
            dirs,
            Dirs {
                config: "/home/u/.config/ferret".into(),
                data: "/home/u/.local/share/ferret".into(),
                state: "/home/u/.local/state/ferret".into(),
                cache: "/home/u/.cache/ferret".into(),
            }
        );
        assert_eq!(
            dirs.ignore_file(),
            Path::new("/home/u/.config/ferret/ignore")
        );
    }

    #[test]
    fn an_absolute_variable_overrides_its_default_only() {
        let dirs = resolve(&[("HOME", "/home/u"), ("XDG_CONFIG_HOME", "/cfg")]).unwrap();
        assert_eq!(dirs.config, Path::new("/cfg/ferret"));
        assert_eq!(dirs.data, Path::new("/home/u/.local/share/ferret"));
    }

    #[test]
    fn empty_and_relative_variables_are_ignored() {
        for value in ["", "cfg", "./cfg"] {
            let dirs = resolve(&[("HOME", "/home/u"), ("XDG_CONFIG_HOME", value)]).unwrap();
            assert_eq!(
                dirs.config,
                Path::new("/home/u/.config/ferret"),
                "{value:?}"
            );
        }
    }

    #[test]
    fn home_is_needed_only_for_a_fallback() {
        let all = [
            ("XDG_CONFIG_HOME", "/c"),
            ("XDG_DATA_HOME", "/d"),
            ("XDG_STATE_HOME", "/s"),
            ("XDG_CACHE_HOME", "/k"),
        ];
        assert_eq!(resolve(&all).unwrap().cache, Path::new("/k/ferret"));
        assert_eq!(
            resolve(&all[..3]),
            Err(Error::NoHome {
                var: "XDG_CACHE_HOME"
            })
        );
        assert_eq!(
            resolve(&[("HOME", "relative")]),
            Err(Error::NoHome {
                var: "XDG_CONFIG_HOME"
            })
        );
    }
}
