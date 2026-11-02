//! The query tree: what every match of a regex contains, and the
//! required-literal variant `ferret-bench` measures through the same fold.

use std::collections::BTreeSet;

use super::*;
use crate::trigram::fold::string_trigrams;

/// What every match of a regex contains: necessary, never sufficient.
///
/// The derivation builds it through constructors that flatten, sort,
/// deduplicate and apply the identities, so in a derived tree `And` and `Or`
/// hold at least two distinct children, never `All` or `None`, and the root
/// holds at most 64 distinct keys.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Trigrams {
    /// No constraint: every document is a candidate.
    All,
    /// No document can match.
    None,
    /// Folded by [`ferret_text::trigram_key`]'s rule.
    Key([u8; 3]),
    And(Vec<Trigrams>),
    Or(Vec<Trigrams>),
}

impl Trigrams {
    pub(super) fn and(parts: impl IntoIterator<Item = Trigrams>) -> Trigrams {
        let mut flat = Vec::new();
        for part in parts {
            match part {
                Trigrams::All => {}
                Trigrams::None => return Trigrams::None,
                Trigrams::And(inner) => flat.extend(inner),
                other => flat.push(other),
            }
        }
        Trigrams::collect(flat, Trigrams::All, Trigrams::And)
    }

    pub(super) fn or(parts: impl IntoIterator<Item = Trigrams>) -> Trigrams {
        let mut flat = Vec::new();
        for part in parts {
            match part {
                Trigrams::None => {}
                Trigrams::All => return Trigrams::All,
                Trigrams::Or(inner) => flat.extend(inner),
                other => flat.push(other),
            }
        }
        Trigrams::collect(flat, Trigrams::None, Trigrams::Or)
    }

    fn collect(
        mut flat: Vec<Trigrams>,
        identity: Trigrams,
        node: fn(Vec<Trigrams>) -> Trigrams,
    ) -> Trigrams {
        flat.sort();
        flat.dedup();
        match flat.len() {
            0 => identity,
            1 => flat.pop().unwrap_or(identity),
            _ => node(flat),
        }
    }

    /// The tree within the 64-key cap, using dictionary frequencies: common
    /// bare keys are discarded first. Alternatives are weakened, never
    /// deleted. A tree within the cap is returned unchanged.
    pub fn fit_by_frequency(
        &self,
        frequencies: &std::collections::BTreeMap<[u8; 3], u64>,
    ) -> Trigrams {
        if self.keys().len() <= LIMITS.keys {
            return self.clone();
        }
        self.clone().fit_by(LIMITS.keys, &|key| {
            frequencies.get(&key).copied().unwrap_or(u64::MAX)
        })
    }

    /// Every distinct key in the tree.
    pub fn keys(&self) -> BTreeSet<[u8; 3]> {
        let mut keys = BTreeSet::new();
        self.collect_keys(&mut keys);
        keys
    }

    fn collect_keys(&self, keys: &mut BTreeSet<[u8; 3]>) {
        match self {
            Trigrams::All | Trigrams::None => {}
            Trigrams::Key(key) => {
                keys.insert(*key);
            }
            Trigrams::And(parts) | Trigrams::Or(parts) => {
                parts.iter().for_each(|p| p.collect_keys(keys));
            }
        }
    }

    /// The tree weakened to at most `budget` keys (counted with
    /// repeats, so distinct keys are fewer still) by replacing subtrees with
    /// `All`, which only admits more documents. An `And` keeps what fits:
    /// its bare keys first, the largest first (M5 orders by df instead),
    /// then its other children, smallest first, each fitted to what is left.
    /// An `Or` must keep every alternative, so each is fitted to an equal
    /// share; a share of zero makes that alternative, and so the `Or`, `All`.
    /// An alternative is never deleted.
    pub(super) fn fit(self, budget: usize) -> Trigrams {
        self.fit_by(budget, &|_| 0)
    }

    fn fit_by(self, budget: usize, frequency: &dyn Fn([u8; 3]) -> u64) -> Trigrams {
        match self {
            Trigrams::Key(_) if budget == 0 => Trigrams::All,
            Trigrams::And(mut parts) => {
                parts.sort_by_cached_key(|p| match p {
                    Trigrams::Key(key) => (0, frequency(*key), std::cmp::Reverse(*key), 0),
                    other => (1, 0, std::cmp::Reverse([0; 3]), other.weight()),
                });
                let mut left = budget;
                let mut kept = Vec::new();
                for part in parts {
                    let part = part.fit_by(left, frequency);
                    left -= part.weight();
                    kept.push(part);
                }
                Trigrams::and(kept)
            }
            Trigrams::Or(parts) => {
                let share = budget / parts.len();
                Trigrams::or(parts.into_iter().map(|p| p.fit_by(share, frequency)))
            }
            other if other.weight() <= budget => other,
            _ => Trigrams::All,
        }
    }

    /// Keys counted with repeats: an upper bound on distinct keys.
    fn weight(&self) -> usize {
        match self {
            Trigrams::All | Trigrams::None => 0,
            Trigrams::Key(_) => 1,
            Trigrams::And(parts) | Trigrams::Or(parts) => parts.iter().map(Trigrams::weight).sum(),
        }
    }
}

/// Folded byte strings that every regex match must satisfy. An alternative
/// with no useful literal remains `All`; no branch may be dropped.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RequiredLiterals {
    All,
    None,
    Literal(Vec<u8>),
    And(Vec<RequiredLiterals>),
    Or(Vec<RequiredLiterals>),
}

/// The fold saves strings at exactly the same points for both consumers.
/// Trigrams retain their shipped simplification and key budget.
pub(super) trait Facts: Clone {
    fn all() -> Self;
    fn and(parts: impl IntoIterator<Item = Self>) -> Self;
    fn or(parts: impl IntoIterator<Item = Self>) -> Self;
    fn string(bytes: &[u8]) -> Self;
}

impl Facts for Trigrams {
    fn all() -> Self {
        Self::All
    }
    fn and(parts: impl IntoIterator<Item = Self>) -> Self {
        Self::and(parts)
    }
    fn or(parts: impl IntoIterator<Item = Self>) -> Self {
        Self::or(parts)
    }
    fn string(bytes: &[u8]) -> Self {
        string_trigrams(bytes)
    }
}

impl Facts for RequiredLiterals {
    fn all() -> Self {
        Self::All
    }
    fn and(parts: impl IntoIterator<Item = Self>) -> Self {
        let mut flat = Vec::new();
        for part in parts {
            match part {
                Self::All => {}
                Self::None => return Self::None,
                Self::And(inner) => flat.extend(inner),
                other => flat.push(other),
            }
        }
        Self::collect(flat, Self::All, Self::And)
    }
    fn or(parts: impl IntoIterator<Item = Self>) -> Self {
        let mut flat = Vec::new();
        for part in parts {
            match part {
                Self::None => {}
                Self::All => return Self::All,
                Self::Or(inner) => flat.extend(inner),
                other => flat.push(other),
            }
        }
        Self::collect(flat, Self::None, Self::Or)
    }
    fn string(bytes: &[u8]) -> Self {
        if bytes.len() < 3 {
            Self::All
        } else {
            Self::Literal(bytes.to_vec())
        }
    }
}

impl RequiredLiterals {
    fn collect(mut flat: Vec<Self>, identity: Self, node: fn(Vec<Self>) -> Self) -> Self {
        flat.sort();
        flat.dedup();
        match flat.len() {
            0 => identity,
            1 => flat.pop().unwrap_or(identity),
            _ => node(flat),
        }
    }
}
