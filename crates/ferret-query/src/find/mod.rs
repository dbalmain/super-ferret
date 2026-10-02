//! Find syntax and execution over interchangeable entry sources. The live
//! source owns traversal; expressions own truth and control effects. Hosts own
//! output and diagnostics, so the engine does not depend on the CLI or index.

mod action;
mod glob;
mod parse;
mod printf;
mod test;
mod walk;

use std::ffi::OsString;
use std::io;
use std::path::Path;

pub use parse::{ParseError, Plan};
pub use walk::{CatalogSource, Entry, EntrySource, FileKind, LiveWalk, WalkError};

#[derive(Clone, Debug, PartialEq)]
enum Expression {
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
    Comma(Box<Self>, Box<Self>),
    Not(Box<Self>),
    Name(glob::Pattern),
    Path(glob::Pattern),
    Type(Vec<FileKind>),
    Constant(bool),
    Print(bool),
    Prune,
    Quit,
    Action(action::Action),
    Test(test::Test),
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Options {
    pub max_depth: Option<usize>,
    pub min_depth: usize,
    pub depth_first: bool,
    pub xdev: bool,
    pub follow: Follow,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Follow {
    #[default]
    Physical,
    Roots,
    All,
}

/// The host supplies process output. A failed print stops the walk and is
/// reported through `error`, like any other I/O failure.
pub trait Effects {
    /// Writes the path's exact bytes, followed by a newline or NUL.
    fn print(&mut self, path: &Path, nul: bool) -> io::Result<()>;
    /// Reports an I/O error. Execution continues after traversal errors.
    fn error(&mut self, error: &WalkError);
    /// Writes formatted bytes without adding a record terminator.
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        std::io::stdout().lock().write_all(bytes)
    }
    /// Flushes host output before a child inherits its descriptors.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
    /// Executes a prepared command with inherited standard descriptors.
    fn command(&mut self, command: &mut std::process::Command) -> io::Result<bool> {
        Ok(command.status()?.success())
    }
    /// Prompts and reads one answer line. Only initial y/Y is yes in C locale.
    fn confirm(&mut self, program: &std::ffi::OsStr, path: &Path) -> io::Result<bool> {
        action::confirm(program, path)
    }
    /// Reports a parse-time warning without making execution fail.
    fn warning(&mut self, message: &str) {
        self.error(&WalkError {
            path: std::path::PathBuf::new(),
            error: io::Error::other(message.to_owned()),
        });
    }
}

/// A recognized feature that this evaluator does not implement yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unsupported {
    /// The primary or leading option that cannot execute.
    pub feature: OsString,
}

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unsupported find feature {}",
            self.feature.to_string_lossy()
        )
    }
}

impl std::error::Error for Unsupported {}

/// Execution outcome; zero errors is success even when nothing matched.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Traversal, metadata and output errors reported to the host.
    pub errors: u64,
}

#[derive(Debug)]
enum EvaluationError {
    Metadata(io::Error),
    Output(io::Error),
}

#[derive(Default)]
struct Control {
    prune: bool,
    quit: bool,
    actions: action::State,
}

impl Plan {
    /// Parses a GNU find argument list, including leading ferret `-I`.
    pub fn parse(args: &[OsString]) -> Result<Self, ParseError> {
        parse::parse(args)
    }

    /// Whether this command prints help, version or debug-option help.
    pub fn is_information(&self) -> bool {
        self.message.is_some()
    }

    /// Whether `-I` / `--no-ignore` selected the live, GNU-compatible mode.
    pub fn no_ignore(&self) -> bool {
        self.no_ignore
    }

    /// Whether parsing observed GNU's warning-producing `-perm /000` form.
    pub fn permission_warning(&self) -> bool {
        self.permission_warning
    }

    /// The first feature that parses but is not evaluated in this milestone.
    /// Check this before opening any source or producing output.
    pub fn unsupported(&self) -> Option<&std::ffi::OsStr> {
        self.unsupported.as_deref()
    }

    /// Creates the sequential live source. It never opens a catalog.
    pub fn live_source(&self) -> LiveWalk {
        LiveWalk::new(self.paths.clone(), self.options)
    }

    /// Sections needed for traversal and the stored fields this plan reads.
    pub fn catalog_sections(&self) -> Vec<ferret_catalog::Section> {
        use ferret_catalog::Section;
        let mut sections = vec![
            Section::Names,
            Section::Links,
            Section::Roots,
            Section::Entries,
        ];
        if self.options.xdev || self.options.follow != Follow::Physical {
            sections.extend([Section::Dev, Section::Ino]);
        }
        expression_sections(&self.expression, &mut sections);
        sections.sort_unstable_by_key(|section| *section as usize);
        sections.dedup();
        sections
    }

    /// Creates a catalog walk. Load `catalog_sections()` before construction.
    pub fn catalog_source(&self, catalog: ferret_catalog::Catalog) -> CatalogSource {
        CatalogSource::new(catalog, self.paths.clone(), self.options)
    }

    /// Evaluates this plan over a source configured for its traversal options.
    /// Unsupported features fail before the source is fetched. The source
    /// receives the previous entry's descent decision; `-quit` stops fetching
    /// entries immediately, including across multiple starting paths.
    pub fn run(
        &self,
        source: &mut impl EntrySource,
        effects: &mut impl Effects,
    ) -> Result<Outcome, Unsupported> {
        if let Some(feature) = &self.unsupported {
            return Err(Unsupported {
                feature: feature.clone(),
            });
        }
        let mut outcome = Outcome::default();
        for warning in &self.warnings {
            effects.warning(warning);
        }
        if let Some(message) = &self.message {
            if let Err(error) = effects.write(message.as_bytes()) {
                effects.error(&WalkError {
                    path: ".".into(),
                    error,
                });
                outcome.errors += 1;
            }
            return Ok(outcome);
        }
        let mut expression = self.expression.clone();
        if let Err(error) = resolve_references(&mut expression, source.catalog()) {
            effects.error(&WalkError {
                path: ".".into(),
                error,
            });
            outcome.errors += 1;
            return Ok(outcome);
        }
        let mut control = Control::default();
        let mut descend = true;
        while let Some(item) =
            source.next_with(descend, &mut || control.actions.flush(effects, true))
        {
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
            control.prune = false;
            control.quit = false;
            if let Err(error) = control.actions.change_directory(entry.path(), effects) {
                effects.error(&WalkError {
                    path: entry.path().to_owned(),
                    error,
                });
                outcome.errors += 1;
                break;
            }
            if let Err(error) = evaluate(&expression, entry, effects, &mut control) {
                let (error, stop) = match error {
                    EvaluationError::Metadata(error) => (error, false),
                    EvaluationError::Output(error) => (error, true),
                };
                effects.error(&WalkError {
                    path: entry.path().to_owned(),
                    error,
                });
                outcome.errors += 1;
                descend = false;
                if stop {
                    break;
                }
                continue;
            }
            descend = !control.prune;
            if control.quit {
                break;
            }
        }
        if let Err(error) = control.actions.flush(effects, false) {
            effects.error(&WalkError {
                path: ".".into(),
                error,
            });
            outcome.errors += 1;
        }
        outcome.errors += control.actions.errors;
        Ok(outcome)
    }
}

fn expression_sections(expression: &Expression, out: &mut Vec<ferret_catalog::Section>) {
    match expression {
        Expression::And(a, b) | Expression::Or(a, b) | Expression::Comma(a, b) => {
            expression_sections(a, out);
            expression_sections(b, out);
        }
        Expression::Not(inner) => expression_sections(inner, out),
        Expression::Test(test) => test.sections(out),
        Expression::Action(action::Action::Output(_, format)) => format.sections(out),
        Expression::Action(action::Action::List(_)) => out.extend([
            ferret_catalog::Section::Dev,
            ferret_catalog::Section::Ino,
            ferret_catalog::Section::Size,
            ferret_catalog::Section::Mode,
            ferret_catalog::Section::Nlink,
            ferret_catalog::Section::Owner,
            ferret_catalog::Section::Mtime,
        ]),
        _ => {}
    }
}

fn resolve_references(
    expression: &mut Expression,
    catalog: Option<&ferret_catalog::Catalog>,
) -> io::Result<()> {
    match expression {
        Expression::And(a, b) | Expression::Or(a, b) | Expression::Comma(a, b) => {
            resolve_references(a, catalog)?;
            resolve_references(b, catalog)
        }
        Expression::Not(inner) => resolve_references(inner, catalog),
        Expression::Test(test) => test.resolve_reference(catalog),
        _ => Ok(()),
    }
}

fn evaluate(
    expression: &Expression,
    entry: &Entry,
    effects: &mut impl Effects,
    control: &mut Control,
) -> Result<bool, EvaluationError> {
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
        Expression::Name(pattern) => pattern.matches(entry.name()),
        Expression::Path(pattern) => pattern.matches(entry.path().as_os_str().as_bytes()),
        Expression::Type(kinds) => kinds.contains(
            &entry
                .kind()
                .map_err(|error| EvaluationError::Metadata(walk::copy_error(error)))?,
        ),
        Expression::Constant(value) => *value,
        Expression::Print(nul) => {
            effects
                .print(entry.path(), *nul)
                .map_err(EvaluationError::Output)?;
            true
        }
        Expression::Prune => {
            // GNU needs the stat of anything but a directory here: a file an
            // earlier -exec removed makes `-prune` report it and exit 1.
            if entry.catalog.is_none() && !matches!(entry.kind(), Ok(FileKind::Directory)) {
                entry
                    .metadata()
                    .map_err(|error| EvaluationError::Metadata(walk::copy_error(error)))?;
            }
            control.prune = true;
            true
        }
        Expression::Action(action) => {
            action::evaluate(action, entry, effects, &mut control.actions)?
        }
        Expression::Quit => {
            control.quit = true;
            true
        }
        Expression::Test(test) => test.evaluate(entry).map_err(EvaluationError::Metadata)?,
    })
}

#[cfg(test)]
mod tests;
