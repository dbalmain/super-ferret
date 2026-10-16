//! Names-only grep uses find's namespace sources without opening an engine or
//! preparing document identities, postings, or content sections.

use ferret_catalog::Catalog;
use ferret_query::line_grep::Names;

use super::*;

pub(super) fn run(plan: &Names, index: Option<&Path>) -> Exit {
    let cancelled = AtomicBool::new(false);
    match execute(plan, index, &mut io::stdout().lock(), &cancelled) {
        Ok(exit) => exit,
        Err(error) => failure(&error),
    }
}

fn open_catalog(index: Option<&Path>) -> Result<Catalog, String> {
    let index = index_path(index)?;
    let catalog = Catalog::open(&index)
        .map_err(|error| format!("{}: {error}; re-index this directory", index.display()))?
        .ok_or_else(|| format!("no index in {}; run ferret index DIR", index.display()))?;
    catalog
        .load(CatalogSource::sections())
        .map_err(|error| error.to_string())?;
    #[cfg(test)]
    OPENED.with_borrow_mut(|opened| *opened = Some(catalog.clone()));
    Ok(catalog)
}

// An explicit directory symlink is followed at its start only, like rg.
// Find's trailing-slash resolution provides this while keeping descendants
// physical: recursive symlinks are still skipped.
fn source(catalog: &Catalog, path: &Path, cwd: &Path) -> CatalogSource {
    let mut path = path.as_os_str().to_owned();
    if !path.as_bytes().ends_with(b"/") {
        path.push("/");
    }
    CatalogSource::paths(catalog.clone(), vec![path.into()], cwd.to_owned())
}

fn execute(
    plan: &Names,
    index: Option<&Path>,
    writer: &mut impl Write,
    cancelled: &AtomicBool,
) -> Result<Exit, String> {
    let mut selection = GrepSelection::default().with_hidden(plan.hidden);
    for glob in &plan.globs {
        selection
            .push(glob)
            .map_err(|error| format!("glob {glob:?}: {error}"))?;
    }
    let inputs = resolve_inputs(&plan.paths, true);
    let catalog = inputs
        .iter()
        .any(|input| matches!(input, Input::Directory(_)))
        .then(|| open_catalog(index))
        .transpose()?;
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    if let Some(catalog) = &catalog {
        for input in &inputs {
            if let Input::Directory(path) = input
                && let Some(Err(error)) = source(catalog, path, &cwd).next(true)
            {
                return Err(format!(
                    "{}: {}; index this directory; live modes arrive in M6",
                    error.path.display(),
                    error.error
                ));
            }
        }
    }
    let mut errors = false;
    for input in &inputs {
        if let Input::Failed(path, error) = input {
            cli::error(&format!("grep: {}: {error}", path.display()));
            errors = true;
        }
    }
    let mut listed = false;
    let mut emit = |path: &Path| -> Result<bool, String> {
        listed = true;
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
        .map_err(|error| {
            cancelled.store(true, Ordering::Release);
            format!("writing stdout: {error}")
        })?;
        Ok(false)
    };
    for input in inputs {
        if cancelled.load(Ordering::Acquire) {
            break;
        }
        match input {
            Input::File(path, _) => {
                if emit(&path)? {
                    return Ok(Exit::Ok);
                }
            }
            Input::Stdin => {
                if emit(Path::new("-"))? {
                    return Ok(Exit::Ok);
                }
            }
            Input::Failed(_, _) => {}
            Input::Directory(path) => {
                let Some(catalog) = &catalog else {
                    unreachable!("directory catalog")
                };
                let mut source = source(catalog, &path, &cwd);
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
                            cli::error(&format!("grep: {}: {}", error.path.display(), error.error));
                            errors = true;
                            continue;
                        }
                    };
                    let relative = entry
                        .path()
                        .strip_prefix(&cwd)
                        .unwrap_or(entry.path())
                        .as_os_str()
                        .as_bytes();
                    match entry.kind() {
                        Ok(FileKind::Directory) => {
                            descend = selection.descend(relative, entry.depth())
                        }
                        Ok(FileKind::File) if selection.allows(relative, entry.depth()) => {
                            if emit(entry.path())? {
                                return Ok(Exit::Ok);
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            cli::error(&format!("grep: {}: {error}", entry.path().display()));
                            errors = true;
                        }
                    }
                }
            }
        }
    }
    writer.flush().map_err(|error| {
        cancelled.store(true, Ordering::Release);
        format!("writing stdout: {error}")
    })?;
    Ok(if errors {
        Exit::Usage
    } else if listed {
        Exit::Ok
    } else {
        Exit::NoMatch
    })
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
