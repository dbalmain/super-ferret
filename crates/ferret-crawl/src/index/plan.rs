//! Which roots a run walks, keeps and drops, and how a root is named
//! (D34).

use std::os::unix::ffi::OsStrExt;
use std::path::Component;

use super::*;
use crate::walk::Boundary;

/// Which roots a run walks, keeps and drops.
pub(super) struct Plan {
    /// Every configured root, normalised and sorted.
    pub(super) roots: Vec<PathBuf>,
    pub(super) refresh: Vec<PathBuf>,
    pub(super) keep: Vec<PathBuf>,
    pub(super) dropped: Vec<PathBuf>,
    pub(super) selections: BTreeMap<PathBuf, std::sync::Arc<crate::refresh::Selection>>,
}

impl Plan {
    pub(super) fn new(
        previous: Option<&Catalog>,
        roots: &[PathBuf],
        refresh: Refresh<'_>,
        sniffer: u32,
        policy: ferret_catalog::Hash,
    ) -> Result<Plan, IndexError> {
        let mut configured = roots
            .iter()
            .map(|r| normalise(r))
            .collect::<Result<Vec<_>, _>>()?;
        configured.sort();
        configured.dedup();
        let old = configured_roots(previous);
        let everything = matches!(refresh, Refresh::All)
            || previous.is_some_and(|p| p.sniffer_version() != sniffer || p.policy() != policy);
        let named = match refresh {
            Refresh::All => Vec::new(),
            Refresh::Only(paths) => {
                let mut named = Vec::with_capacity(paths.len());
                for path in paths {
                    let path = normalise(path)?;
                    if configured.binary_search(&path).is_err() {
                        return Err(IndexError::NotConfigured(path));
                    }
                    named.push(path);
                }
                named
            }
        };
        // Roots added or removed this run: a kept root with one of these
        // strictly inside it would copy a subtree whose boundaries changed.
        let changed: Vec<&PathBuf> = configured
            .iter()
            .filter(|r| !old.contains(r))
            .chain(old.iter().filter(|r| configured.binary_search(r).is_err()))
            .collect();
        let (mut refresh, mut keep) = (Vec::new(), Vec::new());
        for root in &configured {
            let walk = everything
                || named.contains(root)
                || !old.contains(root)
                || changed.iter().any(|c| strictly_inside(c, root));
            if walk {
                refresh.push(root.clone());
            } else {
                keep.push(root.clone());
            }
        }
        let dropped = old
            .into_iter()
            .filter(|r| configured.binary_search(r).is_err())
            .collect();
        Ok(Plan {
            roots: configured,
            refresh,
            keep,
            dropped,
            selections: BTreeMap::new(),
        })
    }

    /// The configured roots strictly inside `root`, relative to it: where its
    /// walk stops, because the innermost root owns its subtree (D34).
    pub(super) fn boundaries(&self, root: &Path) -> Vec<Boundary> {
        self.roots
            .iter()
            .filter(|inner| strictly_inside(inner, root))
            .filter_map(|inner| inner.strip_prefix(root).ok())
            .map(|rel| Boundary {
                path: rel.to_owned(),
                id: None,
            })
            .collect()
    }
}

/// Whether `inner` lies strictly below `outer`, by whole components.
pub(super) fn strictly_inside(inner: &Path, outer: &Path) -> bool {
    inner != outer && inner.starts_with(outer)
}

/// `root` as the catalog names it: absolute, with `.` components and
/// trailing slashes gone. A `..` is refused rather than resolved, since
/// resolving it lexically can name a different directory than the kernel
/// would.
pub(super) fn normalise(root: &Path) -> Result<PathBuf, IndexError> {
    if !root.is_absolute() || root.components().any(|c| c == Component::ParentDir) {
        return Err(IndexError::BadRoot(root.to_owned()));
    }
    Ok(root.components().collect())
}

/// The roots `catalog` holds; none without one.
pub(super) fn configured_roots(catalog: Option<&Catalog>) -> Vec<PathBuf> {
    catalog
        .map(|c| {
            c.roots()
                .map(|(_, path)| PathBuf::from(OsStr::from_bytes(path)))
                .collect()
        })
        .unwrap_or_default()
}
