//! Epoch-sized seen bits replace retained equal file observations. Parent
//! tokens still require generation and continuing-directory validation at
//! reconciliation. Untouched scopes use the same storage, without asserting
//! that the old file version was observed.
use std::collections::{BTreeMap, BTreeSet};

use crate::{DirToken, InoId, NameId};

#[derive(Default)]
pub(super) struct Seen {
    pub names: Vec<u64>,
    pub inodes: Vec<u64>,
    pub parents: BTreeMap<InoId, DirToken>,
    pub included: BTreeSet<DirToken>,
    pub directories: BTreeSet<InoId>,
    pub count: usize,
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
    pub fn insert(
        &mut self,
        parent: DirToken,
        name: NameId,
        child: Option<InoId>,
        directory: bool,
    ) {
        if let Some(old) = parent.previous_directory() {
            self.parents.insert(old, parent);
        }
        self.count += usize::from(insert(&mut self.names, name.0));
        if let Some(child) = child {
            insert(&mut self.inodes, child.0);
            self.included.insert(parent);
            if directory {
                self.directories.insert(child);
            }
        }
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
