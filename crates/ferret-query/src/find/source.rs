//! Choosing an entry source for a plan: the live walk, a parallel or
//! sequential catalog walk, or a catalog walk over the entries an indexed
//! name pattern selects; and the catalog sections each one needs.

use super::*;

/// Paths for a known-entry catalog walk, each with its catalog target, or
/// `None` for a start.
type KnownEntries = (Vec<std::path::PathBuf>, Vec<Option<ferret_catalog::Target>>);

impl Plan {
    /// Creates the sequential live source. It never opens a catalog.
    pub fn live_source(&self) -> LiveWalk {
        LiveWalk::new(self.paths.clone(), self.options.clone())
    }

    /// Sections needed for traversal and the stored fields this plan reads.
    pub fn catalog_sections(&self) -> Vec<ferret_catalog::Section> {
        use ferret_catalog::Section;
        let mut sections = CatalogSource::sections().to_vec();
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

    /// Counted postings are safe only for a name test followed by printing,
    /// without traversal controls, effects, symlink following or live scopes.
    /// Candidates still use the ordinary entry evaluator and output commit.
    /// Without a built `index` this is the catalog walk: building one to
    /// answer a single find costs more than the walk it would save.
    pub fn indexed_catalog_source(
        &self,
        catalog: ferret_catalog::Catalog,
        index: Option<&crate::NameIndex>,
    ) -> LiveWalk {
        let Some((paths, known)) = self
            .indexed_pattern()
            .zip(index.filter(|index| index.can_accelerate_find()))
            .filter(|_| !self.touches_nested_root(&catalog))
            .and_then(|(pattern, index)| self.indexed_entries(&catalog, index, pattern))
        else {
            return self.parallel_catalog_source(catalog);
        };
        let mut options = self.options.clone();
        options.max_depth = Some(0);
        CatalogSource::known(catalog, paths, known, options).walk
    }

    /// The name pattern of a `-name P -print` find that postings can answer.
    fn indexed_pattern(&self) -> Option<&glob::Pattern> {
        let Expression::And(left, right) = &self.expression else {
            return None;
        };
        let (Expression::Name(pattern), Expression::Print(_)) = (&**left, &**right) else {
            return None;
        };
        let plain = !self.no_ignore
            && self.options.max_depth.is_none()
            && self.options.min_depth == 0
            && !self.options.depth_first
            && !self.options.xdev
            && self.options.follow == Follow::Physical
            && self.unsupported.is_none();
        plain.then_some(pattern)
    }

    /// Nested configured roots are separate catalog graph roots but the find
    /// walker attaches them at physical boundaries. Keep that walker for a
    /// start that holds a nested root or lies inside one; nesting elsewhere
    /// does not touch this find.
    fn touches_nested_root(&self, catalog: &ferret_catalog::Catalog) -> bool {
        use std::os::unix::ffi::OsStrExt;
        let roots: Vec<_> = catalog
            .roots()
            .map(|(_, path)| Path::new(std::ffi::OsStr::from_bytes(path)))
            .collect();
        let nested: Vec<_> = roots
            .iter()
            .filter(|path| {
                roots
                    .iter()
                    .any(|other| other != *path && path.starts_with(other))
            })
            .collect();
        self.paths.iter().any(|start| {
            let logical = self.options.logical_path(start);
            nested
                .iter()
                .any(|root| root.starts_with(&logical) || logical.starts_with(root))
        })
    }

    /// Each start and the entries below it whose names match `pattern`, read
    /// from the name postings, with each entry's catalog target. `None` when
    /// a start is not a catalogued directory or postings are not the plan.
    fn indexed_entries(
        &self,
        catalog: &ferret_catalog::Catalog,
        index: &crate::NameIndex,
        pattern: &glob::Pattern,
    ) -> Option<KnownEntries> {
        use ferret_catalog::{Handle, Target};
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let mut paths = Vec::new();
        let mut known = Vec::new();
        for start in &self.paths {
            let logical = self.options.logical_path(start);
            let resolved = catalog.resolve(logical.as_os_str().as_bytes())?;
            let Target::Inode(dir) = resolved.target else {
                return None;
            };
            if !resolved.remainder.is_empty() || !catalog.is_directory(dir) {
                return None;
            }
            let selection = index
                .select(
                    catalog,
                    Some(Handle {
                        generation: catalog.generation(),
                        id: dir,
                    }),
                    &[],
                    |name| pattern.matches(name),
                )
                .ok()?;
            if selection.estimate.plan != crate::NamePlan::Postings {
                return None;
            }
            let rows = selection.rows(catalog).ok()?;
            // Include the starting entry even when its own basename matches.
            paths.push(start.clone());
            known.push(None);
            let mut prefix = Vec::new();
            catalog.dir_path(dir, &mut prefix);
            let mut full = Vec::new();
            for row in rows {
                let target = catalog.name(row).target();
                if !matches!(target, Target::Inode(_)) {
                    continue;
                }
                full.clear();
                catalog.path(row, &mut full);
                let suffix = full
                    .strip_prefix(prefix.as_slice())
                    .and_then(|suffix| suffix.strip_prefix(b"/"))?;
                paths.push(start.join(OsString::from_vec(suffix.to_vec())));
                known.push(Some(target));
            }
        }
        Some((paths, known))
    }

    /// Creates a catalog walk. Load `catalog_sections()` before construction.
    pub fn catalog_source(&self, catalog: ferret_catalog::Catalog) -> CatalogSource {
        let mut options = self.options.clone();
        options.guard = leading_guard(&self.expression);
        CatalogSource::new(catalog, self.paths.clone(), options)
    }
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
