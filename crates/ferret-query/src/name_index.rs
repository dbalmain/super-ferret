//! Generation-bound name planning: counted catalog row postings versus an
//! effective scope walk. Terms are derived here through ferret-text; catalog
//! owns the packed lists. Base state is shared, delta estimates are per view.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, OnceLock};

use ferret_catalog::{
    Catalog, Generation, Handle, InoId, NameId, PackedNameLists, PackedStrings, RetryFromCurrent,
    Target,
};

/// Candidate work, before the ordinary exact evaluator and output transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamePlan {
    Postings,
    ScopeWalk,
}

/// Stored-count estimate. None is an unknown subtree size, requiring a walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NameEstimate {
    pub plan: NamePlan,
    pub hits: u64,
    pub scope_rows: Option<u64>,
}

struct Base {
    terms: OnceLock<TermIndex>,
    scopes: Vec<u32>,
    live_scopes: Vec<InoId>,
}

struct TermIndex {
    terms: PackedStrings,
    keys: PackedNameLists,
}

/// A resident name index for exactly one generation; old query pins retain
/// their own delta. Name keys and scope ids cannot cross a checkpoint epoch.
pub struct NameIndex {
    generation: Generation,
    base: Arc<Base>,
    delta: BTreeMap<Vec<u8>, Vec<NameId>>,
    counts: BTreeMap<u32, i64>,
    scope_changes: BTreeMap<InoId, i64>,
    unknown_scopes: BTreeSet<InoId>,
    live_scopes: bool,
}

/// Selected distinct keys and delta rows. No postings are decoded until the
/// plan is selected, so common scoped names do not allocate a global hit list.
pub struct NameSelection {
    pub estimate: NameEstimate,
    base: Vec<u32>,
    delta: Vec<NameId>,
    generation: Generation,
    scope: Option<InoId>,
}

impl NameIndex {
    pub fn new(catalog: &Catalog) -> Self {
        Self::adopt(catalog, None)
    }

    /// Shares the immutable base only within the same checkpoint epoch.
    pub fn adopt(catalog: &Catalog, previous: Option<&Self>) -> Self {
        let names = catalog
            .resident_names()
            .unwrap_or_else(|| panic!("resident name index requires a resident catalog"));
        let checkpoint = catalog.checkpoint_base();
        let base = previous
            .filter(|old| {
                old.generation.checkpoint == catalog.generation().checkpoint
                    && old.generation.incarnation == catalog.generation().incarnation
            })
            .map_or_else(
                || {
                    let mut scopes = vec![0u32; checkpoint.base_dir_count() as usize];
                    for parent in checkpoint.name_reader().parents() {
                        scopes[parent.0 as usize] += 1;
                    }
                    for dir in (0..checkpoint.base_dir_count()).rev().map(InoId) {
                        if let Some(edge) = checkpoint.dir_name(dir) {
                            scopes[checkpoint.name(edge).parent.0 as usize] +=
                                scopes[dir.0 as usize];
                        }
                    }
                    let live_scopes = checkpoint
                        .dir_ids()
                        .filter(|&dir| {
                            checkpoint.entry_count(dir).is_none()
                                || checkpoint.retained_at(dir).is_some()
                        })
                        .collect();
                    Arc::new(Base {
                        terms: OnceLock::new(),
                        scopes,
                        live_scopes,
                    })
                },
                |old| old.base.clone(),
            );
        let mut out = Self {
            generation: catalog.generation(),
            base,
            delta: BTreeMap::new(),
            counts: BTreeMap::new(),
            scope_changes: BTreeMap::new(),
            unknown_scopes: BTreeSet::new(),
            live_scopes: false,
        };
        for id in catalog.suppressed_base_names() {
            *out.counts.entry(names.key(id)).or_default() -= 1;
            let edge = checkpoint.name(id);
            if let Target::Inode(child) = edge.target()
                && checkpoint.is_directory(child)
                && (!catalog.is_live_inode(child)
                    || catalog
                        .dir_name(child)
                        .is_none_or(|id| catalog.name(id).parent != edge.parent))
            {
                out.mark_unknown(&checkpoint, edge.parent);
            }
            out.adjust_scope(&checkpoint, edge.parent, -1);
        }
        for &(_, row) in catalog.delta_names().1 {
            let id = NameId(row);
            let edge = catalog.name(id);
            if let Target::Inode(child) = edge.target()
                && catalog.is_directory(child)
            {
                let parent = (child.0 < checkpoint.base_dir_count())
                    .then(|| checkpoint.dir_name(child))
                    .flatten()
                    .map(|id| checkpoint.name(id).parent);
                if parent != Some(edge.parent) {
                    out.mark_unknown(catalog, edge.parent);
                }
            }
            out.delta.entry(edge.bytes.to_vec()).or_default().push(id);
            out.adjust_scope(catalog, edge.parent, 1);
        }
        out.live_scopes = out
            .base
            .live_scopes
            .iter()
            .copied()
            .chain(catalog.changed_directory_scopes())
            .any(|dir| {
                catalog.is_live_inode(dir)
                    && (catalog.entry_count(dir).is_none() || catalog.retained_at(dir).is_some())
            });
        out
    }

    fn mark_unknown(&mut self, catalog: &Catalog, mut dir: InoId) {
        loop {
            self.unknown_scopes.insert(dir);
            let Some(edge) = catalog.dir_name(dir) else {
                break;
            };
            dir = catalog.name(edge).parent;
        }
    }

    fn adjust_scope(&mut self, catalog: &Catalog, mut dir: InoId, change: i64) {
        loop {
            *self.scope_changes.entry(dir).or_default() += change;
            let Some(edge) = catalog.dir_name(dir) else {
                break;
            };
            dir = catalog.name(edge).parent;
        }
    }

    /// A postings find source must not bypass live fallback or its diagnostics.
    pub fn can_accelerate_find(&self) -> bool {
        !self.live_scopes
    }

    pub fn generation(&self) -> Generation {
        self.generation
    }
    pub fn bytes(&self) -> usize {
        self.base
            .terms
            .get()
            .map_or(0, |terms| terms.terms.bytes() + terms.keys.bytes())
            + self.base.scopes.capacity() * std::mem::size_of::<u32>()
            + self
                .delta
                .iter()
                .map(|(name, rows)| name.len() + rows.len() * 4)
                .sum::<usize>()
    }

    /// Terms constrain the distinct table first; an empty slice scans all
    /// distinct names. The predicate preserves substring/glob/regex semantics.
    pub fn select(
        &self,
        catalog: &Catalog,
        scope: Option<Handle<InoId>>,
        terms: &[&[u8]],
        mut matches: impl FnMut(&[u8]) -> bool,
    ) -> Result<NameSelection, RetryFromCurrent> {
        catalog.generation().check(self.generation)?;
        let scope = scope
            .map(|handle| catalog.checked_inode(handle))
            .transpose()?;
        let names = catalog
            .resident_names()
            .unwrap_or_else(|| unreachable!("resident index source"));
        let mut keys = if terms.is_empty() {
            (0..names.distinct_count()).collect()
        } else {
            let mut candidates: Option<Vec<u32>> = None;
            let term_index = self.base.terms.get_or_init(|| build_terms(names));
            for term in terms {
                let mut keys = Vec::new();
                if let Ok(list) = term_index.terms.binary_search(term) {
                    term_index.keys.get(list as u32, &mut keys);
                }
                match &mut candidates {
                    None => candidates = Some(keys),
                    Some(current) => current.retain(|key| keys.binary_search(key).is_ok()),
                }
            }
            candidates.unwrap_or_default()
        };
        let mut hits = 0u64;
        keys.retain(|&key| {
            let count = i64::from(names.count(key)) + self.counts.get(&key).copied().unwrap_or(0);
            assert!(count >= 0, "negative effective posting count");
            let keep = count != 0 && matches(names.distinct_name(key));
            if keep {
                hits += count as u64;
            }
            keep
        });
        let mut delta = Vec::new();
        for (name, rows) in &self.delta {
            if terms.iter().all(|term| ferret_text::has_token(name, term)) && matches(name) {
                hits += rows.len() as u64;
                delta.extend_from_slice(rows);
            }
        }
        // A moved/replaced directory changes the membership of an arbitrarily
        // large base subtree. Until a subtree count is known, take the safe
        // effective walk instead of treating the old count as current.
        let scope_rows = match scope {
            None => Some(u64::from(catalog.name_count())),
            Some(dir) if self.unknown_scopes.contains(&dir) => None,
            Some(dir) => self.base.scopes.get(dir.0 as usize).map(|&count| {
                (i64::from(count) + self.scope_changes.get(&dir).copied().unwrap_or(0)).max(0)
                    as u64
            }),
        };
        // Scoped postings pay for ancestry and merging, while a walk reads one
        // row per scope entry. The factor is provisional, exposed by estimates
        // and the production benchmark rather than always choosing postings.
        let plan = if scope.is_none() && hits < u64::from(catalog.name_count())
            || scope_rows.is_some_and(|rows| hits.saturating_mul(8) < rows)
        {
            NamePlan::Postings
        } else {
            NamePlan::ScopeWalk
        };
        Ok(NameSelection {
            estimate: NameEstimate {
                plan,
                hits,
                scope_rows,
            },
            base: keys,
            delta,
            generation: self.generation,
            scope,
        })
    }
}

fn build_terms(names: &ferret_catalog::ResidentNames) -> TermIndex {
    let mut terms: BTreeMap<Vec<u8>, Vec<u32>> = BTreeMap::new();
    for key in 0..names.distinct_count() {
        let mut tokens = Vec::new();
        ferret_text::tokens(names.distinct_name(key), |token| {
            tokens.push(token.to_vec())
        });
        tokens.sort_unstable();
        tokens.dedup();
        for token in tokens {
            terms.entry(token).or_default().push(key);
        }
    }
    let strings = terms.keys().cloned().collect::<Vec<_>>();
    let keys = PackedNameLists::new(terms.values().map(Vec::as_slice));
    TermIndex {
        terms: PackedStrings::new(strings),
        keys,
    }
}

impl NameSelection {
    /// Effective row candidates in epoch-id order. A caller that observes find
    /// traversal order must use a safe evaluator seam, not reorder effects.
    pub fn rows(&self, catalog: &Catalog) -> Result<Vec<NameId>, RetryFromCurrent> {
        catalog.generation().check(self.generation)?;
        let mut rows = Vec::new();
        match self.estimate.plan {
            NamePlan::Postings => {
                let names = catalog
                    .resident_names()
                    .unwrap_or_else(|| unreachable!("resident index source"));
                for &key in &self.base {
                    names.postings(key, &mut rows);
                }
                rows.retain(|&id| catalog.base_name_live(NameId(id)));
                rows.sort_unstable();
                let mut delta: Vec<_> = self.delta.iter().map(|id| id.0).collect();
                delta.sort_unstable();
                rows.extend(delta);
                if let Some(scope) = self.scope {
                    let mut memo = HashMap::new();
                    rows.retain(|&id| {
                        below(catalog, catalog.name(NameId(id)).parent, scope, &mut memo)
                    });
                }
            }
            NamePlan::ScopeWalk => match self.scope {
                None => rows.extend(catalog.names().map(|(id, _)| id.0)),
                Some(scope) => {
                    let mut pending = vec![scope];
                    while let Some(dir) = pending.pop() {
                        for entry in catalog.entries(dir) {
                            rows.push(entry.name.0);
                            if let Target::Inode(child) = entry.target
                                && catalog.is_directory(child)
                            {
                                pending.push(child);
                            }
                        }
                    }
                    rows.sort_unstable();
                }
            },
        }
        Ok(rows.into_iter().map(NameId).collect())
    }
}

fn below(catalog: &Catalog, mut dir: InoId, scope: InoId, memo: &mut HashMap<InoId, bool>) -> bool {
    let mut chain = Vec::new();
    let answer = loop {
        if dir == scope {
            break true;
        }
        if let Some(&answer) = memo.get(&dir) {
            break answer;
        }
        chain.push(dir);
        let Some(edge) = catalog.dir_name(dir) else {
            break false;
        };
        dir = catalog.name(edge).parent;
    };
    for dir in chain {
        memo.insert(dir, answer);
    }
    answer
}
