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
        // Follow an explicit directory symlink only at its start. Descendant
        // symlinks remain physical, using find's trailing-slash resolution.
        let mut start = path.as_os_str().to_owned();
        if !start.as_bytes().ends_with(b"/") {
            start.push("/");
        }
        let starts = vec![start.into()];
        Ok(if unrestricted {
            Self::Live(LiveWalk::paths(starts, cwd.to_owned()))
        } else {
            let catalog =
                catalog.ok_or_else(|| io::Error::other("directory input has no catalog"))?;
            Self::Catalog(CatalogSource::paths(
                catalog.clone(),
                starts,
                cwd.to_owned(),
            ))
        })
    }
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
