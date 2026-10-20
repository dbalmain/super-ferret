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
    ) -> io::Result<Self> {
        Ok(if unrestricted {
            Self::Live(live(path, cwd))
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

/// A walk of the live tree below directory operand `path`.
pub(super) fn live(path: &Path, cwd: &Path) -> LiveWalk {
    LiveWalk::paths(vec![start(path)], cwd.to_owned())
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
            Self::Catalog(source) => source.next(descend),
            Self::Live(source) => source.next(descend),
        }
    }
    fn next_with(
        &mut self,
        descend: bool,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
    ) -> Option<Result<&Entry, WalkError>> {
        match self {
            Self::Catalog(source) => source.next_with(descend, before_directory),
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
            Self::Catalog(source) => source.next_with_cancel(descend, before_directory, cancelled),
            Self::Live(source) => source.next_with_cancel(descend, before_directory, cancelled),
        }
    }
}
