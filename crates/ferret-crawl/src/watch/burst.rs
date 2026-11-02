//! A burst's refresh request: the paths it touched, narrowed to what the
//! catalog view still names, and the rename hints its paired moves give.

use super::*;

#[derive(Default)]
struct MovePair {
    old: Vec<(InoId, Vec<u8>)>,
    new: Vec<(InoId, Vec<u8>)>,
}

impl Burst {
    /// Whether this observation can retire directory watches. Ordinary file
    /// bursts do not scan the entire watch set at each publication.
    pub fn reconcile_watches(&self) -> bool {
        self.marker.is_some()
            || !self.roots.is_empty()
            || self.pending.iter().any(|((_, name), h)| {
                h.subtree || name == b".git" || ferret_policy::is_ignore_file(name)
            })
    }
    /// Loss/backstop reason associated with this detached watermark.
    pub fn reason(&self) -> RefreshReason {
        self.marker.map_or(RefreshReason::Burst, |(_, r)| r)
    }
    /// Resolves parent/name locators in the current checked generation. Missing
    /// ancestry or changed root boundaries widen observation conservatively.
    pub fn request(&self, view: &Catalog) -> RefreshRequest {
        let expected_generation = view.generation();
        if self.marker.is_some() {
            return RefreshRequest {
                expected_generation,
                scopes: Vec::new(),
                rename_hints: Vec::new(),
                reason: self.reason(),
            };
        }
        let mut scopes = Vec::new();
        let mut roots = self.roots.clone();
        let mut moves: BTreeMap<u32, MovePair> = BTreeMap::new();
        for ((_, name), hint) in &self.pending {
            for directory in &hint.directories {
                if name.is_empty() || name == b".git" || ferret_policy::is_ignore_file(name) {
                    roots.insert((*directory.root).clone());
                } else if let Some(parent) = directory.resolve(view) {
                    scopes.push(RefreshScope::Entry {
                        parent,
                        basename: name.clone(),
                    });
                    if hint.cookie != 0 {
                        let pair = moves.entry(hint.cookie).or_default();
                        if hint.from {
                            pair.old.push((parent, name.clone()));
                        }
                        if hint.to {
                            pair.new.push((parent, name.clone()));
                        }
                    }
                } else {
                    roots.insert((*directory.root).clone());
                }
            }
        }
        let changed_boundary = roots
            .iter()
            .any(|root| !view.roots().any(|(_, p)| p == root.as_os_str().as_bytes()));
        if changed_boundary {
            return RefreshRequest {
                expected_generation,
                scopes: Vec::new(),
                rename_hints: Vec::new(),
                reason: RefreshReason::Backstop,
            };
        }
        scopes.retain(|scope| match scope {
            RefreshScope::Entry { parent, .. } => {
                let mut path = Vec::new();
                view.dir_path(*parent, &mut path);
                !roots
                    .iter()
                    .any(|root| Path::new(OsStr::from_bytes(&path)).starts_with(root))
            }
            _ => true,
        });
        scopes.extend(roots.into_iter().map(RefreshScope::Root));
        RefreshRequest {
            expected_generation,
            scopes,
            rename_hints: rename_hints(moves),
            reason: self.reason(),
        }
    }
}

/// A hint for each move cookie seen exactly once on each side.
fn rename_hints(moves: BTreeMap<u32, MovePair>) -> Vec<RenameHint> {
    moves
        .into_values()
        .filter_map(|MovePair { mut old, mut new }| {
            if old.len() != 1 || new.len() != 1 {
                return None;
            }
            let (old_parent, old_name) = old.pop()?;
            let (new_parent, new_name) = new.pop()?;
            Some(RenameHint {
                old_parent,
                old_name,
                new_parent,
                new_name,
            })
        })
        .collect()
}
