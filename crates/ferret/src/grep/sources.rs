//! Grep's namespace choice over find's constructors. Both content and
//! names-only modes retain physical traversal, cancellation and start spelling.

use super::*;
use ferret_catalog::Catalog;
use ferret_query::find::{Entry, LiveWalk, WalkError};

pub(super) enum Directory {
    Catalog(CatalogSource),
    Live(LiveWalk),
}

impl Directory {
    pub fn new(
        path: &Path,
        catalog: Option<&Catalog>,
        cwd: &Path,
        unrestricted: bool,
        policy: bool,
    ) -> io::Result<Self> {
        Ok(if unrestricted || policy {
            Self::Live(live(path, cwd, policy))
        } else {
            let catalog =
                catalog.ok_or_else(|| io::Error::other("directory input has no catalog"))?;
            Self::Catalog(CatalogSource::paths(
                catalog.clone(),
                vec![start(path)],
                cwd.to_owned(),
            ))
        })
    }

    pub fn following_links(self, follow: bool) -> Self {
        match self {
            Self::Live(source) => Self::Live(source.following_links(follow)),
            other => other,
        }
    }

    /// Keeps each listed directory open for the files listed in it, which
    /// content searches open beneath it (D21). Names-only listings open no
    /// file and leave it.
    pub fn retaining_parents(self) -> Self {
        match self {
            Self::Catalog(source) => Self::Catalog(source.retaining_parents()),
            Self::Live(source) => Self::Live(source.retaining_parents()),
        }
    }
}

/// A walk of the live tree below directory operand `path`. Under live
/// `policy`, each listed directory stays open with its hidden names, which
/// [`live::Policy`] reads the directory's ignore files through (D132).
pub(super) fn live(path: &Path, cwd: &Path, policy: bool) -> LiveWalk {
    let walk = LiveWalk::paths(vec![start(path)], cwd.to_owned()).lazy_directories();
    match policy {
        true => walk.retaining_parents().keeping_hidden_names(),
        false => walk,
    }
}

// Follow an explicit directory symlink only at its start. Descendant symlinks
// remain physical, using find's trailing-slash resolution.
fn start(path: &Path) -> PathBuf {
    let mut start = path.as_os_str().to_owned();
    if !start.as_bytes().ends_with(b"/") {
        start.push("/");
    }
    start.into()
}

impl EntrySource for Directory {
    fn next(&mut self, descend: bool) -> Option<Result<&Entry, WalkError>> {
        match self {
            Self::Catalog(source) => source.next(descend).map(grep_error),
            Self::Live(source) => source.next(descend),
        }
    }
    fn next_with(
        &mut self,
        descend: bool,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
    ) -> Option<Result<&Entry, WalkError>> {
        match self {
            Self::Catalog(source) => source.next_with(descend, before_directory).map(grep_error),
            Self::Live(source) => source.next_with(descend, before_directory),
        }
    }
    fn next_with_cancel(
        &mut self,
        descend: bool,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Option<Result<&Entry, WalkError>> {
        match self {
            Self::Catalog(source) => source
                .next_with_cancel(descend, before_directory, cancelled)
                .map(grep_error),
            Self::Live(source) => source.next_with_cancel(descend, before_directory, cancelled),
        }
    }
}

// Find's -I hint has a different meaning in grep.
fn grep_error(entry: Result<&Entry, WalkError>) -> Result<&Entry, WalkError> {
    entry.map_err(|mut error| {
        if error.is_outside_catalog() {
            error.error = io::Error::other("start is outside the catalog or the index is stale; run ferret index DIR or use -u");
        }
        error
    })
}
