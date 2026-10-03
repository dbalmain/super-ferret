//! Immutable sorted runs, shared across views. Geometric carries copy changed
//! rows only; per-row sequences preserve newest wins across unequal-size
//! bursts.
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct Row<K, V> {
    pub(super) key: K,
    pub(super) sequence: u64,
    pub(super) value: V,
}

struct Run<K, V> {
    weight: usize,
    rows: Vec<Row<K, V>>,
}

pub(super) struct Runs<K, V> {
    levels: Vec<Option<Arc<Run<K, V>>>>,
}
impl<K, V> Default for Runs<K, V> {
    fn default() -> Self {
        Self { levels: Vec::new() }
    }
}
impl<K, V> Clone for Runs<K, V> {
    fn clone(&self) -> Self {
        Self {
            levels: self.levels.clone(),
        }
    }
}
impl<K: Ord + Clone, V: Clone> Runs<K, V> {
    pub(super) fn insert(&mut self, mut rows: Vec<Row<K, V>>) {
        if rows.is_empty() {
            return;
        }
        let mut weight = rows.len();
        // Stable sort: the last producer value wins a repeated key within a
        // transaction. Merging carries never resurrects its older value.
        rows.reverse();
        rows.sort_by(|a, b| a.key.cmp(&b.key));
        rows.dedup_by(|a, b| a.key == b.key);
        loop {
            let level = weight.ilog2() as usize;
            if self.levels.len() <= level {
                self.levels.resize_with(level + 1, || None);
            }
            let Some(old) = self.levels[level].take() else {
                self.levels[level] = Some(Arc::new(Run { weight, rows }));
                break;
            };
            weight += old.weight;
            let mut combined = Vec::with_capacity(rows.len() + old.rows.len());
            let (mut a, mut b) = (rows.iter().peekable(), old.rows.iter().peekable());
            while let (Some(x), Some(y)) = (a.peek(), b.peek()) {
                match x.key.cmp(&y.key) {
                    std::cmp::Ordering::Less => {
                        if let Some(row) = a.next() {
                            combined.push(row.clone());
                        }
                    }
                    std::cmp::Ordering::Greater => {
                        if let Some(row) = b.next() {
                            combined.push(row.clone());
                        }
                    }
                    std::cmp::Ordering::Equal => {
                        let a_wins = x.sequence >= y.sequence;
                        let winner = if a_wins { a.next() } else { b.next() };
                        if let Some(row) = winner {
                            combined.push(row.clone());
                        }
                        // Advance whichever side did not win.
                        if a_wins {
                            b.next();
                        } else {
                            a.next();
                        }
                    }
                }
            }
            combined.extend(a.cloned());
            combined.extend(b.cloned());
            rows = combined;
        }
    }
    pub(super) fn get(&self, key: &K) -> Option<&V> {
        self.levels
            .iter()
            .flatten()
            .filter_map(|run| {
                run.rows
                    .binary_search_by(|r| r.key.cmp(key))
                    .ok()
                    .map(|i| &run.rows[i])
            })
            .max_by_key(|r| r.sequence)
            .map(|r| &r.value)
    }
    pub(super) fn latest_range(&self, start: K, end: K) -> Vec<&Row<K, V>> {
        let mut rows: Vec<_> = self.range(start, end).collect();
        rows.sort_by(|a, b| a.key.cmp(&b.key).then_with(|| b.sequence.cmp(&a.sequence)));
        rows.dedup_by(|a, b| a.key == b.key);
        rows
    }
    pub(super) fn range(&self, start: K, end: K) -> impl Iterator<Item = &Row<K, V>> {
        self.levels
            .iter()
            .flatten()
            .flat_map(move |run| {
                let first = run.rows.partition_point(|r| r.key < start);
                let last = run.rows.partition_point(|r| r.key < end);
                run.rows[first..last].iter()
            })
            .filter(|row| {
                self.levels.iter().flatten().all(|run| {
                    run.rows
                        .binary_search_by(|r| r.key.cmp(&row.key))
                        .map_or(true, |i| run.rows[i].sequence <= row.sequence)
                })
            })
    }
    pub(super) fn run_count(&self) -> usize {
        self.levels.iter().flatten().count()
    }
}
