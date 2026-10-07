//! Regex candidate plans over the trigram source. Dictionary estimates choose
//! four children per conjunction before any postings are read; verification
//! remains in ferret-verify. Uncovered documents are added once at the root.

use std::collections::BTreeMap;

use ferret_index::{Atom, Candidates, Certainty, Cursor, Pinned, ReadError};
use ferret_verify::{Grep, Trigrams};

/// A content regex with its selected trigram postings read from the index.
#[derive(Debug)]
pub struct GrepAtom {
    tree: Trigrams,
    lists: Vec<([u8; 3], Option<Candidates>)>,
}

fn df(key: [u8; 3], pinned: &Pinned<'_>, cancelled: &dyn Fn() -> bool) -> Result<u64, ReadError> {
    for source in pinned.sources() {
        if let Some(found) = source.estimate_until(&Atom::Trigram(key), cancelled)? {
            return Ok(found.docs);
        }
    }
    Ok(u64::from(pinned.live().len()))
}

/// Select before estimating: the bound must describe the weakened cursor,
/// rather than constraints discarded by the four-child limit.
fn select(
    tree: &Trigrams,
    frequencies: &BTreeMap<[u8; 3], u64>,
    live: u64,
    cancelled: &dyn Fn() -> bool,
) -> Result<(Trigrams, u64), ReadError> {
    if cancelled() {
        return Err(ReadError::Cancelled);
    }
    Ok(match tree {
        Trigrams::All => (Trigrams::All, live),
        Trigrams::None => (Trigrams::None, 0),
        Trigrams::Key(key) => (tree.clone(), frequencies.get(key).copied().unwrap_or(live)),
        Trigrams::And(parts) | Trigrams::Or(parts) => {
            let mut children = parts
                .iter()
                .map(|part| select(part, frequencies, live, cancelled))
                .collect::<Result<Vec<_>, _>>()?;
            if matches!(tree, Trigrams::And(_)) {
                children.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
                children.truncate(4);
                let bound = children.iter().map(|c| c.1).min().unwrap_or(live).min(live);
                (
                    Trigrams::And(children.into_iter().map(|c| c.0).collect()),
                    bound,
                )
            } else {
                let bound = children
                    .iter()
                    .fold(0u64, |sum, c| sum.saturating_add(c.1))
                    .min(live);
                (
                    Trigrams::Or(children.into_iter().map(|c| c.0).collect()),
                    bound,
                )
            }
        }
    })
}

fn ranked(
    grep: &Grep,
    pinned: &Pinned<'_>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(Trigrams, BTreeMap<[u8; 3], u64>), ReadError> {
    let frequencies = grep
        .planning_trigrams()
        .keys()
        .into_iter()
        .map(|key| Ok((key, df(key, pinned, cancelled)?)))
        .collect::<Result<BTreeMap<_, _>, ReadError>>()?;
    Ok((grep.trigrams_by_frequency(&frequencies), frequencies))
}

impl GrepAtom {
    /// Dictionary-only upper bound, including uncovered documents.
    pub fn estimate_until(
        grep: &Grep,
        pinned: &Pinned<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<u64, ReadError> {
        let (tree, frequencies) = ranked(grep, pinned, cancelled)?;
        Ok(select(
            &tree,
            &frequencies,
            u64::from(pinned.live().len()),
            cancelled,
        )?
        .1
        .saturating_add(u64::from(pinned.uncovered().len()))
        .min(u64::from(pinned.live().len())))
    }

    /// Reads only the selected keys. Missing sources conservatively admit all.
    pub fn read_until(
        grep: Grep,
        pinned: &Pinned<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self, ReadError> {
        Self::read_selected_until(
            &explain(&grep, pinned, cancelled)?.selected,
            pinned,
            cancelled,
        )
    }

    pub(crate) fn read_selected_until(
        tree: &Trigrams,
        pinned: &Pinned<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self, ReadError> {
        let mut lists = Vec::new();
        for key in tree.keys() {
            let mut found = None;
            for source in pinned.sources() {
                if let Some(answer) = source.read_until(&Atom::Trigram(key), cancelled)? {
                    found = Some(answer);
                    break;
                }
            }
            lists.push((key, found));
        }
        Ok(Self {
            tree: tree.clone(),
            lists,
        })
    }

    /// Necessary candidates, always Maybe, with uncovered documents included.
    pub fn cursor_until<'a>(
        &'a self,
        pinned: &'a Pinned<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Cursor<'a>, ReadError> {
        fn compile<'a>(
            tree: &Trigrams,
            lists: &'a [([u8; 3], Option<Candidates>)],
            pinned: &'a Pinned<'_>,
            cancelled: &dyn Fn() -> bool,
        ) -> Result<Cursor<'a>, ReadError> {
            if cancelled() {
                return Err(ReadError::Cancelled);
            }
            Ok(match tree {
                Trigrams::All => Cursor::bits(pinned.live(), Certainty::Maybe),
                Trigrams::None => Cursor::Empty,
                Trigrams::Key(key) => match lists
                    .iter()
                    .find(|(k, _)| k == key)
                    .and_then(|(_, c)| c.as_ref())
                {
                    Some(c) => Cursor::maybe(c.cursor()),
                    None => Cursor::bits(pinned.live(), Certainty::Maybe),
                },
                Trigrams::And(parts) => Cursor::and(
                    parts
                        .iter()
                        .map(|p| compile(p, lists, pinned, cancelled))
                        .collect::<Result<_, _>>()?,
                ),
                Trigrams::Or(parts) => Cursor::or_until(
                    parts
                        .iter()
                        .map(|p| compile(p, lists, pinned, cancelled))
                        .collect::<Result<_, _>>()?,
                    pinned.live().bound(),
                    cancelled,
                )?,
            })
        }
        Ok(pinned.atom(compile(&self.tree, &self.lists, pinned, cancelled)?))
    }
}

/// Dictionary-only details for explaining selected lists before execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepReport {
    pub derived: Trigrams,
    pub selected: Trigrams,
    pub frequencies: Vec<([u8; 3], u64)>,
    pub read_keys: Vec<[u8; 3]>,
}

pub(crate) fn explain(
    grep: &Grep,
    pinned: &Pinned<'_>,
    cancelled: &dyn Fn() -> bool,
) -> Result<GrepReport, ReadError> {
    let (tree, frequencies) = ranked(grep, pinned, cancelled)?;
    let selected = select(
        &tree,
        &frequencies,
        u64::from(pinned.live().len()),
        cancelled,
    )?
    .0;
    let frequencies = frequencies.into_iter().collect();
    let read_keys = selected.keys().into_iter().collect();
    Ok(GrepReport {
        derived: grep.planning_trigrams().clone(),
        selected,
        frequencies,
        read_keys,
    })
}
