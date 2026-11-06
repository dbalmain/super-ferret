//! Checked retained-hash lookup and count. A checkpoint's columns and their
//! hash ordinal are built once and shared by every view over it; each checked
//! transaction adds only its own changed hashes, in an immutable run.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::format::u32_at;
use crate::overlay::runs::{Row, Runs};
use crate::{DecodeError, DocId, Hash};

/// A checkpoint's dormant section and its rows ordered by hash.
struct Base {
    bytes: Arc<[u8]>,
    by_hash: Vec<u32>,
}
impl Base {
    fn hash(&self, row: u32) -> &[u8] {
        let start = self.by_hash.len() * 4 + row as usize * 16;
        &self.bytes[start..start + 16]
    }

    fn find(&self, hash: Hash) -> Option<DocId> {
        let at = self
            .by_hash
            .binary_search_by(|&row| self.hash(row).cmp(&hash[..]))
            .ok()?;
        Some(DocId(u32_at(&self.bytes, self.by_hash[at] as usize * 4)))
    }
}

/// Retained permissions as of one checked sequence: the checkpoint's rows,
/// overridden by every later transaction's hash changes (`None` revokes).
#[derive(Clone)]
pub(crate) struct State {
    base: Arc<Base>,
    changes: Runs<Hash, Option<DocId>>,
    count: usize,
}
impl State {
    /// Sorts a checked checkpoint section by hash. Writer views warm this at
    /// open, recovery or checkpoint adoption; their transactions inherit the
    /// initialized base without rebuilding it.
    pub(crate) fn new(bytes: Arc<[u8]>) -> Self {
        #[cfg(test)]
        BUILDS.set(BUILDS.get() + 1);
        let count = bytes.len() / 20;
        let hashes = &bytes[count * 4..];
        let mut by_hash: Vec<u32> = (0..count as u32).collect();
        by_hash.sort_unstable_by_key(|&row| &hashes[row as usize * 16..(row as usize + 1) * 16]);
        Self {
            base: Arc::new(Base { bytes, by_hash }),
            changes: Runs::default(),
            count,
        }
    }

    #[cfg(test)]
    pub(crate) fn builds() -> usize {
        BUILDS.get()
    }

    pub(crate) fn count(&self) -> usize {
        self.count
    }

    /// The retained id holding `hash`, if any.
    pub(crate) fn find(&self, hash: Hash) -> Option<DocId> {
        match self.changes.get(&hash) {
            Some(id) => *id,
            None => self.base.find(hash),
        }
    }

    /// Applies one transaction's permission changes: `removed` rows must be
    /// retained now, and `added` hashes must be free once they are gone.
    /// Costs are in the changed rows, never the retained table.
    pub(crate) fn advance(
        &mut self,
        sequence: u64,
        removed: &[(DocId, Hash)],
        added: &[(DocId, Hash)],
    ) -> Result<(), DecodeError> {
        let mut changes = BTreeMap::new();
        for &(id, hash) in removed {
            if self.find(hash) != Some(id) {
                return Err(DecodeError::Corrupt("dormant documents"));
            }
            changes.insert(hash, None);
        }
        for &(id, hash) in added {
            let prior = match changes.get(&hash) {
                Some(prior) => *prior,
                None => self.find(hash),
            };
            if prior.is_some() {
                return Err(DecodeError::Corrupt("duplicate dormant hash"));
            }
            changes.insert(hash, Some(id));
        }
        let count = self
            .count
            .checked_sub(removed.len())
            .and_then(|count| count.checked_add(added.len()))
            .filter(|&count| count <= super::CAP)
            .ok_or(DecodeError::Corrupt("dormant cap"))?;
        self.changes.insert(
            changes
                .into_iter()
                .map(|(key, value)| Row {
                    key,
                    sequence,
                    value,
                })
                .collect(),
        );
        self.count = count;
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    static BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
