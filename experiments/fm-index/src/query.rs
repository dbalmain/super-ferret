//! One query against one segment: count every required literal, locate
//! its occurrences, and combine them per document as the AND/OR tree says.
//!
//! An AND is an intersection of documents. Its lines are the lines of its
//! part with the fewest hits, restricted to those documents: every match lies
//! on one line (when the query is line-sound) and holds every AND part's
//! literal, so that part's hit lines include every match line. An OR is a
//! union of documents and of lines.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use ferret_verify::trigram::RequiredLiterals;

use crate::index::{Fm, Sa, Table};

/// One candidate document: global id, and the offsets of its hit lines'
/// anchors, or `None` for whole-document verification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cand {
    pub doc: u32,
    pub anchors: Option<Vec<u32>>,
}

#[derive(Default, Clone, Copy)]
pub struct Cost {
    pub count: Duration,
    pub locate: Duration,
    pub combine: Duration,
    pub total: Duration,
    pub occurrences: u64,
    pub located: u64,
    pub steps: u64,
    pub capped_leaves: u64,
}

pub enum Index<'a> {
    Sa(&'a Sa),
    Fm(&'a Fm),
}

pub fn leaves(tree: &RequiredLiterals, out: &mut BTreeSet<Vec<u8>>) {
    match tree {
        RequiredLiterals::Literal(b) => {
            out.insert(b.clone());
        }
        RequiredLiterals::And(p) | RequiredLiterals::Or(p) => p.iter().for_each(|p| leaves(p, out)),
        RequiredLiterals::All | RequiredLiterals::None => {}
    }
}

enum Res {
    All,
    Set { docs: Vec<u32>, anchors: Vec<(u32, u32)> },
}

fn eval(tree: &RequiredLiterals, hits: &BTreeMap<Vec<u8>, Option<Vec<(u32, u32)>>>) -> Res {
    match tree {
        RequiredLiterals::All => Res::All,
        RequiredLiterals::None => Res::Set {
            docs: Vec::new(),
            anchors: Vec::new(),
        },
        RequiredLiterals::Literal(b) => match &hits[b] {
            None => Res::All,
            Some(h) => {
                let mut docs: Vec<u32> = h.iter().map(|p| p.0).collect();
                docs.dedup();
                Res::Set {
                    docs,
                    anchors: h.clone(),
                }
            }
        },
        RequiredLiterals::And(parts) => {
            let sets: Vec<_> = parts
                .iter()
                .map(|p| eval(p, hits))
                .filter_map(|r| match r {
                    Res::All => None,
                    Res::Set { docs, anchors } => Some((docs, anchors)),
                })
                .collect();
            if sets.is_empty() {
                return Res::All;
            }
            let mut docs = sets[0].0.clone();
            for (d, _) in &sets[1..] {
                docs.retain(|x| d.binary_search(x).is_ok());
            }
            let rarest = sets.iter().min_by_key(|s| s.1.len()).expect("one");
            let anchors = rarest
                .1
                .iter()
                .copied()
                .filter(|a| docs.binary_search(&a.0).is_ok())
                .collect();
            Res::Set { docs, anchors }
        }
        RequiredLiterals::Or(parts) => {
            let mut docs = Vec::new();
            let mut anchors = Vec::new();
            for p in parts {
                match eval(p, hits) {
                    Res::All => return Res::All,
                    Res::Set { docs: d, anchors: a } => {
                        docs.extend(d);
                        anchors.extend(a);
                    }
                }
            }
            docs.sort_unstable();
            docs.dedup();
            anchors.sort_unstable();
            anchors.dedup();
            Res::Set { docs, anchors }
        }
    }
}

/// The leaves worth locating when only each AND's rarest part is located:
/// counts are cheap, and the rarest part's lines already hold every match
/// line. Returns the estimated occurrences (`None` for unbounded, i.e. `All`).
fn rarest(
    tree: &RequiredLiterals,
    counts: &BTreeMap<Vec<u8>, u64>,
    cap: Option<u64>,
    out: &mut BTreeSet<Vec<u8>>,
) -> Option<u64> {
    match tree {
        RequiredLiterals::All => None,
        RequiredLiterals::None => Some(0),
        RequiredLiterals::Literal(b) => {
            let n = counts[b];
            if cap.is_some_and(|c| n > c) {
                None
            } else {
                out.insert(b.clone());
                Some(n)
            }
        }
        RequiredLiterals::And(parts) => {
            let mut best: Option<(u64, BTreeSet<Vec<u8>>)> = None;
            for p in parts {
                let mut s = BTreeSet::new();
                if let Some(n) = rarest(p, counts, cap, &mut s)
                    && best.as_ref().is_none_or(|b| n < b.0)
                {
                    best = Some((n, s));
                }
            }
            let (n, s) = best?;
            out.extend(s);
            Some(n)
        }
        RequiredLiterals::Or(parts) => {
            let mut total = 0u64;
            let mut s = BTreeSet::new();
            for p in parts {
                total += rarest(p, counts, cap, &mut s)?;
            }
            out.extend(s);
            Some(total)
        }
    }
}

/// FM_DRY=1: count and plan, but locate nothing; `located` then says how
/// many occurrences the plan would locate. Candidates are meaningless.
static DRY: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var("FM_DRY").is_ok());

/// How much of the tree a lookup locates.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Every literal: an AND intersects documents.
    Full,
    /// Each AND's rarest part only, by count; the other parts are `All`.
    Rarest,
}

/// Runs one query on one segment. `cap`: a literal with more occurrences
/// than this is not located and counts as `All`, which only weakens the
/// screen. `lines`: whether line verification is sound for the query.
pub fn run(
    index: Index,
    table: &Table,
    sizes: &[u64],
    tree: &RequiredLiterals,
    lines: bool,
    cap: Option<u64>,
    plan: Plan,
) -> (Vec<Cand>, Cost) {
    let start = Instant::now();
    let mut cost = Cost::default();
    let mut set = BTreeSet::new();
    leaves(tree, &mut set);
    // Count.
    let t = Instant::now();
    let ranges: Vec<(Vec<u8>, (u64, u64))> = set
        .into_iter()
        .map(|lit| {
            let r = match &index {
                Index::Sa(sa) => {
                    let (lo, hi) = sa.range(&lit);
                    (lo as u64, hi as u64)
                }
                Index::Fm(fm) => fm.range(&lit),
            };
            (lit, r)
        })
        .collect();
    cost.count = t.elapsed();
    // Locate.
    let t = Instant::now();
    let wanted = match plan {
        Plan::Full => None,
        Plan::Rarest => {
            let counts = ranges.iter().map(|(l, (lo, hi))| (l.clone(), hi - lo)).collect();
            let mut s = BTreeSet::new();
            rarest(tree, &counts, cap, &mut s);
            Some(s)
        }
    };
    let mut positions: Vec<(Vec<u8>, Option<Vec<u32>>)> = Vec::with_capacity(ranges.len());
    for (lit, (lo, hi)) in ranges {
        let n = hi - lo;
        cost.occurrences += n;
        if wanted.as_ref().is_some_and(|w| !w.contains(&lit)) {
            positions.push((lit, None));
            continue;
        }
        if cap.is_some_and(|c| n > c) {
            cost.capped_leaves += 1;
            positions.push((lit, None));
            continue;
        }
        cost.located += n;
        if *DRY {
            positions.push((lit, None));
            continue;
        }
        let mut out = Vec::with_capacity(n as usize);
        match &index {
            Index::Sa(sa) => sa.positions((lo as usize, hi as usize), &mut out),
            Index::Fm(fm) => {
                for row in lo..hi {
                    let (pos, steps) = fm.locate(row);
                    out.push(pos);
                    cost.steps += steps as u64;
                }
            }
        }
        positions.push((lit, Some(out)));
    }
    cost.locate = t.elapsed();
    // Combine: positions to (document, offset), dropping hits that cross a
    // document's end, then the tree.
    let t = Instant::now();
    let mut hits = BTreeMap::new();
    for (lit, pos) in positions {
        let mapped = pos.map(|mut pos| {
            pos.sort_unstable();
            let mut out = Vec::with_capacity(pos.len());
            let mut d = 0usize;
            for p in pos {
                while table.starts[d + 1] <= p {
                    d += 1;
                }
                if p as u64 + lit.len() as u64 <= table.starts[d + 1] as u64 {
                    out.push((d as u32, p - table.starts[d]));
                }
            }
            out
        });
        hits.insert(lit, mapped);
    }
    let cands = match eval(tree, &hits) {
        Res::All => table
            .docs
            .iter()
            .map(|&doc| Cand { doc, anchors: None })
            .collect(),
        Res::Set { docs, anchors } => {
            let mut out = Vec::with_capacity(docs.len());
            let mut a = 0usize;
            for d in docs {
                while a < anchors.len() && anchors[a].0 < d {
                    a += 1;
                }
                let from = a;
                while a < anchors.len() && anchors[a].0 == d {
                    a += 1;
                }
                let doc = table.docs[d as usize];
                let offs: Vec<u32> = anchors[from..a].iter().map(|x| x.1).collect();
                // Dense hits: the lines cover most of the document anyway.
                let dense = offs.len() as u64 * 256 >= sizes[doc as usize];
                out.push(Cand {
                    doc,
                    anchors: (lines && !dense && !offs.is_empty()).then_some(offs),
                });
            }
            out
        }
    };
    cost.combine = t.elapsed();
    cost.total = start.elapsed();
    (cands, cost)
}

pub fn encode(cands: &[Cand]) -> Vec<u8> {
    let mut out = Vec::new();
    for c in cands {
        out.extend_from_slice(&c.doc.to_le_bytes());
        match &c.anchors {
            None => out.extend_from_slice(&u32::MAX.to_le_bytes()),
            Some(a) => {
                out.extend_from_slice(&(a.len() as u32).to_le_bytes());
                for x in a {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
        }
    }
    out
}

pub fn decode(bytes: &[u8]) -> Vec<Cand> {
    let mut out = Vec::new();
    let mut i = 0;
    let next = |i: &mut usize| {
        let v = crate::index::u32_at(bytes, *i);
        *i += 1;
        v
    };
    while 4 * i < bytes.len() {
        let doc = next(&mut i);
        let n = next(&mut i);
        let anchors = (n != u32::MAX).then(|| (0..n).map(|_| next(&mut i)).collect());
        out.push(Cand { doc, anchors });
    }
    out
}
