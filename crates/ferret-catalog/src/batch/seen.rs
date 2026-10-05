//! Epoch-sized seen bits replace retained equal file observations. Parent
//! tokens still require generation and continuing-directory validation at
//! reconciliation. Untouched scopes use the same storage, without asserting
//! that the old file version was observed.
use std::collections::BTreeMap;

use crate::{DirToken, InoId, NameId};

#[derive(Default)]
pub(super) struct Seen {
    pub names: Vec<u64>,
    pub inodes: Vec<u64>,
    pub parents: BTreeMap<InoId, (DirToken, bool)>,
    pub count: usize,
    last_parent: Option<DirToken>,
    last_included: bool,
}
fn insert(words: &mut Vec<u64>, id: u32) -> bool {
    let at = id as usize / 64;
    words.resize(words.len().max(at + 1), 0);
    let bit = 1 << (id % 64);
    let new = words[at] & bit == 0;
    words[at] |= bit;
    new
}
impl Seen {
    pub fn insert(&mut self, parent: DirToken, name: NameId, child: Option<InoId>) {
        if self.last_parent != Some(parent) {
            if let Some(old) = parent.previous_directory() {
                self.parents.entry(old).or_insert((parent, false)).0 = parent;
            }
            self.last_parent = Some(parent);
            self.last_included = false;
        }
        self.count += usize::from(insert(&mut self.names, name.0));
        if let Some(child) = child {
            insert(&mut self.inodes, child.0);
            if !self.last_included {
                if let Some(old) = parent.previous_directory() {
                    self.parents.entry(old).or_insert((parent, false)).1 = true;
                }
                self.last_included = true;
            }
        }
    }
    pub fn included(&self) -> impl Iterator<Item = DirToken> + '_ {
        self.parents
            .values()
            .filter_map(|&(parent, included)| included.then_some(parent))
    }
    pub fn ids(&self) -> impl Iterator<Item = NameId> + '_ {
        self.names.iter().enumerate().flat_map(|(at, &word)| {
            let mut remaining = word;
            std::iter::from_fn(move || {
                if remaining == 0 {
                    return None;
                }
                let bit = remaining.trailing_zeros();
                remaining &= remaining - 1;
                Some(NameId(at as u32 * 64 + bit))
            })
        })
    }
}
