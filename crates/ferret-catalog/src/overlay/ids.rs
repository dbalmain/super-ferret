//! A set of ids in copy-on-write pages: views share the pages they did not
//! change, so cloning a projection per transaction costs a page table, not
//! a bit per id the catalog holds.
use std::sync::Arc;

const PAGE_IDS: usize = 4096;
type Page = [u64; PAGE_IDS / 64];

#[derive(Clone, Default)]
pub(super) struct Ids {
    pages: Vec<Option<Arc<Page>>>,
}
impl Ids {
    pub(super) fn contains(&self, id: u32) -> bool {
        let id = id as usize;
        self.pages
            .get(id / PAGE_IDS)
            .and_then(Option::as_ref)
            .is_some_and(|page| page[id % PAGE_IDS / 64] >> (id % 64) & 1 == 1)
    }
    pub(super) fn insert(&mut self, id: u32) {
        let id = id as usize;
        let at = id / PAGE_IDS;
        if self.pages.len() <= at {
            self.pages.resize(at + 1, None);
        }
        let page = self.pages[at].get_or_insert_with(|| Arc::new([0; PAGE_IDS / 64]));
        Arc::make_mut(page)[id % PAGE_IDS / 64] |= 1 << (id % 64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_exactly_what_was_inserted_across_pages() {
        let mut ids = Ids::default();
        for id in [0, 63, 64, 4095, 4096, 40_000, u32::MAX] {
            ids.insert(id);
        }
        let shared = ids.clone();
        ids.insert(1);
        for id in [0, 63, 64, 4095, 4096, 40_000, u32::MAX, 1] {
            assert!(ids.contains(id), "{id}");
        }
        for id in [2, 62, 65, 4094, 4097, 39_999, 8192, u32::MAX - 1] {
            assert!(!ids.contains(id), "{id}");
        }
        // The clone kept its page: the insert after it copied, not shared.
        assert!(!shared.contains(1));
    }
}
