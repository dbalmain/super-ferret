//! Find syntax and execution over interchangeable entry sources. The live
//! source owns traversal; expressions own truth and control effects. Hosts own
//! output and diagnostics, so the engine does not depend on the CLI or index.

mod action;
mod glob;
mod output;
mod parallel;
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

#[derive(Clone, Debug, Default)]
pub(crate) struct Options {
    pub max_depth: Option<usize>,
    pub min_depth: usize,
    pub depth_first: bool,
    pub xdev: bool,
    pub follow: Follow,
    pub live_checks: bool,
    guard: Option<CandidateGuard>,
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
    /// Executes a prepared command, draining stdout through the host capture
    /// hook.
    fn command(&mut self, command: &mut std::process::Command) -> io::Result<bool> {
        self.capture(command, &mut io::stdout().lock())
    }
    /// Drains a child's stdout into the entry buffer while it runs. Stderr and
    /// stdin retain the host's normal process policy.
    fn capture(
        &mut self,
        command: &mut std::process::Command,
        output: &mut dyn std::io::Write,
    ) -> io::Result<bool> {
        use std::process::Stdio;
        let mut child = command
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let copied = match child.stdout.take() {
            Some(mut stdout) => io::copy(&mut stdout, output).map(|_| ()),
            None => Ok(()),
        };
        let status = child.wait();
        copied?;
        Ok(status?.success())
    }
    /// Commits this entry and latches cancellation. Hosts normally use the
    /// evaluator's entry adapter rather than overriding this hook.
    fn quit(&mut self) -> io::Result<()> {
        Ok(())
    }
    /// Writes an output-file record through the evaluator's entry adapter.
    fn file(
        &mut self,
        file: &std::sync::Arc<std::sync::Mutex<std::io::BufWriter<std::fs::File>>>,
        bytes: &[u8],
    ) -> io::Result<()> {
        use std::io::Write;
        file.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .write_all(bytes)
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
    cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
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
        LiveWalk::new(self.paths.clone(), self.options.clone())
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

    /// Creates the shared DFS engine for parallel catalog execution.
    pub fn parallel_catalog_source(&self, catalog: ferret_catalog::Catalog) -> LiveWalk {
        self.catalog_source(catalog).walk
    }

    /// Creates a catalog walk. Load `catalog_sections()` before construction.
    pub fn catalog_source(&self, catalog: ferret_catalog::Catalog) -> CatalogSource {
        let mut options = self.options.clone();
        options.live_checks = has_actions(&self.expression);
        options.guard = leading_guard(&self.expression);
        CatalogSource::new(catalog, self.paths.clone(), options)
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
        let buffered = output::needs_record(&expression);
        let mut record = output::Record::default();
        let gate = std::sync::Mutex::new(());
        let quit = std::sync::atomic::AtomicBool::new(false);
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
            let result = if buffered {
                let mut output = output::EntryEffects {
                    host: effects,
                    record: &mut record,
                    gate: &gate,
                    quit: &quit,
                };
                let result = evaluate(&expression, entry, &mut output, &mut control);
                let committed = output.commit(false);
                committed
                    .map_err(EvaluationError::Output)
                    .and(result.map(|_| ()))
            } else {
                evaluate(&expression, entry, effects, &mut control).map(|_| ())
            };
            if let Err(error) = result {
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
            if control.quit
                || control
                    .cancelled
                    .as_ref()
                    .is_some_and(|quit| quit.load(std::sync::atomic::Ordering::Acquire))
            {
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
        outcome.errors +=
            action::flush_shared(&control.actions.shared, effects).unwrap_or_else(|error| {
                effects.error(&WalkError {
                    path: ".".into(),
                    error,
                });
                1
            });
        outcome.errors += control.actions.errors;
        Ok(outcome)
    }
}

// Only guards that reject before any effects may remove candidates. This is
// independent of traversal order; directories still carry descendant work.
#[derive(Clone, Debug)]
enum CandidateGuard {
    Kind(u8),
    Name(glob::Pattern),
    Or(Box<Self>, Box<Self>),
}

impl CandidateGuard {
    fn matches(&self, kind: FileKind, name: &[u8]) -> bool {
        match self {
            Self::Kind(mask) => mask & (1 << kind as u8) != 0,
            Self::Name(pattern) => pattern.matches(name),
            Self::Or(left, right) => left.matches(kind, name) || right.matches(kind, name),
        }
    }
}

fn leading_guard(expression: &Expression) -> Option<CandidateGuard> {
    match expression {
        Expression::Type(kinds) => Some(CandidateGuard::Kind(
            kinds.iter().fold(0, |mask, kind| mask | 1 << *kind as u8),
        )),
        Expression::Name(pattern) => Some(CandidateGuard::Name(pattern.clone())),
        Expression::And(left, _) => leading_guard(left),
        Expression::Or(left, right) => Some(CandidateGuard::Or(
            Box::new(leading_guard(left)?),
            Box::new(leading_guard(right)?),
        )),
        Expression::Not(inner) => match leading_guard(inner)? {
            CandidateGuard::Kind(mask) if matches!(&**inner, Expression::Type(_)) => {
                Some(CandidateGuard::Kind(0x7f ^ mask))
            }
            _ => None,
        },
        _ => None,
    }
}

fn has_actions(expression: &Expression) -> bool {
    match expression {
        Expression::And(a, b) | Expression::Or(a, b) | Expression::Comma(a, b) => {
            has_actions(a) || has_actions(b)
        }
        Expression::Not(inner) => has_actions(inner),
        Expression::Action(
            action::Action::Exec(_)
            | action::Action::Delete
            | action::Action::Output(action::Target::File(..), _)
            | action::Action::List(action::Target::File(..)),
        ) => true,
        _ => false,
    }
}

/// Whether start operands run one after another. Actions can change what a
/// later start sees, and `-quit` must stop at the first start that reaches it
/// before a later start can report a missing path.
fn sequential_starts(expression: &Expression) -> bool {
    fn has_quit(expression: &Expression) -> bool {
        match expression {
            Expression::And(a, b) | Expression::Or(a, b) | Expression::Comma(a, b) => {
                has_quit(a) || has_quit(b)
            }
            Expression::Not(inner) => has_quit(inner),
            Expression::Quit => true,
            _ => false,
        }
    }
    has_actions(expression) || has_quit(expression)
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
    if control.quit
        || control
            .cancelled
            .as_ref()
            .is_some_and(|quit| quit.load(std::sync::atomic::Ordering::Acquire))
    {
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
            effects.quit().map_err(EvaluationError::Output)?;
            true
        }
        Expression::Test(test) => test.evaluate(entry).map_err(EvaluationError::Metadata)?,
    })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "../../../../tests/support/gnu_find.rs"]
mod gnu;
