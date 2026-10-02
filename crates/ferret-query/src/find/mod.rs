//! Find syntax and execution over interchangeable entry sources. The live
//! source owns traversal; expressions own truth and control effects. Hosts own
//! output and diagnostics, so the engine does not depend on the CLI or index.

mod glob;
mod parse;
mod walk;

use std::ffi::OsString;
use std::io;
use std::path::Path;

pub use parse::{ParseError, Plan};
pub use walk::{Entry, EntrySource, FileKind, LiveWalk, WalkError};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Expression {
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
    Comma(Box<Self>, Box<Self>),
    Not(Box<Self>),
    Name(Vec<u8>, bool),
    Path(Vec<u8>, bool),
    Type(Vec<FileKind>),
    Constant(bool),
    Print(bool),
    Prune,
    Quit,
    Unsupported(OsString),
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Options {
    pub max_depth: Option<usize>,
    pub min_depth: usize,
    pub depth_first: bool,
    pub xdev: bool,
}

/// The host supplies process output. A failed print stops the walk and is
/// reported through `error`, like any other I/O failure.
pub trait Effects {
    /// Writes the path's exact bytes, followed by a newline or NUL.
    fn print(&mut self, path: &Path, nul: bool) -> io::Result<()>;
    /// Reports an I/O error. Execution continues after traversal errors.
    fn error(&mut self, error: &WalkError);
}

/// Execution outcome; zero errors is success even when nothing matched.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Traversal, metadata and output errors reported to the host.
    pub errors: u64,
}

#[derive(Default)]
struct Control {
    prune: bool,
    quit: bool,
}

impl Plan {
    /// Parses a GNU find argument list, including leading ferret `-I`.
    pub fn parse(args: &[OsString]) -> Result<Self, ParseError> {
        parse::parse(args)
    }

    /// Whether `-I` / `--no-ignore` selected the live, GNU-compatible mode.
    pub fn no_ignore(&self) -> bool {
        self.no_ignore
    }

    /// The first feature that parses but is not evaluated in this milestone.
    /// Check this before opening any source or producing output.
    pub fn unsupported(&self) -> Option<&std::ffi::OsStr> {
        self.unsupported.as_deref()
    }

    /// Leading debug flags request diagnostics. The host may report that its
    /// evaluator performs no GNU optimizations; this does not fail the query.
    pub fn debug_requested(&self) -> bool {
        self.debug
    }

    /// Creates the sequential live source. It never opens a catalog.
    pub fn live_source(&self) -> LiveWalk {
        LiveWalk::new(self.paths.clone(), self.options)
    }

    /// Evaluates this plan over a source configured for its traversal options.
    /// The caller must reject `unsupported()` before execution. The source
    /// receives the previous entry's descent decision; `-quit` stops fetching
    /// entries immediately, including across multiple starting paths.
    pub fn run(&self, source: &mut impl EntrySource, effects: &mut impl Effects) -> Outcome {
        let mut outcome = Outcome::default();
        let mut descend = true;
        while let Some(item) = source.next(descend) {
            descend = true;
            let entry = match item {
                Ok(entry) => entry,
                Err(error) => {
                    effects.error(&error);
                    outcome.errors += 1;
                    continue;
                }
            };
            if entry.depth() < self.options.min_depth {
                continue;
            }
            let mut control = Control::default();
            if let Err(error) = evaluate(&self.expression, &entry, effects, &mut control) {
                effects.error(&WalkError {
                    path: entry.path().to_owned(),
                    error,
                });
                outcome.errors += 1;
                break;
            }
            descend = !control.prune;
            if control.quit {
                break;
            }
        }
        outcome
    }
}

fn evaluate(
    expression: &Expression,
    entry: &Entry,
    effects: &mut impl Effects,
    control: &mut Control,
) -> io::Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    if control.quit {
        return Ok(false);
    }
    Ok(match expression {
        Expression::And(left, right) => {
            evaluate(left, entry, effects, control)? && evaluate(right, entry, effects, control)?
        }
        Expression::Or(left, right) => {
            evaluate(left, entry, effects, control)? || evaluate(right, entry, effects, control)?
        }
        Expression::Comma(left, right) => {
            evaluate(left, entry, effects, control)?;
            evaluate(right, entry, effects, control)?
        }
        Expression::Not(inner) => !evaluate(inner, entry, effects, control)?,
        Expression::Name(pattern, fold) => glob::matches(pattern, entry.name(), *fold),
        Expression::Path(pattern, fold) => {
            glob::matches(pattern, entry.path().as_os_str().as_bytes(), *fold)
        }
        Expression::Type(kinds) => kinds.contains(&entry.kind()),
        Expression::Constant(value) => *value,
        Expression::Print(nul) => {
            effects.print(entry.path(), *nul)?;
            true
        }
        Expression::Prune => {
            control.prune = true;
            true
        }
        Expression::Quit => {
            control.quit = true;
            true
        }
        Expression::Unsupported(_) => false,
    })
}

#[cfg(test)]
mod tests;
