//! Sparse physical hard-link proofs. A file with an unindexed link needs
//! polling because directory events do not cover writes through that name.
use super::*;

type PhysicalName = ((u64, u64), Vec<u8>);

#[derive(Clone, Debug)]
pub(super) struct FileAliases {
    pub(super) nlink: u64,
    // Bind occurrences of one parent/name are one filesystem hard link.
    pub(super) names: BTreeMap<PhysicalName, BTreeSet<PathBuf>>,
}
impl FileAliases {
    pub(super) fn unproven_roots(&self) -> impl Iterator<Item = &PathBuf> {
        self.names
            .values()
            .flatten()
            .filter(|_| self.nlink > self.names.len() as u64)
    }
}

impl Watch {
    /// Accounts for distinct physical hard-link names at observed handles.
    /// A link outside indexed occurrences cannot promise directory events.
    pub(crate) fn file(&self, root: &Path, entry: &crate::Decided<'_, ferret_catalog::DirToken>) {
        let Some(stat) = entry.stat else {
            return;
        };
        if entry.kind != ferret_catalog::Kind::File {
            return;
        }
        let identity = (stat.dev, stat.ino);
        if stat.nlink <= 1 {
            return;
        }
        let Ok(parent) = fstat(entry.parent_fd) else {
            self.gap(root);
            return;
        };
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let alias = s
            .file_aliases
            .entry(identity)
            .or_insert_with(|| FileAliases {
                nlink: stat.nlink,
                names: BTreeMap::new(),
            });
        alias.nlink = stat.nlink;
        alias
            .names
            .entry((
                (parent.st_dev, parent.st_ino),
                entry.name.as_bytes().to_vec(),
            ))
            .or_default()
            .insert(root.to_owned());
    }

    /// Validates the sparse hard-link proof against the checked successor.
    /// Retired names cannot inflate the number of observable physical links.
    pub fn adopt_aliases(&self, view: &Catalog) {
        let (mut aliases, parents) = {
            let s = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let parents = s
                .file_aliases
                .values()
                .flat_map(|a| a.names.keys().map(|(p, _)| *p))
                .map(|p| {
                    (
                        p,
                        s.identities
                            .get(&p)
                            .and_then(|wd| s.descriptors.get(wd))
                            .cloned()
                            .unwrap_or_default(),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            (s.file_aliases.clone(), parents)
        };
        aliases.retain(|identity, alias| {
            alias.names.retain(|(parent, name), owners| {
                owners.clear();
                for d in &parents[parent] {
                    if d.resolve(view).is_none() {
                        continue;
                    }
                    let path = d.path().join(OsStr::from_bytes(name));
                    let Some(resolved) = view.resolve(path.as_os_str().as_bytes()) else {
                        continue;
                    };
                    let ferret_catalog::Target::Inode(id) = resolved.target else {
                        continue;
                    };
                    let stat = view.inode(id).stat;
                    if resolved.remainder.is_empty() && (stat.dev, stat.ino) == *identity {
                        owners.insert((*d.root).clone());
                        alias.nlink = stat.nlink;
                    }
                }
                !owners.is_empty()
            });
            alias.nlink > 1 && !alias.names.is_empty()
        });
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .file_aliases = aliases;
    }
}
