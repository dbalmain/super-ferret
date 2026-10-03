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

pub use output::OutputBuffer;
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

impl Expression {
    /// Visits every leaf in expression order, independently of its truth value.
    fn visit(&self, visitor: &mut impl FnMut(&Self)) {
        match self {
            Self::And(left, right) | Self::Or(left, right) | Self::Comma(left, right) => {
                left.visit(visitor);
                right.visit(visitor);
            }
            Self::Not(inner) => inner.visit(visitor),
            leaf => visitor(leaf),
        }
    }

    /// The mutable counterpart for preparation, stopping at the first error.
    fn try_visit_mut<E>(
        &mut self,
        visitor: &mut impl FnMut(&mut Self) -> Result<(), E>,
    ) -> Result<(), E> {
        match self {
            Self::And(left, right) | Self::Or(left, right) | Self::Comma(left, right) => {
                left.try_visit_mut(visitor)?;
                right.try_visit_mut(visitor)
            }
            Self::Not(inner) => inner.try_visit_mut(visitor),
            leaf => visitor(leaf),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Options {
    pub max_depth: Option<usize>,
    pub min_depth: usize,
    pub depth_first: bool,
    pub xdev: bool,
    pub follow: Follow,
    pub live_checks: bool,
    pub retain_parent: bool,
    guard: Option<CandidateGuard>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Follow {
    #[default]
    Physical,
    Roots,
    All,
}

/// Marks an `io::Error` as happening after a child was successfully
/// launched (capture, wait, or writing the captured bytes to their
/// destination), rather than as a failure to launch it. `-exec`'s result is
/// only ever false for a launch failure; anything marked here is a fatal
/// output error instead (#3).
#[derive(Debug)]
struct OutputFailure(io::Error);

impl std::fmt::Display for OutputFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for OutputFailure {}

/// Marks a captured command's output error so a caller's own `command`
/// override is held to the same contract as the default: a failure writing
/// out already-captured bytes is fatal, never a false `-exec` result.
pub fn mark_output_failure(error: io::Error) -> io::Error {
    io::Error::other(OutputFailure(error))
}

pub(super) fn is_output_failure(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.downcast_ref::<OutputFailure>().is_some())
}

/// Unwraps a marked error back to its original kind and message, for
/// display; a plain error passes through unchanged.
pub(super) fn unmark_output_failure(error: io::Error) -> io::Error {
    let kind = error.kind();
    match error.into_inner() {
        Some(inner) => match inner.downcast::<OutputFailure>() {
            Ok(marked) => marked.0,
            Err(inner) => io::Error::new(kind, inner),
        },
        None => io::Error::from(kind),
    }
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
        let mut output = OutputBuffer::default();
        let success = self.capture(command, &mut output)?;
        output
            .write_to(&mut io::stdout().lock())
            .map_err(mark_output_failure)?;
        Ok(success)
    }
    /// Drains a child's stdout into the entry buffer while it runs. Stderr and
    /// stdin retain the host's normal process policy. Only the initial spawn
    /// can be a launch failure; a failure draining, waiting for, or writing
    /// the child's output is fatal and must never read as the command simply
    /// having failed (#3).
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
        let status = child.wait().map_err(mark_output_failure)?;
        copied.map_err(mark_output_failure)?;
        Ok(status.success())
    }
    /// Commits this entry and latches cancellation. Hosts normally use the
    /// evaluator's entry adapter rather than overriding this hook.
    fn quit(&mut self) -> io::Result<()> {
        Ok(())
    }
    /// Writes an output-file record through the evaluator's entry adapter.
    fn file(&mut self, file: &action::SharedFile, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let mut guard = file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let writer = guard.as_mut().ok_or_else(|| {
            io::Error::other("output file target was not opened during preparation")
        })?;
        writer.write_all(bytes)
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
        let mut plan = parse::parse(args)?;
        // Whether an action needs a live filesystem check or a kept
        // directory handle (a cataloged child another start deleted earlier
        // in the same walk, or a parent fd for -delete/-execdir) - true for
        // either source, not only the catalog one, which is why this is set
        // once here rather than only inside `catalog_source` (#10: a live
        // -I walk needs it exactly as much as a catalog walk does).
        plan.options.live_checks = has_actions(&plan.expression);
        Ok(plan)
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
        parallel::run(self, source, effects)
    }

    fn prepare(
        &self,
        source: &impl EntrySource,
        effects: &mut impl Effects,
        outcome: &mut Outcome,
    ) -> Result<Option<Expression>, Unsupported> {
        if let Some(feature) = &self.unsupported {
            return Err(Unsupported {
                feature: feature.clone(),
            });
        }
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
            return Ok(None);
        }
        let mut expression = self.expression.clone();
        if let Err(error) = prepare_expression(&mut expression, source.catalog()) {
            effects.error(&WalkError {
                path: ".".into(),
                error,
            });
            outcome.errors += 1;
            return Ok(None);
        }
        Ok(Some(expression))
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
    let mut found = false;
    expression.visit(&mut |leaf| found |= effectful(leaf));
    found
}

fn effectful(leaf: &Expression) -> bool {
    matches!(
        leaf,
        Expression::Action(
            action::Action::Exec(_)
                | action::Action::Delete
                | action::Action::Output(action::Target::File(..), _)
                | action::Action::List(action::Target::File(..))
        )
    )
}

/// Actions can change later starts; quit must precede a later missing start.
fn sequential_starts(expression: &Expression) -> bool {
    let mut sequential = false;
    expression.visit(&mut |leaf| sequential |= effectful(leaf) || matches!(leaf, Expression::Quit));
    sequential
}

fn expression_sections(expression: &Expression, out: &mut Vec<ferret_catalog::Section>) {
    expression.visit(&mut |leaf| match leaf {
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
    });
}

/// One ordered pass over the expression, left to right, that does the two
/// things parsing could not: observe `-newer`-style references against the
/// live catalog, and open (truncating) each `-fprint`/`-fprintf` output
/// file. Both must happen in the expression's written order - `-newer ref
/// -fprint ref` must read `ref`'s mtime before `-fprint ref` truncates it,
/// and `-fprint ref -newer ref` must truncate first (#6) - so one function
/// does both rather than two passes racing on order.
fn prepare_expression(
    expression: &mut Expression,
    catalog: Option<&ferret_catalog::Catalog>,
) -> io::Result<()> {
    expression.try_visit_mut(&mut |leaf| match leaf {
        Expression::Test(test) => test.resolve_reference(catalog),
        Expression::Action(action::Action::Output(target, _) | action::Action::List(target)) => {
            target.open()
        }
        _ => Ok(()),
    })
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
        Expression::Type(kinds) => {
            kinds.contains(&entry.kind().map_err(EvaluationError::Metadata)?)
        }
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
            if entry.catalog().is_none() && !matches!(entry.kind(), Ok(FileKind::Directory)) {
                entry.metadata().map_err(EvaluationError::Metadata)?;
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
