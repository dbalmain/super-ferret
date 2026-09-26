//! Interned rules and rule lists, shared by every directory of one root.
//!
//! A directory's rules are one list of basename rules (see `rules`). Most
//! directories have the same list as many others: the global file plus the
//! same work tree's `.gitignore` files, with no anchored pattern live. So a
//! list is identified by its rules' text in order, compiled once into an
//! index, and every directory with that list holds the one [`Arc`].
//!
//! Two tables do this, both behind a [`Mutex`]:
//!
//! - rules, by [`RuleText`] plus band: each distinct rule gets an id when an
//!   ignore file is compiled, so building a list's key hashes no globs;
//! - lists, by the ids of their rules in order.
//!
//! A rule list lookup takes the lock only when a directory's list differs
//! from its parent's (the caller compares keys first), and compiles a miss
//! outside the lock. Neither table evicts: the rule table holds at most the
//! distinct lines (and anchored projections) of the ignore files read under
//! the root, and the list table at most one list per directory entered, in
//! practice 611 for the 76,771 directories under a home directory. Total
//! retained positions are the sum of the lengths of the distinct lists, which
//! is quadratic in depth for a chain in which every level adds rules. Both are
//! dropped with the last [`DirRules`](crate::DirRules) of the root.

use std::sync::{Arc, Mutex, PoisonError};

use crate::gitignore::{FxHashMap, Gitignore, Match, Pattern, RuleText};

/// One distinct basename rule.
#[derive(Debug)]
pub(crate) struct Rule {
    id: usize,
    /// Basename-only. Its line is meaningless: a list renumbers it.
    pattern: Pattern,
    /// From a `.ferretignore`: part of the rule's identity, because only
    /// these whitelist while traversing an excluded directory.
    ferret: bool,
}

/// One directory's rules compiled for matching, lowest precedence first.
#[derive(Debug)]
pub(crate) struct List {
    /// The rule ids in order: the list's identity.
    key: Arc<[usize]>,
    index: Gitignore,
    /// Positions from here on came from a `.ferretignore`. Ferret files come
    /// last in every list, so one boundary is enough.
    ferret_from: usize,
}

/// The rule and list tables of one root.
#[derive(Debug, Default)]
pub(crate) struct Lists {
    rules: Mutex<FxHashMap<(RuleText, bool), Arc<Rule>>>,
    lists: Mutex<FxHashMap<Arc<[usize]>, Arc<List>>>,
}

impl Rule {
    pub(crate) fn id(&self) -> usize {
        self.id
    }
}

impl List {
    /// Compiles `rules`, lowest precedence first. The index orders by a
    /// pattern's position, and in a list merged from several files a
    /// source line is not a position, so each pattern is renumbered by its
    /// place in the list.
    pub(crate) fn compile(rules: &[&Rule]) -> Self {
        let ferret_from = rules
            .iter()
            .position(|rule| rule.ferret)
            .unwrap_or(rules.len());
        debug_assert!(rules[ferret_from..].iter().all(|rule| rule.ferret));
        Self {
            key: rules.iter().map(|rule| rule.id).collect(),
            index: Gitignore::from_patterns(
                rules
                    .iter()
                    .enumerate()
                    .map(|(at, rule)| rule.pattern.renumbered(at)),
            ),
            ferret_from,
        }
    }

    pub(crate) fn key(&self) -> &[usize] {
        &self.key
    }

    /// The last rule matching entry `name`: whether it whitelists, and
    /// whether it came from a `.ferretignore`.
    pub(crate) fn last_match(&self, name: &[u8], is_dir: bool) -> Option<(bool, bool)> {
        self.index
            .best(name, is_dir)
            .map(|(at, result)| (result == Match::Whitelist, at >= self.ferret_from))
    }
}

impl Lists {
    /// The one rule with `pattern`'s text and band. `pattern` must be a
    /// basename pattern.
    pub(crate) fn rule(&self, pattern: Pattern, ferret: bool) -> Arc<Rule> {
        let text = (pattern.text(), ferret);
        let mut rules = self.rules.lock().unwrap_or_else(PoisonError::into_inner);
        let id = rules.len();
        Arc::clone(rules.entry(text).or_insert_with(|| {
            Arc::new(Rule {
                id,
                pattern,
                ferret,
            })
        }))
    }

    /// The one compiled list of `rules`, compiling it on first use. `key`
    /// is their ids, which the caller has already built.
    pub(crate) fn list(&self, key: &[usize], rules: &[&Rule]) -> Arc<List> {
        debug_assert!(key.iter().eq(rules.iter().map(|rule| &rule.id)));
        if let Some(list) = self.lock_lists().get(key) {
            return Arc::clone(list);
        }
        // Compile outside the lock. Two threads may both miss and compile the
        // same list; the first to insert wins and the other's copy is dropped.
        let list = Arc::new(List::compile(rules));
        Arc::clone(
            self.lock_lists()
                .entry(Arc::clone(&list.key))
                .or_insert(list),
        )
    }

    /// How many distinct lists have been compiled.
    #[cfg(test)]
    pub(crate) fn list_count(&self) -> usize {
        self.lock_lists().len()
    }

    fn lock_lists(&self) -> std::sync::MutexGuard<'_, FxHashMap<Arc<[usize]>, Arc<List>>> {
        self.lists.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
