//! A screen's plan: the boolean tree of index lookups an arm reads, built
//! from a regex's trigrams, its gram needs, or its required literals.

use std::collections::BTreeSet;

use ferret_verify::Trigrams;
use ferret_verify::trigram::RequiredLiterals;

use super::super::query::Need;
use super::hashed::Hashed;
use super::units::{self, Unit};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Leaf {
    Tri(Vec<u8>),
    Term(Vec<u8>),
    Prefix(Vec<u8>),
    /// Every term containing these lowercase bytes: one dictionary pass.
    Substring(Vec<u8>),
    /// A key of the arm's own segment: word 4+5 or a hashed variant.
    Extra(Vec<u8>),
    /// Documents whose terms are incomplete: T1 suppressed a run, or a
    /// token over the 64-byte cap lost its tail.
    Fallback,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Plan {
    All,
    None,
    Leaf(Leaf),
    And(Vec<Plan>),
    Or(Vec<Plan>),
}

impl Plan {
    pub(super) fn and(parts: impl IntoIterator<Item = Self>) -> Self {
        let mut flat = Vec::new();
        for part in parts {
            match part {
                Self::All => {}
                Self::None => return Self::None,
                Self::And(inner) => flat.extend(inner),
                other => flat.push(other),
            }
        }
        flat.sort();
        flat.dedup();
        match flat.len() {
            0 => Self::All,
            1 => flat.remove(0),
            _ => Self::And(flat),
        }
    }

    pub(super) fn or(parts: impl IntoIterator<Item = Self>) -> Self {
        let mut flat = Vec::new();
        for part in parts {
            match part {
                Self::None => {}
                Self::All => return Self::All,
                Self::Or(inner) => flat.extend(inner),
                other => flat.push(other),
            }
        }
        flat.sort();
        flat.dedup();
        match flat.len() {
            0 => Self::None,
            1 => flat.remove(0),
            _ => Self::Or(flat),
        }
    }

    pub(super) fn leaves(&self, out: &mut BTreeSet<Leaf>) {
        match self {
            Self::Leaf(leaf) => {
                out.insert(leaf.clone());
            }
            Self::And(parts) | Self::Or(parts) => parts.iter().for_each(|p| p.leaves(out)),
            Self::All | Self::None => {}
        }
    }

    pub(super) fn has(&self, want: &impl Fn(&Leaf) -> bool) -> bool {
        let mut leaves = BTreeSet::new();
        self.leaves(&mut leaves);
        leaves.iter().any(want)
    }
}

pub(super) fn from_trigrams(tree: &Trigrams) -> Plan {
    match tree {
        Trigrams::All => Plan::All,
        Trigrams::None => Plan::None,
        Trigrams::Key(key) => Plan::Leaf(Leaf::Tri(key.to_vec())),
        Trigrams::And(parts) => Plan::and(parts.iter().map(from_trigrams)),
        Trigrams::Or(parts) => Plan::or(parts.iter().map(from_trigrams)),
    }
}

pub(super) fn from_need(need: &Need) -> Plan {
    match need {
        Need::All => Plan::All,
        Need::None => Plan::None,
        Need::Key(key) => Plan::Leaf(Leaf::Extra(key.clone())),
        Need::And(parts) => Plan::and(parts.iter().map(from_need)),
        Need::Or(parts) => Plan::or(parts.iter().map(from_need)),
    }
}

/// How the term screen reads a literal: the brief's units (exact and
/// prefix lookups, sound only case-sensitive), substrings of whole terms
/// (sound in either case), or both.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Lookups {
    Units,
    Substrings,
    Both,
}

/// The term screen of a regex's facts: each literal's lookups ANDed, `All`
/// where a literal has none. OR branches stay.
pub(super) fn term_plan(facts: &RequiredLiterals, lookups: Lookups) -> Plan {
    match facts {
        RequiredLiterals::All => Plan::All,
        RequiredLiterals::None => Plan::None,
        RequiredLiterals::Literal(bytes) => {
            let mut leaves = Vec::new();
            if lookups != Lookups::Substrings {
                leaves.extend(units::units(bytes).into_iter().map(|unit| match unit {
                    Unit::Exact(term) => Plan::Leaf(Leaf::Term(term)),
                    Unit::Prefix(prefix) => Plan::Leaf(Leaf::Prefix(prefix)),
                }));
            }
            if lookups != Lookups::Units {
                leaves.extend(
                    units::substrings(bytes)
                        .into_iter()
                        .map(|needle| Plan::Leaf(Leaf::Substring(needle))),
                );
            }
            Plan::and(leaves)
        }
        RequiredLiterals::And(parts) => Plan::and(parts.iter().map(|p| term_plan(p, lookups))),
        RequiredLiterals::Or(parts) => Plan::or(parts.iter().map(|p| term_plan(p, lookups))),
    }
}

/// Hashed keys of each literal, as a gram tree the four-rarest rule can
/// select from.
pub(super) fn hashed_need(facts: &RequiredLiterals, variant: Hashed) -> Need {
    let and = |parts: Vec<Need>| {
        let mut flat = Vec::new();
        for part in parts {
            match part {
                Need::All => {}
                Need::None => return Need::None,
                Need::And(inner) => flat.extend(inner),
                other => flat.push(other),
            }
        }
        flat.sort();
        flat.dedup();
        if flat.is_empty() {
            Need::All
        } else {
            Need::And(flat)
        }
    };
    match facts {
        RequiredLiterals::All => Need::All,
        RequiredLiterals::None => Need::None,
        RequiredLiterals::Literal(bytes) => and(variant
            .literal_keys(bytes)
            .into_iter()
            .map(|key| Need::Key(Hashed::encode(key)))
            .collect()),
        RequiredLiterals::And(parts) => {
            and(parts.iter().map(|p| hashed_need(p, variant)).collect())
        }
        RequiredLiterals::Or(parts) => {
            Need::Or(parts.iter().map(|p| hashed_need(p, variant)).collect())
        }
    }
}
