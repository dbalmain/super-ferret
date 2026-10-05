//! The command line, parsed into a [`Command`]. Pure over the argument list:
//! nothing here reads the environment or the file system.
//!
//! Flags may come before or after the command, as `--flag VALUE` or
//! `--flag=VALUE`. `--` ends the flags, so a query atom that starts with `-`
//! is written `ferret search -- -atom`. Every other argument that starts with
//! `-` is a flag, and an unknown one is an error rather than an atom. Find's
//! arguments pass intact to its own parser once its command name is seen.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// A parsed command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Args {
    /// `--index DIR`: where the index lives, overriding `FERRET_INDEX` and
    /// the XDG default.
    pub index: Option<PathBuf>,
    /// What to do.
    pub command: Command,
}

/// One command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// `index [DIR...]`: add each directory as a root and refresh it; with
    /// none, refresh every root.
    Index(Vec<PathBuf>),
    /// `roots list`.
    RootsList,
    /// `roots remove DIR...`.
    RootsRemove(Vec<PathBuf>),
    /// `search [--json] [--limit N] ATOM...`.
    Search {
        /// One query atom per argument, as bytes: a name need not be UTF-8.
        atoms: Vec<OsString>,
        /// JSON lines instead of one path per line.
        json: bool,
        /// Stop after this many rows.
        limit: Option<u64>,
    },
    /// `find [OPTIONS] [PATH...] [EXPRESSION]`, passed intact to the find
    /// parser. `--json`, given before the `find` operand, requests the
    /// structured-event host instead of find's raw stdout/stderr; it is not
    /// passed to find's own parser, so a `--json` operand after `find` (or
    /// after `--`) is unaffected and reaches find intact.
    Find {
        args: Vec<OsString>,
        json: bool,
    },
    /// `stats`.
    Stats,
    Status,
    /// Explicit migration of the index directory's v3 snapshot.
    ImportV3,
    /// `help`, `-h` or `--help`.
    Help,
    /// `--version`.
    Version,
}

/// Why a command line is not valid. Each variant carries what the message
/// needs; `ferret` exits with status 2 for every one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsageError {
    /// No command was given.
    NoCommand,
    /// The first operand is not a command.
    UnknownCommand(OsString),
    /// A flag this program does not have.
    UnknownFlag(OsString),
    /// A flag that takes a value came last, or `--flag=` was empty.
    MissingValue(&'static str),
    /// A flag's value is not what it takes.
    BadValue(&'static str, OsString),
    /// A flag given to a command that does not take it.
    NotFor(&'static str, &'static str),
    /// A command that takes no more operands was given one.
    Unexpected(&'static str, OsString),
    /// A command that needs an operand got none.
    Missing(&'static str),
}

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let show = |arg: &OsString| arg.as_bytes().escape_ascii().to_string();
        match self {
            Self::NoCommand => write!(f, "no command given"),
            Self::UnknownCommand(c) => write!(f, "`{}` is not a command", show(c)),
            Self::UnknownFlag(flag) => write!(
                f,
                "unknown flag `{}` (a query atom starting with `-` goes after `--`)",
                show(flag)
            ),
            Self::MissingValue(flag) => write!(f, "{flag} needs a value"),
            Self::BadValue(flag, value) => {
                write!(f, "{flag}: `{}` is not a positive number", show(value))
            }
            Self::NotFor(flag, command) => write!(f, "{flag} is not an option of `{command}`"),
            Self::Unexpected(command, arg) => {
                write!(f, "`{command}` takes no operand `{}`", show(arg))
            }
            Self::Missing(what) => write!(f, "{what}"),
        }
    }
}

impl std::error::Error for UsageError {}

/// Parses the arguments after the program name.
pub fn parse<I: IntoIterator<Item = OsString>>(args: I) -> Result<Args, UsageError> {
    let mut index = None;
    let mut json = false;
    let mut limit = None;
    let (mut help, mut version) = (false, false);
    let mut operands = Vec::new();
    let mut flags_done = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if operands.is_empty() && arg == "find" {
            if limit.is_some() {
                return Err(UsageError::NotFor("--limit", "find"));
            }
            return Ok(Args {
                index,
                command: Command::Find {
                    args: args.collect(),
                    json,
                },
            });
        }
        let bytes = arg.as_bytes();
        if flags_done || bytes.len() < 2 || bytes[0] != b'-' {
            operands.push(arg);
            continue;
        }
        let (name, inline) = match bytes.iter().position(|&b| b == b'=') {
            Some(at) if bytes.starts_with(b"--") => (&bytes[..at], Some(&bytes[at + 1..])),
            _ => (bytes, None),
        };
        let mut value = |flag: &'static str| match inline {
            Some([]) => Err(UsageError::MissingValue(flag)),
            Some(v) => Ok(OsStr::from_bytes(v).to_owned()),
            None => args.next().ok_or(UsageError::MissingValue(flag)),
        };
        match name {
            b"--" if inline.is_none() => flags_done = true,
            b"--index" => index = Some(PathBuf::from(value("--index")?)),
            b"--limit" => {
                let value = value("--limit")?;
                let n = value
                    .to_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .filter(|&n| n > 0)
                    .ok_or_else(|| UsageError::BadValue("--limit", value.clone()))?;
                limit = Some(n);
            }
            b"--json" if inline.is_none() => json = true,
            b"-h" | b"--help" if inline.is_none() => help = true,
            b"--version" if inline.is_none() => version = true,
            _ => return Err(UsageError::UnknownFlag(arg)),
        }
    }
    if help {
        return Ok(Args {
            index,
            command: Command::Help,
        });
    }
    if version {
        return Ok(Args {
            index,
            command: Command::Version,
        });
    }

    let mut operands = operands.into_iter();
    let name = operands.next().ok_or(UsageError::NoCommand)?;
    let rest: Vec<OsString> = operands.collect();
    let none = |command: &'static str, rest: Vec<OsString>| match rest.into_iter().next() {
        Some(arg) => Err(UsageError::Unexpected(command, arg)),
        None => Ok(()),
    };
    let paths = |rest: Vec<OsString>| rest.into_iter().map(PathBuf::from).collect();
    let command = match name.as_bytes() {
        b"search" => Command::Search {
            atoms: rest,
            json,
            limit,
        },
        other => {
            let command = match other {
                b"index" => "index",
                b"roots" => "roots",
                b"status" => "status",
                b"stats" => "stats",
                b"import-v3" => "import-v3",
                b"help" => "help",
                _ => return Err(UsageError::UnknownCommand(name)),
            };
            if json && other != b"status" {
                return Err(UsageError::NotFor("--json", command));
            }
            if limit.is_some() {
                return Err(UsageError::NotFor("--limit", command));
            }
            match other {
                b"index" => Command::Index(paths(rest)),
                b"status" => {
                    if rest.len() == 1 && rest[0] == "--json" {
                        Command::Status
                    } else {
                        none("status", rest).map(|()| Command::Status)?
                    }
                }
                b"stats" => none("stats", rest).map(|()| Command::Stats)?,
                b"import-v3" => none("import-v3", rest).map(|()| Command::ImportV3)?,
                b"help" => none("help", rest).map(|()| Command::Help)?,
                _ => roots(rest)?,
            }
        }
    };
    Ok(Args { index, command })
}

fn roots(rest: Vec<OsString>) -> Result<Command, UsageError> {
    let mut rest = rest.into_iter();
    let missing = "`roots` needs `list` or `remove DIR...`";
    let sub = rest.next().ok_or(UsageError::Missing(missing))?;
    match sub.as_bytes() {
        b"list" => match rest.next() {
            Some(arg) => Err(UsageError::Unexpected("roots list", arg)),
            None => Ok(Command::RootsList),
        },
        b"remove" => {
            let dirs: Vec<PathBuf> = rest.map(PathBuf::from).collect();
            if dirs.is_empty() {
                return Err(UsageError::Missing("`roots remove` needs a directory"));
            }
            Ok(Command::RootsRemove(dirs))
        }
        _ => Err(UsageError::Missing(missing)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(args: &[&str]) -> Result<Args, UsageError> {
        parse(args.iter().map(OsString::from))
    }

    fn command(args: &[&str]) -> Command {
        parse_str(args).unwrap().command
    }

    #[test]
    fn commands_parse() {
        let cases: &[(&[&str], Command)] = &[
            (&["index"], Command::Index(vec![])),
            (
                &["index", "a", "/b"],
                Command::Index(vec!["a".into(), "/b".into()]),
            ),
            (&["roots", "list"], Command::RootsList),
            (
                &["roots", "remove", "x"],
                Command::RootsRemove(vec!["x".into()]),
            ),
            (&["stats"], Command::Stats),
            (&["help"], Command::Help),
            (&["search", "--help"], Command::Help),
            (&["--version"], Command::Version),
            (
                &["search", "a", "size:>1k"],
                Command::Search {
                    atoms: vec!["a".into(), "size:>1k".into()],
                    json: false,
                    limit: None,
                },
            ),
            (
                &["--json", "search", "--limit", "3", "a"],
                Command::Search {
                    atoms: vec!["a".into()],
                    json: true,
                    limit: Some(3),
                },
            ),
            (
                &["search", "--limit=7", "--", "-a", "--json"],
                Command::Search {
                    atoms: vec!["-a".into(), "--json".into()],
                    json: false,
                    limit: Some(7),
                },
            ),
            // A lone `-` is an operand, as it is to most tools.
            (
                &["search", "-"],
                Command::Search {
                    atoms: vec!["-".into()],
                    json: false,
                    limit: None,
                },
            ),
            (
                &["find", "a", "-print"],
                Command::Find {
                    args: vec!["a".into(), "-print".into()],
                    json: false,
                },
            ),
            // --json before `find` selects the structured host; find's own
            // argv, including a later --json, passes through untouched.
            (
                &["--json", "find", "a", "-name", "--json", "-print"],
                Command::Find {
                    args: vec!["a".into(), "-name".into(), "--json".into(), "-print".into()],
                    json: true,
                },
            ),
        ];
        for (args, expected) in cases {
            assert_eq!(&command(args), expected, "{args:?}");
        }
    }

    #[test]
    fn the_index_flag_goes_anywhere_in_either_form() {
        for args in [
            &["--index", "/i", "stats"][..],
            &["stats", "--index", "/i"],
            &["--index=/i", "stats"],
        ] {
            let parsed = parse_str(args).unwrap();
            assert_eq!(parsed.index, Some(PathBuf::from("/i")), "{args:?}");
            assert_eq!(parsed.command, Command::Stats);
        }
    }

    #[test]
    fn bad_command_lines_are_usage_errors() {
        let cases: &[(&[&str], UsageError)] = &[
            (&[], UsageError::NoCommand),
            (&["unknown"], UsageError::UnknownCommand("unknown".into())),
            (&["search", "-x"], UsageError::UnknownFlag("-x".into())),
            (
                &["search", "--jsn"],
                UsageError::UnknownFlag("--jsn".into()),
            ),
            (
                &["search", "--json=1"],
                UsageError::UnknownFlag("--json=1".into()),
            ),
            (&["search", "--limit"], UsageError::MissingValue("--limit")),
            (&["search", "--limit="], UsageError::MissingValue("--limit")),
            (&["stats", "--index"], UsageError::MissingValue("--index")),
            (
                &["search", "--limit", "0"],
                UsageError::BadValue("--limit", "0".into()),
            ),
            (
                &["search", "--limit", "x"],
                UsageError::BadValue("--limit", "x".into()),
            ),
            (&["stats", "--json"], UsageError::NotFor("--json", "stats")),
            (
                &["index", "--limit", "2"],
                UsageError::NotFor("--limit", "index"),
            ),
            (&["stats", "x"], UsageError::Unexpected("stats", "x".into())),
            (
                &["help", "typo"],
                UsageError::Unexpected("help", "typo".into()),
            ),
            (
                &["roots", "list", "x"],
                UsageError::Unexpected("roots list", "x".into()),
            ),
            (
                &["roots", "remove"],
                UsageError::Missing("`roots remove` needs a directory"),
            ),
            (
                &["roots"],
                UsageError::Missing("`roots` needs `list` or `remove DIR...`"),
            ),
        ];
        for (args, expected) in cases {
            assert_eq!(parse_str(args).as_ref(), Err(expected), "{args:?}");
        }
    }

    #[test]
    fn operands_keep_their_bytes() {
        let atom = OsString::from(OsStr::from_bytes(b"caf\xe9"));
        let parsed = parse([OsString::from("search"), atom.clone()]).unwrap();
        assert_eq!(
            parsed.command,
            Command::Search {
                atoms: vec![atom],
                json: false,
                limit: None,
            }
        );
    }
}
