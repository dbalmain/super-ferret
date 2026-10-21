//! Shared names-only host over find's namespace sources. The local adapter
//! loads catalog namespace sections; remote::serve supplies its pinned catalog.
//! Neither path prepares document identities, postings or content sections.

use ferret_catalog::Catalog;
use ferret_query::line_grep::Names;
use std::io::IsTerminal;

use super::*;

pub(super) fn run(plan: &Names, index: Option<&Path>, args: &[OsString], tty: bool) -> Exit {
    if !plan.traversal.unrestricted
        && !plan.traversal.live
        && !test_injections()
        && resolve_inputs(&plan.paths, true)
            .iter()
            .any(|input| matches!(input, Input::Directory(_)))
    {
        if let Err(error) = selection(plan) {
            return failure(&error);
        }
        if let Ok(index) = index_path(index)
            && let Some(exit) = remote::names(args, plan, tty, &index)
        {
            return exit;
        }
    }
    let cancelled = AtomicBool::new(false);
    let stdout = io::stdout();
    let mut writer = output::Writer::new(
        &stdout,
        // As in `block_stdout`: live policy warnings bypass the writer.
        !plan.presentation.null && !plan.traversal.live,
    );
    match execute(plan, index, &mut writer, &cancelled) {
        Ok(exit) => exit,
        Err(error) => failure(&error),
    }
}

fn open_catalog(index: Option<&Path>) -> Result<Catalog, String> {
    let index = index_path(index)?;
    let catalog = Catalog::open(&index)
        .map_err(|error| format!("{}: {error}; re-index this directory", index.display()))?
        .ok_or_else(|| {
            format!(
                "no index in {}; run ferret index DIR, use -u, or use --live with a covering catalog root",
                index.display()
            )
        })?;
    catalog
        .load(CatalogSource::sections())
        .map_err(|error| error.to_string())?;
    #[cfg(test)]
    OPENED.with_borrow_mut(|opened| *opened = Some(catalog.clone()));
    Ok(catalog)
}

fn execute(
    plan: &Names,
    index: Option<&Path>,
    writer: &mut impl Write,
    cancelled: &AtomicBool,
) -> Result<Exit, String> {
    let selection = selection(plan)?;
    let inputs = resolve_inputs(&plan.paths, true);
    let catalog = inputs
        .iter()
        .any(|input| {
            matches!(input, Input::Directory(_))
                && !plan.traversal.unrestricted
                && !plan.traversal.live
        })
        .then(|| open_catalog(index))
        .transpose()?;
    let live = if inputs
        .iter()
        .any(|input| matches!(input, Input::Directory(_)))
        && plan.traversal.live
        && !plan.traversal.unrestricted
    {
        Some(live::Anchors::open(index)?)
    } else {
        None
    };
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let stderr = |message: &str| {
        cli::error(message);
        Ok(())
    };
    let runtime = Runtime {
        cancelled,
        stderr: &stderr,
        tty: io::stdout().is_terminal(),
        warned: AtomicBool::new(false),
        descriptors: None,
    };
    Listing {
        plan,
        selection,
        inputs,
        catalog: catalog.as_ref(),
        live: live.as_ref(),
        cwd: &cwd,
        runtime: &runtime,
    }
    .run(writer)
}

fn selection(plan: &Names) -> Result<GrepSelection, String> {
    let mut selection = GrepSelection::default().with_hidden(plan.hidden);
    for glob in &plan.globs {
        selection
            .push(glob)
            .map_err(|error| format!("glob {glob:?}: {error}"))?;
    }
    Ok(selection)
}

/// Lists against the request's resident pin without loading catalog sections
/// or consulting the daemon's cwd, terminal or environment.
pub(super) fn pinned(
    plan: &Names,
    catalog: &Catalog,
    cwd: &Path,
    writer: &mut impl Write,
    runtime: &Runtime<'_>,
) -> Result<Exit, String> {
    let defaults = [PathBuf::from(".")];
    let paths = if plan.paths.is_empty() {
        &defaults[..]
    } else {
        &plan.paths
    };
    Listing {
        plan,
        selection: selection(plan)?,
        inputs: resolve_inputs_at(paths, true, cwd),
        catalog: Some(catalog),
        live: None,
        cwd,
        runtime,
    }
    .run(writer)
}

struct Listing<'a> {
    plan: &'a Names,
    selection: GrepSelection,
    inputs: Vec<Input>,
    catalog: Option<&'a Catalog>,
    live: Option<&'a live::Anchors>,
    cwd: &'a Path,
    runtime: &'a Runtime<'a>,
}
impl Listing<'_> {
    fn run(self, writer: &mut impl Write) -> Result<Exit, String> {
        let Self {
            plan,
            selection,
            inputs,
            catalog,
            live,
            cwd,
            runtime,
        } = self;
        let cancelled = runtime.cancelled;
        let flush_each = plan.presentation.null || plan.traversal.live || runtime.tty;
        for input in &inputs {
            if let Input::Directory(path) = input
                && let Some(anchors) = live
            {
                anchors
                    .validate(path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
            }
        }
        if let Some(catalog) = catalog {
            for input in &inputs {
                if let Input::Directory(path) = input
                    && let Some(Err(error)) =
                        sources::Directory::new(path, Some(catalog), cwd, false)
                            .map_err(|e| e.to_string())?
                            .next(true)
                {
                    return Err(format!(
                        "{}: {}; index this directory, use -u, or use --live with a covering catalog root",
                        error.path.display(),
                        error.error
                    ));
                }
            }
        }
        let mut errors = false;
        for input in &inputs {
            if let Input::Failed(path, error) = input {
                if diagnostic(
                    writer,
                    runtime,
                    &format!("grep: {}: {error}", path.display()),
                )? {
                    return Ok(Exit::Ok);
                }
                errors = true;
            }
        }
        let mut listed = false;
        for input in inputs {
            if cancelled.load(Ordering::Acquire) {
                break;
            }
            match input {
                Input::File(path, _) => {
                    if emit(plan, writer, cancelled, &path, flush_each, &mut listed)? {
                        return Ok(Exit::Ok);
                    }
                }
                Input::Stdin => {
                    if emit(
                        plan,
                        writer,
                        cancelled,
                        Path::new("-"),
                        flush_each,
                        &mut listed,
                    )? {
                        return Ok(Exit::Ok);
                    }
                }
                Input::Failed(_, _) => {}
                Input::Directory(path) => {
                    let stopped = || cancelled.load(Ordering::Acquire);
                    let mut policy = live
                        .as_ref()
                        .map(|anchors| anchors.start(&path, &stopped))
                        .transpose()
                        .map_err(|e| e.to_string())?;
                    let mut source = sources::Directory::new(
                        &path,
                        catalog,
                        cwd,
                        plan.traversal.unrestricted || live.is_some(),
                    )
                    .map_err(|e| e.to_string())?;
                    let mut descend = true;
                    while let Some(entry) =
                        source.next_with_cancel(descend, &mut || Ok(()), &mut || {
                            cancelled.load(Ordering::Acquire)
                        })
                    {
                        descend = false;
                        let entry = match entry {
                            Ok(entry) => entry,
                            Err(error) => {
                                if diagnostic(
                                    writer,
                                    runtime,
                                    &format!("grep: {}: {}", error.path.display(), error.error),
                                )? {
                                    return Ok(Exit::Ok);
                                }
                                errors = true;
                                continue;
                            }
                        };
                        if plan
                            .traversal
                            .max_depth
                            .is_some_and(|limit| entry.depth() > limit)
                        {
                            continue;
                        }
                        let relative = entry
                            .path()
                            .strip_prefix(cwd)
                            .unwrap_or(entry.path())
                            .as_os_str()
                            .as_bytes();
                        match entry.kind() {
                            Ok(FileKind::Directory) => {
                                descend = plan
                                    .traversal
                                    .max_depth
                                    .is_none_or(|limit| entry.depth() < limit)
                                    && selection.descend(relative, entry.depth())
                                    && !selection.hides_stripped(
                                        entry.path().as_os_str().as_bytes(),
                                        relative,
                                        entry.depth(),
                                    );
                                if descend && let Some(policy) = &mut policy {
                                    match policy.accepts(entry, false, &stopped) {
                                        Ok(allowed) => descend = allowed,
                                        Err(error) => {
                                            errors = true;
                                            descend = false;
                                            if diagnostic(
                                                writer,
                                                runtime,
                                                &format!(
                                                    "grep: {}: {error}",
                                                    entry.path().display()
                                                ),
                                            )? {
                                                return Ok(Exit::Ok);
                                            }
                                        }
                                    }
                                }
                            }
                            Ok(FileKind::File)
                                if selection.allows(relative, entry.depth())
                                    && !selection.hides_stripped(
                                        entry.path().as_os_str().as_bytes(),
                                        relative,
                                        entry.depth(),
                                    ) =>
                            {
                                if let Some(policy) = &mut policy {
                                    match policy.accepts(entry, false, &stopped) {
                                        Ok(true) => {}
                                        Ok(false) => continue,
                                        Err(error) => {
                                            errors = true;
                                            if diagnostic(
                                                writer,
                                                runtime,
                                                &format!(
                                                    "grep: {}: {error}",
                                                    entry.path().display()
                                                ),
                                            )? {
                                                return Ok(Exit::Ok);
                                            }
                                            continue;
                                        }
                                    }
                                }
                                if emit(
                                    plan,
                                    writer,
                                    cancelled,
                                    entry.path(),
                                    flush_each,
                                    &mut listed,
                                )? {
                                    return Ok(Exit::Ok);
                                }
                            }
                            Ok(_) => {}
                            Err(error) => {
                                if diagnostic(
                                    writer,
                                    runtime,
                                    &format!("grep: {}: {error}", entry.path().display()),
                                )? {
                                    return Ok(Exit::Ok);
                                }
                                errors = true;
                            }
                        }
                    }
                }
            }
            if !flush_each && let Some(error) = flush_output(writer, cancelled)? {
                return Ok(error);
            }
        }
        if let Some(exit) = flush_output(writer, cancelled)? {
            return Ok(exit);
        }
        Ok(if errors {
            Exit::Usage
        } else if listed {
            Exit::Ok
        } else {
            Exit::NoMatch
        })
    }
}

/// Flush accepted names before a diagnostic so stderr cannot overtake them.
/// A closed pipe remains a quiet success (D94).
fn diagnostic(
    writer: &mut impl Write,
    runtime: &Runtime<'_>,
    message: &str,
) -> Result<bool, String> {
    let cancelled = runtime.cancelled;
    if flush_output(writer, cancelled)?.is_some() {
        return Ok(true);
    }
    (runtime.stderr)(message).map_err(|error| error.to_string())?;
    Ok(false)
}

fn emit(
    plan: &Names,
    writer: &mut impl Write,
    cancelled: &AtomicBool,
    path: &Path,
    flush_each: bool,
    listed: &mut bool,
) -> Result<bool, String> {
    *listed = true;
    if plan.quiet {
        cancelled.store(true, Ordering::Release);
        return Ok(true);
    }
    presentation::path(
        &plan.presentation,
        writer,
        label(path, plan.paths.is_empty()),
    )
    .and_then(|()| writer.write_all(if plan.presentation.null { b"\0" } else { b"\n" }))
    .and_then(|()| if flush_each { writer.flush() } else { Ok(()) })
    .map_or_else(
        |error| {
            cancelled.store(true, Ordering::Release);
            // D94: a closed pipe stops the listing quietly, as in rg.
            if error.kind() == io::ErrorKind::BrokenPipe {
                Ok(true)
            } else {
                Err(format!("writing stdout: {error}"))
            }
        },
        |()| Ok(false),
    )
}

fn flush_output(writer: &mut impl Write, cancelled: &AtomicBool) -> Result<Option<Exit>, String> {
    match writer.flush() {
        Ok(()) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
            cancelled.store(true, Ordering::Release);
            Ok(Some(Exit::Ok))
        }
        Err(error) => {
            cancelled.store(true, Ordering::Release);
            Err(format!("writing stdout: {error}"))
        }
    }
}

#[cfg(test)]
thread_local! {
    // A clone shares the real lazy section cells, including loads performed
    // after open_catalog returns. Inspect it after the complete production run.
    static OPENED: std::cell::RefCell<Option<Catalog>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferret_catalog::Section;
    use ferret_crawl::{IndexOptions, Refresh, RefreshReason, RefreshRequest, RefreshScope};

    #[test]
    fn names_only_leaves_content_sections_unread_in_base_and_overlay() {
        let base =
            std::env::temp_dir().join(format!("ferret-names-sections-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("root");
        let index = base.join("index");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("empty"), b"").unwrap();
        fs::write(root.join("binary"), b"\0red").unwrap();
        fs::write(root.join("plain"), b"red\n").unwrap();
        fs::File::create(root.join("large"))
            .unwrap()
            .set_len(9 * 1024 * 1024)
            .unwrap();
        let options = IndexOptions::default();
        ferret_crawl::index(&index, std::slice::from_ref(&root), Refresh::All, &options).unwrap();
        for overlay in [false, true] {
            if overlay {
                fs::write(root.join("new"), b"green\n").unwrap();
                let engine =
                    Engine::from_writer(ferret_catalog::WriterSession::open(&index).unwrap());
                engine
                    .refresh(
                        RefreshRequest {
                            expected_generation: engine.pin().generation(),
                            scopes: vec![RefreshScope::Root(root.clone())],
                            rename_hints: Vec::new(),
                            reason: RefreshReason::Burst,
                        },
                        &options,
                    )
                    .unwrap();
            }
            for color in [line_grep::Color::Never, line_grep::Color::Always] {
                let plan = Names {
                    paths: vec![root.clone()],
                    globs: Vec::new(),
                    quiet: false,
                    hidden: false,
                    traversal: Default::default(),
                    presentation: line_grep::Presentation {
                        color,
                        null: color.enabled(),
                        ..Default::default()
                    },
                };
                let mut output = Vec::new();
                let exit =
                    execute(&plan, Some(&index), &mut output, &AtomicBool::new(false)).unwrap();
                assert!(matches!(exit, Exit::Ok));
                let mut expected: Vec<_> = ["binary", "empty", "large", "plain"]
                    .into_iter()
                    .chain(overlay.then_some("new"))
                    .map(|name| {
                        let path = root.join(name).into_os_string().into_encoded_bytes();
                        let mut bytes = Vec::new();
                        presentation::path(&plan.presentation, &mut bytes, &path).unwrap();
                        bytes
                    })
                    .collect();
                expected.sort();
                let mut actual: Vec<_> = output
                    .split(|&b| b == if color.enabled() { 0 } else { b'\n' })
                    .filter(|line| !line.is_empty())
                    .map(<[u8]>::to_vec)
                    .collect();
                actual.sort();
                assert_eq!(actual, expected);
                OPENED.with_borrow_mut(|opened| {
                    let catalog = opened.take().unwrap();
                    for section in [
                        Section::Doc,
                        Section::States,
                        Section::Docs,
                        Section::DocRefs,
                    ] {
                        assert!(
                            !catalog.is_loaded(section),
                            "{section:?}, overlay={overlay}"
                        );
                    }
                });
            }
        }
        fs::remove_dir_all(base).unwrap();
    }
}
