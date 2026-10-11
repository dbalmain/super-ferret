//! Required literal facts become gram conjunctions. Flatten AND before
//! choosing four rare children, preserve every OR branch, and retain `All`
//! when a branch has no usable gram. This is only a candidate screen.

use std::collections::{BTreeSet, HashMap};

use ferret_verify::trigram::RequiredLiterals;

use super::grams::{Variant, extract};

pub(super) type Lists = HashMap<Box<[u8]>, Vec<u32>>;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Need {
    All,
    None,
    Key(Vec<u8>),
    And(Vec<Need>),
    Or(Vec<Need>),
}

impl Need {
    fn and(parts: impl IntoIterator<Item = Self>) -> Self {
        let mut flat = Vec::new();
        for part in parts {
            match part {
                Self::All => {}
                Self::None => return Self::None,
                Self::And(parts) => flat.extend(parts),
                other => flat.push(other),
            }
        }
        flat.sort();
        flat.dedup();
        if flat.is_empty() {
            Self::All
        } else {
            Self::And(flat)
        }
    }

    pub(super) fn literals(
        facts: &RequiredLiterals,
        variant: Variant,
        weights: &[(u64, u16)],
    ) -> Self {
        match facts {
            RequiredLiterals::All => Self::All,
            RequiredLiterals::None => Self::None,
            RequiredLiterals::Literal(bytes) => {
                let mut keys = BTreeSet::new();
                extract(bytes, variant, weights, |gram| {
                    keys.insert(gram.to_vec());
                });
                Self::and(keys.into_iter().map(Self::Key))
            }
            RequiredLiterals::And(parts) => {
                Self::and(parts.iter().map(|p| Self::literals(p, variant, weights)))
            }
            RequiredLiterals::Or(parts) => Self::Or(
                parts
                    .iter()
                    .map(|p| Self::literals(p, variant, weights))
                    .collect(),
            ),
        }
    }

    fn estimate(&self, lists: &Lists, live: &[u32]) -> usize {
        match self {
            Self::All => live.len(),
            Self::None => 0,
            Self::Key(key) => lists.get(key.as_slice()).map_or(0, Vec::len),
            Self::And(parts) => parts
                .iter()
                .map(|p| p.estimate(lists, live))
                .min()
                .unwrap_or(live.len()),
            Self::Or(parts) => parts.iter().map(|p| p.estimate(lists, live)).sum(),
        }
    }

    pub(super) fn candidates(&self, lists: &Lists, live: &[u32]) -> Vec<u32> {
        match self {
            Self::All => live.to_vec(),
            Self::None => Vec::new(),
            Self::Key(key) => lists.get(key.as_slice()).cloned().unwrap_or_default(),
            Self::And(parts) => {
                let mut ranked: Vec<_> = parts.iter().collect();
                ranked.sort_by_key(|p| (p.estimate(lists, live), *p));
                let mut selected = ranked.into_iter().take(4);
                let Some(first) = selected.next() else {
                    return live.to_vec();
                };
                let mut docs = first.candidates(lists, live);
                for part in selected {
                    let other = part.candidates(lists, live);
                    docs.retain(|doc| other.binary_search(doc).is_ok());
                }
                docs
            }
            Self::Or(parts) => {
                let mut docs: Vec<_> = parts
                    .iter()
                    .flat_map(|p| p.candidates(lists, live))
                    .collect();
                docs.sort_unstable();
                docs.dedup();
                docs
            }
        }
    }
}
