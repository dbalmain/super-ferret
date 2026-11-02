//! [`CatalogSource`]: the walk over catalog children, with live listings
//! where the catalog holds none.

use super::*;

/// Catalog order, visibility and stored metadata. Explicit ignored starts and
/// opaque directories use live entries. The caller loads the plan's sections.
pub struct CatalogSource {
    pub(in super::super) walk: LiveWalk,
}

impl CatalogSource {
    /// Namespace sections needed by physical traversal, including their
    /// validated dependencies. No document/content sections are required.
    pub fn sections() -> &'static [ferret_catalog::Section] {
        use ferret_catalog::Section;
        &[
            Section::Names,
            Section::Links,
            Section::Roots,
            Section::Entries,
        ]
    }

    /// Catalog visibility with find's ordinary physical traversal and live
    /// fallback for explicit ignored/opaque starts. No expression is parsed.
    pub fn paths(catalog: Catalog, paths: Vec<PathBuf>, cwd: PathBuf) -> Self {
        Self::new(catalog, paths, Options::at(cwd))
    }

    /// [`LiveWalk::retaining_parents`] for the live listings of opaque and
    /// ignored directories. Catalogued listings open no directory to retain.
    pub fn retaining_parents(mut self) -> Self {
        self.walk.options.retain_parent = true;
        self.walk.options.bounded_parents = true;
        self
    }

    pub(in super::super) fn new(catalog: Catalog, paths: Vec<PathBuf>, options: Options) -> Self {
        Self::known(catalog, paths, Vec::new(), options)
    }

    /// As [`CatalogSource::new`], with each start's physical catalog target
    /// where the caller already resolved it, in step with `paths`.
    pub(in super::super) fn known(
        catalog: Catalog,
        paths: Vec<PathBuf>,
        known: Vec<Option<Target>>,
        options: Options,
    ) -> Self {
        let mut walk = LiveWalk::new(paths, options);
        walk.known = known.into_iter();
        walk.removed_children = walk
            .options
            .delete
            .then(|| Arc::new(Mutex::new(Removals::default())));
        for (id, path) in catalog.roots() {
            let path = Path::new(OsStr::from_bytes(path));
            if let Some(parent) = path.parent()
                && let Some(resolved) = catalog.resolve(parent.as_os_str().as_bytes())
                && resolved.remainder.is_empty()
                && let Target::Inode(parent) = resolved.target
            {
                walk.nested_roots.push((
                    parent,
                    id,
                    path.file_name()
                        .unwrap_or(OsStr::new(""))
                        .as_bytes()
                        .to_vec(),
                ));
            }
        }
        walk.nested_roots
            .sort_unstable_by_key(|(parent, _, _)| *parent);
        walk.catalog = Some(Arc::new(catalog));
        Self { walk }
    }
}

impl EntrySource for CatalogSource {
    fn catalog(&self) -> Option<&Catalog> {
        self.walk.catalog.as_deref()
    }
    fn next(&mut self, descend: bool) -> Option<Result<&Entry, WalkError>> {
        self.walk.next(descend)
    }
    fn next_with(
        &mut self,
        descend: bool,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
    ) -> Option<Result<&Entry, WalkError>> {
        self.walk.next_with(descend, before_directory)
    }
    fn next_with_cancel(
        &mut self,
        descend: bool,
        before_directory: &mut dyn FnMut() -> io::Result<()>,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Option<Result<&Entry, WalkError>> {
        self.walk
            .next_with_cancel(descend, before_directory, cancelled)
    }
}
